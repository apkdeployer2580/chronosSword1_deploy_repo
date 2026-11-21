//! ChronosSword native runtime.
//!
//! The shipped game is `lib/armeabi/libchronosMian.so`: 887 KB of ARMv5TE code
//! (2734 named functions) that EA Mobile Korea produced by compiling the
//! original WIPI-C clet against their "midas" porting layer. There is no source,
//! and no tool turns ARM32 machine code into ARM64 machine code, so a literal
//! 32-to-64-bit rebuild of that library does not exist.
//!
//! What does exist is a very small seam. The library imports exactly 79 symbols
//! — libc, three libm functions, and nineteen GLES 1.x calls — exposes fourteen
//! JNI entry points, and reaches back into Java through about fifty static
//! methods on one class. Everything else, including the entire WIPI `MC_*` API,
//! is inside the binary. So this crate runs the original code on an ARMv5TE
//! interpreter inside a pure AArch64 process and implements that seam natively.
//! The game logic is the shipped binary, bit for bit; the host around it is
//! 64-bit.
//!
//! ```text
//!   Android arm64-v8a app  (cs_android)      headless probe  (cs_probe)
//!                     \                     /
//!                      +---- cs_core ------+
//!                      | Vm: cpu, memory,  |
//!                      | HLE libc/GL/JNI   |
//!                      +--------+----------+
//!                               |  ARMv5TE
//!                      libchronosMian.so (guest, unmodified)
//! ```

pub mod audio;
pub mod cave;
pub mod des;
pub mod elf;
pub mod files;
pub mod fmt;
pub mod font;
pub mod gl;
pub mod heap;
pub mod hle;
pub mod idclaim;
pub mod identity;
pub mod joy;
pub mod mem;
pub mod net;
pub mod netitem;
pub mod pad;
pub mod patch;
pub mod potions;
pub mod recipe2;
pub mod savebundle;
pub mod savexfer;
pub mod vault;

use arm32_cpu::{reg, Cpu, Mode};
use std::path::PathBuf;
use std::sync::Arc;

use crate::elf::Elf;
use crate::files::Files;
use crate::gl::Gl;
use crate::heap::Heap;
use crate::mem::{
    GuestMem, HEAP_BASE, HEAP_SIZE, IMAGE_BASE, RETURN_MAGIC, STACK_BASE, STACK_SIZE,
};
use arm32_cpu::Memory as _;

/// The WIPI logical screen this title was authored for. Comes from
/// `chronosMian`'s activity constructor: `SKTActivity(…, 400, 240, …, 2.0f, …)`.
pub const WIPI_W: i32 = 400;
pub const WIPI_H: i32 = 240;

/// WIPI event codes (`co.kr.eamobile.constant.WIPIConstant`).
pub const MV_KEY_PRESS: i32 = 2;
pub const MV_KEY_RELEASE: i32 = 3;
pub const MV_POINTER_PRESS: i32 = 19;
pub const MV_POINTER_RELEASE: i32 = 20;
pub const MV_POINTER_MOVE: i32 = 21;

/// WIPI key codes, as `WIPIConstant.getWIPIKeyCode` maps them.
pub const KEY_UP: i32 = -1;
pub const KEY_DOWN: i32 = -2;
pub const KEY_LEFT: i32 = -3;
pub const KEY_RIGHT: i32 = -4;
pub const KEY_SELECT: i32 = -5;
pub const KEY_CLEAR: i32 = -16;

pub struct Config {
    /// Contents of `assets/data.vfs.mp3` — the game's whole asset archive,
    /// despite the extension.
    pub vfs: Arc<Vec<u8>>,
    /// Where save files go. The guest builds absolute paths under this.
    pub storage: PathBuf,
    /// Physical surface size in pixels.
    pub screen_w: i32,
    pub screen_h: i32,
    /// Use a virtual clock instead of the wall clock. The guest idles on WIPI
    /// timers, so a headless run that does not advance time renders nothing —
    /// the same trap the Hero3 probe documents.
    pub virtual_clock: bool,
    /// Snap the scale factor to a whole number when one fits. 400x240 upscaled
    /// by a non-integer factor with GL_NEAREST shimmers badly on pixel art.
    pub integer_scale: bool,
    /// The logical screen handed to `nativeInitDisplay`.
    ///
    /// The original always says 400x240 (from `chronosMian`'s activity
    /// constructor), but it is an *argument*, not a constant in the binary: the
    /// guest allocates its framebuffer from it and reports it back through
    /// `MC_grpGetDisplayInfo`. Whether the game's own layout follows is an
    /// empirical question — see `--wipi` in the probe.
    pub wipi_w: i32,
    pub wipi_h: i32,
    /// Widen the picture to fill the surface.
    ///
    /// The game is 400x240 (5:3) and a modern phone in landscape is around
    /// 20:9, so a uniform scale — what the original does — always leaves black
    /// bars at the sides. With this on, the picture is stretched horizontally
    /// to the full width: no bars, at the cost of characters being about 33%
    /// wider than drawn. Vertical scale and all game logic are untouched.
    pub fill_width: bool,
    /// What the game is told about the device it is running on.
    pub host: HostInfo,
    /// Software mixer. Shared because the guest fills it from the game thread
    /// while the audio device drains it from its own callback. The host creates
    /// it so it can hold the other end.
    pub audio: std::sync::Arc<std::sync::Mutex<audio::Mixer>>,
    /// Decode clips on a worker thread instead of inline.
    ///
    /// A one-minute Vorbis track costs 55-140 ms to decode, and the guest asks
    /// for one at the moment a dialogue ends — on the frame thread, holding the
    /// mixer lock the audio callback also wants. That is a visible hitch and an
    /// audible glitch at once. On the device this is on; the headless probe
    /// leaves it off so `--audio-wav` stays reproducible.
    pub async_audio: bool,
    /// Survive a guest access outside the mapped window instead of ending the
    /// run with an error.
    ///
    /// The guest's own bugs reach us as faults the original hardware would
    /// also have taken — `MONSTER_getCastingTime` dereferencing a map object
    /// whose payload pointer has been overwritten with `7`, seen twice on an
    /// S25 while SKIP was being hammered on the way into a boss fight. On the
    /// phone the difference is not "fault or no fault", it is "the game closed"
    /// or "one monster is inert": the read already returned 0 and the write was
    /// already dropped, so continuing costs nothing the host can be hurt by,
    /// and the guest's own null paths take over — that monster reads as
    /// invisible and `MONSTER_preProcess` returns early.
    ///
    /// Off here, and on only in `cs_android`. The probe must keep faulting:
    /// every A/B in this repo compares instruction counts against a run that
    /// is supposed to be identical, and a tolerated fault is exactly the kind
    /// of divergence those runs exist to catch.
    pub tolerate_faults: bool,
}

/// Values the guest asks about the handset, through `CletEntry`.
#[derive(Clone, Debug)]
pub struct HostInfo {
    pub model: String,
    pub manufacturer: String,
    pub os_version: i32,
    pub gles_version: String,
}

impl Default for HostInfo {
    fn default() -> Self {
        Self {
            model: "SM-G991N".into(),
            manufacturer: "samsung".into(),
            os_version: 30,
            gles_version: "OpenGL ES 3.2".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Loaded,
    Running,
    Exited,
}

pub struct Vm {
    pub cpu: Cpu,
    pub mem: GuestMem,
    pub heap: Heap,
    pub gl: Gl,
    pub files: Files,
    pub elf: Elf,
    pub cfg: Config,

    /// Trampoline id -> imported symbol name.
    pub imports: Vec<String>,
    pub jni: hle::JniState,

    pub phase: Phase,
    pub clock_ms: i64,
    clock_origin: std::time::Instant,
    /// This install's minted number. Stable across runs, and still what
    /// `MC_knlGetSystemProperty("PHONENUMBER")` answers unless the player has
    /// claimed an ID *and* `CS_ID_AS_PHONE` is on. See `identity.rs`.
    pub identity: String,
    /// The same install seen as "who am I called" rather than "what number was
    /// minted" — the chosen name, whether the server agreed to it, and how many
    /// times the first-run prompt has been offered.
    pub ident: identity::Identity,
    /// The worker that talks to `cs_server`'s control channel. Started lazily:
    /// a player who never opens the ID screen never spawns a thread.
    pub(crate) claimer: Option<idclaim::Claimer>,
    /// `wall_now_ms` when the outstanding claim was submitted, or `None`.
    /// Comparing against this is the 30-second give-up — a timer would need the
    /// frame thread to own one, and this needs no state at all.
    pub(crate) claim_since: Option<i64>,
    /// Guest buffer holding `patch::DATA_MANAGE_MSG`, allocated on first use.
    data_manage_msg: Option<u32>,
    /// The 「내 ID」 tile's two strings — its label and its popup — allocated on
    /// first use and kept. Both answer button presses, and a fresh block per
    /// press would leak one per press.
    pub(crate) my_id_label: Option<u32>,
    pub(crate) my_id_text: Option<u32>,
    /// The letter screen's two replacement strings, allocated on first use.
    pub(crate) letter_ask_msg: Option<u32>,
    pub(crate) letter_none_msg: Option<u32>,
    /// The recipient as it stood the instant before the guest erased it, so
    /// re-opening the field can offer it back. Refreshed from the guest's own
    /// storage on every open; never carried across screens.
    pub(crate) letter_last_to: Vec<u8>,
    /// The 「내 ID」 popup is the one on screen, so a touch on its text means
    /// "change it". Cleared as soon as that popup goes away.
    pub(crate) my_id_popup: bool,
    /// Reused guest buffers: one for whatever `setPopUp` is saying, one for
    /// `MX_imeGetString` to fill.
    pub(crate) popup_msg: Option<u32>,
    pub(crate) popup_len: usize,
    pub(crate) ime_scratch: Option<u32>,
    /// Our keyboard is up and `onCloseIME_MainFrame` belongs to us this once.
    pub(crate) id_pending: bool,
    /// Scratch guest words handed out to imports that must return a pointer
    /// (`__errno`, `strerror`).
    pub scratch: u32,
    pub errno_word: u32,

    /// Screen geometry the Java side used to compute, mirrored here.
    pub magnification: f32,
    pub margin_x: i32,
    pub margin_y: i32,
    pub auto_margin_y: i32,

    /// Instruction budget consumed by the current `run`, for diagnostics.
    pub steps: u64,
    /// Guest accesses outside the mapped window that were survived rather than
    /// fatal. See [`Config::tolerate_faults`].
    pub faults: u64,
    /// One entry per faulting PC already reported, so a fault the guest repeats
    /// every frame costs one log line instead of sixty a second. Capped at
    /// [`FAULT_SITES_MAX`]; past that the count still climbs.
    fault_sites: std::collections::HashSet<u32>,
    /// Full report for a fault site seen for the first time, for the host to
    /// file next to the crash log. Taken, not read.
    pub fault_report: Option<String>,
    /// Guest word watched for writes (`CS_WATCH_MEM`), or 0. Its previous value
    /// is kept so the run loop can name the instruction that changed it.
    watch_addr: u32,
    watch_last: u32,
    pub log: Vec<String>,
    /// Log every HLE call. Driven by `CS_TRACE=1` so a failing device build can
    /// be turned up without a rebuild.
    pub trace: bool,
    /// Guest PC histogram, sampled every `PROFILE_PERIOD` instructions. `None`
    /// unless profiling was asked for.
    pub profile: Option<std::collections::HashMap<u32, u64>>,
    /// Instructions left before the next profile sample.
    ///
    /// A countdown, not `steps % PERIOD`: a 64-bit modulo costs tens of cycles
    /// and this runs once per guest instruction. Leaving it in — with profiling
    /// enabled by default on the device build — cost about 40% of interpreter
    /// throughput (46 -> 27 M inst/s, measured on a Galaxy S21).
    profile_countdown: u64,

    /// Guest functions served by `patch.rs`, and the address window they fall
    /// in. The window lets the run loop reject the common case with two
    /// compares instead of scanning the table on every guest instruction.
    patches: Vec<(u32, patch::Patch)>,
    patch_lo: u32,
    patch_hi: u32,
    /// The code-cave features that went in at boot (see `cave.rs`).
    pub cave_installed: Vec<&'static str>,
    /// 외형 칸: a guest item carrying just the donor's kind, and the
    /// look-to-kind answers already found. Both are pure caches — the
    /// table lives in the cave and the templates in the archive.
    pub skin_buf: u32,
    pub skin_kinds: Vec<((u8, u8, i16), (u8, u8))>,
    /// 외형 칸의 정보 패널: the donor as a whole item, and which one it is.
    pub skin_item: u32,
    pub skin_item_of: Option<(u8, u8)>,
    /// `[base, bss_end)` of the code cave once it is in, for patches that
    /// match on a return address (`host_menu_arrow_hit`).
    pub cave_range: Option<(u32, u32)>,
    /// How many calls each patch served, and how many it declined. A zero
    /// count on a running game means the patch never installed.
    pub blit_host: u64,
    pub fill_host: u64,
    pub vfs_host: u64,
    pub rot_host: u64,
    pub rot_ns: u64,
    pub screen_host: u64,
    /// `DM_DrawImageRotateEx` served, and the wall clock it took.
    ///
    /// Its own pair rather than folded into `rot_*` because the two answer
    /// different questions on the one frame that raises them: the rotate
    /// blitters run on every frame that draws a sprite, this one runs only
    /// while a boss-kill zoom is on screen. Timed as well as counted for the
    /// same reason `radial_ns` is — the blend is host work that the guest
    /// instruction count cannot see at all once it is served.
    pub imgrot_host: u64,
    pub imgrot_ns: u64,
    pub des_host: u64,
    pub fnt_host: u64,
    /// Box blurs served, and radial blurs served.
    ///
    /// The two most expensive calls in the game get counters of their own
    /// because neither is visible otherwise: both cost more than the rest of
    /// their frame put together (55 M and 60 M guest instructions), and both
    /// have a decline path that adds 1 to `patch_declined` and nothing else,
    /// which is indistinguishable from noise. Separate rather than pooled
    /// because "which blur ran" is exactly the question a boss-death frame
    /// raises, and a shared counter cannot answer it.
    pub blur_host: u64,
    pub radial_host: u64,
    /// Wall-clock nanoseconds spent inside the radial blur, like `rot_ns`.
    ///
    /// The one host cost that is not explained by the guest instruction count,
    /// and now the one that decides whether the retimed zoom below is a
    /// flourish or a stutter — it runs for the whole of `CS_BOSS_ZOOM_MS`
    /// instead of five frames, so its per-frame cost is the budget.
    pub radial_ns: u64,
    /// Per-pixel polar map for the radial blur, and its two working buffers.
    /// See `patch::RadialMap`: all three survive the frame because the effect
    /// now asks for them a hundred times rather than five.
    radial_map: Option<patch::RadialMap>,
    radial_px: Vec<u16>,
    radial_out: Vec<u8>,
    /// The boss-kill zoom's retime — see `patch::host_boss_zoom`.
    ///
    /// `zoom_ms` is `CS_BOSS_ZOOM_MS` resolved at install (0 when the hook is
    /// not installed at all), `zoom_t0` when the running effect started,
    /// `zoom_last` the counter we last handed the guest — which is how a fresh
    /// effect is told from a continuing one — and `zoom_fine` the same value in
    /// eighths, which is what `host_radial_blur` scales its ray length by.
    zoom_ms: i32,
    zoom_t0: i64,
    zoom_last: i32,
    zoom_fine: i32,
    zoom_frames: u32,
    /// Whether a zoom is on screen right now, and which of the shipped five
    /// world ticks has been let through — see `patch::host_zoom_world`, which
    /// holds `GameFrame_Process` shut in between so the scene slows with the
    /// zoom rather than running on underneath it.
    zoom_running: bool,
    zoom_world_bucket: i32,
    zoom_world_due: i32,
    /// Which of the five the blur has already been applied for. The blur
    /// compounds — see `host_radial_blur` — so the count of applications is
    /// part of what the effect looks like, and it stays at the shipped five
    /// however long the animation is stretched over.
    zoom_blur_done: i32,
    zoom_blur_due: i32,
    /// How many applications the effect gets and how strong each one is —
    /// `CS_BOSS_ZOOM_STEPS` / `CS_BOSS_ZOOM_STRENGTH`, resolved at install. The
    /// two are one dial between them: the blur compounds, so more applications
    /// is smoother *and* heavier. 100 leaves each one exactly what the shipped
    /// code draws for its counter.
    zoom_steps: i64,
    pub zoom_strength: i32,
    /// How many times the world is let through — `CS_BOSS_ZOOM_TICKS`. Five is
    /// the shipped count; see `patch::boss_zoom_ticks`.
    zoom_ticks: i64,
    /// Where the sweep starts and how the duration is spread across it —
    /// `CS_BOSS_ZOOM_FROM` / `CS_BOSS_ZOOM_CURVE`, resolved at install. Neither
    /// changes what any counter draws; they change which counter is on screen
    /// when. See `patch::boss_zoom_from`.
    zoom_from: i32,
    zoom_curve: i32,
    /// Where the boss that set the effect off is standing, in map space, and
    /// where that comes out on screen. `None` centres the effect on the middle
    /// of the screen, which is what the guest does. See `patch::host_zoom_kill`.
    zoom_boss: Option<(i32, i32)>,
    zoom_centre: Option<(i32, i32)>,
    /// `CS_BOSS_ZOOM_R` as a percentage of the screen height, resolved at
    /// install. **0 is the shipped reach** — every corner — and is what the tests
    /// that use the guest's own code as the oracle set, together with a `None`
    /// centre, because anything else is deliberately a different image.
    ///
    /// It says nothing about the centring: `zoom_centre` follows the boss on
    /// every value of this knob, so `0` alone is the reference footage's look
    /// (corner-to-corner streaks converging on the boss) rather than the shipped
    /// bytes. Only `CS_BOSS_ZOOM_MS = 0` gives those back.
    pub zoom_r_pct: i32,
    /// MIDAS primitive draws served, and where `_midas_drawDIR` lives.
    ///
    /// Its own counter for the opposite reason to the blurs: these are *tiny* and
    /// there are tens of thousands of them in a frame (20k while the pet-naming
    /// keyboard is up), so pooling them into `fill_host` would drown every other
    /// number in a slow-frame report.
    ///
    /// The address is resolved once at install. This is the hottest entry hook in
    /// the build and `Vm::symbol` is a string hash — paying that per call cost
    /// ~8 ms a frame on the device, measured.
    pub midas_host: u64,
    midas_dir: u32,
    /// `UM_Sleep` waits served on the host, and the milliseconds handed over.
    ///
    /// Their own counters because a served sleep is *invisible* everywhere
    /// else: it costs no guest instructions and no host work, so a frame that
    /// slept 50 ms would otherwise look like an unexplained wall-clock hole —
    /// the exact shape the slow-frame line exists to explain. See
    /// `patch::host_um_sleep`.
    pub sleep_host: u64,
    pub sleep_ms: u64,
    /// Whether the last `frame()` can have changed the picture — i.e. whether
    /// any texture upload happened in it. `false` means the framebuffer holds
    /// byte-for-byte what it held after the previous frame, so the host can
    /// skip its copy and present entirely. Always `true` when `CS_LAZY_GL` is
    /// off. See `gl::Gl::end_frame`.
    pub frame_dirty: bool,
    /// Guest scratch for the IME's sub-window framebuffer, allocated on first
    /// use. See `patch::Vm::host_ime_surface`.
    ime_fb: u32,
    /// The narrowed width `host_ime_layout` is currently asking the keyboard to
    /// lay itself out for, or 0 for "answer with the real surface". It is set
    /// only across the `imeLocal_button_init` calls that search for a fit, so
    /// every other reader — including the paint — sees the truth.
    ime_lay_w: i32,
    pub patch_declined: u64,
    /// Guest addresses of the font stack's globals, or `None` on a guest
    /// whose symbols don't match — every font patch declines then.
    font_globals: Option<font::FontGlobals>,
    /// Guest scratch for the cash-shop price patch, allocated on first use.
    cash_buf: u32,
    /// Guest scratch for the buff HUD's lines, likewise.
    potion_buf: u32,
    /// Whether a gamepad is attached right now, as told by the shell: the
    /// Android host polls `InputDevice` and keeps this current
    /// (`cs_android::pad`), the probe forces it with `--pad-hud`, and it is
    /// false everywhere else. While set (and `CS_PAD_HUD` allows it),
    /// `host_hud` draws a badge on each of the world screen's six skill
    /// buttons; the moment it clears, the next render draws without them.
    pub gamepad_hud: bool,
    /// Globals the four ported drawing leaves read, resolved once at boot.
    /// They are the hottest entry hooks in the build — the map pair runs tens of
    /// thousands of times a second — and a name lookup per call is measurable.
    /// See `patch::Leaves`.
    leaves: patch::Leaves,
    /// Where `GameFrame_PressTouchKeypad` keeps the six skill-button rects the
    /// badges sit on, resolved and verified once at boot (0 = it moved, and the
    /// warning was logged there). See `patch::Vm::pad_slot_tables`.
    pad_tbl: u32,
    /// The three timed cash 비약, held here rather than in the guest's save:
    /// they last a week of wall-clock time and the guest's own buff table is
    /// wiped on every death. See `potions.rs`.
    pub potions: potions::PotionState,
    /// Set when 작명의 비약 opened the pet-naming screen, cleared when the name
    /// is committed. The pet's name is the guest's own state, so nothing is held
    /// here but "this rename was ours, save it" — see
    /// `patch::Vm::host_pet_rename_save`.
    pet_rename_pending: bool,
    /// Guest scratch for the DES self-check, and its result.
    des_buf: u32,
    des: des::Des,
    /// Return addresses of the menu D-pad's draw calls, resolved once. Empty
    /// when the nudge is off or the guest's symbols don't match, which makes
    /// `host_menu_arrow` decline everything.
    menu_arrow_lrs: Vec<u32>,
    /// `[start, end)` of the sixteen menu key handlers the D-pad's hit boxes
    /// are copy-pasted into, so `host_menu_arrow_hit` can require the caller as
    /// well as the rect.
    menu_arrow_keys: Vec<(u32, u32)>,
    /// `Draw_TouchPad`'s address once `CS_JOY_HERO5` verified every site it
    /// hooks (`patch::joy_sites_ok`). `None` declines the whole feature, so a
    /// guest whose pad draw moved keeps its shipped joystick.
    joy_base: Option<u32>,
    /// Directory of `data.vfs.mp3`, built once. See `files::VfsIndex`.
    pub vfs_index: crate::files::VfsIndex,
    /// Per-kind network item indexes, read from the archive's item templates
    /// once. Feeds `host_net_item_drop`; empty when the archive is absent or
    /// unrecognised, which makes that hook decline everything.
    pub netitems: netitem::NetItems,

    /// 세이브 내보내기 / 불러오기 / 되돌리기 — the conversation behind the
    /// three save rows of 환경설정, and the shell's document-screen traffic.
    /// See `savexfer.rs`.
    pub savexfer: savexfer::SaveXfer,
    /// 공용창고 — the account-wide item chest, and the in-flight slot that
    /// keeps a crash mid-transfer from duplicating or losing an item. Read
    /// from its own file at boot; see `vault.rs`.
    pub vault: vault::Vault,
    /// Probe only (`--vault-stop`): abandon a transfer after step 1 or step 2,
    /// which is what a crash in the gap looks like from the chest's side. The
    /// next run's `vault_resume` then has to put it right, and that is the
    /// only way to exercise the four cells of the protocol's table against a
    /// real bag and a real save file. Never set on a device.
    pub vault_stop_after: Option<u8>,
    /// 공용창고's screen: which of its two steps is on the main menu's letter
    /// frame, which panel the grid is showing, and the slot whose bag the other
    /// half of a transfer is. `Off` is re-asserted every time the 3x3 tile menu
    /// is drawn, so it cannot outlive the screen. See `patch::host_vault_*`.
    pub(crate) vault_screen: vault::Screen,
    pub(crate) vault_panel: vault::Panel,
    pub(crate) vault_slot: u8,
    /// The chest laid out as a bag: a storage image of the same shape as the
    /// guest's own, swapped into it for as long as one shipped bag call runs.
    /// Empty when the grid is not up — its items are pool objects.
    pub(crate) vault_view: Vec<u8>,
    /// Chest ids in the order they were materialised into that view.
    pub(crate) vault_ids: Vec<u64>,
    /// Guest buffers for the tile's label and for the empty string that clears
    /// the grid's shipped heading.
    pub(crate) vault_label: Option<u32>,
    pub(crate) vault_blank: Option<u32>,
    /// The numeric keypad is up for a gold amount, and which way the gold goes
    /// when it closes. `onCloseIME_MainFrame` belongs to the chest while this
    /// is set; see `patch::host_id_input_done`.
    pub(crate) vault_gold_input: Option<vault::Op>,
    /// 편지's tile is retired — every hook that does it fitted this binary at
    /// boot (`patch::letter_sites`). Off means the tile is exactly as shipped,
    /// whatever `CS_LETTER` says.
    pub(crate) letter_retired: bool,
    /// The move a crash left half-done has been settled — or found not to
    /// exist — once this process, at the first drawing of the 3x3 tile menu.
    pub(crate) vault_resumed: bool,

    /// 「레시피2」 — the recipes the shipped shop does not sell, read out of the
    /// archive once. See `recipe2.rs`.
    pub recipe2: recipe2::Recipes,
    /// Set when every one of that feature's hooks installed. The two hooks it
    /// *shares* (`NpcUI_setMerchant` and `getInCodeString`) are live for other
    /// reasons, so they need a flag rather than their own presence to know.
    pub(crate) recipe2_armed: bool,
    /// Guest buffer holding [`patch`]'s 레시피2 label, allocated on first use.
    pub(crate) recipe2_label: Option<u32>,
    /// 펫 「스마트모드」: which shipped arm the last tick picked, so
    /// `Pet_move_process` hears the same answer as `Pet_Process` did.
    pub(crate) pet_smart_last: u8,
    /// Last object state `CS_BOX_DEBUG` reported, so the two gates it sits on
    /// log a line when something changes rather than several times a frame.
    pub(crate) box_dbg_last: Option<(u32, i32, u8, i8, u32)>,
    /// Same idea for the pet diagnostic lines, which would otherwise print
    /// every world tick.
    pub(crate) pet_dbg_last: Option<(u8, u8, u8, u8, u8)>,
    /// ...and whether the description dispatch we just steered was ours.
    pub(crate) pet_smart_desc: bool,
    /// Guest buffers for that mode's label and description.
    pub(crate) pet_label: Option<u32>,
    pub(crate) pet_desc_msg: Option<u32>,

    /// The cutscene skip watchdog's two words: how many `EVENT_Tick` calls in a
    /// row have found a running skip that has not moved, and what it had moved
    /// to last time. See `patch::host_skip_watchdog`.
    skip_stall: u32,
    skip_last: u8,

    /// The last level gap (hero minus monster) `patch::host_exp_gap_gate` saw,
    /// and the two return addresses inside `MONSTER_dropExp` whose awards the
    /// decay in `patch::host_exp_mul` applies to. Empty when the gap fix is
    /// off or the guest's code drifted, which leaves every award whole.
    exp_gap: i32,
    exp_gap_lrs: [u32; 2],

    /// Clip id -> the byte range in the VFS it was loaded from. The guest hands
    /// audio over as an offset and a length into `data.vfs.mp3` (which is what
    /// `MediaPlayer.setDataSource(fd, offset, length)` wanted), so the decode
    /// happens here.
    pub audio_sources: std::collections::HashMap<i64, (u64, u64)>,
    /// Started on the first clip when `Config::async_audio` is on.
    audio_decoder: Option<audio::Decoder>,
}

/// Instructions between profiler samples. Large enough that the sampling cost
/// disappears, small enough that a hot loop of a few hundred instructions still
/// shows up.
const PROFILE_PERIOD: u64 = 997;

/// How many distinct faulting PCs a tolerated run will describe in full.
///
/// The bound is on the set, not the log: it exists so a guest that has gone
/// properly wrong cannot grow a report per address until the phone runs out of
/// memory. Faults past it still count.
const FAULT_SITES_MAX: usize = 32;

/// The clock the probe runs on, and the origin every run used to share.
///
/// Keeping it fixed is what makes a dumped frame comparable between runs, so
/// the virtual clock still starts here.
const FIXED_CLOCK_MS: i64 = 1_700_000_000_000;

/// Where [`Vm::now_ms`] starts.
///
/// **This is an item's identity.** `ITEM_CreateItem+0x44` stamps every item it
/// makes with `MC_knlCurrentTime()` — `ftime()` reduced to epoch milliseconds —
/// into `item+0xd0`, and that 64-bit number is the *only* thing that says which
/// bag item is the one in an equip slot: `Item_saveStorage+0x344` writes the
/// eight equipped items' stamps, and `Item_loadStorage+0x350` walks the bag
/// comparing each item's stamp against those eight to rebuild the pointers.
///
/// The guest does defend itself, but only against the collision it expected:
/// `+0x54` compares the fresh stamp with the last one it handed out (kept in
/// `_menuOpt+0`) and adds one if they are equal, which covers a burst of items
/// made inside the same millisecond.
///
/// It cannot cover *this* one. With a constant origin, `now_ms()` is
/// "1.7e12 plus how long the app has been open", so the stamps a session hands
/// out are the same numbers the previous session handed out — an item picked up
/// four minutes into Tuesday's session is stamped identically to one picked up
/// four minutes into Monday's. Two items in the same save then share an id, and
/// on the next load *both* match the same equip slot; the later one in bag
/// order wins, because `ITEM_setEquip` overwrites. That is the reported "a
/// certain mace in the bag ends up in the weapon slot after a trip to the main
/// menu", and it needed a specific mace because it needed the one item whose
/// stamp collided.
///
/// The original phone had no such problem: `ftime()` there was the real clock,
/// which does not repeat. Seeding from it restores that, and keeping
/// `clock_origin.elapsed()` on top means the result is still monotonic within a
/// run even if the system clock is moved — which is better than the original,
/// where a backwards clock change stalled every WIPI timer (all 54 callers of
/// `MC_knlCurrentTime` derive from this one value, so it only has to be
/// consistent with itself).
fn boot_clock_ms(virtual_clock: bool) -> i64 {
    if virtual_clock {
        return FIXED_CLOCK_MS;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(FIXED_CLOCK_MS)
}

impl Vm {
    pub fn new(so: &[u8], cfg: Config) -> Result<Self, String> {
        let mut mem = GuestMem::new();

        // Imports are discovered during load; each gets a trampoline word in the
        // HLE window, and the JUMP_SLOT relocation points the guest's PLT there.
        let mut imports: Vec<String> = Vec::new();
        let data_syms = hle::data_import_addr();
        let elf = elf::load(so, IMAGE_BASE, &mut mem, |name| {
            if let Some(addr) = data_syms(name) {
                return Some(addr);
            }
            let idx = match imports.iter().position(|n| n == name) {
                Some(i) => i,
                None => {
                    imports.push(name.to_string());
                    imports.len() - 1
                }
            };
            Some(hle::tramp_addr(idx))
        })?;

        // One read of the identity files, shared by both views: `Identity`
        // calls `number()` itself, so constructing it first and taking the
        // number from it means the two cannot disagree.
        let ident = identity::Identity::load(&cfg.storage, cfg.virtual_clock);

        let mut vm = Self {
            cpu: Cpu::new(),
            mem,
            heap: Heap::new(HEAP_BASE, HEAP_SIZE),
            gl: Gl::new(cfg.screen_w, cfg.screen_h),
            files: Files::new(cfg.storage.clone(), cfg.vfs.clone()),
            elf,
            imports,
            jni: hle::JniState::default(),
            phase: Phase::Loaded,
            clock_ms: boot_clock_ms(cfg.virtual_clock),
            clock_origin: std::time::Instant::now(),
            identity: ident.number().to_owned(),
            ident,
            claimer: None,
            claim_since: None,
            data_manage_msg: None,
            my_id_label: None,
            my_id_text: None,
            letter_ask_msg: None,
            letter_none_msg: None,
            letter_last_to: Vec::new(),
            my_id_popup: false,
            popup_msg: None,
            popup_len: 0,
            ime_scratch: None,
            id_pending: false,
            scratch: 0,
            errno_word: 0,
            magnification: 1.0,
            margin_x: 0,
            margin_y: 0,
            auto_margin_y: 0,
            steps: 0,
            faults: 0,
            fault_sites: std::collections::HashSet::new(),
            fault_report: None,
            watch_addr: patch::knob("CS_WATCH_MEM", 0) as u32,
            watch_last: 0,
            log: Vec::new(),
            trace: std::env::var("CS_TRACE").is_ok_and(|v| v != "0"),
            profile: None,
            profile_countdown: PROFILE_PERIOD,
            patches: Vec::new(),
            patch_lo: u32::MAX,
            patch_hi: 0,
            cave_installed: Vec::new(),
            skin_buf: 0,
            skin_kinds: Vec::new(),
            skin_item: 0,
            skin_item_of: None,
            cave_range: None,
            blit_host: 0,
            fill_host: 0,
            vfs_host: 0,
            rot_host: 0,
            rot_ns: 0,
            screen_host: 0,
            imgrot_host: 0,
            imgrot_ns: 0,
            des_host: 0,
            fnt_host: 0,
            blur_host: 0,
            radial_host: 0,
            radial_ns: 0,
            radial_map: None,
            radial_px: Vec::new(),
            radial_out: Vec::new(),
            zoom_ms: 0,
            zoom_t0: 0,
            zoom_last: 0,
            zoom_fine: 0,
            zoom_frames: 0,
            zoom_running: false,
            zoom_world_bucket: -1,
            zoom_world_due: 0,
            zoom_blur_done: 0,
            zoom_blur_due: 0,
            zoom_steps: 5,
            zoom_strength: 100,
            zoom_ticks: patch::ZOOM_WORLD_TICKS,
            zoom_from: patch::ZOOM_FIRST,
            zoom_curve: 1,
            zoom_boss: None,
            zoom_centre: None,
            zoom_r_pct: 0,
            midas_host: 0,
            midas_dir: 0,
            sleep_host: 0,
            sleep_ms: 0,
            frame_dirty: true,
            ime_fb: 0,
            ime_lay_w: 0,
            patch_declined: 0,
            font_globals: None,
            cash_buf: 0,
            potion_buf: 0,
            gamepad_hud: false,
            leaves: patch::Leaves::default(),
            pad_tbl: 0,
            potions: potions::PotionState::load(&cfg.storage),
            pet_rename_pending: false,
            des_buf: 0,
            des: Default::default(),
            menu_arrow_lrs: Vec::new(),
            menu_arrow_keys: Vec::new(),
            joy_base: None,
            vfs_index: crate::files::VfsIndex::build(&cfg.vfs, 0, cfg.vfs.len() as u64),
            netitems: netitem::NetItems::default(),
            savexfer: savexfer::SaveXfer::default(),
            vault: vault::Vault::open(&cfg.storage),
            vault_stop_after: None,
            vault_screen: vault::Screen::Off,
            vault_panel: vault::Panel::Bag,
            vault_slot: 0,
            vault_view: Vec::new(),
            vault_ids: Vec::new(),
            vault_label: None,
            vault_blank: None,
            vault_gold_input: None,
            letter_retired: false,
            vault_resumed: false,
            recipe2: recipe2::Recipes::default(),
            recipe2_armed: false,
            recipe2_label: None,
            pet_smart_last: 0,
            box_dbg_last: None,
            pet_dbg_last: None,
            pet_smart_desc: false,
            pet_label: None,
            pet_desc_msg: None,
            skip_stall: 0,
            skip_last: 0,
            exp_gap: 0,
            exp_gap_lrs: [0; 2],
            audio_sources: std::collections::HashMap::new(),
            audio_decoder: None,
            cfg,
        };

        // User mode, ARM state, interrupts masked — the guest never touches
        // CPSR, but the interpreter reads the mode bits on every access.
        vm.cpu.reg_set(Mode::User, reg::CPSR, 0x1f);
        vm.cpu.reg_set(Mode::User, reg::SP, STACK_BASE + STACK_SIZE - 16);

        vm.scratch = vm.heap.alloc(1024).ok_or("heap: scratch")?;
        vm.errno_word = vm.heap.alloc(4).ok_or("heap: errno")?;
        vm.build_jni()?;

        // Skip the display work on frames whose picture cannot have changed.
        // Not a guest patch — it never touches guest state — so it rides its
        // own knob rather than `CS_NO_PATCH`. See `gl::Gl::end_frame`.
        vm.gl.lazy = patch::lazy_gl();

        // The guest-side code cave goes in first, so a host patch below that
        // checks an instruction word sees the hooked word and refuses — that
        // refusal is the conflict check between the two mechanisms.
        let cave_features = cave::wanted();
        vm.install_cave(&cave_features);

        // Install the host implementations unless switched off. Keyed on
        // symbol names rather than hard-coded addresses, so a differently built
        // guest either patches correctly or not at all.
        if !std::env::var("CS_NO_PATCH").is_ok_and(|v| v != "0") {
            // `CS_NO_FNT=1` keeps the blitters but leaves the font stack to
            // the interpreter — the A/B knob for the font port alone.
            let no_fnt = std::env::var("CS_NO_FNT").is_ok_and(|v| v != "0");
            // `CS_NO_SCREEN=1` is the same idea for the two `DM_DrawScreen565*`
            // blitters: leaving every other patch on makes the A/B run at a
            // usable speed, which `CS_NO_PATCH=1` does not.
            let no_screen = std::env::var("CS_NO_SCREEN").is_ok_and(|v| v != "0");
            let no_rot = std::env::var("CS_NO_ROT").is_ok_and(|v| v != "0");
            // `CS_NO_RADIAL=1` hands the boss-death zoom streak back to the
            // interpreter. Its own knob because it is the one patch a player
            // can feel being wrong — it is the last thing drawn before a boss
            // dies — and because at ~60 M guest instructions a frame, turning
            // it off is also how you re-measure the stall it exists to remove.
            let no_radial = std::env::var("CS_NO_RADIAL").is_ok_and(|v| v != "0");
            // `CS_NO_MIDAS=1` hands the two MIDAS drawing leaves back to the
            // interpreter. Its own knob because they are 96% of the pet-naming
            // keyboard's frame and sit under 24 other rounded-rectangle draws —
            // so it is both the A/B for the port and the way to re-measure what
            // it removes.
            let no_midas = std::env::var("CS_NO_MIDAS").is_ok_and(|v| v != "0");
            // `CS_NO_IMGROT=1` hands `DM_DrawImageRotateEx` back. Its own knob
            // because the probe cannot reach the boss-kill zoom that calls it,
            // so the A/B that matters for this one is a unit test against the
            // guest's own code — and this is how that test turns the patch off.
            let no_imgrot = std::env::var("CS_NO_IMGROT").is_ok_and(|v| v != "0");
            for (sym, kind) in patch::PATCHED {
                if no_fnt && matches!(kind, patch::Patch::FntDrawString | patch::Patch::FntPixel(_))
                {
                    continue;
                }
                if no_screen
                    && matches!(kind, patch::Patch::ScreenBlit | patch::Patch::ScreenBlitEx)
                {
                    continue;
                }
                if no_rot && matches!(kind, patch::Patch::RotBlit | patch::Patch::RotBlitEx) {
                    continue;
                }
                if no_radial && matches!(kind, patch::Patch::RadialBlur) {
                    continue;
                }
                if no_midas && matches!(kind, patch::Patch::MidasLine(_)) {
                    continue;
                }
                if no_imgrot && matches!(kind, patch::Patch::ImageRotateEx) {
                    continue;
                }
                if let Some(addr) = vm.symbol(sym) {
                    vm.patches.push((addr, kind));
                }
            }
            // What the naming keyboard thinks was pressed. Diagnostic only.
            if patch::ime_debug() {
                for (sym, kind) in patch::PATCHED_IME_LOG {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // `←` on an empty name is the shipped keyboard's cancel, and the
            // frame below treats a cancel as a commit. See `patch::IME_BS_SITE`.
            let bs_base = vm.symbol("imeLocal_onHandleEventKey");
            match patch::ime_backspace_site(bs_base, &mut vm.mem) {
                Some(at) => vm.patches.push((at, patch::Patch::ImeBackspace)),
                None if patch::ime_backspace_floor() => {
                    log::warn!("CS_NAME_BACKSPACE: the backspace block moved; left as shipped")
                }
                None => {}
            }
            // The naming confirm popup is drawn over the cash shop, whose OK
            // button reaches nobody while it is up. See `patch::NAME_OK_SITE`.
            let mfk = vm.symbol("MainFrame_KeyPressed");
            match patch::name_ok_site(mfk, &mut vm.mem) {
                Some(at) => vm.patches.push((at, patch::Patch::NameOk)),
                None if patch::name_ok_button() => {
                    log::warn!("CS_NAME_OK: the popup branch moved; OK button left dead")
                }
                None => {}
            }
            // The pet-naming keyboard's size. One of the seven callers of
            // `__knlGR_getScreenFrameBuffer` is the keyboard's own layout, and
            // handing *that one* a narrower surface is what makes the keyboard
            // small — see `patch::IME_SURFACE_SITES`. Both halves go in together
            // or neither does: the narrowing is only ever asked for from inside
            // the search that `Patch::ImeLayout` runs.
            if let Some(get_fb) = vm.symbol("__knlGR_getScreenFrameBuffer") {
                let resolved: Vec<Option<u32>> =
                    patch::IME_SURFACE_NAMES.iter().map(|n| vm.symbol(n)).collect();
                let sites = patch::ime_surface_sites(&resolved, get_fb, &mut vm.mem);
                if sites.len() < patch::IME_SURFACE_SITE_COUNT {
                    if patch::ime_height_pct() != 0 {
                        log::warn!(
                            "CS_IME_H: only {} of {} sites matched; keyboard left as shipped",
                            sites.len(),
                            patch::IME_SURFACE_SITE_COUNT
                        );
                    }
                } else {
                    for a in sites {
                        vm.patches.push((a, patch::Patch::ImeSurface));
                    }
                    for (sym, kind) in patch::PATCHED_IME_SIZE {
                        if let Some(addr) = vm.symbol(sym) {
                            vm.patches.push((addr, kind));
                        }
                    }
                }
            }
            // The five drawing leaves the combat profile left on top. Each is
            // a reimplementation rather than a re-issue, so each waits on its
            // own words matching — see `patch::LEAF_SITES`.
            for (sym, kind) in patch::LEAF_SITES {
                match vm.symbol(sym) {
                    Some(base) if patch::leaf_ok(kind, base, &mut vm.mem) => {
                        vm.patches.push((base, kind))
                    }
                    _ if patch::leaf_wanted(kind) => {
                        log::warn!("{sym} is not the leaf this build ported; left to the guest")
                    }
                    _ => {}
                }
            }
            // `UM_Sleep` is a busy-wait on `MC_knlCurrentTime` — the hit-stop
            // flash alone spins the interpreter flat out for 50 ms per landed
            // hit. Hooked at the loop, not the entry, so the guest keeps every
            // observable side effect; see `patch::host_um_sleep`.
            if let (Some(base), Some(ct)) = (vm.symbol("UM_Sleep"), vm.symbol("MC_knlCurrentTime"))
            {
                match patch::um_sleep_site(base, ct, &mut vm.mem) {
                    Some(at) => vm.patches.push((at, patch::Patch::SleepLoop)),
                    None if patch::host_sleep() => log::warn!(
                        "CS_HOST_SLEEP: UM_Sleep does not match; its busy-wait stays interpreted"
                    ),
                    None => {}
                }
            }
            // The MIDAS draws are the hottest entry hooks here — tens of
            // thousands a frame — so `_midas_drawDIR` is resolved once. A
            // `Vm::symbol` per call is a string hash, and paying it cost about
            // 8 ms a frame on the device.
            vm.midas_dir = vm.symbol("_midas_drawDIR").unwrap_or(0);
            if vm.midas_dir == 0 {
                log::warn!("_midas_drawDIR missing; the MIDAS draw patches decline");
            }
            // The cash shop's price line. Unlike everything above this changes
            // what the guest draws, so it rides the same switch as the charge
            // answering in `net.rs` rather than being on unconditionally.
            if net::free_cash() {
                for (sym, kind) in patch::PATCHED_FREE_CASH {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // The string table, shared by the fee wording above and the 「내 ID」
            // tile below. One address can carry one patch kind, so it is
            // installed if either wants it and each half re-checks its own knob.
            if net::free_cash() || patch::my_id_tile() || patch::vault_on() {
                let (sym, kind) = patch::PATCHED_IN_CODE_STRING;
                if let Some(addr) = vm.symbol(sym) {
                    vm.patches.push((addr, kind));
                }
            }
            // 「내 ID」 — main-menu tile 7, which shipped grey and led to
            // 「사용되지 않는 메뉴입니다」. Two one-instruction hooks; both refuse
            // to install unless the surrounding words are the ones they were
            // written against.
            if let Some(base) = vm.symbol("MainMenuFrame_KeyPressed") {
                if let Some(at) = patch::my_id_key_site(base, &mut vm.mem) {
                    vm.patches.push((at, patch::Patch::MyIdKey));
                } else if patch::my_id_tile() || patch::vault_on() || !patch::letter_tile() {
                    log::warn!("my id: the menu commit moved; the tiles are left alone");
                }
            }
            if let Some(base) = vm.symbol("MainMenuFrame_drawMainMenu") {
                if let Some(at) = patch::my_id_draw_site(base, &mut vm.mem) {
                    vm.patches.push((at, patch::Patch::MyIdTile));
                }
            }
            // 편지, retired: the two hooks above plus the fee branch, all or
            // nothing — a tile that is grey but alive, or alive but grey, is
            // worse than the shipped one.
            if !patch::letter_tile() {
                let sites = match (
                    vm.symbol("MainMenuFrame_KeyPressed"),
                    vm.symbol("MainMenuFrame_drawMainMenu"),
                    vm.symbol("UI_RoundRect"),
                ) {
                    (Some(k), Some(d), Some(r)) => patch::letter_sites(k, d, r, &mut vm.mem),
                    _ => None,
                };
                match sites {
                    Some(at) => {
                        vm.patches.push((at, patch::Patch::LetterTileBranch));
                        vm.letter_retired = true;
                    }
                    None => log::warn!(
                        "CS_LETTER: the letter tile's fee branch or the grey box moved; \
                         편지 is left as it shipped"
                    ),
                }
            }
            // 공용창고 — main-menu tile 1, the other one that shipped grey.
            // Four hooks, each standing in for a whole shipped function or
            // block, so the install checks both the words they replace and the
            // branches that say what those blocks are for.
            if patch::vault_on() {
                let syms = [
                    "MainMenuFrame_KeyPressed",
                    "MainMenuFrame_drawLetter",
                    "MainMenuFrame_ItemBagKeyPressed",
                    "draw_CommonItembag",
                    "MainMenuFrame_drawMenuSlot",
                    "ItemBagKey",
                    "drawItemBag",
                    "getInCodeString",
                    "Load_TalentData",
                    "GameFrame_load_hero_data",
                    "GameFrame_loadGame",
                ];
                let found: Option<Vec<u32>> = syms.iter().map(|s| vm.symbol(s)).collect();
                match found.and_then(|a| {
                    patch::vault_sites(
                        a[0],
                        a[1],
                        a[2],
                        a[3],
                        a[4],
                        a[5],
                        a[6],
                        a[7],
                        a[8],
                        a[9],
                        a[10],
                        &mut vm.mem,
                    )
                }) {
                    Some(sites) => vm.patches.extend(sites),
                    None => log::warn!(
                        "CS_VAULT: the letter frame's slot list or bag grid moved; \
                         the chest has no screen"
                    ),
                }
            }
            // 장비 탭의 외형 칸. Rides the cave's `skin` feature: with no
            // transmog installed there is nothing for the cells to report,
            // and the table they read would never be filled.
            if patch::skin_cells() && vm.cave_installed.contains(&"skin") {
                let syms = [
                    "IGM_DrawEquip",
                    "UI_RoundRect",
                    "drawEqip_ItemIcon",
                    "GameMenuFrame_Render",
                    "GameMenuFrame_KeyPressed",
                ];
                let found: Option<Vec<u32>> = syms.iter().map(|s| vm.symbol(s)).collect();
                match found {
                    Some(a) if patch::equip_grid_site(a[0], a[1], a[2], &mut vm.mem) => {
                        vm.patches.push((a[0], patch::Patch::EquipSkins));
                        for off in patch::EQ_PANEL_ITEM {
                            vm.patches.push((a[0] + off, patch::Patch::EquipPanelItem));
                        }
                        // The cursor reaching the look cells is a second,
                        // separable thing: without it they are drawn but
                        // cannot be selected, which is still the old build's
                        // behaviour rather than a broken one.
                        match (vm.symbol("getEqipNum"), vm.symbol("MP_getEquip")) {
                            (Some(slot_num), Some(worn))
                                if patch::equip_cursor_site(a[4], &mut vm.mem) =>
                            {
                                vm.patches.push((slot_num, patch::Patch::EquipSlotNum));
                                vm.patches.push((worn, patch::Patch::EquipWorn));
                                vm.patches
                                    .push((a[4] + patch::EQ_TOUCH, patch::Patch::EquipTouch));
                                vm.patches.push((
                                    a[4] + patch::EQ_OK_EMPTY_AT,
                                    patch::Patch::EquipOkEmpty,
                                ));
                                // The four arrows. Nothing is rewritten here:
                                // the arms keep their own words and the hooks
                                // answer before those run, so `0` leaves the
                                // shipped 4x2 walk with no cells to walk over.
                                for (off, ..) in patch::EQ_STEP_SITES {
                                    vm.patches.push((a[4] + off, patch::Patch::EquipStep));
                                }
                            }
                            _ => log::warn!(
                                "CS_SKIN_CELLS: the 장비 tab's cursor moved;                                  the 외형 cells are drawn but cannot be selected"
                            ),
                        }
                    }
                    _ => log::warn!(
                        "CS_SKIN_CELLS: the 장비 tab's cell grid moved;                          the 외형 cells are not drawn"
                    ),
                }
            }
            // The keyboard's close callback. Rides the tile's knob because the
            // tile is the only thing that opens our keyboard, and it declines
            // on every other close — pet naming and letters still go to the
            // guest's own body.
            // 공용창고's amount keypad closes through the same callback.
            if patch::my_id_tile() || patch::vault_on() {
                let (sym, kind) = patch::PATCHED_ID_INPUT;
                if let Some(addr) = vm.symbol(sym) {
                    vm.patches.push((addr, kind));
                }
            }
            if patch::my_id_tile() {
                let (sym, kind) = patch::PATCHED_ID_POPUP;
                if let Some(addr) = vm.symbol(sym) {
                    vm.patches.push((addr, kind));
                }
            }
            // The letter's recipient line. Rides `CS_ID_RECIPIENT`, which is
            // itself gated on IDs being reported — while identities are still
            // numbers the shipped wording and keypad are the correct ones.
            if patch::id_recipient() {
                match (vm.symbol("MainMenuLetterSendKey"), vm.symbol(patch::PATCHED_LETTER_INPUT.0))
                {
                    (Some(send), Some(init)) if patch::letter_input_site(send, &mut vm.mem) => {
                        vm.patches.push((init, patch::PATCHED_LETTER_INPUT.1));
                    }
                    _ => log::warn!("id: the letter's InputInit moved; it still asks for a number"),
                }
                // And keep the recipient across a re-open.
                if let Some(send) = vm.symbol("MainMenuLetterSendKey") {
                    match patch::letter_keep_site(send, &mut vm.mem) {
                        Some(at) => vm.patches.push((at, patch::PATCHED_LETTER_KEEP.1)),
                        None => log::warn!("id: the recipient erase moved; re-open starts empty"),
                    }
                }
                // And the wording for a letter the server refuses.
                if let Some(addr) = vm.symbol(patch::PATCHED_SEND_ERROR.0) {
                    vm.patches.push((addr, patch::PATCHED_SEND_ERROR.1));
                }
            }
            // Stop the guest reshaping the identity on the way out. Only with
            // `CS_ID_AS_PHONE`, because that reshape is what every mailbox on
            // the server was keyed by until now.
            if let Some(base) = vm.symbol(patch::PATCHED_SKT_PHONE.0) {
                if let Some(at) = patch::skt_phone_site(base, &mut vm.mem) {
                    vm.patches.push((at, patch::PATCHED_SKT_PHONE.1));
                    // The one shape this migrates: a number minted before
                    // 2026-08-16 whose stored and wire forms differ.
                    let was = identity::wire_number(vm.identity.as_bytes());
                    if was != vm.identity.as_bytes() {
                        log::warn!(
                            "identity: {} used to reach the server as {};                              with CS_ID_AS_PHONE it now arrives unchanged, so                              its old mailbox is at the second name",
                            vm.identity,
                            String::from_utf8_lossy(&was)
                        );
                    }
                } else if patch::id_as_phone() {
                    log::warn!("identity: getSKTPhoneNumber moved; the guest will reshape the ID");
                }
            }
            // The two whole-screen draws the HUD lines ride on: the running
            // buffs (when `CS_POTIONS`/`CS_POTION_HUD` want them) and the
            // gamepad badges (when a pad is attached and `CS_PAD_HUD` allows
            // it). One address, one patch — `host_hud` decides per call which
            // of the two draws, and declines when neither does.
            for (sym, kind) in patch::hud_patches() {
                if let Some(addr) = vm.symbol(sym) {
                    vm.patches.push((addr, kind));
                }
            }
            // Where the badges go: the six skill-button rects the guest's own
            // hit test measures. Resolved once, here, so that a table that has
            // moved says so in the log — checking it per render tick meant the
            // badges just silently never appeared, which is indistinguishable
            // from "no pad attached" or "knob off".
            vm.pad_tbl = vm.pad_slot_tables().unwrap_or(0);
            if vm.pad_tbl == 0 && patch::pad_hud() {
                log::warn!(
                    "CS_PAD_HUD: GameFrame_PressTouchKeypad's slot tables moved; no pad badges"
                );
            }
            // The three cash 비약. Like the price line above these change what
            // the guest does, and they ride `CS_POTIONS` (and `CS_FREE_CASH`,
            // without which the purchase can never complete).
            if potions::enabled() {
                for (sym, kind) in patch::PATCHED_POTIONS {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
                // 작명의 비약 needs one hook of its own: the tail of the
                // pet-name commit inside `MainFrame_KeyPressed`, so a rename
                // bought in the shop reaches the disk instead of waiting for
                // whatever autosave happens next. Six words are verified,
                // including that the two `bl` still reach `MX_imeEnd` and
                // `MC_knlSprintk`.
                if potions::rename_enabled() {
                    match (
                        vm.symbol("MainFrame_KeyPressed"),
                        vm.symbol("MX_imeEnd"),
                        vm.symbol("MC_knlSprintk"),
                        vm.symbol("MP_getHero"),
                    ) {
                        (Some(base), Some(ime_end), Some(sprintk), Some(get_hero)) => {
                            match patch::pet_rename_site(
                                base,
                                ime_end,
                                sprintk,
                                get_hero,
                                &mut vm.mem,
                            ) {
                                Some(site) => vm.patches.push((site, patch::Patch::PetRenameSave)),
                                None => log::warn!(
                                    "CS_POTION_RENAME: guest code does not match; not installed"
                                ),
                            }
                        }
                        _ => log::warn!("CS_POTION_RENAME: symbols missing; not installed"),
                    }
                }
                // Nine mid-function loads of `item[7]`, each installed only
                // against the exact instruction it stands in for.
                let sites = patch::item_level_sites(&vm.elf.symbols, &mut vm.mem);
                if sites.len() < patch::ITEM_LEVEL_SITE_COUNT && potions::level_enabled() {
                    log::warn!(
                        "CS_POTION_LEVEL: only {} of {} sites matched",
                        sites.len(),
                        patch::ITEM_LEVEL_SITE_COUNT
                    );
                }
                vm.patches.extend(sites);
            }
            // 「레시피2」 — a fifth row in the town merchant's category menu,
            // selling the ten recipes the shipped 「레시피」 row does not. It
            // has to be a new row and not ten more entries on the old one
            // because `_merchant` is exactly 100 slots and the shipped recipe
            // file is exactly 100 rows; `recipe2.rs` has the arithmetic.
            //
            // Five one-instruction hooks, plus two addresses shared with
            // features above that may already have claimed them.
            if patch::recipe2() {
                vm.recipe2 = recipe2::Recipes::build(&vm.cfg.vfs, &vm.vfs_index);
                if vm.recipe2.is_empty() {
                    if !vm.cfg.vfs.is_empty() {
                        log::warn!(
                            "CS_RECIPE2: the archive has no unsold recipes; menu left alone"
                        );
                    }
                } else {
                    match (
                        vm.symbol("NpcUI_DrawMerChant_Main"),
                        vm.symbol("NpcUI_merchantKeyPressed"),
                        vm.symbol("NpcUI_setMerchant"),
                        vm.symbol("getInCodeString"),
                    ) {
                        (Some(draw), Some(key), Some(load), Some(strings)) => {
                            match patch::recipe2_sites(draw, key, load, strings, &mut vm.mem) {
                                Some(sites) => {
                                    vm.patches.extend(sites);
                                    if !potions::enabled() {
                                        vm.patches.push((load, patch::Patch::CashPotionList));
                                    }
                                    if !(net::free_cash()
                                        || patch::my_id_tile()
                                        || patch::vault_on())
                                    {
                                        vm.patches.push((strings, patch::Patch::InCodeString));
                                    }
                                    vm.recipe2_armed = true;
                                    log::info!(
                                        "recipe2: {} recipes the shop never sold, {} gold each",
                                        vm.recipe2.rows().len(),
                                        patch::recipe2_price()
                                    );
                                }
                                None => log::warn!(
                                    "CS_RECIPE2: the shop menu moved; it keeps its four rows"
                                ),
                            }
                        }
                        _ => log::warn!("CS_RECIPE2: symbols missing; not installed"),
                    }
                }
            }
            // 펫 「스마트모드」 — a fifth 전투타입 that hunts treasure when
            // there is any and fights when there is not. Both halves are
            // shipped arms of `Pet_Process`; all this does is choose between
            // them each tick, and let the menu reach a fifth value.
            if patch::pet_smart() {
                match (
                    vm.symbol("Pet_Process"),
                    vm.symbol("Pet_move_process"),
                    vm.symbol("HERO_Check_Autotarget"),
                    vm.symbol("GameMenuCharPetKey"),
                    vm.symbol("draw_pet_info"),
                    vm.symbol("draw_char_pet"),
                    vm.symbol("Pet_set_condition"),
                    vm.symbol("getInCodeString"),
                ) {
                    (
                        Some(process),
                        Some(move_process),
                        Some(autotarget),
                        Some(menu),
                        Some(info),
                        Some(label),
                        Some(set_cond),
                        Some(strings),
                    ) => {
                        match patch::pet_smart_sites(
                            process,
                            move_process,
                            autotarget,
                            menu,
                            info,
                            label,
                            set_cond,
                            strings,
                            &mut vm.mem,
                        ) {
                            Some(sites) => {
                                vm.patches.extend(sites);
                                // The string table is shared; it may already
                                // be in the table for one of three reasons.
                                if !(net::free_cash()
                                    || patch::my_id_tile()
                                    || patch::vault_on()
                                    || (patch::recipe2() && vm.recipe2_armed))
                                {
                                    vm.patches.push((strings, patch::Patch::InCodeString));
                                }
                            }
                            None => log::warn!(
                                "CS_PET_SMART: the pet AI moved; the menu keeps its four types"
                            ),
                        }
                    }
                    _ => log::warn!("CS_PET_SMART: symbols missing; not installed"),
                }
            }
            // A chest the pet uncovered is still unhittable, because every
            // object is cloned with HP 0 and that is the first thing the
            // attack filter refuses. Let that one comparison through.
            if patch::chest_break() {
                match vm.symbol("Hero_canAttack").and_then(|a| {
                    patch::chest_break_site(a, &mut vm.mem)
                }) {
                    Some(site) => {
                        vm.patches.push((site, patch::Patch::ChestBreak));
                        log::info!("chest break: uncovered chests are attackable");
                    }
                    None => log::warn!(
                        "CS_CHEST_BREAK: Hero_canAttack moved; chests stay unbreakable"
                    ),
                }
            }
            // Diagnostic only: report what the two attack gates see when the
            // candidate is a treasure object. Declines, so nothing changes.
            if patch::box_debug() {
                let mut n = 0;
                for name in ["Hero_canAttack", "MONSTER_Check_type_canAttack"] {
                    if let Some(a) = vm.symbol(name) {
                        vm.patches.push((a, patch::Patch::BoxDebug));
                        n += 1;
                    }
                }
                if let Some(k) = vm.symbol("GameFrame_PressTouchKeypad") {
                    // The two `cmp r0, #0` after Event_CheckOKKey and
                    // ItemBag_pickUp — everything the OK press can do before
                    // it would reach the attack.
                    for off in [0x1d0u32, 0x1e8] {
                        vm.patches.push((k.wrapping_add(off), patch::Patch::OkDebug));
                        n += 1;
                    }
                }
                log::info!("CS_BOX_DEBUG: watching {n} sites");
            }
            // A kill more than five levels below the hero pays no experience
            // at all — the shipped gate at `MONSTER_dropExp+0x3c`, and the
            // whole of "몬스터를 잡아도 경험치가 안 오른다" once the growth
            // 비약 outlevel an area. Bypass the cliff and decay the award
            // instead; quest rewards never pass this gate and are untouched.
            if patch::exp_gap() {
                if let (Some(drop), Some(add)) =
                    (vm.symbol("MONSTER_dropExp"), vm.symbol("HERO_add_Exp"))
                {
                    match patch::exp_gap_site(drop, add, &mut vm.mem) {
                        Some(site) => {
                            vm.exp_gap_lrs = patch::EXP_GAP_LRS.map(|lr| drop + lr);
                            vm.patches.push((site, patch::Patch::ExpGapGate));
                            // The decay itself runs in `host_exp_mul`; the
                            // 비약 may already have installed that hook.
                            if !potions::enabled() {
                                vm.patches.push((add, patch::Patch::ExpMul));
                            }
                        }
                        None => {
                            log::warn!("CS_EXP_GAP: guest code does not match; not installed")
                        }
                    }
                }
            }
            // The zoom a dying boss leaves on the screen used to be five
            // *stalls* of 55 M guest instructions; porting the blur to the host
            // left five ordinary frames, and the flourish with them. Step its
            // counter from the clock instead of by four so the same animation
            // spans `CS_BOSS_ZOOM_MS`. Five instruction words are verified, and
            // that the `bl` beside them still reaches the blur whose ray length
            // that counter scales.
            match (vm.symbol("Map_drawMiddleObject"), vm.symbol("DM_RadialBlurLcd")) {
                (Some(base), Some(blur)) => {
                    match patch::boss_zoom_site(base, blur, &mut vm.mem) {
                        Some(site) => {
                            vm.zoom_ms = patch::boss_zoom_ms();
                            vm.zoom_from = patch::boss_zoom_from();
                            vm.zoom_curve = patch::boss_zoom_curve();
                            vm.zoom_r_pct = patch::boss_zoom_radius_pct();
                            vm.zoom_steps = patch::boss_zoom_steps();
                            vm.zoom_strength = patch::boss_zoom_strength();
                            vm.zoom_ticks = patch::boss_zoom_ticks();
                            vm.patches.push((site, patch::Patch::BossZoom));
                            // The arm is the one instruction of the effect that
                            // runs before its first blur, so it is where the
                            // sweep's start and its centre are both settled.
                            vm.patches
                                .push((patch::boss_zoom_arm_site(base), patch::Patch::ZoomArm));
                            // And where the boss that set it off is standing,
                            // read off the two killing blows themselves.
                            if let Some(is_boss) = vm.symbol("MONSTER_isBoss") {
                                if let Some(atk) = vm.symbol("HERO_Attack_Monster") {
                                    let sites = patch::zoom_kill_sites(atk, is_boss, &mut vm.mem);
                                    if sites.is_empty() {
                                        log::warn!(
                                            "CS_BOSS_ZOOM_R: kill sites do not match; \
                                             centring on the screen"
                                        );
                                    }
                                    vm.patches.extend(sites);
                                }
                            }
                            // And slow the world to the same pace, which is
                            // what the effect used to look like — the stalled
                            // build ran the world five times across the whole
                            // 2.9 s. Rides the same knob plus one of its own,
                            // and does nothing at all unless a zoom is running.
                            if patch::boss_zoom_slowmo() {
                                match vm.symbol("GameFrame_Process") {
                                    Some(a) => vm.patches.push((a, patch::Patch::ZoomWorld)),
                                    None => {
                                        log::warn!("CS_BOSS_ZOOM_SLOWMO: GameFrame_Process missing")
                                    }
                                }
                            }
                        }
                        None if patch::boss_zoom_ms() > 0 => {
                            log::warn!("CS_BOSS_ZOOM_MS: guest code does not match; not installed")
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            // Network-only equipment, back in the single-player game. Seven
            // mid-function loads of `item[8]` open the equip gates, and a
            // hook after each monster load adds a network item to its drop
            // table — see `netitem.rs` for the evidence trail.
            if patch::net_equip() {
                let sites = patch::net_item_sites(&vm.elf.symbols, &mut vm.mem);
                if sites.len() < patch::NET_ITEM_SITE_COUNT {
                    log::warn!(
                        "CS_NET_EQUIP: only {} of {} sites matched",
                        sites.len(),
                        patch::NET_ITEM_SITE_COUNT
                    );
                }
                vm.patches.extend(sites);
            }
            if patch::net_drop() {
                vm.netitems = netitem::NetItems::build(&vm.cfg.vfs, &vm.vfs_index);
                if vm.netitems.is_empty() {
                    if !vm.cfg.vfs.is_empty() {
                        log::warn!("CS_NET_DROP: no network items found in the archive");
                    }
                } else {
                    log::info!(
                        "net items: {} across {} kinds join the drop pool at {}/65536",
                        vm.netitems.total(),
                        vm.netitems.kinds(),
                        patch::net_drop_rate()
                    );
                    for (sym, kind) in patch::PATCHED_NET_DROP {
                        if let Some(addr) = vm.symbol(sym) {
                            vm.patches.push((addr, kind));
                        }
                    }
                }
            }
            // The menu D-pad inset. Resolve the call sites once — the draw hook
            // is on a very hot entry point and must not do symbol lookups per
            // call — and install nothing at all if there is no nudge to make.
            // RIGHT on the bag icon enters the bag. A mid-function hook, so it
            // installs only against the exact two instructions it stands in
            // for — see `patch::BAG_RIGHT_TEST_WORD`.
            // The in-game touch control scheme. The settings screen owns this
            // byte, so the hook only rescues a value that screen cannot produce
            // (or forces one outright when `CS_PAD_MODE` is set) — see
            // `patch::MENU_OPT_PAD`.
            if patch::pad_mode() >= 0 || patch::pad_migrate() {
                for (sym, kind) in patch::PATCHED_PAD {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // The opening SMS consent screen asks about a carrier service that
            // is gone. Reporting the consent record as already present is the
            // state a returning player is in, so no new path is invented.
            if !patch::sms_popup() {
                for (sym, kind) in patch::PATCHED_SMS {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // The three save rows of 환경설정. The rows and the modal are the
            // cave's (feature `savexfer`); the host sits on the two cave
            // entries a press and an answer come through. Without the cave
            // feature there are no rows, so nothing to hook.
            if patch::savexfer() && vm.cave_installed.contains(&"savexfer") {
                match (vm.symbol(savexfer::CAVE_ACTION), vm.symbol(savexfer::CAVE_ANSWER)) {
                    (Some(action), Some(answer)) => {
                        vm.patches.push((action, patch::Patch::XferAction));
                        vm.patches.push((answer, patch::Patch::XferAnswer));
                    }
                    _ => log::warn!("CS_SAVEXFER: the cave has no save-row entries; the rows will say so"),
                }
            }
            // 데이터 관리 was a one-way save-destroying button in this build.
            // The cave's options feature retires the row itself; this is the
            // fallback for a build without the cave, where the row is still
            // drawn — it says so instead of running. Both sites are checked by
            // instruction word — if a rebuild ever moves them, the hook simply
            // does not install rather than firing on something else.
            if !patch::data_manage() {
                if let Some(base) = vm.symbol("MainMenuOptionKey") {
                    let ok = patch::DATA_MANAGE_SITES
                        .iter()
                        .all(|&(off, word)| vm.mem.r32(base.wrapping_add(off)) == word);
                    if ok {
                        for (off, _) in patch::DATA_MANAGE_SITES {
                            vm.patches
                                .push((base.wrapping_add(off), patch::Patch::DataManageBlocked));
                        }
                    } else {
                        log::warn!("data manage: the two return sites moved; leaving it alone");
                    }
                }
            }
            // Who this install is. Without it every client looks identical to
            // the private server — one mailbox and one save slot shared by
            // everybody. `CS_PHONE=0` leaves the hook out entirely, which is
            // exactly what every build before 2026-08-16 did.
            if patch::phone_identity() {
                for (sym, kind) in patch::PATCHED_PHONE {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
                // **Print what actually goes out, not what is stored.** Those
                // are different strings whenever the player has claimed an ID,
                // and they were also different for the ~10 % of pre-2026-08-16
                // numbers the guest's rebuild reshaped. A log line that prints
                // only `cs_id.txt` is how someone ends up telling a friend an
                // address their letters never arrive at — the exact bug this
                // feature exists to close, reproduced in the one place anyone
                // would look to check it.
                //
                // **Not through `String::from_utf8_lossy`.** A chosen ID is
                // EUC-KR, and lossy decoding turns every Hangul byte into
                // U+FFFD — the log then shows `?????`, which is worse than
                // useless because it looks like corruption rather than a
                // charset. ASCII prints as itself; anything else prints as the
                // bytes, which is what the server keys by and what a bug report
                // can be matched against. The player reads the name off the
                // 「내 ID」 tile, where the game's own font draws it properly.
                let wire = vm.wire_identity();
                let shown = match std::str::from_utf8(&wire) {
                    Ok(s) if s.is_ascii() => s.to_owned(),
                    _ => format!(
                        "euc-kr:{}",
                        wire.iter().map(|b| format!("{b:02x}")).collect::<String>()
                    ),
                };
                if shown == vm.identity {
                    log::info!("identity: reporting {shown} as PHONENUMBER");
                } else {
                    log::info!(
                        "identity: reporting {shown} as PHONENUMBER (minted {})",
                        vm.identity
                    );
                }
            }
            if patch::opt_debug() {
                for (sym, kind) in patch::PATCHED_OPT_LOG {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // Finish the cast-abort teardown the guest leaves half done.
            if patch::cast_fix() {
                for (sym, kind) in patch::PATCHED_CAST {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // Equipment restore: keep the worn item when a bag spare shares its
            // creation stamp. Installs at `ITEM_setEquip` but only acts for the
            // one call inside `Item_loadStorage`, whose encoding is verified.
            match (vm.symbol("Item_loadStorage"), vm.symbol("ITEM_setEquip")) {
                (Some(loader), Some(setter)) => {
                    match patch::equip_restore_ret(loader, setter, &mut vm.mem) {
                        Some(_) => vm.patches.push((setter, patch::Patch::EquipRestore)),
                        None if patch::equip_fix() => {
                            log::warn!("CS_EQUIP_FIX: guest code does not match; not installed")
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            // A cutscene skip only advances while its own renderer is running,
            // and the tick and the renderer do not agree on when that is. Not
            // tied to `CS_ALWAYS_SKIP`: the deadlock is the shipped game's.
            if patch::skip_watchdog() {
                for (sym, kind) in patch::PATCHED_SKIP_WATCHDOG {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            // The cutscene SKIP button through narration / fade / timed message.
            if patch::always_skip() {
                for (sym, kind) in patch::PATCHED_ALWAYS_SKIP {
                    if let Some(addr) = vm.symbol(sym) {
                        vm.patches.push((addr, kind));
                    }
                }
            }
            if let Some(base) = vm.symbol("Event_Key") {
                let sites = patch::skip_key_sites(base, &mut vm.mem);
                if sites.is_empty() && patch::always_skip() {
                    log::warn!("CS_ALWAYS_SKIP: guest code does not match; input not opened");
                }
                for a in sites {
                    vm.patches.push((a, patch::Patch::EventSkipKey));
                }
            }
            match vm.symbol("GameMenuFrame_KeyPressed") {
                Some(base) => match patch::bag_right_site(base, &mut vm.mem) {
                    Some(a) => vm.patches.push((a, patch::Patch::BagRight)),
                    None if patch::bag_right() => {
                        log::warn!("CS_BAG_RIGHT: guest code does not match; not installed")
                    }
                    None => {}
                },
                None => {}
            }
            match (vm.symbol("GameMenuFrame_KeyPressed"), vm.symbol("ITEMSTORAGE_removeCurItem")) {
                (Some(base), Some(remove_cur)) => {
                    match patch::autoloot_dup_site(base, &mut vm.mem, remove_cur) {
                        Some(a) => vm.patches.push((a, patch::Patch::AutoLootDup)),
                        None if patch::autoloot_dup() => {
                            log::warn!("CS_AUTOLOOT_DUP: guest code does not match; not installed")
                        }
                        None => {}
                    }
                }
                _ => {}
            }
            // hero5 의 인게임 조이스틱 (`CS_JOY_HERO5`). 훅 자리는 D-pad 와
            // 같은 두 진입(`DM_DrawImgRotateEx`, `Check_Rect`)이라 설치도
            // 같이 간다 — 다만 이 기능은 자기 사이트 검증이 따로 있고,
            // 하나라도 다르면 통째로 빠진다.
            if patch::joy_hero5() {
                vm.joy_base = vm.joy_sites_ok();
                if vm.joy_base.is_none() {
                    log::warn!("CS_JOY_HERO5: Draw_TouchPad does not match; not installed");
                } else {
                    log::info!("joy: the world joystick is hero5's");
                }
            }
            if patch::dpad_changed() || vm.joy_base.is_some() {
                vm.menu_arrow_lrs = patch::menu_arrow_lrs(|n| vm.symbol(n));
                vm.menu_arrow_keys = patch::menu_arrow_key_ranges(&vm.elf);
                // The cave's key handlers test the same four D-pad rects
                // (cave/src/opt.c, skin.c); by construction nothing else in
                // the cave passes those values to Check_Rect.
                if !vm.cave_installed.is_empty() {
                    if let Some(r) = vm.cave_range {
                        vm.menu_arrow_keys.push(r);
                    }
                }
                let dpad_ok = patch::dpad_changed()
                    && !vm.menu_arrow_lrs.is_empty()
                    && !vm.menu_arrow_keys.is_empty();
                if dpad_ok || vm.joy_base.is_some() {
                    for (sym, kind) in patch::PATCHED_DPAD {
                        if let Some(addr) = vm.symbol(sym) {
                            vm.patches.push((addr, kind));
                        }
                    }
                }
            }
        }
        // `CS_WATCH=Sym1,Sym2` traces those guest functions' arguments without
        // changing what they do.
        if let Ok(list) = std::env::var("CS_WATCH") {
            for sym in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match vm.symbol(sym) {
                    Some(addr) => vm.patches.push((addr, patch::Patch::Watch)),
                    None => log::warn!("CS_WATCH: no such guest symbol: {sym}"),
                }
            }
        }
        // Sorted so the run loop can binary-search. The window below spans
        // most of `.text`, so a linear scan here would be paid on nearly every
        // guest instruction and every patch added would make it worse.
        vm.patches.sort_unstable_by_key(|(a, _)| *a);
        for (addr, _) in &vm.patches {
            vm.patch_lo = vm.patch_lo.min(*addr);
            vm.patch_hi = vm.patch_hi.max(*addr);
        }
        // Resolved by name so a differently built guest simply declines.
        vm.font_globals = font::FontGlobals::resolve(|n| vm.symbol(n));
        vm.leaves = patch::Leaves::resolve(|n| vm.symbol(n));
        for line in vm.potions.describe(vm.wall_now_ms()) {
            log::info!("potions: {line}");
        }
        Ok(vm)
    }

    pub fn symbol(&self, name: &str) -> Option<u32> {
        self.elf.symbols.get(name).copied()
    }

    // ---- clock ----------------------------------------------------------

    pub fn now_ms(&self) -> i64 {
        if self.cfg.virtual_clock {
            self.clock_ms
        } else {
            self.clock_ms + self.clock_origin.elapsed().as_millis() as i64
        }
    }

    /// Epoch milliseconds, for the one thing that has to outlive the process:
    /// when a 비약 runs out (`potions.rs`).
    ///
    /// [`Vm::now_ms`] cannot answer this. It is process-relative — a seeded
    /// origin plus however long this run has been going — so a seven-day buff
    /// measured with it would only ever count play time and would never expire.
    ///
    /// The virtual clock is used as-is when it is on, and that is not a
    /// fallback but a requirement: the probe draws the remaining time into the
    /// HUD, and a wall-clock read would make every dumped frame depend on when
    /// it was dumped, which is the end of "byte for byte identical" as a test.
    /// Its origin is already an epoch-shaped constant, so the arithmetic is the
    /// same either way.
    ///
    /// `None` means the host clock could not be read at all (a device set
    /// before 1970). Callers treat that as "does not expire" — losing what was
    /// bought is worse than a buff outstaying its week.
    pub fn wall_now_ms(&self) -> Option<i64> {
        if self.cfg.virtual_clock {
            return Some(self.now_ms());
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as i64)
    }

    /// Advance the virtual clock. No effect in wall-clock mode.
    pub fn advance_time(&mut self, ms: i64) {
        if self.cfg.virtual_clock {
            self.clock_ms += ms;
        }
    }

    // ---- guest execution -------------------------------------------------

    /// Call a guest function and return r0. Re-entrant: an HLE import that has
    /// to call back into the guest (`qsort`'s comparator, the WIPI timer
    /// callbacks) can use this from inside a `run`.
    pub fn call(&mut self, func: u32, args: &[u32]) -> Result<u32, String> {
        let saved = self.cpu;

        let mut sp = self.cpu.reg_get(Mode::User, reg::SP) & !7;
        if args.len() > 4 {
            let extra = args.len() - 4;
            sp -= ((extra * 4 + 7) & !7) as u32;
            for (i, a) in args[4..].iter().enumerate() {
                self.mem.write_bytes(sp + (i * 4) as u32, &a.to_le_bytes());
            }
        }
        for (i, a) in args.iter().take(4).enumerate() {
            self.cpu.reg_set(Mode::User, i as u8, *a);
        }
        self.cpu.reg_set(Mode::User, reg::SP, sp);
        self.cpu.reg_set(Mode::User, reg::LR, RETURN_MAGIC);
        self.cpu.reg_set(Mode::User, reg::PC, func);

        let r = self.run();
        let ret = self.cpu.reg_get(Mode::User, 0);
        self.cpu = saved;
        r?;
        Ok(ret)
    }

    /// Run until PC reaches [`RETURN_MAGIC`].
    fn run(&mut self) -> Result<(), String> {
        // A frame of this game is on the order of a million guest instructions.
        // The ceiling only exists so a guest that wedges surfaces as an error
        // instead of hanging the UI thread.
        const BUDGET: u64 = 2_000_000_000;
        let mut budget = BUDGET;

        loop {
            let pc = self.cpu.reg_get(Mode::User, reg::PC);
            if pc == RETURN_MAGIC {
                return Ok(());
            }
            if let Some(id) = hle::tramp_index(pc) {
                self.dispatch(id)?;
                continue;
            }
            // Two predictable compares per guest instruction to reject the
            // common case, then a binary search over a sorted table — the
            // window covers most of `.text`, so the search is what most
            // instructions actually pay. See patch.rs.
            if pc >= self.patch_lo && pc <= self.patch_hi {
                if let Some(kind) = self
                    .patches
                    .binary_search_by_key(&pc, |(a, _)| *a)
                    .ok()
                    .map(|i| self.patches[i].1)
                {
                    let served = match kind {
                        patch::Patch::Blit => self.host_blit()?,
                        patch::Patch::FillRect => self.host_fill_rect()?,
                        patch::Patch::FillAlpha => self.host_fill_alpha()?,
                        patch::Patch::MidasLine(op) => self.host_midas_line(op)?,
                        patch::Patch::ImeSurface => self.host_ime_surface()?,
                        patch::Patch::ImeKeyLog => self.host_ime_key_log()?,
                        patch::Patch::ImeBackspace => self.host_ime_backspace()?,
                        patch::Patch::NameOk => self.host_name_ok()?,
                        patch::Patch::ImeLayout => self.host_ime_layout()?,
                        patch::Patch::BoxBlur => self.host_box_blur()?,
                        patch::Patch::RadialBlur => {
                            // Timed rather than counted: it is the one host
                            // cost a guest instruction count cannot see, and
                            // the retimed zoom multiplies how often it is paid.
                            let t0 = std::time::Instant::now();
                            let served = self.host_radial_blur()?;
                            self.radial_ns += t0.elapsed().as_nanos() as u64;
                            served
                        }
                        patch::Patch::BossZoom => self.host_boss_zoom()?,
                        patch::Patch::ZoomWorld => self.host_zoom_world()?,
                        patch::Patch::ZoomArm => self.host_zoom_arm()?,
                        patch::Patch::ZoomKill { rn } => self.host_zoom_kill(rn)?,
                        patch::Patch::Div { signed } => self.host_div(signed)?,
                        patch::Patch::SleepLoop => self.host_um_sleep()?,
                        patch::Patch::VfsLookup => self.host_vfs_lookup()?,
                        patch::Patch::RotBlitEx => self.host_rotate_blit(true)?,
                        patch::Patch::RotBlit => self.host_rotate_blit(false)?,
                        patch::Patch::ScreenBlitEx => self.host_draw_screen(true)?,
                        patch::Patch::ScreenBlit => self.host_draw_screen(false)?,
                        patch::Patch::ImageRotateEx => self.host_image_rotate()?,
                        patch::Patch::StartDes => self.host_start_des()?,
                        patch::Patch::FntDrawString => self.host_fnt_draw_string()?,
                        patch::Patch::FntPixel(k) => self.host_fnt_pixel(k)?,
                        patch::Patch::CashPrice => self.host_cash_price()?,
                        patch::Patch::MenuArrow => self.host_menu_arrow()?,
                        patch::Patch::MenuArrowHit => self.host_menu_arrow_hit()?,
                        patch::Patch::InCodeString => self.host_in_code_string()?,
                        patch::Patch::MyIdKey => self.host_my_id_key()?,
                        patch::Patch::MyIdTile => self.host_my_id_tile()?,
                        patch::Patch::LetterTileBranch => self.host_letter_tile_branch()?,
                        patch::Patch::VaultSlotOk => self.host_vault_slot_ok()?,
                        patch::Patch::VaultBagKey => self.host_vault_bag_key()?,
                        patch::Patch::VaultBagDraw => self.host_vault_bag_draw()?,
                        patch::Patch::VaultSlotDraw => self.host_vault_slot_draw()?,
                        patch::Patch::EquipSkins => self.host_equip_skins()?,
                        patch::Patch::EquipSlotNum => self.host_equip_slot_num()?,
                        patch::Patch::EquipWorn => self.host_equip_worn()?,
                        patch::Patch::EquipPanelItem => self.host_equip_panel_item()?,
                        patch::Patch::EquipTouch => self.host_equip_touch()?,
                        patch::Patch::EquipOkEmpty => self.host_equip_ok_empty()?,
                        patch::Patch::EquipStep => self.host_equip_step()?,
                        patch::Patch::MapClip => self.host_map_clip()?,
                        patch::Patch::MapTileIndex => self.host_map_tile_index()?,
                        patch::Patch::CalcAnchor => self.host_calc_anchor()?,
                        patch::Patch::RestorePalette => self.host_restore_palette()?,
                        patch::Patch::PalBlend => self.host_pal_blend()?,
                        patch::Patch::PutPixel => self.host_put_pixel()?,
                        patch::Patch::FntInvalidate => self.host_fnt_invalidate()?,
                        patch::Patch::IdInputDone => self.host_id_input_done()?,
                        patch::Patch::MyIdPopupTouch => self.host_my_id_popup_touch()?,
                        patch::Patch::SktPhoneNumber => self.host_skt_phone_number()?,
                        patch::Patch::LetterRecipient => self.host_letter_recipient()?,
                        patch::Patch::SendError => self.host_send_error()?,
                        patch::Patch::LetterKeepTo => self.host_letter_keep_recipient()?,
                        patch::Patch::CashSkipConfirm => self.host_cash_skip_confirm()?,
                        patch::Patch::BagRight => self.host_bag_right()?,
                        patch::Patch::AutoLootDup => self.host_autoloot_dup()?,
                        patch::Patch::EventCanSkip => self.host_event_can_skip()?,
                        patch::Patch::EventSkipKey => self.host_event_skip_key()?,
                        patch::Patch::CastCancel => self.host_cast_cancel()?,
                        patch::Patch::CastGauge => self.host_cast_gauge()?,
                        patch::Patch::EquipRestore => self.host_equip_restore()?,
                        patch::Patch::SkipWatchdog => self.host_skip_watchdog()?,
                        patch::Patch::PadMode => self.host_pad_mode()?,
                        patch::Patch::OptSaveLog => self.host_opt_save_log()?,
                        patch::Patch::CashPotionList => self.host_cash_potion_list()?,
                        patch::Patch::CashPotionBuy => self.host_cash_potion_buy()?,
                        patch::Patch::Recipe2Rows => self.host_recipe2_rows()?,
                        patch::Patch::Recipe2WrapUp => self.host_recipe2_wrap_up()?,
                        patch::Patch::Recipe2WrapDown => self.host_recipe2_wrap_down()?,
                        patch::Patch::Recipe2HitRows => self.host_recipe2_hit_rows()?,
                        patch::Patch::Recipe2State => self.host_recipe2_state()?,
                        patch::Patch::PetSmartPick => self.host_pet_smart_pick()?,
                        patch::Patch::PetSmartMove => self.host_pet_smart_move()?,
                        patch::Patch::ChestBreak => self.host_chest_break()?,
                        patch::Patch::BoxDebug => self.host_box_debug()?,
                        patch::Patch::OkDebug => self.host_ok_debug()?,
                        patch::Patch::PetSmartTarget => self.host_pet_smart_target()?,
                        patch::Patch::PetSmartWrapUp => self.host_pet_smart_wrap_up()?,
                        patch::Patch::PetSmartWrapDown => self.host_pet_smart_wrap_down()?,
                        patch::Patch::PetSmartDesc => self.host_pet_smart_desc()?,
                        patch::Patch::PetRenameSave => self.host_pet_rename_save()?,
                        patch::Patch::ExpMul => self.host_exp_mul()?,
                        patch::Patch::ExpGapGate => self.host_exp_gap_gate()?,
                        patch::Patch::ItemLevel { rd, rn } => self.host_item_level(rd, rn)?,
                        patch::Patch::NetItemFlag { rd, rn } => self.host_net_item_flag(rd, rn)?,
                        patch::Patch::NetItemDrop => self.host_net_item_drop()?,
                        patch::Patch::ItemInfoLevel => self.host_item_info_level()?,
                        patch::Patch::Hud => self.host_hud()?,
                        patch::Patch::PhoneNumber => self.host_phone_number()?,
                        patch::Patch::SmsAgreed => self.host_sms_agreed()?,
                        patch::Patch::DataManageBlocked => self.host_data_manage_blocked()?,
                        patch::Patch::XferAction => self.host_xfer_action()?,
                        patch::Patch::XferAnswer => self.host_xfer_answer()?,
                        patch::Patch::Watch => self.watch_call(pc)?,
                    };
                    if served {
                        continue;
                    }
                    self.patch_declined += 1;
                }
            }
            if !self.cpu.step(&mut self.mem) {
                return Err(format!(
                    "undefined instruction at {:#010x} ({})
{}",
                    pc,
                    self.describe(pc),
                    self.context(pc)
                ));
            }
            self.steps += 1;
            if self.profile.is_some() {
                self.profile_countdown -= 1;
                if self.profile_countdown == 0 {
                    self.profile_countdown = PROFILE_PERIOD;
                    let here = self.cpu.reg_get(Mode::User, reg::PC);
                    if let Some(p) = self.profile.as_mut() {
                        *p.entry(here).or_insert(0) += 1;
                    }
                }
            }
            // A word the build was told to watch (`CS_WATCH_MEM`). Compared
            // here rather than trapped inside `GuestMem`, because the point of
            // it is to name the *instruction* that changed the word, and only
            // the run loop knows which one just ran. Off by default: one
            // compare against zero per instruction.
            if self.watch_addr != 0 {
                let now = self.mem.peek32(self.watch_addr);
                if now != self.watch_last {
                    log::warn!(
                        "watch {:#010x}: {:#010x} -> {:#010x} by {}",
                        self.watch_addr,
                        self.watch_last,
                        now,
                        self.describe(pc)
                    );
                    self.watch_last = now;
                }
            }
            // Check before taking: `take` writes the Option back every
            // instruction, a read does not.
            if self.mem.fault.is_some() {
                let f = self.mem.take_fault().expect("just checked");
                if !self.cfg.tolerate_faults {
                    return Err(format!(
                        "guest {} of {} bytes at {:#010x} from {} - outside the mapped window\n{}",
                        if f.write { "write" } else { "read" },
                        f.width,
                        f.addr,
                        self.describe(pc),
                        self.context(pc)
                    ));
                }
                // Survived. The load already returned 0 and the store already
                // went nowhere, so there is nothing to undo; what is left is to
                // make sure it never becomes invisible. A site the guest faults
                // on every frame reports once - with the registers and the
                // backtrace, which is what turned the first of these into an
                // address - and after that only moves the counter that the perf
                // line prints.
                self.faults += 1;
                if self.fault_sites.len() < FAULT_SITES_MAX && self.fault_sites.insert(pc) {
                    let report = format!(
                        "guest {} of {} bytes at {:#010x} from {} - outside the mapped window, \
                         tolerated\n{}",
                        if f.write { "write" } else { "read" },
                        f.width,
                        f.addr,
                        self.describe(pc),
                        self.context(pc)
                    );
                    log::error!("{report}");
                    if self.fault_report.is_none() {
                        self.fault_report = Some(report);
                    }
                }
            }
            budget -= 1;
            if budget == 0 {
                return Err(format!("guest ran {BUDGET} instructions without returning"));
            }
            let _ = &budget;
        }
    }

    /// Is the MIDAS soft keyboard up?
    ///
    /// One byte, `_midas_internalImeInstance[0x40]`, set by `MX_imeCall` and
    /// cleared by `imeLocal_Release`. While it is set, `clsidAuthHandle+0x104`
    /// sends every event to `imeLocal_onHandleEvent` instead of the game — so a
    /// key that means "walk right" to the world means "type 6" here, and the
    /// host has to know which one it is asking for.
    pub fn ime_open(&mut self) -> bool {
        match self.symbol("_midas_internalImeInstance") {
            Some(ime) => self.mem.peek32(ime + 0x40) & 0xff != 0,
            None => false,
        }
    }

    /// Registers plus a best-effort backtrace, for fault reports.
    ///
    /// There are no frame pointers to walk, so the stack is scanned for words
    /// that point just past a `bl` inside the image. That over-reports — dead
    /// slots from earlier calls survive — but with symbol names attached it
    /// still identifies the path into a fault in seconds.
    pub fn context(&mut self, pc: u32) -> String {
        let mut s = String::new();
        let names =
            ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12"];
        for (i, n) in names.iter().enumerate() {
            s.push_str(&format!("{n}={:#010x} ", self.cpu.reg_get(Mode::User, i as u8)));
            if i % 6 == 5 {
                s.push('\n');
            }
        }
        let sp = self.cpu.reg_get(Mode::User, reg::SP);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        s.push_str(&format!(
            "\nsp={sp:#010x} lr={lr:#010x} ({})\npc={pc:#010x}\n",
            self.describe(lr)
        ));

        s.push_str("stack (probable return addresses):\n");
        let mut shown = 0;
        for k in 0..512u32 {
            let v = self.mem.peek32(sp + k * 4);
            if v <= IMAGE_BASE || v >= self.elf.image_end || v & 3 != 0 {
                continue;
            }
            // A return address always follows a `bl`/`blx`: bits 27..25 == 0b101.
            let prev = self.mem.peek32(v - 4);
            if (prev >> 25) & 0b111 != 0b101 || (prev >> 24) & 1 == 0 {
                continue;
            }
            s.push_str(&format!("  [sp+{:#05x}] {:#010x}  {}\n", k * 4, v, self.describe(v - 4)));
            shown += 1;
            if shown >= 24 {
                break;
            }
        }
        s
    }

    /// Nearest preceding symbol, for error messages. The binary ships its
    /// `.symtab`, which is the single biggest reason this port is debuggable.
    pub fn describe(&self, addr: u32) -> String {
        let mut best: Option<(&String, u32)> = None;
        for (name, &val) in &self.elf.symbols {
            if val <= addr && addr - val < 0x4000 {
                match best {
                    Some((_, b)) if b >= val => {}
                    _ => best = Some((name, val)),
                }
            }
        }
        match best {
            Some((n, v)) if v == addr => n.clone(),
            Some((n, v)) => format!("{n}+{:#x}", addr - v),
            None => format!("{addr:#010x}"),
        }
    }

    // ---- lifecycle -------------------------------------------------------

    /// Reproduce `CletEntry.onSurfaceCreated`, which is where the original app
    /// does its entire start-up in one go.
    pub fn boot(&mut self) -> Result<(), String> {
        let env = self.jni.env_ptr;
        let thiz = self.jni.this_obj;

        self.jni_call("nativeInitKernel", &[env, thiz])?;

        // `CletEntry.adjustAutoScreen`: fit 400x240 into the surface, keeping
        // aspect, and centre what is left over.
        let (sw, sh) = (self.cfg.screen_w, self.cfg.screen_h);
        let (gw, gh) = (self.cfg.wipi_w, self.cfg.wipi_h);
        let mut mag = (sw as f32 / gw as f32).min(sh as f32 / gh as f32);
        if self.cfg.integer_scale && mag >= 1.0 {
            mag = mag.floor();
        }
        self.magnification = mag;
        self.margin_x = (((sw as f32 - gw as f32 * mag) / 2.0).round() as i32).max(0);
        self.margin_y = (((sh as f32 - gh as f32 * mag) / 2.0).round() as i32).max(0);

        // The guest still lays out for the letterboxed rectangle; the present
        // step widens it. Touch is inverted with the same numbers below.
        let drawn_w = gw as f32 * mag;
        self.gl.stretch = if self.cfg.fill_width && drawn_w > 0.0 && sw as f32 > drawn_w {
            Some((self.margin_x as f32, sw as f32 / drawn_w))
        } else {
            None
        };

        let ret = self.jni_call(
            "nativeInitDisplay",
            &[
                env,
                thiz,
                sw as u32,
                sh as u32,
                5, // red bits   — the EGL config the original chooses for
                6, // green bits   pixelType 0 is RGB565
                5, // blue bits
                0, // alpha bits
                gw as u32,
                gh as u32,
                self.margin_x as u32,
                self.margin_y as u32,
                0, // wipiColorType
                mag.to_bits(),
                0, // magFilterType 0 -> GL_NEAREST
                1, // viewMode
                0, // bUseSublayer
                0, // sublayerPixelType
            ],
        )?;
        self.auto_margin_y = ret as i32;

        let storage = self.files.root().to_string_lossy().replace('\\', "/");
        let internal = self.jni.new_string(&storage);
        let external = self.jni.new_string(&storage);
        self.jni_call("nativeInitFileStorage", &[env, thiz, internal, external])?;

        let vfs_len = self.files.vfs_len();
        let fd = self.files.open_vfs(0, vfs_len);
        let fd_obj = self.jni.new_file_descriptor(fd);
        // (jobject fd, jlong startOffset, jlong length) — AAPCS pads each 64-bit
        // argument to an even register/slot pair.
        self.jni_call(
            "nativeInitVFS",
            &[env, thiz, fd_obj, 0, 0, 0, vfs_len as u32, (vfs_len >> 32) as u32],
        )?;

        // 0x000316a0 = 202400, the PCM threshold the Java side passes.
        self.jni_call("nativeInitAudioConfiguration", &[env, thiz, 0, 202400, 0])?;
        self.jni_call("nativeStartApp", &[env, thiz])?;

        // `nativeStartApp` is where `onStartApp` runs `MX_desInit`, so the key
        // schedule exists from here on and the host DES can prove itself against
        // the guest before the game has a save to lose. See `des.rs`.
        self.des_selftest();

        self.phase = Phase::Running;
        Ok(())
    }

    /// One `CletEntry.onDrawFrame`: fire due WIPI timers, then present.
    pub fn frame(&mut self) -> Result<(), String> {
        if self.phase != Phase::Running {
            return Ok(());
        }
        // **Before the guest runs, not during.** Answering a claim pops a guest
        // dialog through `Vm::call`, and doing that from the middle of
        // `nativeLoop` would re-enter the interpreter with a half-finished
        // frame on the stack. Here the CPU is between frames and `Vm::call`'s
        // save/restore is over a quiescent state.
        self.poll_id_claim();
        // The phone's file screen answered since the last frame: the save
        // rows' message or next question goes up now, for the same reason.
        savexfer::deliver(self)?;
        let (env, thiz) = (self.jni.env_ptr, self.jni.this_obj);
        self.jni_call("nativeLoop", &[env, thiz])?;
        // Settle whether this frame made a new picture. `updateScreen` redraws
        // its quad on every `nativeLoop`, but the game only *uploads* a new one
        // on its own ~21 Hz tick — the other frames' redraws (already skipped
        // inside `draw_arrays`) and the host's copy/present would move
        // identical pixels, which on a phone is pure heat.
        self.frame_dirty = self.gl.end_frame();
        Ok(())
    }

    /// What the private server will key this install by, right now.
    ///
    /// **One place, because two would drift.** `host_phone_number` sends this
    /// and the 「내 ID」 tile shows it, and the entire point of the tile is that
    /// those two are the same string — the bug it closes is a player reading one
    /// address off a screen while their letters arrive at another.
    ///
    /// Which of the two forms depends on whether `host_skt_phone_number` is
    /// installed. With `CS_ID_AS_PHONE` on it is, and the guest's rebuild — the
    /// one that prepends `'0'` and eats leading zeros — never runs, so what we
    /// report *is* what arrives. With it off the guest still rebuilds, and
    /// [`identity::wire_number`] is the transcription of that.
    pub fn wire_identity(&self) -> Vec<u8> {
        if patch::id_as_phone() {
            self.ident.wire().to_vec()
        } else {
            identity::wire_number(self.identity.as_bytes())
        }
    }

    /// Has the claim worker answered? Never blocks; usually does nothing.
    fn poll_id_claim(&mut self) {
        let outstanding = match self.claim_since {
            Some(t) => t,
            None => return,
        };
        let answer = self.claimer.as_ref().and_then(|c| c.poll());
        let answer = match answer {
            Some(a) => a,
            None => {
                // The give-up is a comparison, not a timer: the frame thread
                // owns no clock of its own and a timer would have to be
                // cancelled on every other path out of here.
                // No wall clock (it can fail) means no deadline — the worker
                // will still answer, and a made-up "now" would either give up
                // instantly or never.
                match self.wall_now_ms() {
                    Some(now) if now - outstanding >= idclaim::GIVE_UP_MS => {}
                    _ => return,
                }
                idclaim::Answer::Unreachable("no answer in 30 s".into())
            }
        };
        self.claim_since = None;
        let msg: &[u8] = match answer {
            idclaim::Answer::Ours(ref raw) => {
                if self.ident.pending() != Some(&raw[..]) {
                    // The server echoes back what it recorded. A different
                    // string means it stored something else, and storing our
                    // version would leave the two disagreeing about who we are.
                    log::error!(
                        "id claim: server kept {raw:?}, we asked for {:?}",
                        self.ident.pending()
                    );
                }
                self.ident.confirm();
                self.ident.save();
                self.my_id_text = None; // the tile must show the new address
                patch::ID_SET_MSG
            }
            idclaim::Answer::Taken | idclaim::Answer::InUse => {
                self.ident.withdraw();
                patch::ID_TAKEN_MSG
            }
            idclaim::Answer::Moved | idclaim::Answer::Rejected(_) => {
                self.ident.withdraw();
                patch::ID_REFUSED_MSG
            }
            idclaim::Answer::Unreachable(ref why) => {
                log::warn!("id claim: {why}");
                self.ident.withdraw();
                patch::ID_OFFLINE_MSG
            }
        };
        if let Err(e) = self.popup(msg) {
            log::warn!("id claim: could not show the result: {e}");
        }
    }

    pub fn key_event(&mut self, event: i32, code: i32, repeat: i32) -> Result<(), String> {
        let (env, thiz) = (self.jni.env_ptr, self.jni.this_obj);
        self.jni_call(
            "nativeKeyHandleEvent",
            &[env, thiz, event as u32, code as u32, repeat as u32],
        )?;
        Ok(())
    }

    /// Physical surface pixels to WIPI screen space, exactly as
    /// `CletEntry.onTouchHandleEvent` does it.
    pub fn to_wipi(&self, x: i32, y: i32) -> (i32, i32) {
        let mag = self.magnification.max(0.001);
        // Undo the present-time stretch before mapping into WIPI space.
        let x = match self.gl.stretch {
            Some((origin, scale)) => (x as f32 / scale + origin) as i32,
            None => x,
        };
        let wx = (((x - self.margin_x) as f32) / mag) as i32;
        let wy = (((y - self.margin_y - self.auto_margin_y) as f32) / mag) as i32;
        (wx.clamp(0, self.cfg.wipi_w - 1), wy.clamp(0, self.cfg.wipi_h - 1))
    }

    /// `x`/`y` are in physical surface pixels; they are mapped into WIPI screen
    /// space exactly as `CletEntry.onTouchHandleEvent` does.
    pub fn touch_event(&mut self, event: i32, x: i32, y: i32, id: i32) -> Result<(), String> {
        let (wx, wy) = self.to_wipi(x, y);
        let (env, thiz) = (self.jni.env_ptr, self.jni.this_obj);
        self.jni_call(
            "nativeTouchHandleEvent",
            &[env, thiz, event as u32, wx as u32, wy as u32, id as u32],
        )?;
        Ok(())
    }

    /// Slice a clip's bytes out of the VFS blob.
    pub fn vfs_slice(&self, offset: u64, len: u64) -> &[u8] {
        let start = (offset as usize).min(self.cfg.vfs.len());
        let end = (start + len as usize).min(self.cfg.vfs.len());
        &self.cfg.vfs[start..end]
    }

    pub fn pause(&mut self) -> Result<(), String> {
        if let Ok(mut m) = self.cfg.audio.lock() {
            m.muted = true;
        }
        let (env, thiz) = (self.jni.env_ptr, self.jni.this_obj);
        self.jni_call("nativePauseApp", &[env, thiz])?;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), String> {
        if let Ok(mut m) = self.cfg.audio.lock() {
            m.muted = false;
        }
        let (env, thiz) = (self.jni.env_ptr, self.jni.this_obj);
        self.jni_call("nativeResumeApp", &[env, thiz])?;
        Ok(())
    }

    fn jni_call(&mut self, entry: &str, args: &[u32]) -> Result<u32, String> {
        let sym = format!("Java_co_kr_eamobile_CletEntry_{entry}");
        let addr = self
            .symbol(&sym)
            .ok_or_else(|| format!("{sym} missing from the guest's symbol table"))?;
        self.call(addr, args)
    }

    /// Collapse the PC histogram onto function names, hottest first.
    ///
    /// This is the tool for the one piece of work this port still wants: the
    /// guest rasterises every sprite itself, so if a scene is slow it is slow
    /// inside one identifiable guest function, and that function can be
    /// reimplemented on the host (the sibling Hero3 port cut 36% of its guest
    /// instructions this way).
    pub fn profile_report(&self, top: usize) -> Vec<(String, u64, f64)> {
        let Some(p) = self.profile.as_ref() else { return Vec::new() };
        let total: u64 = p.values().sum();
        if total == 0 {
            return Vec::new();
        }
        let mut by_fn: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for (&pc, &n) in p {
            let name = match self.describe(pc).split_once('+') {
                Some((f, _)) => f.to_string(),
                None => self.describe(pc),
            };
            *by_fn.entry(name).or_insert(0) += n;
        }
        let mut v: Vec<(String, u64, f64)> =
            by_fn.into_iter().map(|(k, n)| (k, n, 100.0 * n as f64 / total as f64)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(top);
        v
    }

    pub fn framebuffer(&self) -> (&[u32], i32, i32) {
        (&self.gl.fb, self.gl.fb_w, self.gl.fb_h)
    }
}
