//! `cs_android` — the aarch64 Android host for the ChronosSword guest.
//!
//! Everything interesting is in `cs_core`; this crate is the shell around it:
//! it pulls the two payload files out of the APK's assets, sizes the guest
//! screen from the real window, and pumps frames and touches. The whole
//! resulting library is AArch64 — nothing here is 32-bit, and the APK ships a
//! single `arm64-v8a` ABI.
//!
//! The VM runs on the winit thread. That is a deliberate simplification over
//! the worker-thread arrangement a JVM-hosted emulator needs: the guest's
//! memory is one heap allocation rather than a large stack object, and a frame
//! is a bounded amount of work that ends in exactly the blit we then present.

mod audio_out;
#[cfg(target_os = "android")]
mod pad;
#[cfg(target_os = "android")]
mod savedoc;

use cs_core::{Config, Phase, Vm};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey, NativeKey};
use winit::window::{Window, WindowId};

/// Files bundled in the APK under `assets/`.
pub const ASSET_GUEST: &str = "libchronosMian.so";
pub const ASSET_VFS: &str = "data.vfs.mp3";

/// The original renders at 15 fps on a feature phone and its own WIPI timers
/// pace the game; we tick at 60 and let `timerLoop` decide what actually
/// advances, which is what `CletEntry.onDrawFrame` did too. Presenting is a
/// separate question — see `frame_dirty` in `tick_and_present`: the picture
/// only changes on the game's own ~21 Hz tick, so most of these host frames
/// end without touching the display.
///
/// `CS_HOST_FPS` moves the tick rate (default 60). Lowering it trades timer
/// jitter — the game's timers fire up to one host frame late — for fewer
/// wakeups, which is a lever for a phone that runs hot even idle.
fn host_frame() -> Duration {
    static F: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        let fps: u64 = std::env::var("CS_HOST_FPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| option_env!("CS_HOST_FPS").and_then(|v| v.parse().ok()))
            .unwrap_or(60)
            .clamp(10, 120);
        Duration::from_micros(1_000_000 / fps)
    })
}

/// Whether a guest access outside the mapped window ends the process or is
/// survived and reported. On by default here; see the `Config` field.
///
/// Same shape as `host_frame` above and for the same reason: an APK has no
/// environment, so the value has to be able to come from the build.
fn tolerate_faults() -> bool {
    std::env::var("CS_FAULT_TOLERANT")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .or_else(|| option_env!("CS_FAULT_TOLERANT").and_then(|v| v.parse().ok()))
        .unwrap_or(1)
        != 0
}

/// What a hardware key means to the guest.
///
/// ── Why the arrows become digits ────────────────────────────────────────────
///
/// This is a keypad game. `GameFrame_gamekey+0x1f4`'s jump table is the whole
/// of in-game hardware input, and in it **the WIPI arrows -1..-4 do nothing at
/// all**: walking is `2/4/6/8` (`HERO_Key_DirectKeyPress`), the six skill slots
/// are `1/3/7/9/0/*`, `5`/OK is attack-talk-pickup, CLEAR opens the in-game
/// menu, `-6` the world map, `-7` the sell shop, `-10` an immediate save. The
/// feature phone's number pad *was* the D-pad; the Android port added a touch
/// stick beside it, not a key path.
///
/// Which digit is which direction is the keypad's own geometry, and the guest
/// says so itself: `HERO_Key_Convert_Dir`'s jump table sends `'2'` to the same
/// arm as WIPI UP (-1), `'8'` to DOWN (-2), `'4'` to LEFT (-3) and `'6'` to
/// RIGHT (-4) — `2` sits above `5` on a phone keypad. The first build shipped
/// `Up→'8'`/`Down→'2'` and the player report was exactly what that predicts:
/// the pad's Y axis walked inverted while X was fine. The claim that justified
/// it — "'2' and DOWN are pixel-identical on the main menu" — was measured on
/// a screen where `MainMenuFrame_KeyPressed+0xb00` sends `'2'` **and** `'8'`
/// to the same toggle, so it said nothing about orientation; on the 3x3 menu
/// (`+0x1c98`) `'2'` falls into the -1 (UP) arm, same as in-game. One mapping,
/// keypad-oriented, covers menus and the world alike ('6' already enters the
/// bag from the game menu). Pinned against the guest's own table by
/// `the_pad_walk_digits_mean_what_the_stick_arrows_mean` in `cs_core`.
///
/// ── Except while the keyboard is up ─────────────────────────────────────────
///
/// `clsidAuthHandle+0x104` hands every event to the IME while
/// `_midas_internalImeInstance[0x40]` is set, and there a digit is a digit: a
/// D-pad press would type into the pet's name or the ID field. So the arrows
/// stay arrows while that byte is set, which is also the only thing they can
/// usefully be there.
///
/// ── Where the button codes come from ────────────────────────────────────────
///
/// winit's Android backend forwards every `KeyEvent` regardless of source, so a
/// pad's D-pad already arrives as `NamedKey::Arrow*` and its face buttons as
/// `Unidentified(Android(96..110))` — `KEYCODE_BUTTON_A` and friends. Sticks
/// are not `KeyEvent`s, but GameActivity's motion filter passes only
/// touchscreen sources, and an unhandled joystick motion is exactly what makes
/// the framework synthesise D-pad keys from one, so those land here as well.
fn wipi_key(key: &Key, ime_open: bool) -> Option<i32> {
    // ASCII: the guest's jump table is indexed by the character itself.
    Some(match key {
        Key::Named(NamedKey::GoBack) | Key::Named(NamedKey::Escape) => cs_core::KEY_CLEAR,
        Key::Named(NamedKey::ArrowUp) => {
            if ime_open {
                cs_core::KEY_UP
            } else {
                b'2' as i32
            }
        }
        Key::Named(NamedKey::ArrowDown) => {
            if ime_open {
                cs_core::KEY_DOWN
            } else {
                b'8' as i32
            }
        }
        Key::Named(NamedKey::ArrowLeft) => {
            if ime_open {
                cs_core::KEY_LEFT
            } else {
                b'4' as i32
            }
        }
        Key::Named(NamedKey::ArrowRight) => {
            if ime_open {
                cs_core::KEY_RIGHT
            } else {
                b'6' as i32
            }
        }
        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => cs_core::KEY_SELECT,
        // A USB keyboard on the bench, so the keypad can be exercised without a
        // pad in hand. Same codes the pad produces.
        Key::Character(s) => match s.as_bytes() {
            [c @ (b'0'..=b'9' | b'*' | b'#')] => *c as i32,
            _ => return None,
        },
        Key::Unidentified(NativeKey::Android(code)) => match code {
            96 => b'5' as i32,        // A     attack / talk / pick up
            97 => cs_core::KEY_CLEAR, // B     in-game menu, and back out of one
            99 => b'#' as i32,        // X     the game's other action key
            100 => -6,                // Y     world map
            108 => -10,               // START   save
            109 => -7,                // SELECT  sell shop
            // The shoulders and stick clicks are the six skill slots. Which
            // one each fires comes from `cs_core::pad::SLOTS`, the same table
            // the badge drawn on that slot's button reads back through
            // `pos_for_key` — one table, so the badge cannot come to label a
            // button that does something else.
            _ => cs_core::pad::key_for_android(*code)?,
        },
        _ => return None,
    })
}

struct App {
    guest: Vec<u8>,
    vfs: Arc<Vec<u8>>,
    storage: PathBuf,
    /// Owns the output stream; dropping it silences the game.
    audio: audio_out::Audio,

    window: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    /// Dimensions the surface was last configured to. The VM's framebuffer is
    /// sized once at boot; if the surface later disagrees (a rotation in
    /// flight, split-screen, a fold), presenting must rescale rather than
    /// memcpy rows of the wrong stride.
    surf_w: u32,
    surf_h: u32,
    vm: Option<Vm>,
    /// Set once a fault has been reported, so a broken guest logs once instead
    /// of once per frame.
    dead: bool,
    /// The guest called `CletEntry.exitApp()` (the 종료 menu item). The original
    /// finished the Activity; without acting on it the game just sits there
    /// logging the request every frame.
    quit: bool,
    next_frame: Instant,
    /// Rolling frame/instruction counters for the periodic performance line.
    /// This is the only way to know what the interpreter is actually achieving
    /// on a given phone, which is the number that decides whether more guest
    /// functions need host implementations.
    stat_since: Instant,
    stat_frames: u32,
    stat_steps: u64,
    /// Where the frame time actually goes. Splitting these apart is the only
    /// way to tell "the interpreter is slow" from "presenting is slow", and on
    /// this app they are very different problems.
    t_guest: Duration,
    t_copy: Duration,
    t_present: Duration,
    stat_reports: u32,
    /// Frames left with the sampling profiler armed, and whether the player
    /// asked for it (`CS_PROFILE`) rather than a stall arming it.
    ///
    /// The counters on the slow-frame line say what *kind* of work a stall was
    /// — and on the boss-death stall they answered "not drawing, not DES, not
    /// asset loading", which is progress but not a function name. The profiler
    /// gives the name, and costs 40% of interpreter throughput, so it cannot
    /// simply be left on.
    ///
    /// A stall that matters lasts several frames (the measured one ran six),
    /// so arming *after* the first slow frame still catches the rest of it
    /// while costing exactly nothing during normal play. The histogram is
    /// cleared before each armed frame, which makes a dump that frame alone
    /// rather than a running average — per-frame attribution is the whole
    /// point, since the frames either side are ordinary.
    profile_armed: u32,
    profile_sticky: bool,
    /// Present even though the frame says nothing changed. Set whenever the
    /// surface is (re)created or resized — its buffers hold garbage until the
    /// next full write — and cleared by the present that satisfies it.
    force_present: bool,
    /// Direction keys are re-shaped before the guest sees them: this game's
    /// walk is a latch and a single stray release cancels it, so a held
    /// direction must arrive as exactly one press. See [`cs_core::pad`].
    pad: cs_core::pad::Pad,
    /// Whether the guest's IME overlay was up as of the last time we looked.
    /// The arrows change meaning across that edge — while it is up they go to
    /// the keyboard as `-1..-4` and never reach the conditioner — so a gesture
    /// in flight has to be dropped at the boundary rather than left to a
    /// release that will never come. See `Self::ime_edge`.
    ime_up: bool,
    /// Frames of the current stat window that actually reached the display.
    /// The gap between this and `stat_frames` is the display work `CS_LAZY_GL`
    /// removed, which is the number that says what the skip is worth on this
    /// phone.
    stat_shown: u32,
    /// When to next ask Android whether a gamepad is attached, and the last
    /// answer. The poll (`pad::gamepad_connected`, every `PAD_POLL`) feeds
    /// `Vm::gamepad_hud`, which is what makes the pad legend appear on the
    /// world screen while a controller is plugged in and vanish when it is
    /// pulled — there is no connect/disconnect event to be had without a Java
    /// listener class, so staleness is bounded by the poll instead.
    pad_next: Instant,
    pad_connected: bool,
    /// The phone's document screens behind the 세이브 내보내기 / 불러오기
    /// rows. `None` when `CsSave.java` could not be resolved, in which case
    /// every request is answered as failed and the row says so. See
    /// [`savedoc`].
    #[cfg(target_os = "android")]
    savedoc: Option<savedoc::SaveDoc>,
    /// Fingers currently down: winit's raw id, one slot each, with the last
    /// position seen. Slot index is what the guest is told.
    ///
    /// The in-game joystick re-anchors to wherever the finger lands, and it
    /// tracks which finger owns it in a **signed byte**: `onHandleEvent+0x408`
    /// does `strb id, [mainFrame, #0x89]` on press, and the release arm at
    /// `+0x138` clears the anchor only if `(s8)mainFrame[0x89] == id`. If that
    /// compare never matches — a press whose release never arrives, or an id
    /// the low byte cannot represent — `mainFrame[0x88]` stays set, the anchor
    /// block becomes unreachable and the pad is stuck at its last position for
    /// the rest of the session: a joystick that no longer follows the touch.
    ///
    /// Slot indices make the round trip exact by construction, and the release
    /// replay in `resumed` covers the other half: winit delivers no release for
    /// a finger that was down when the activity was backgrounded.
    touch_ids: Vec<Option<(u64, i32, i32)>>,
}

impl App {
    /// Map a raw touch id onto a small stable slot, freeing it on release. See
    /// [`App::touch_ids`] for why the guest cannot be given the raw one.
    ///
    /// An id that was never pressed (a move or release the app did not see the
    /// start of, which happens across a pause) takes a slot too, so its release
    /// still carries the same number its move did.
    fn touch_slot(&mut self, raw: u64, x: i32, y: i32, release: bool) -> i32 {
        let slot = match self.touch_ids.iter().position(|v| v.is_some_and(|(r, ..)| r == raw)) {
            Some(i) => i,
            None => match self.touch_ids.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    self.touch_ids.push(None);
                    self.touch_ids.len() - 1
                }
            },
        };
        self.touch_ids[slot] = if release { None } else { Some((raw, x, y)) };
        // A phone will not report more than a handful at once, but the guest
        // stores this in a signed byte, so it must never exceed 0x7f.
        (slot & 0x7f) as i32
    }

    /// Tell the guest every finger still down has lifted.
    ///
    /// Backgrounding the activity mid-touch loses the release, and the guest
    /// has state that only a matching release clears — see [`App::touch_ids`].
    /// Replaying at the last position is what the finger would have produced.
    fn release_all_touches(&mut self) {
        let held: Vec<(usize, i32, i32)> = self
            .touch_ids
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|(_, x, y)| (i, x, y)))
            .collect();
        self.touch_ids.iter_mut().for_each(|v| *v = None);
        let Some(vm) = self.vm.as_mut() else { return };
        for (slot, x, y) in held {
            let _ = vm.touch_event(cs_core::MV_POINTER_RELEASE, x, y, (slot & 0x7f) as i32);
        }
    }

    /// Build the VM against the real window size. Deferred until `resumed`
    /// because the guest is told its screen geometry exactly once, in
    /// `nativeInitDisplay`, and it keeps the framebuffer it allocates there.
    fn boot(&mut self, w: u32, h: u32) {
        if self.vm.is_some() || self.dead {
            return;
        }
        let cfg = Config {
            audio: self.audio.mixer.clone(),
            vfs: self.vfs.clone(),
            storage: self.storage.clone(),
            screen_w: w as i32,
            screen_h: h as i32,
            virtual_clock: false,
            // Match the original app exactly: it scales by min(w/400, h/240)
            // with no rounding, which on a 2400x1080 phone is 4.5x and fills
            // the full height. Snapping to an integer 4x (crisper, but a
            // smaller picture with wider bars) is what made this build look
            // letterboxed next to the original.
            integer_scale: false,
            // The logical width below is floored, so the uniform scale always
            // fills the full height and the drawn width comes out at most a
            // couple of pixels short of the surface. This stretch covers that
            // remainder — sub-pixel per row, invisible — so no aspect ratio
            // leaves a black edge on either axis.
            fill_width: true,
            // ── Widescreen, without distorting or cropping anything ──────────
            //
            // The original hands the guest 400x240 and letterboxes it, which on
            // a 20:9 phone leaves a third of the display black. But that size is
            // an *argument* to `nativeInitDisplay`, not a constant inside the
            // game: the guest allocates its framebuffer from it, reports it back
            // through `MC_grpGetDisplayInfo`, and — verified by running at
            // 533x240 — centres its own UI on whatever it is told.
            //
            // So the logical screen is made to match the display's aspect at the
            // game's design height. 2400x1080 gives 533x240, an exact 20:9: the
            // picture fills the surface edge to edge, pixels stay square, and
            // the player simply sees more of the scene to the left and right
            // instead of black bars.
            // Floored, not rounded: wipi_w/240 must not exceed the surface's
            // aspect, so the height-filling scale is always the smaller one.
            // Rounding up would ask for a picture wider than the screen, which
            // the present step can only letterbox — flooring leaves at most a
            // fraction of a logical pixel for fill_width to stretch over.
            wipi_w: ((240.0 * w as f64 / h.max(1) as f64).floor() as i32).clamp(240, 1024),
            wipi_h: cs_core::WIPI_H,
            host: Default::default(),
            // Decode clips on a worker: a one-minute Vorbis track costs 100 ms
            // or more here, and the guest asks for one on the frame a dialogue
            // ends — holding the lock the audio callback also wants.
            async_audio: true,
            // ── Do not close the game over the guest's own bad pointer ───────
            //
            // Two reports on an S25, 96 seconds apart, identical to the
            // instruction: `MONSTER_getCastingTime` loading through a map
            // object whose payload pointer (`obj+0x18`) held `7`. The player
            // was hammering SKIP on the way into a boss fight; what they saw
            // was the app disappearing, because a fault ended the run and the
            // run ending exits the process.
            //
            // Nothing about that read can hurt the host — it returned 0 and the
            // interpreter carried on — and the guest handles the 0 by itself:
            // `MONSTER_getCastingTime` answers -1, `CHAR_getVisible` answers
            // "invisible", and `MONSTER_preProcess` returns before touching
            // anything else. So the corrupt object goes inert and the game
            // keeps running, which is what a player wants from a bug that the
            // original phone would have crashed on too.
            //
            // The fault is not swallowed: every distinct site is logged with
            // registers and a backtrace, written to `cs_crash.txt`, and counted
            // in the ten-second perf line. `CS_FAULT_TOLERANT=0` restores the
            // old behaviour for anyone bisecting one of these.
            tolerate_faults: tolerate_faults(),
        };
        let mut vm = match Vm::new(&self.guest, cfg) {
            Ok(v) => v,
            Err(e) => {
                log::error!("guest load failed: {e}");
                self.dead = true;
                return;
            }
        };
        log::info!("guest loaded: {} symbols, {} imports", vm.elf.symbols.len(), vm.imports.len());
        #[cfg(target_os = "android")]
        if let Some(d) = self.savedoc.as_ref() {
            vm.savexfer.utc_offset_min = d.utc_offset_min;
        }
        // Opt-in, via `adb shell setprop debug.chronos.profile 1` before
        // launching (read here through the environment the app was started
        // with, so `CS_PROFILE=1` works too). It is genuinely useful — it names
        // the guest function costing frames in the scene the player is actually
        // in — but sampling is not free, and leaving it on cost 40% of
        // interpreter throughput on a Galaxy S21.
        if std::env::var("CS_PROFILE").is_ok_and(|v| v != "0") {
            log::info!("guest profiling enabled");
            vm.profile = Some(Default::default());
            self.profile_sticky = true;
        }
        if let Err(e) = vm.boot() {
            log::error!("guest boot failed after {} instructions: {e}", vm.steps);
            self.dead = true;
            return;
        }
        log::info!(
            "boot ok in {} guest instructions; surface {w}x{h}, scale {:.2}, margins {},{}",
            vm.steps,
            vm.magnification,
            vm.margin_x,
            vm.margin_y
        );
        // A pad that was already plugged in when the app launched: the poll
        // may have run before there was a VM to tell.
        vm.gamepad_hud = self.pad_connected;
        self.vm = Some(vm);
    }

    fn configure_surface(&mut self, w: u32, h: u32) {
        let (Some(ctx), Some(win)) = (self.context.as_ref(), self.window.as_ref()) else {
            return;
        };
        if self.surface.is_none() {
            match Surface::new(ctx, win.clone()) {
                Ok(s) => self.surface = Some(s),
                Err(e) => {
                    log::error!("Surface::new failed: {e}");
                    return;
                }
            }
        }
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else {
            return;
        };
        if let Some(s) = self.surface.as_mut() {
            if let Err(e) = s.resize(nw, nh) {
                log::error!("surface resize failed: {e}");
            } else {
                self.surf_w = w;
                self.surf_h = h;
                // Fresh or resized buffers hold garbage; the next tick must
                // present even if the game's picture has not changed.
                self.force_present = true;
            }
        }
    }

    /// The guest went outside its address space (or ran a frame's worth of
    /// budget without returning), which is where a run of this port ends.
    ///
    /// It used to end by setting `dead` and returning, and that is worse than it
    /// sounds: `tick_and_present` then returns immediately for ever, the last
    /// frame stays on the screen, no further input does anything, and the player
    /// is left looking at a game that has simply stopped — with the diagnosis
    /// sitting in a logcat buffer they will never read. "Frozen on a black
    /// screen" is what that looks like when the fault lands during a fade.
    ///
    /// So: write the report where the player can actually reach it, and then let
    /// the process go. Restarting loses nothing that was not already lost — the
    /// VM's memory is inconsistent by definition at this point — and it gets the
    /// game back to its last autosave in one tap instead of none.
    fn crashed(&mut self, what: &str, err: &str) {
        if self.dead {
            return;
        }
        self.dead = true;
        let steps = self.vm.as_ref().map_or(0, |v| v.steps);
        log::error!("guest fault during {what} after {steps} instructions: {err}");
        write_report(
            &self.storage,
            &format!("guest fault during {what}\nafter {steps} instructions\n\n{err}\n"),
        );
        self.quit = true;
    }

    /// The save rows' traffic with the phone's document screens: an answer
    /// that arrived from Java goes to the VM, and a new request goes to Java.
    /// A request that cannot reach Java is answered as failed here, so the
    /// row shows its own failure message rather than waiting forever.
    #[cfg(target_os = "android")]
    fn save_doc_flush(&mut self) {
        let Some(vm) = self.vm.as_mut() else { return };
        let answers = SAVE_DOC_RESULT.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
        for (id, code) in answers {
            vm.save_doc_done(id as u32, cs_core::savexfer::DocStatus::from_code(code));
        }
        if let Some(req) = vm.take_save_doc_request() {
            let sent = self.savedoc.as_ref().is_some_and(|d| d.request(&req));
            if !sent {
                vm.save_doc_done(req.id(), cs_core::savexfer::DocStatus::Failed);
            }
        }
    }

    #[cfg(not(target_os = "android"))]
    fn save_doc_flush(&mut self) {
        if let Some(vm) = self.vm.as_mut() {
            if let Some(req) = vm.take_save_doc_request() {
                vm.save_doc_done(req.id(), cs_core::savexfer::DocStatus::Failed);
            }
        }
    }

    /// Keep `Vm::gamepad_hud` matching what is actually plugged in. Runs at
    /// most once per `PAD_POLL`; the HUD itself is drawn (or not) by the next
    /// `GameFrame_drawGameUI`, so end-to-end latency is one poll plus one game
    /// render tick — about half a second either way.
    #[cfg(target_os = "android")]
    fn poll_gamepad(&mut self) {
        let now = Instant::now();
        if now < self.pad_next {
            return;
        }
        self.pad_next = now + PAD_POLL;
        let connected = pad::gamepad_connected();
        if connected != self.pad_connected {
            self.pad_connected = connected;
            log::info!("gamepad {}", if connected { "attached; HUD on" } else { "gone; HUD off" });
        }
        if let Some(vm) = self.vm.as_mut() {
            vm.gamepad_hud = connected;
        }
    }

    /// Hand one conditioned direction edge to the guest.
    ///
    /// An associated function rather than a method so it can be called while
    /// `self.vm` is already borrowed — the conditioner lives on `self` and the
    /// clock it needs lives on the VM.
    fn pad_emit(vm: &mut Vm, emit: cs_core::pad::Emit) -> Result<(), String> {
        match emit {
            cs_core::pad::Emit::None => Ok(()),
            cs_core::pad::Emit::Press(c) => vm.key_event(cs_core::MV_KEY_PRESS, c, 0),
            cs_core::pad::Emit::Release(c) => vm.key_event(cs_core::MV_KEY_RELEASE, c, 0),
        }
    }

    /// The keyboard overlay just went up, or just came down.
    ///
    /// Either way the conditioner's idea of what is held is void: while the
    /// overlay is up `wipi_key` turns a direction into `-1..-4` and it goes
    /// straight to the guest, so the release that ends a gesture started
    /// before the overlay never reaches [`cs_core::pad`]. Left there, that
    /// stale entry makes the *next* release of any direction fall back to it,
    /// and the hero walks off unbidden.
    ///
    /// On the way up the gesture is dropped but the walk the guest was told
    /// about is remembered; on the way down it is paid off with the release it
    /// is owed. Not the other way round — a key event while the overlay is up
    /// belongs to the guest's IME (`clsidAuthHandle+0x104`), which would read
    /// our release as its own.
    ///
    /// An associated function for the same reason as `pad_emit`, and called
    /// from both the key path and the frame path: from the key path so a press
    /// arriving in the same breath as the overlay closing is not undone by the
    /// reconcile, and from the frame path so an overlay that opens and closes
    /// with no keys in between is still noticed.
    fn ime_edge(pad: &mut cs_core::pad::Pad, was: &mut bool, now: bool) -> cs_core::pad::Emit {
        if now == *was {
            return cs_core::pad::Emit::None;
        }
        *was = now;
        if now {
            pad.forget();
            cs_core::pad::Emit::None
        } else {
            pad.release_all()
        }
    }

    fn tick_and_present(&mut self) {
        #[cfg(target_os = "android")]
        self.poll_gamepad();
        if self.dead {
            return;
        }
        // A direction let go of is released here rather than on the event, so
        // that the framework re-latching an axis cancels itself out.
        if let Some(vm) = self.vm.as_mut() {
            // The keyboard overlay may have opened or closed since the last
            // frame with no key event in between.
            let ime = vm.ime_open();
            let emit = Self::ime_edge(&mut self.pad, &mut self.ime_up, ime);
            if let Err(e) = Self::pad_emit(vm, emit) {
                self.crashed("key", &e);
                return;
            }
            // `now_ms` and not `wall_now_ms`: this is a 60 ms debounce, so it
            // wants the monotonic process clock, not an epoch that can jump.
            let now = vm.now_ms();
            let emit = self.pad.tick(now);
            if let Err(e) = Self::pad_emit(vm, emit) {
                self.crashed("key", &e);
                return;
            }
        }
        self.save_doc_flush();
        let Some(vm) = self.vm.as_mut() else { return };

        // Armed by an earlier slow frame. Clearing here rather than after the
        // dump is what makes the histogram below this frame's work alone.
        if self.profile_armed > 0 && !self.profile_sticky {
            if let Some(p) = vm.profile.as_mut() {
                p.clear();
            }
        }
        let t0 = Instant::now();
        let before = FrameWork::of(vm);
        if let Err(e) = vm.frame() {
            self.crashed("frame", &e);
            return;
        }
        // A fault the run survived (`Config::tolerate_faults`). It goes in the
        // same file as a fatal one, because the file is the only channel that
        // reaches a player who is not plugged into adb — and this file is
        // exactly how the `MONSTER_getCastingTime` pointer was found. One entry
        // per distinct site; the run loop does not offer a second.
        if let Some(report) = vm.fault_report.take() {
            let steps = vm.steps;
            write_report(
                &self.storage,
                &format!("tolerated guest fault\nafter {steps} instructions\n\n{report}\n"),
            );
        }
        let took = t0.elapsed();
        self.t_guest += took;
        // One frame that runs long is invisible in the ten-second average above
        // — that average is what says "the game runs at 90 fps" while a player
        // is reporting a stall — so a frame over the threshold reports itself,
        // with what it *did* rather than only how long it took.
        //
        // The deltas are the diagnosis. A spike with `declined` climbing is
        // guest drawing code this port does not serve, and its size says how
        // much; a spike with `screen`/`rot` climbing is the host blitters being
        // asked to draw far more than usual, which is a different problem with
        // a different fix. Costing nothing on a normal frame is why this is a
        // subtraction of counters that were already being kept, and not the
        // sampling profiler.
        if took >= slow_frame_threshold() {
            let d = FrameWork::of(vm).since(&before);
            // `rot` in milliseconds as well as calls, because the two answer
            // different questions: a frame whose guest instructions do not
            // account for its wall time is being spent on the host side, and
            // the rotation blitter is the only host patch that can take long
            // enough to see. `blur` for the reason its counter exists.
            log::warn!(
                "slow frame: {} ms, {} guest inst; \
                 +{} screen, +{} rot ({:.1} ms), +{} imgrot ({:.1} ms), +{} blits, +{} fills, +{} fnt, \
                 +{} des, +{} blur, +{} radial ({:.1} ms), +{} sleep ({} ms), +{} declined",
                took.as_millis(),
                d.steps,
                d.screen,
                d.rot,
                d.rot_ns as f64 / 1e6,
                d.imgrot,
                d.imgrot_ns as f64 / 1e6,
                d.blit,
                d.fill,
                d.fnt,
                d.des,
                d.blur,
                d.radial,
                d.radial_ns as f64 / 1e6,
                d.sleep,
                d.sleep_ms,
                d.declined
            );
            // Only when an earlier slow frame armed this one, so the histogram
            // is known to hold this frame and nothing else. Under `CS_PROFILE`
            // it is a running average instead, which the ten-second report
            // already prints.
            if self.profile_armed > 0 && !self.profile_sticky {
                for (name, _, pct) in vm.profile_report(6) {
                    log::warn!("  slow frame hot {pct:5.1}%  {name}");
                }
            }
            if !self.profile_sticky {
                if vm.profile.is_none() {
                    vm.profile = Some(Default::default());
                }
                self.profile_armed = PROFILE_ARM_FRAMES;
            }
        } else if self.profile_armed > 0 && !self.profile_sticky {
            self.profile_armed -= 1;
            if self.profile_armed == 0 {
                vm.profile = None;
            }
        }
        if vm.phase == Phase::Exited && !self.quit {
            log::info!("guest asked to exit; closing");
            self.quit = true;
        }

        self.stat_frames += 1;
        let elapsed = self.stat_since.elapsed();
        if elapsed >= Duration::from_secs(10) {
            let secs = elapsed.as_secs_f64();
            let steps = vm.steps - self.stat_steps;
            let n = self.stat_frames.max(1) as f64;
            log::info!(
                "perf: {:.1} fps ({:.1} shown), {:.1} M guest inst/s ({:.2} M/frame);                  per frame: guest {:.1} ms, copy {:.1} ms, present {:.1} ms;                  host {} screen, {} rot, {} blits, {} fills, {} des, {} radial, {} vfs, {} sleeps ({} ms), {} declined, {} faults",
                self.stat_frames as f64 / secs,
                self.stat_shown as f64 / secs,
                steps as f64 / secs / 1e6,
                steps as f64 / n / 1e6,
                self.t_guest.as_secs_f64() * 1000.0 / n,
                self.t_copy.as_secs_f64() * 1000.0 / n,
                self.t_present.as_secs_f64() * 1000.0 / n,
                // `screen` first: `DM_DrawScreen565*` is now the busiest patch by
                // an order of magnitude, and a run where it stays at zero while
                // `declined` climbs is the signature of a guest this port does
                // not match.
                vm.screen_host,
                vm.rot_host,
                vm.blit_host,
                vm.fill_host,
                vm.des_host,
                vm.radial_host,
                vm.vfs_host,
                vm.sleep_host,
                vm.sleep_ms,
                vm.patch_declined,
                vm.faults
            );
            self.stat_reports += 1;
            if self.stat_reports % 3 == 0 && vm.profile.is_some() {
                for (name, _, pct) in vm.profile_report(6) {
                    log::info!("  hot {pct:5.1}%  {name}");
                }
                vm.profile = Some(Default::default());
            }
            if let Ok(m) = self.audio.mixer.lock() {
                let (loaded, sounding) = m.stats();
                log::info!(
                    "audio: {} decoded ({} undecodable), {} loaded, {} sounding, {} KB",
                    m.decoded_clips,
                    m.decode_failures,
                    loaded,
                    sounding,
                    m.bytes() / 1024
                );
            }
            // The guest heap alongside the audio line above, because the two
            // are the only things in this process that grow with play. A crash
            // that leaves no report — the kind Android's low-memory killer
            // produces — is diagnosed from the last of these lines and nothing
            // else, and "which of the two was climbing" is the whole question.
            log::info!(
                "mem: guest heap {} KB live, {} KB peak of {} MB",
                vm.heap.live_bytes / 1024,
                vm.heap.peak_bytes / 1024,
                vm.heap.total() / 1024 / 1024
            );
            self.stat_since = Instant::now();
            self.stat_frames = 0;
            self.stat_shown = 0;
            self.stat_steps = vm.steps;
            self.t_guest = Duration::ZERO;
            self.t_copy = Duration::ZERO;
            self.t_present = Duration::ZERO;
        }

        // The game only makes a new picture on its own ~21 Hz tick; on every
        // other host frame the framebuffer holds byte-for-byte what was last
        // presented (`gl::Gl::end_frame`), and copying + presenting it again
        // is two full-surface passes of identical pixels — sustained memory
        // traffic that shows up as heat on a phone, and as nothing at all on
        // the screen.
        if !vm.frame_dirty && !self.force_present {
            return;
        }
        self.stat_shown += 1;

        let t1 = Instant::now();
        let Some(surface) = self.surface.as_mut() else { return };
        let Ok(mut buf) = surface.buffer_mut() else { return };
        let (fb, fw, fh) = vm.framebuffer();
        // The guest framebuffer is created at the window size, so the normal
        // case is a straight copy. If the surface disagrees — a rotation in
        // flight, split-screen, a fold — a row-blind memcpy would smear the
        // picture diagonally (the strides differ), so scale it instead. The
        // guest keeps its geometry either way: it was told its screen exactly
        // once, at nativeInitDisplay.
        let (bw, bh) = (self.surf_w as usize, self.surf_h as usize);
        if bw * bh == buf.len() && fw > 0 && fh > 0 {
            if bw == fw as usize && bh == fh as usize {
                let n = buf.len();
                buf.copy_from_slice(&fb[..n]);
            } else {
                for y in 0..bh {
                    let sy = y * fh as usize / bh;
                    let src = &fb[sy * fw as usize..(sy + 1) * fw as usize];
                    let dst = &mut buf[y * bw..(y + 1) * bw];
                    for (x, d) in dst.iter_mut().enumerate() {
                        *d = src[x * fw as usize / bw];
                    }
                }
            }
        }
        self.t_copy += t1.elapsed();

        let t2 = Instant::now();
        match buf.present() {
            Ok(()) => self.force_present = false,
            Err(e) => log::error!("present failed: {e}"),
        }
        self.t_present += t2.elapsed();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes();
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                log::error!("create_window failed: {e}");
                return;
            }
        };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        match Context::new(window.clone()) {
            Ok(c) => self.context = Some(c),
            Err(e) => {
                log::error!("softbuffer Context::new failed: {e}");
                return;
            }
        }
        self.window = Some(window);
        self.configure_surface(w, h);
        self.boot(w, h);
        // A finger that was down when the activity went away never sent its
        // release, and the guest is still holding state that waits for one.
        self.release_all_touches();

        if let Some(vm) = self.vm.as_mut() {
            let _ = vm.resume();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + host_frame()));
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // Android destroys the native window on suspend; drop everything that
        // borrows it. The VM keeps running state and its framebuffer, so a
        // resume picks up where it left off.
        if let Some(vm) = self.vm.as_mut() {
            // Java stops dispatching, so a still-held direction would leave
            // the hero walking into a wall for the rest of the session.
            let emit = self.pad.release_all();
            let _ = Self::pad_emit(vm, emit);
            let _ = vm.pause();
        }
        self.surface = None;
        self.context = None;
        self.window = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.quit {
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = now + host_frame();
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.tick_and_present(),
            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                self.configure_surface(w, h);
                self.boot(w, h);
            }
            WindowEvent::Touch(t) => {
                let released = matches!(t.phase, TouchPhase::Ended | TouchPhase::Cancelled);
                let (x, y) = (t.location.x as i32, t.location.y as i32);
                let id = self.touch_slot(t.id, x, y, released);
                let Some(vm) = self.vm.as_mut() else { return };
                let ev = match t.phase {
                    TouchPhase::Started => cs_core::MV_POINTER_PRESS,
                    TouchPhase::Moved => cs_core::MV_POINTER_MOVE,
                    TouchPhase::Ended | TouchPhase::Cancelled => cs_core::MV_POINTER_RELEASE,
                };
                if let Err(e) = vm.touch_event(ev, x, y, id) {
                    self.crashed("touch", &e);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(vm) = self.vm.as_mut() else { return };
                let ime = vm.ime_open();
                // Before anything else: if the overlay went up or came down
                // since the last look, settle that first, so this event is
                // conditioned against the right state rather than undone by
                // the next frame's reconcile.
                let emit = Self::ime_edge(&mut self.pad, &mut self.ime_up, ime);
                if let Err(e) = Self::pad_emit(vm, emit) {
                    self.crashed("key", &e);
                    return;
                }
                let Some(code) = wipi_key(&event.logical_key, ime) else { return };
                let down = event.state == ElementState::Pressed;
                // Walking directions go through the conditioner; everything
                // else — attack, the six skill slots, menu, save — is passed
                // straight through. The guest's key mailbox holds one press
                // per tick, so re-asserting a direction on top of a skill
                // press would eat the skill.
                if cs_core::pad::is_direction(code) && !ime {
                    let now = vm.now_ms();
                    let emit = self.pad.key(code, down, event.repeat, now);
                    if let Err(e) = Self::pad_emit(vm, emit) {
                        self.crashed("key", &e);
                    }
                    return;
                }
                let kind = if down { cs_core::MV_KEY_PRESS } else { cs_core::MV_KEY_RELEASE };
                if let Err(e) = vm.key_event(kind, code, 0) {
                    self.crashed("key", &e);
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(vm) = self.vm.as_mut() {
                    let _ = vm.pause();
                }
                event_loop.exit();
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use winit::event_loop::EventLoop;
    use winit::platform::android::EventLoopBuilderExtAndroid;

    // Everything at warn, our own crate (the cdylib is named `main`) at info.
    // The guest's `MC_knlPrintk` output arrives through `main` too, and it is
    // chatty — the intro alone emits a font-conversion notice per frame — so
    // debug stays off unless someone is looking for it.
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            // `cs_core` at info too, otherwise every diagnostic the runtime has
            // — `CS_NET_DEBUG`, `CS_VFS_DEBUG`, `CS_ROT_DEBUG`, the patch
            // install lines — is silently dropped on the device, which is the
            // one place they are needed.
            .with_filter(
                android_logger::FilterBuilder::new().parse("warn,main=info,cs_core=info").build(),
            )
            .with_tag("chronos"),
    );
    let Some(guest) = read_asset(&app, ASSET_GUEST) else {
        log::error!("bundled asset {ASSET_GUEST} is missing; aborting");
        return;
    };
    let Some(vfs) = read_asset(&app, ASSET_VFS) else {
        log::error!("bundled asset {ASSET_VFS} is missing; aborting");
        return;
    };
    let Some(data_dir) = app.internal_data_path() else {
        log::error!("no internal_data_path; aborting");
        return;
    };
    log::info!(
        "assets: guest {} KB, vfs {} MB; storage {data_dir:?}",
        guest.len() / 1024,
        vfs.len() / 1024 / 1024
    );

    // The guest writes its save (`kickass`) straight into the storage root, the
    // same shape as /data/data/co.kr.eamobile.chronosMian/files on the original.
    let storage = data_dir.join("files");

    // Now that there is somewhere to put it, a panic leaves a note behind as
    // well as a log line. Bug reports for this app arrive as a sentence typed by
    // a player — "it sometimes force-closes when I use that skill" — and without
    // a file on their phone there is no way to turn one into an address.
    {
        let where_to = storage.clone();
        std::panic::set_hook(Box::new(move |info| {
            log::error!("PANIC: {info}");
            write_report(&where_to, &format!("panic: {info}\n"));
        }));
    }
    // Resolved before the event loop takes the app handle: it needs the
    // activity's class loader, which `ndk_context` already holds. And before
    // the staged save below, which may have something to tell the player.
    let savedoc = savedoc::SaveDoc::new();
    // A save the 세이브 불러오기 / 되돌리기 rows staged before the last close,
    // swapped in — or an earlier apply finished — before anything below reads
    // a file of the player's: the chest, the potions and the identity are all
    // read by `Vm::new` (cs_core::savebundle). A panic in there is a blocked
    // start too: it stopped somewhere in the middle, and only the next start
    // can tell where.
    let report = std::panic::catch_unwind(|| {
        cs_core::savebundle::apply_pending(&storage, cs_core::savebundle::wall_ms())
    })
    .unwrap_or_else(|_| cs_core::savebundle::Report {
        outcome: cs_core::savebundle::Outcome::Blocked,
        log: vec!["savexfer: applying the staged save panicked".to_string()],
    });
    for line in &report.log {
        log::info!("{line}");
    }
    let blocked = !report.may_start();
    match report.outcome {
        cs_core::savebundle::Outcome::Blocked => {
            write_report(&storage, &format!("save apply blocked the start:\n{}\n", report.log.join("\n")));
            let told = savedoc.as_ref().is_some_and(|d| d.notice(SAVE_BLOCKED_TITLE, SAVE_BLOCKED, true));
            if !told {
                // Nothing to show it on: closing is the honest answer, and the
                // next launch tries again. A black screen would not be.
                log::error!("savexfer: the start is blocked and there is no way to say so; closing");
                std::process::exit(0);
            }
        }
        cs_core::savebundle::Outcome::NotApplied => {
            if let Some(d) = savedoc.as_ref() {
                d.notice(SAVE_NOT_APPLIED_TITLE, SAVE_NOT_APPLIED, false);
            }
        }
        cs_core::savebundle::Outcome::Idle | cs_core::savebundle::Outcome::Applied => {}
    }

    // NB: `EventLoop::new()` must never be called on Android — it panics with
    // "An `AndroidApp` as passed to android_main() is required". The app handle
    // has to go in through the builder.
    let event_loop = EventLoop::builder()
        .with_android_app(app)
        .build()
        .expect("failed to build the android event loop");

    let mut handler = App {
        guest,
        vfs: Arc::new(vfs),
        storage,
        audio: audio_out::Audio::start(),
        window: None,
        context: None,
        surface: None,
        surf_w: 0,
        surf_h: 0,
        vm: None,
        // A blocked start keeps the loop running, so the notice can be read
        // and its button can close the app, and never boots the guest.
        dead: blocked,
        quit: false,
        next_frame: Instant::now(),
        stat_since: Instant::now(),
        stat_frames: 0,
        stat_steps: 0,
        t_guest: Duration::ZERO,
        t_copy: Duration::ZERO,
        t_present: Duration::ZERO,
        stat_reports: 0,
        profile_armed: 0,
        profile_sticky: false,
        force_present: true,
        pad: Default::default(),
        ime_up: false,
        stat_shown: 0,
        pad_next: Instant::now(),
        pad_connected: false,
        savedoc,
        touch_ids: Vec::new(),
    };
    // Catching here is not about carrying on — the panic hook above has already
    // logged and filed it, and there is nothing sensible to resume into. It is
    // about reaching the `process::exit` below.
    //
    // A panic unwinding out of `android_main` skips that exit, and the comment
    // there explains what a surviving process does to the next launch: the app
    // becomes permanently unstartable, on every launch, until Android reaps it.
    // So the difference this makes to a player who hits a rare panic mid-fight
    // is "the game closed, reopen it" instead of "the game will not open again".
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Err(e) = event_loop.run_app(&mut handler) {
            log::error!("event loop exited with error: {e}");
        }
    }));
    if ran.is_err() {
        log::error!("the event loop panicked; closing the process rather than leaving it wedged");
    }

    // Insurance only: a 비약 is written the moment it is bought, and what is
    // stored is an absolute expiry, so there is nothing about "when the app
    // closed" left to record. This costs one `write` on a clean exit and
    // covers the case where a future purchase path forgets to save.
    if let Some(vm) = handler.vm.as_mut() {
        let storage = vm.cfg.storage.clone();
        vm.potions.save(&storage);
    }

    // Returning from `android_main` does NOT end the process — the GameActivity
    // glue just lets this thread finish and leaves the Java Activity and the
    // whole VM alive. That is fatal here, because the next launch reuses the
    // surviving process and calls `android_main` again, where
    // `EventLoop::build` refuses with `RecreationAttempt` (winit allows exactly
    // one event loop per process). The `.expect` above then panics, the
    // activity dies before it draws, and since the process STILL does not go
    // away it happens again on every launch: the app becomes permanently
    // unstartable until Android reaps the process.
    //
    // That is how the 데이터 관리 report reproduced. Nothing there corrupts the
    // save — the guest simply reaches `CletEntry.exitApp()` when the server
    // handshake fails, exactly as the 종료 menu item does, and *that* is what
    // wedged the process. Verified on an S25 Ultra: after "guest asked to exit;
    // closing" the pid was still alive 24 s later, and relaunching logged
    // `PANIC: failed to build the android event loop: RecreationAttempt`.
    //
    // Exiting here is safe for the save: `cs_core::files` writes through a
    // plain `std::fs::File` with no `BufWriter`, so every byte the guest wrote
    // is already in the kernel, not in a Rust-side buffer that skipping
    // destructors would drop.
    log::info!("android_main returning; terminating the process");
    std::process::exit(0);
}

/// How often to re-ask Android whether a gamepad is attached. Half a second
/// keeps "unplug the pad, the HUD goes away" feeling immediate while costing a
/// handful of in-process JNI calls per poll.
#[cfg(target_os = "android")]
const PAD_POLL: Duration = Duration::from_millis(500);

/// Frames the sampling profiler stays armed after a slow one.
///
/// Long enough to cover a stall that keeps going — the boss-death stall this
/// was built for runs five heavy frames back to back after a 1.4 s gap, and
/// re-arming on each slow frame means the window follows it for as long as it
/// lasts. Short enough that an isolated hitch costs the 40% sampling penalty
/// for a fifth of a second and then stops.
const PROFILE_ARM_FRAMES: u32 = 12;

/// How long a single frame has to run before it reports itself.
///
/// 80 ms is about two and a half frames' budget — well past anything the game
/// does normally (the whole probe replay to the first town has two frames over
/// 33 ms, and both are boot and a monster load), and low enough to catch a
/// stall long before it is long enough for Android to consider the app hung.
/// `CS_SLOW_FRAME_MS` moves it; 0 turns the line off.
fn slow_frame_threshold() -> Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let ms = *MS.get_or_init(|| {
        std::env::var("CS_SLOW_FRAME_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| option_env!("CS_SLOW_FRAME_MS").and_then(|v| v.parse().ok()))
            .unwrap_or(80)
    });
    if ms == 0 {
        Duration::MAX
    } else {
        Duration::from_millis(ms)
    }
}

/// The work counters, sampled either side of a frame so a slow one can say what
/// it was doing. All of these are already maintained by `cs_core`; this only
/// reads them.
#[derive(Clone, Copy)]
struct FrameWork {
    steps: u64,
    screen: u64,
    rot: u64,
    rot_ns: u64,
    imgrot: u64,
    imgrot_ns: u64,
    blit: u64,
    fill: u64,
    fnt: u64,
    des: u64,
    blur: u64,
    radial: u64,
    radial_ns: u64,
    sleep: u64,
    sleep_ms: u64,
    declined: u64,
}

impl FrameWork {
    fn of(vm: &Vm) -> Self {
        Self {
            steps: vm.steps,
            screen: vm.screen_host,
            rot: vm.rot_host,
            rot_ns: vm.rot_ns,
            imgrot: vm.imgrot_host,
            imgrot_ns: vm.imgrot_ns,
            blit: vm.blit_host,
            fill: vm.fill_host,
            fnt: vm.fnt_host,
            des: vm.des_host,
            blur: vm.blur_host,
            radial: vm.radial_host,
            radial_ns: vm.radial_ns,
            sleep: vm.sleep_host,
            sleep_ms: vm.sleep_ms,
            declined: vm.patch_declined,
        }
    }

    fn since(&self, then: &Self) -> Self {
        Self {
            steps: self.steps - then.steps,
            screen: self.screen - then.screen,
            rot: self.rot - then.rot,
            rot_ns: self.rot_ns - then.rot_ns,
            imgrot: self.imgrot - then.imgrot,
            imgrot_ns: self.imgrot_ns - then.imgrot_ns,
            blit: self.blit - then.blit,
            fill: self.fill - then.fill,
            fnt: self.fnt - then.fnt,
            des: self.des - then.des,
            blur: self.blur - then.blur,
            radial: self.radial - then.radial,
            radial_ns: self.radial_ns - then.radial_ns,
            sleep: self.sleep - then.sleep,
            sleep_ms: self.sleep_ms - then.sleep_ms,
            declined: self.declined - then.declined,
        }
    }
}

/// Where a crash report is left for the player to fetch.
///
/// Next to the save, because that is the directory that already exists and the
/// one `adb pull`-shaped instructions already point at.
const CRASH_FILE: &str = "cs_crash.txt";

/// Cap on the file. Crashes that repeat every launch would otherwise fill the
/// player's storage, and the first one is the one worth having anyway.
const CRASH_FILE_MAX: u64 = 256 * 1024;

/// Answers from the document screens the save rows opened, `(request, status)`,
/// waiting for the winit thread ([`App::save_doc_flush`], once a frame). A
/// queue, not a slot: the rows wait for one request at a time, but a request
/// they gave up on may still answer, and that must not overwrite the answer
/// of the one after it.
#[cfg(target_os = "android")]
static SAVE_DOC_RESULT: std::sync::Mutex<Vec<(i32, i32)>> = std::sync::Mutex::new(Vec::new());

/// What a start says about a staged save it could not apply. Java strings, so
/// UTF-8 here — the guest's EUC-KR is not involved. A blocked start is not
/// only a half-applied save: a stage or a journal whose removal could not be
/// made durable blocks it too (cs_core::savebundle), so the wording covers
/// both — what was not written, and why playing on would be worse.
#[cfg(target_os = "android")]
const SAVE_BLOCKED_TITLE: &str = "게임을 시작하지 않습니다";
#[cfg(target_os = "android")]
const SAVE_BLOCKED: &str = "세이브를 불러오거나 되돌리는 데 필요한 내용을 저장소에 쓰지 못했습니다.\n\n\
    이대로 시작하면 세이브가 섞이거나, 나중에 그 사이 플레이한 내용이 덮어써질 수 있습니다. \
    저장 공간을 확인한 뒤 앱을 다시 켜면 이어서 처리합니다.";
#[cfg(target_os = "android")]
const SAVE_NOT_APPLIED_TITLE: &str = "세이브를 적용하지 못했습니다";
#[cfg(target_os = "android")]
const SAVE_NOT_APPLIED: &str = "불러온 세이브를 적용하지 못해 이전 세이브 그대로 시작합니다.";

/// `CsSave.nativeDocResult` — any Java thread; it only relays. The frame
/// loop polls the queue at 60 Hz, so no wake-up is needed.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_kr_chronos_sword_CsSave_nativeDocResult(
    _env: *mut jni::sys::JNIEnv,
    _class: jni::sys::jclass,
    id: jni::sys::jint,
    status: jni::sys::jint,
) {
    if let Ok(mut r) = SAVE_DOC_RESULT.lock() {
        // Bounded by the requests made, one answer each; the cap only keeps a
        // misbehaving caller from growing it without end.
        if r.len() < 64 {
            r.push((id, status));
        }
    }
}

/// Append a crash report, best effort and panic-free.
///
/// Called from the panic hook, so every failure here is swallowed: a panic
/// inside a panic hook aborts the process before the log line is flushed, which
/// would destroy the very evidence this exists to keep.
fn write_report(storage: &std::path::Path, body: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(storage);
    let path = storage.join(CRASH_FILE);
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > CRASH_FILE_MAX {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(f, "---- {stamp} ----\n{body}");
}

#[cfg(target_os = "android")]
fn read_asset(app: &winit::platform::android::activity::AndroidApp, name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let manager = app.asset_manager();
    let cname = std::ffi::CString::new(name).ok()?;
    let mut asset = manager.open(&cname)?;
    let mut buf = Vec::new();
    asset.read_to_end(&mut buf).ok()?;
    Some(buf)
}

