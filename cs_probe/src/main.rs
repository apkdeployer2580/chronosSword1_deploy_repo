//! Headless runner for the ChronosSword guest.
//!
//! Boots `libchronosMian.so` with no window, no device and a virtual clock,
//! then dumps frames as PNGs. This is the development loop: it reproduces the
//! Android runtime exactly (same `cs_core`, same HLE) while running on the
//! desktop, and it is deterministic, so two builds that disagree can be diffed
//! frame by frame.
//!
//! ```text
//! cargo run --release -p cs_probe -- \
//!     --so payload/libchronosMian.so --vfs payload/data.vfs.mp3 \
//!     --frames 600 --ms 33 --dump 1,60,300,600 --out out/
//! ```
//!
//! `--ms` is not optional in spirit: the guest idles on WIPI timers, so a run
//! that never advances the clock renders a black screen forever.

use cs_core::{Config, Vm};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

struct Args {
    so: PathBuf,
    vfs: PathBuf,
    storage: PathBuf,
    out: PathBuf,
    width: i32,
    height: i32,
    frames: u32,
    ms: i64,
    dump: Vec<u32>,
    keys: Vec<(u32, i32, i8)>,
    /// `(frame, x, y, kind)`; kind 0 = tap, 1 = press only, -1 = release only.
    taps: Vec<(u32, i32, i32, i8)>,
    /// `(frame, x, y)` — a pointer MOVE at a physical pixel, for a finger a
    /// `+` tap left held. The only way to steer the guest's own touch
    /// joystick in a replay; a press alone sits in its deadzone.
    moves: Vec<(u32, i32, i32)>,
    integer_scale: bool,
    fill_width: bool,
    wipi: (i32, i32),
    trace_log: bool,
    peek: Vec<(u32, u32)>,
    profile: bool,
    /// Frame window `--profile` samples over, inclusive. Defaults to the whole
    /// run.
    profile_from: u32,
    profile_to: u32,
    /// How many hot functions `--profile` prints. See `--profile-top`.
    profile_top: usize,
    heap: bool,
    audio_wav: Option<PathBuf>,
    potions: Vec<cs_core::potions::Kind>,
    /// Frame to open the pet-naming screen on; 0 is never. See `--pet-rename`.
    pet_rename_at: u32,
    smith_at: u32,
    /// Frame the blacksmith screen is left on; `None` is never. See `--smith`.
    smith_until: Option<u32>,
    merchant_at: u32,
    pet_at: (u32, Option<u32>),
    gives: Vec<(u32, String, u32, u32)>,
    /// `(frame, what)` for 공용창고: `list`, `resume`, `put:<tab>:<index>`,
    /// `take:<id>`.
    vault: Vec<(u32, String)>,
    vault_stop: Option<u8>,
    monsters: Vec<(u32, u32, i32, i32)>,
    /// Frame to set the whole-screen zoom percent on, and the percent.
    /// 0 is never. See `--resize`.
    resize_at: (u32, i32),
    /// Frame to arm the boss-kill zoom on; 0 is never. See `--boss-zoom`.
    boss_zoom_at: u32,
    /// Pretend a gamepad is attached for the whole run. See `--pad-hud`.
    pad_hud: bool,
    /// `--save-doc <path>` — the stand-in for the phone's document screens:
    /// 세이브 내보내기 copies its bundle to this path, 세이브 불러오기 reads
    /// this path. Without it every document screen answers "cancelled".
    save_doc: Option<PathBuf>,
    /// `--poke <frame>:<addr>:<u32 value>` — write a guest word on that frame.
    /// A blunt instrument for standing the hero somewhere a replay cannot walk
    /// to, so a claim about a map coordinate can be settled.
    pokes: Vec<(u32, u32, u32)>,
}

fn parse_args() -> Args {
    let mut a = Args {
        so: PathBuf::from("payload/libchronosMian.so"),
        vfs: PathBuf::from("payload/data.vfs.mp3"),
        storage: PathBuf::from("out/storage"),
        out: PathBuf::from("out"),
        width: 800,
        height: 480,
        frames: 300,
        ms: 33,
        dump: vec![],
        keys: vec![],
        taps: vec![],
        moves: vec![],
        integer_scale: true,
        fill_width: false,
        wipi: (cs_core::WIPI_W, cs_core::WIPI_H),
        trace_log: false,
        peek: vec![],
        profile: false,
        profile_from: 0,
        profile_to: u32::MAX,
        profile_top: 20,
        heap: false,
        audio_wav: None,
        potions: vec![],
        pet_rename_at: 0,
        smith_at: 0,
        smith_until: None,
        merchant_at: 0,
        pet_at: (0, None),
        gives: Vec::new(),
        vault: Vec::new(),
        vault_stop: None,
        monsters: Vec::new(),
        resize_at: (0, 150),
        boss_zoom_at: 0,
        pad_hud: false,
        save_doc: None,
        pokes: Vec::new(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].clone();
        let mut next = || {
            i += 1;
            argv.get(i).cloned().unwrap_or_default()
        };
        match flag.as_str() {
            "--so" => a.so = next().into(),
            "--vfs" => a.vfs = next().into(),
            "--storage" => a.storage = next().into(),
            "--out" => a.out = next().into(),
            "--w" => a.width = next().parse().unwrap_or(800),
            "--h" => a.height = next().parse().unwrap_or(480),
            "--frames" => a.frames = next().parse().unwrap_or(300),
            "--ms" => a.ms = next().parse().unwrap_or(33),
            "--no-integer-scale" => a.integer_scale = false,
            "--fill-width" => a.fill_width = true,
            // --wipi 533x240 — hand the guest a different logical screen and
            // see whether its own layout follows.
            "--wipi" => {
                let v = next();
                let mut it = v.split(['x', 'X']);
                if let (Some(w), Some(h)) = (it.next(), it.next()) {
                    if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                        a.wipi = (w, h);
                    }
                }
            }
            // --peek 0x1990e80:27789 — summarise a guest memory range at the
            // end of the run. Runs are deterministic, so an address read off a
            // CS_WATCH trace stays valid on the next run; this is how you tell
            // "the asset decoded to nothing" from "the draw dropped it".
            "--peek" => {
                let spec = next();
                let mut it = spec.split(':');
                if let (Some(a_s), Some(l_s)) = (it.next(), it.next()) {
                    let parse = |s: &str| {
                        let s = s.trim();
                        match s.strip_prefix("0x") {
                            Some(h) => u32::from_str_radix(h, 16),
                            None => s.parse(),
                        }
                    };
                    if let (Ok(addr), Ok(len)) = (parse(a_s), parse(l_s)) {
                        a.peek.push((addr, len));
                    }
                }
            }
            // --potion exp2,finisher — grant a cash 비약 without going through
            // the shop. The effects are the part worth regression-testing and
            // reaching the shop takes a hundred scripted taps.
            "--potion" => {
                for k in next().split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    match cs_core::potions::KINDS.iter().find(|p| p.tag() == k) {
                        Some(&kind) => a.potions.push(kind),
                        None => eprintln!("unknown potion {k}; expected exp2, exp4 or finisher"),
                    }
                }
            }
            // --pet-rename <frame> — open the pet-naming screen at that frame,
            // which is what 작명의 비약 does when it is bought.
            //
            // Reaching the shop entry itself is impossible here: the row is only
            // shelved once there is a pet, and the pet comes from an event-script
            // opcode well past any scripted tap sequence. Faking the flag is not
            // an option either — `Pet_Process` dereferences a null sprite on the
            // next frame. So the part that had never been exercised in this port
            // gets exercised directly: keyboard, touch, commit and save.
            "--pet-rename" => a.pet_rename_at = next().parse().unwrap_or(0),
            // --smith <frame>[:<until>] — open the blacksmith at that frame,
            // the way an event script does. There is no blacksmith within
            // reach of a scripted tap sequence; the cave's 외형변경 screens
            // live in its menu. `until` is the frame the screen is left on:
            // input goes to the NPC screen between the two and back to the
            // normal touch path after, so one run can go into the smith and
            // out again.
            "--smith" => {
                let spec = next();
                let mut p = spec.split(':');
                a.smith_at = p.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                a.smith_until = p.next().and_then(|v| v.trim().parse().ok());
            }
            // --merchant <frame> — the same for the town merchant, whose
            // category menu is where 「레시피2」 adds its fifth row.
            "--merchant" => a.merchant_at = next().parse().unwrap_or(0),
            // --pet <frame> — give the hero a pet at that frame, the way the
            // story does (`Event_getPet`). Nothing a scripted tap sequence
            // reaches gives one, and faking the flag kills the next frame.
            "--pet" => {
                let spec = next();
                let mut p = spec.split(':');
                let frame = p.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                a.pet_at = (frame, p.next().and_then(|v| v.trim().parse().ok()));
            }
            // --monster 8020:0:48:0 — spawn monster data id 0 at that frame,
            // offset (48, 0) from the hero (`Vm::spawn_monster`).
            "--monster" => {
                for spec in next().split(',') {
                    let p: Vec<i32> =
                        spec.split(':').filter_map(|v| v.trim().parse().ok()).collect();
                    if p.len() >= 2 {
                        a.monsters.push((
                            p[0] as u32,
                            p[1] as u32,
                            p.get(2).copied().unwrap_or(48),
                            p.get(3).copied().unwrap_or(0),
                        ));
                    }
                }
            }
            // --give 8020:worn:0:1,8020:7:3 — put an item in the bag at that
            // frame: `<frame>:<kind>:<idx>[:<count>]`, kind either a number or
            // `worn:<slot>` for the kind the hero wears in that slot (see
            // `Vm::give_item`). Separate entries with commas.
            "--give" => {
                for spec in next().split(',') {
                    let p: Vec<&str> = spec.split(':').collect();
                    if p.len() < 3 {
                        continue;
                    }
                    let (frame, kind, rest) = if p[1] == "worn" && p.len() >= 4 {
                        (p[0], format!("worn:{}", p[2]), &p[3..])
                    } else {
                        (p[0], p[1].to_string(), &p[2..])
                    };
                    let Ok(frame) = frame.trim().parse::<u32>() else { continue };
                    let idx = rest.first().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    let count = rest.get(1).and_then(|v| v.trim().parse().ok()).unwrap_or(1);
                    a.gives.push((frame, kind, idx, count));
                }
            }
            // --vault 8020:list,8030:put:0:0,8040:take:0 — drive 공용창고
            // without a screen: `list` prints the chest, `put:<tab>:<index>`
            // deposits that bag entry, `take:<id>` withdraws that chest entry,
            // `putgold:<n>` / `takegold:<n>` move gold, `hero-gold:<n>` gives
            // the hero some to move, `resume` finishes a transfer an earlier
            // run abandoned. The slot is whichever one is loaded
            // (`_selectSlotNumber`).
            "--vault" => {
                for spec in next().split(',') {
                    let (frame, what) = spec.split_once(':').unwrap_or((spec, "list"));
                    if let Ok(frame) = frame.trim().parse::<u32>() {
                        a.vault.push((frame, what.trim().to_string()));
                    }
                }
            }
            // --vault-stop 1|2 — abandon every transfer after that step, which
            // is what a crash in the gap looks like. Run once with it and once
            // without, on the same storage, to exercise a resume for real.
            "--vault-stop" => a.vault_stop = next().trim().parse().ok(),
            // --boss-zoom <frame> — arm the flourish a dying boss leaves on the
            // screen, which is otherwise unreachable here for the same reason:
            // it takes a boss. One byte (`hero[0x899]`), and the guest does the
            // rest. Pair it with `--profile-from/--profile-to` to see what the
            // effect costs, or with `--dump` to look at it.
            // --resize <frame>[:<percent>] — switch on the whole-screen zoom
            // (`HERO_Draw_Resize_Background`). Its call site is gated on
            // `hero[0x282] != 100` and nothing a tap sequence reaches moves
            // that, so this writes the halfword. Stays on for the rest of the
            // run, which is what makes it measurable.
            "--resize" => {
                let v = next();
                let mut parts = v.split(':');
                let frame = parts.next().unwrap_or("0").parse().unwrap_or(0);
                let pct = parts.next().and_then(|p| p.parse().ok()).unwrap_or(150);
                a.resize_at = (frame, pct);
            }
            "--boss-zoom" => a.boss_zoom_at = next().parse().unwrap_or(0),
            "--poke" => {
                for spec in next().split(',') {
                    let f: Vec<&str> = spec.trim().split(':').collect();
                    let num = |s: &str| {
                        let s = s.trim();
                        match s.strip_prefix("0x") {
                            Some(h) => u32::from_str_radix(h, 16).ok(),
                            None => s.parse().ok(),
                        }
                    };
                    if let [fr, ad, va] = f[..] {
                        if let (Some(fr), Some(ad), Some(va)) = (num(fr), num(ad), num(va)) {
                            a.pokes.push((fr, ad, va));
                        }
                    }
                }
            }
            // --pad-hud — hold `Vm::gamepad_hud` up for the whole run, the way
            // the Android shell does while `InputDevice` reports a pad. This is
            // how the legend is looked at (`--dump`) and how the off-state A/B
            // is measured, since no pad can reach a headless run.
            "--pad-hud" => a.pad_hud = true,
            "--save-doc" => a.save_doc = Some(next().into()),
            "--trace-log" => a.trace_log = true,
            "--profile" => a.profile = true,
            // Profile a window of frames rather than the whole run. The global
            // profile is dominated by whatever the run spends most of its time
            // in — which for any scenario that has to walk menus to reach the
            // game is the menus — so it cannot answer "what is expensive in
            // *that* frame". Point this at the frame numbers the frame-cost
            // report calls out and it can.
            // --profile-top 45 — how many hot functions to print. The tail is
            // where the small leaves worth porting hide; twenty stops just above
            // them once the big ones are gone.
            "--profile-top" => a.profile_top = next().parse().unwrap_or(20),
            "--profile-from" => {
                a.profile = true;
                a.profile_from = next().parse().unwrap_or(0);
            }
            "--profile-to" => {
                a.profile = true;
                a.profile_to = next().parse().unwrap_or(u32::MAX);
            }
            "--heap" => a.heap = true,
            "--audio" => a.audio_wav = Some(next().into()),
            "--dump" => a.dump = next().split(',').filter_map(|s| s.trim().parse().ok()).collect(),
            // --taps 60:1314x596,90:1200x662 — a press+release at a physical
            // surface pixel on the given frame. Coordinates are the same ones
            // `adb shell input tap` takes, so a sequence that reproduces a
            // device session can be replayed here frame-for-frame.
            "--taps" => {
                for spec in next().split(',') {
                    let mut it = spec.split(':');
                    let (Some(f), Some(xy)) = (it.next(), it.next()) else { continue };
                    let Ok(frame) = f.trim().parse::<u32>() else { continue };
                    let xy = xy.trim();
                    // A `+`/`-` suffix presses or releases without the other
                    // half, so a finger can be **held** across frames — which
                    // is the only way to see what the game does while one is
                    // down rather than on the way in and out.
                    let (xy, kind) = match xy.strip_suffix('+') {
                        Some(r) => (r, 1i8),
                        None => match xy.strip_suffix('-') {
                            Some(r) => (r, -1),
                            None => (xy, 0),
                        },
                    };
                    let mut c = xy.split(['x', 'X']);
                    let (Some(xs), Some(ys)) = (c.next(), c.next()) else { continue };
                    let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().parse::<i32>())
                    else {
                        continue;
                    };
                    a.taps.push((frame, x, y, kind));
                }
            }
            // --moves 8030:320x240 — a pointer MOVE on that frame, physical
            // pixels like --taps. Pairs with a `+` tap to drag a held finger.
            "--moves" => {
                for spec in next().split(',') {
                    let mut it = spec.split(':');
                    let (Some(f), Some(xy)) = (it.next(), it.next()) else { continue };
                    let Ok(frame) = f.trim().parse::<u32>() else { continue };
                    let mut c = xy.trim().split(['x', 'X']);
                    let (Some(xs), Some(ys)) = (c.next(), c.next()) else { continue };
                    if let (Ok(x), Ok(y)) = (xs.trim().parse::<i32>(), ys.trim().parse::<i32>()) {
                        a.moves.push((frame, x, y));
                    }
                }
            }
            // --keys 120:OK,300:DOWN  — fire a WIPI key press+release on the
            // given frame.
            "--keys" => {
                for spec in next().split(',') {
                    let mut it = spec.split(':');
                    let (Some(f), Some(k)) = (it.next(), it.next()) else { continue };
                    let Ok(frame) = f.trim().parse::<u32>() else { continue };
                    let k = k.trim().to_ascii_uppercase();
                    // A trailing +/- sends only the press or only the release,
                    // for holding a key across frames the way a finger (or a
                    // gamepad d-pad) actually does.
                    let (name, kind) = match k.strip_suffix('+') {
                        Some(n) => (n.to_string(), 1i8),
                        None => match k.strip_suffix('-') {
                            Some(n) => (n.to_string(), -1),
                            None => (k, 0),
                        },
                    };
                    let code = match name.as_str() {
                        "OK" | "SELECT" => cs_core::KEY_SELECT,
                        "UP" => cs_core::KEY_UP,
                        "DOWN" => cs_core::KEY_DOWN,
                        "LEFT" => cs_core::KEY_LEFT,
                        "RIGHT" => cs_core::KEY_RIGHT,
                        "CLEAR" => cs_core::KEY_CLEAR,
                        other => other.parse().unwrap_or(0),
                    };
                    a.keys.push((frame, code, kind));
                }
            }
            other => eprintln!("ignoring unknown option {other}"),
        }
        i += 1;
    }
    a
}

fn main() {
    let args = parse_args();
    init_logger(if args.trace_log { log::LevelFilter::Debug } else { log::LevelFilter::Info });

    let so = match std::fs::read(&args.so) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {}: {e}", args.so.display());
            std::process::exit(2);
        }
    };
    let vfs = match std::fs::read(&args.vfs) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {}: {e}", args.vfs.display());
            std::process::exit(2);
        }
    };
    let _ = std::fs::create_dir_all(&args.out);

    println!(
        "guest {} ({} KB), vfs {} ({:.1} MB), surface {}x{}",
        args.so.display(),
        so.len() / 1024,
        args.vfs.display(),
        vfs.len() as f64 / 1024.0 / 1024.0,
        args.width,
        args.height
    );

    // A save the 세이브 불러오기 / 되돌리기 rows staged before the last run,
    // swapped in before anything below reads a file of the player's — the
    // same point the Android shell does it (cs_core::savebundle).
    let report = cs_core::savebundle::apply_pending(&args.storage, cs_core::savebundle::wall_ms());
    for line in &report.log {
        println!("{line}");
    }
    if !report.may_start() {
        // What the phone does: a disk that may be two saves at once is not
        // played on. The next run tries again.
        eprintln!("the staged save could not be applied or undone; not starting the guest");
        std::process::exit(3);
    }

    // 22 050 Hz is what every clip in the archive is recorded at, so the probe
    // mixes at the source rate and the resampler is a no-op — the WAV it writes
    // is the decoder's output, unaltered.
    let mixer = cs_core::audio::Mixer::shared(22_050);
    let mut vm = match Vm::new(
        &so,
        Config {
            audio: mixer.clone(),
            vfs: Arc::new(vfs),
            storage: args.storage.clone(),
            screen_w: args.width,
            screen_h: args.height,
            virtual_clock: true,
            integer_scale: args.integer_scale,
            fill_width: args.fill_width,
            host: Default::default(),
            wipi_w: args.wipi.0,
            wipi_h: args.wipi.1,
            // Synchronous, so `--audio-wav` is reproducible run to run.
            async_audio: false,
            // A fault is a divergence, and these runs exist to catch divergence.
            tolerate_faults: false,
        },
    ) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "loaded: {} symbols, {} imports, exidx {:#x}+{:#x}",
        vm.elf.symbols.len(),
        vm.imports.len(),
        vm.elf.exidx.0,
        vm.elf.exidx.1
    );

    // Armed here only when the window starts at frame 0; otherwise the loop
    // arms it when it gets there.
    if args.profile && args.profile_from == 0 {
        vm.profile = Some(Default::default());
    }
    // Where the window's histogram waits after sampling stops. See the loop.
    let mut held_profile = None;
    // Not saved: a forced buff is a switch for this run, not a purchase.
    for &kind in &args.potions {
        let now = vm.wall_now_ms();
        vm.potions.grant(kind, now);
    }
    vm.gamepad_hud = args.pad_hud;

    let t0 = std::time::Instant::now();
    if let Err(e) = vm.boot() {
        eprintln!("boot failed after {} guest instructions: {e}", vm.steps);
        dump_log(&vm);
        std::process::exit(1);
    }
    println!(
        "boot ok: {} guest instructions in {:.2}s, mag {:.2}, margins {},{} (+{})",
        vm.steps,
        t0.elapsed().as_secs_f64(),
        vm.magnification,
        vm.margin_x,
        vm.margin_y,
        vm.auto_margin_y
    );

    let mut pcm: Vec<i16> = Vec::new();
    let mut last = 0u64;
    let mut cost: Vec<u64> = Vec::with_capacity(args.frames as usize);
    // Frames that made a new picture — what a device would actually present.
    // The gap to `--frames` is the display work `CS_LAZY_GL` skips.
    let mut dirty_frames = 0u32;
    for frame in 1..=args.frames {
        if args.pet_rename_at == frame && frame != 0 {
            match vm.open_pet_rename(false) {
                Ok(true) => println!("frame {frame}: pet-naming screen opened"),
                other => eprintln!("--pet-rename: {other:?}"),
            }
        }
        for (f, kind, idx, count) in &args.gives {
            if *f == frame {
                match vm.give_item(kind, *idx, *count) {
                    Ok(true) => println!("frame {frame}: gave {kind}:{idx} x{count}"),
                    other => eprintln!("--give {kind}:{idx}: {other:?}"),
                }
            }
        }
        for (f, what) in &args.vault {
            if *f != frame {
                continue;
            }
            // `put:0:4@1` abandons that one transfer after step 1, the way a
            // crash in the gap would; `--vault-stop` does it to every one.
            let (what, stop) = match what.split_once('@') {
                Some((w, n)) => (w, n.trim().parse().ok()),
                None => (what.as_str(), args.vault_stop),
            };
            vm.vault_stop_after = stop;
            let slot = vm.current_slot();
            let p: Vec<&str> = what.split(':').collect();
            let r = match p.as_slice() {
                ["list"] => Ok(()),
                ["resume"] => vm.vault_resume(false).map(|s| {
                    println!("frame {frame}: vault resume -> {s:?}");
                }),
                ["put", tab, i] => {
                    let (tab, i) = (tab.parse().unwrap_or(0), i.parse().unwrap_or(0));
                    vm.vault_deposit(slot, tab, i)
                }
                ["take", id] => vm.vault_withdraw(slot, id.parse().unwrap_or(0)).map(|_| ()),
                // Gold: the same three steps, through the same in-flight slot.
                ["putgold", n] => vm.vault_gold_deposit(slot, n.parse().unwrap_or(0)),
                ["takegold", n] => vm.vault_gold_withdraw(slot, n.parse().unwrap_or(0)),
                // Give the loaded hero this much gold, so a gold move has
                // something to move; a fresh save starts with none.
                ["hero-gold", n] => vm.set_hero_gold(n.parse().unwrap_or(0)),
                // Deposit and take straight back out, then check every field
                // of the record survived.
                ["roundtrip", tab, i] => {
                    let (tab, i) = (tab.parse().unwrap_or(0), i.parse().unwrap_or(0));
                    vm.vault_round_trip(slot, tab, i).map(|ok| println!("frame {frame}: {ok}"))
                }
                // Re-read the slot off disk, so what the bag shows next is
                // what step 2 actually wrote and not what memory remembers.
                ["reload"] => vm.load_slot(slot).map(|ok| {
                    println!("frame {frame}: slot {slot} reloaded: {ok}");
                }),
                _ => Err(format!("--vault: do not know {what:?}")),
            };
            if let Err(why) = r {
                eprintln!("frame {frame}: --vault {what}: {why}");
            }
            for line in vm.vault_describe() {
                println!("frame {frame}: slot {slot} {line}");
            }
            vm.vault_stop_after = None;
        }
        for &(f, id, dx, dy) in &args.monsters {
            if f == frame {
                match vm.spawn_monster(id, dx, dy) {
                    Ok(true) => println!("frame {frame}: monster {id} spawned at +({dx},{dy})"),
                    other => eprintln!("--monster {id}: {other:?}"),
                }
            }
        }
        if args.smith_at == frame && frame != 0 {
            match vm.open_blacksmith() {
                Ok(true) => println!("frame {frame}: blacksmith opened"),
                other => eprintln!("--smith: {other:?}"),
            }
        }
        if args.pet_at.0 == frame && frame != 0 {
            match vm.give_pet(args.pet_at.1) {
                Ok(true) => println!("frame {frame}: pet given ({:?})", args.pet_at.1),
                other => eprintln!("--pet: {other:?}"),
            }
        }
        if args.pet_at.0 != 0 && frame > args.pet_at.0 && std::env::var_os("CS_PET_DEBUG").is_some()
        {
            if let Ok(Some(st)) = vm.pet_state() {
                println!("frame {frame}: pet {st:?}");
            }
        }
        if args.merchant_at == frame && frame != 0 {
            match vm.open_merchant() {
                Ok(true) => println!("frame {frame}: merchant opened"),
                other => eprintln!("--merchant: {other:?}"),
            }
        }
        if args.resize_at.0 == frame && frame != 0 {
            let pct = args.resize_at.1;
            match vm.arm_resize_background(pct) {
                Ok(true) => println!("frame {frame}: background zoom set to {pct}%"),
                other => eprintln!("--resize: {other:?}"),
            }
        }
        for &(f, addr, val) in &args.pokes {
            if f == frame && frame != 0 {
                vm.mem.write_bytes(addr, &val.to_le_bytes());
                println!("frame {frame}: poke {addr:#x} = {val:#x}");
            }
        }
        if args.boss_zoom_at == frame && frame != 0 {
            match vm.arm_boss_zoom() {
                Ok(true) => println!("frame {frame}: boss-kill zoom armed"),
                other => eprintln!("--boss-zoom: {other:?}"),
            }
        }
        // Once --smith or --merchant has opened an NPC screen, input goes
        // straight to it (see `Vm::npc_ui_input`).
        let npc_open = (args.smith_at != 0
            && frame > args.smith_at
            && args.smith_until.is_none_or(|u| frame <= u))
            || (args.merchant_at != 0 && frame > args.merchant_at);
        for &(f, code, kind) in &args.keys {
            if f == frame {
                if npc_open {
                    if let Err(e) = vm.npc_ui_input(2, code, 0) {
                        eprintln!("frame {frame}: npc key failed: {e}");
                    }
                    continue;
                }
                if kind >= 0 {
                    let _ = vm.key_event(cs_core::MV_KEY_PRESS, code, 0);
                }
                if kind <= 0 {
                    let _ = vm.key_event(cs_core::MV_KEY_RELEASE, code, 0);
                }
            }
        }
        for &(f, x, y, kind) in &args.taps {
            if f == frame && kind >= 0 {
                if npc_open {
                    let (wx, wy) = vm.to_wipi(x, y);
                    if let Err(e) = vm.npc_ui_input(0x13, wx, wy) {
                        eprintln!("frame {frame}: npc tap failed: {e}");
                    }
                    continue;
                }
                let _ = vm.touch_event(cs_core::MV_POINTER_PRESS, x, y, 0);
            }
            if f == frame && kind < 0 {
                let _ = vm.touch_event(cs_core::MV_POINTER_RELEASE, x, y, 0);
            }
            // Release two frames later: a press and release in the same tick
            // is a zero-length touch, and the game's UI arms on press but only
            // acts on the release that follows it. A `+` tap skips this and
            // waits for its own `-`.
            if f + 2 == frame && kind == 0 {
                let _ = vm.touch_event(cs_core::MV_POINTER_RELEASE, x, y, 0);
            }
        }
        for &(f, x, y) in &args.moves {
            if f == frame {
                let _ = vm.touch_event(cs_core::MV_POINTER_MOVE, x, y, 0);
            }
        }
        // Sample only inside the window. Starting clears whatever the frames
        // before it collected; stopping takes the histogram out of the VM —
        // which is what turns sampling off — but keeps it for the report.
        if args.profile {
            if frame == args.profile_from {
                vm.profile = Some(Default::default());
            } else if frame == args.profile_to.saturating_add(1) {
                held_profile = vm.profile.take();
            }
        }
        vm.advance_time(args.ms);
        if args.audio_wav.is_some() {
            // One frame's worth of audio, so the track lines up with the run.
            let n = (22_050 * args.ms as usize / 1000) * 2;
            let base = pcm.len();
            pcm.resize(base + n, 0);
            if let Ok(mut m) = mixer.lock() {
                m.render(&mut pcm[base..]);
            }
        }
        if let Err(e) = vm.frame() {
            eprintln!("frame {frame} failed: {e}");
            let pc = vm.cpu.reg_get(arm32_cpu::Mode::User, arm32_cpu::reg::PC);
            eprintln!("{}", vm.context(pc));
            dump_log(&vm);
            write_png(&args.out.join(format!("fail_{frame:05}.png")), &vm);
            std::process::exit(1);
        }
        dirty_frames += u32::from(vm.frame_dirty);
        // The save rows asked for a document screen: `--save-doc` is the
        // phone's "save as" / "open", answered on the spot.
        if let Some(req) = vm.take_save_doc_request() {
            use cs_core::savexfer::{DocRequest, DocStatus};
            let id = req.id();
            let status = match (&args.save_doc, req) {
                (Some(doc), DocRequest::Export { file, name, .. }) => {
                    println!("frame {frame}: save doc: export {} as {name:?}", doc.display());
                    let r = std::fs::copy(&file, doc);
                    let _ = std::fs::remove_file(&file);
                    if r.is_ok() { DocStatus::Done } else { DocStatus::Failed }
                }
                (Some(doc), DocRequest::Import { file, .. }) => {
                    println!("frame {frame}: save doc: import {}", doc.display());
                    if std::fs::copy(doc, &file).is_ok() { DocStatus::Done } else { DocStatus::Failed }
                }
                (None, req) => {
                    println!("frame {frame}: save doc: no --save-doc, cancelling {req:?}");
                    DocStatus::Cancelled
                }
            };
            vm.save_doc_done(id, status);
        }
        if args.dump.contains(&frame) {
            let p = args.out.join(format!("frame_{frame:05}.png"));
            write_png(&p, &vm);
            println!(
                "frame {frame}: {} inst (+{}), {} draws -> {}",
                vm.steps,
                vm.steps - last,
                vm.gl.draws,
                p.display()
            );
        }
        cost.push(vm.steps - last);
        last = vm.steps;
        if vm.phase == cs_core::Phase::Exited {
            println!("guest exited at frame {frame}");
            break;
        }
    }
    report_frame_cost(&cost);

    let secs = t0.elapsed().as_secs_f64();
    println!(
        "done: {} frames, {} guest instructions, {:.1} M inst/s, {:.1} fps",
        args.frames,
        vm.steps,
        vm.steps as f64 / secs / 1e6,
        args.frames as f64 / secs
    );
    println!(
        "host patches: {} blits, {} rot ({:.1} ms), {} imgrot ({:.1} ms), {} screen, {} fills, {} fnt, {} vfs lookups, {} des, {} declined to the guest",
        vm.blit_host,
        vm.rot_host,
        vm.rot_ns as f64 / 1e6,
        vm.imgrot_host,
        vm.imgrot_ns as f64 / 1e6,
        vm.screen_host,
        vm.fill_host,
        vm.fnt_host,
        vm.vfs_host,
        vm.des_host,
        vm.patch_declined
    );
    // The two blurs cost more than the rest of their frame put together, and
    // the radial one now runs for as long as `CS_BOSS_ZOOM_MS` asks rather than
    // five frames — so its per-call wall time is the number that decides
    // whether the flourish is smooth. See `patch::host_boss_zoom`.
    if vm.blur_host + vm.radial_host > 0 {
        println!(
            "blurs: {} box, {} radial ({:.1} ms, {:.2} ms each)",
            vm.blur_host,
            vm.radial_host,
            vm.radial_ns as f64 / 1e6,
            vm.radial_ns as f64 / 1e6 / vm.radial_host.max(1) as f64
        );
    }
    // Served `UM_Sleep` waits. Under the probe's virtual clock these advance
    // the clock instead of sleeping, so the line reports what a device build
    // would have handed to the scheduler rather than to the interpreter.
    if vm.sleep_host > 0 {
        println!("sleeps: {} served, {} ms taken off the interpreter", vm.sleep_host, vm.sleep_ms);
    }
    println!(
        "pictures: {} of {} frames made a new one ({:.1}%) — the rest skip upscale/copy/present",
        dirty_frames,
        args.frames,
        100.0 * dirty_frames as f64 / args.frames.max(1) as f64
    );
    for &(addr, len) in &args.peek {
        match vm.mem.slice(addr, len) {
            Some(b) => {
                let nz = b.iter().filter(|&&v| v != 0).count();
                let head: Vec<String> = b.iter().take(24).map(|v| format!("{v:02x}")).collect();
                println!(
                    "peek {addr:#x}+{len}: {nz} non-zero ({:.1}%), head {}",
                    100.0 * nz as f64 / len.max(1) as f64,
                    head.join(" ")
                );
            }
            None => println!("peek {addr:#x}+{len}: not mapped"),
        }
    }
    if let Ok(m) = mixer.lock() {
        let (loaded, playing) = m.stats();
        println!(
            "audio: {} clips decoded, {} undecodable, {} loaded ({} KB), {} sounding",
            m.decoded_clips,
            m.decode_failures,
            loaded,
            m.bytes() / 1024,
            playing
        );
    }
    if let Some(p) = args.audio_wav.as_ref() {
        write_wav(p, &pcm, 22_050);
        println!("audio track -> {} ({:.1}s)", p.display(), pcm.len() as f64 / 2.0 / 22_050.0);
    }
    if args.heap {
        println!("--- largest live guest allocations ---");
        for (addr, size) in vm.heap.live_blocks().into_iter().take(12) {
            let nz = vm
                .mem
                .slice(addr, size)
                .map(|b| b.iter().filter(|&&x| x != 0).count())
                .unwrap_or(0);
            println!(
                "  {addr:#010x} {size:9} bytes, {:5.1}% non-zero",
                100.0 * nz as f64 / size.max(1) as f64
            );
        }
    }
    if let Some(p) = held_profile.take() {
        vm.profile = Some(p);
    }
    let hot = vm.profile_report(args.profile_top);
    if !hot.is_empty() {
        println!("--- guest hot functions (PC samples) ---");
        for (name, n, pct) in hot {
            println!("  {pct:5.1}%  {n:8}  {name}");
        }
    }
    dump_log(&vm);
}

/// What a Galaxy S21 measures for this interpreter, in guest instructions per
/// second. Only used to turn an instruction count into the number a player
/// actually feels; the counts themselves are exact and device-independent.
const DEVICE_INST_PER_SEC: f64 = 46e6;

/// Per-frame cost, as a distribution rather than a mean.
///
/// A mean hides exactly the thing that gets reported as "the game stutters
/// after a boss fight": the average can sit at a comfortable 15 ms while one
/// frame in fifty costs 60. So this prints the tail — and the worst frames by
/// number, because `--dump <that frame>` is then the next command to run.
fn report_frame_cost(cost: &[u64]) {
    if cost.is_empty() {
        return;
    }
    let ms = |inst: u64| inst as f64 / DEVICE_INST_PER_SEC * 1e3;
    let mut sorted: Vec<u64> = cost.to_vec();
    sorted.sort_unstable();
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    let mean = cost.iter().sum::<u64>() / cost.len() as u64;
    // 33 ms is the frame budget the game asks for (`--ms 33`).
    let over = cost.iter().filter(|&&c| ms(c) > 33.0).count();
    println!(
        "frame cost (guest inst -> ms at {:.0} M inst/s): mean {mean} ({:.1}), p50 {} ({:.1}), p90 {} ({:.1}), p99 {} ({:.1}), max {} ({:.1})",
        DEVICE_INST_PER_SEC / 1e6,
        ms(mean),
        at(0.50),
        ms(at(0.50)),
        at(0.90),
        ms(at(0.90)),
        at(0.99),
        ms(at(0.99)),
        sorted[sorted.len() - 1],
        ms(sorted[sorted.len() - 1])
    );
    println!(
        "  {over}/{} frames over the 33 ms budget ({:.1}%)",
        cost.len(),
        100.0 * over as f64 / cost.len() as f64
    );
    let mut worst: Vec<(usize, u64)> = cost.iter().copied().enumerate().collect();
    worst.sort_unstable_by_key(|&(_, c)| std::cmp::Reverse(c));
    let list: Vec<String> =
        worst.iter().take(8).map(|&(i, c)| format!("{}:{:.0}ms", i + 1, ms(c))).collect();
    println!("  worst frames {}", list.join(" "));
}

/// 16-bit stereo PCM WAV. Only used by the probe, to hear what the mixer
/// produced without needing an audio device.
fn write_wav(path: &std::path::Path, pcm: &[i16], rate: u32) {
    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&2u16.to_le_bytes()); // stereo
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(&w);
    }
}

/// A stderr logger. `env_logger` would do, but on this host it drags in
/// `windows-sys`, whose build needs a `dlltool` the gnu toolchain here does not
/// ship — not worth a dependency for a debugging aid.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("[{}] {}", r.level(), r.args());
    }
    fn flush(&self) {}
}

fn init_logger(level: log::LevelFilter) {
    static LOGGER: Stderr = Stderr;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

fn dump_log(vm: &Vm) {
    if vm.log.is_empty() {
        return;
    }
    // Collapse the repeats: the guest emits the same font-conversion notice
    // every frame, and it buries everything else.
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for l in &vm.log {
        *seen.entry(l.as_str()).or_insert(0) += 1;
    }
    let mut v: Vec<(&&str, &usize)> = seen.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    println!("--- guest log ({} lines, {} distinct) ---", vm.log.len(), v.len());
    for (l, n) in v.iter().take(40) {
        println!("  {n:6}x  {l}");
    }
}

/// Minimal PNG writer — one uncompressed-deflate IDAT. Avoids a dependency for
/// what is only a debugging artefact.
fn write_png(path: &std::path::Path, vm: &Vm) {
    let (fb, w, h) = vm.framebuffer();
    let (w, h) = (w as u32, h as u32);
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for y in 0..h {
        raw.push(0u8);
        for x in 0..w {
            let p = fb[(y * w + x) as usize];
            raw.push((p >> 16) as u8);
            raw.push((p >> 8) as u8);
            raw.push(p as u8);
        }
    }

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(&png);
    }
}

fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut c = crc32(tag);
    c = crc32_update(c, data);
    out.extend_from_slice(&(c ^ 0xffff_ffff).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for (i, block) in data.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xffff_ffff, data)
}

fn crc32_update(mut c: u32, data: &[u8]) -> u32 {
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    c
}
