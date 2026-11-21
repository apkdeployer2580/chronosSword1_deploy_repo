//! The guest-side code cave: features too large for one host patch per
//! instruction, compiled for the guest itself and copied into the hole above
//! its `.bss` at boot.
//!
//! The Chronos Sword 2 mod this repository ports its bag/skin/options features
//! from was built exactly this way — C compiled for ARM and dropped into a
//! `.bag10` section — except that it rewrote the `.so` on disk. Here the file
//! is never touched: `cave/build.py` compiles `cave/src/*` against the guest's
//! own symbol table into `cs_core/cave/cave.bin`, and writes the hook table
//! (`cave.manifest`) of every guest word that must change to reach it. Both
//! are embedded here, so a cargo build does not need the NDK; rerun
//! `python cave/build.py` after editing anything under `cave/`.
//!
//! Features (`FEATURES`, one `cave_cfg` byte each): `bag10` ten bag pages,
//! `skin` 외형변경 at the blacksmith, `options` the extra 환경설정 rows
//! (투구외형 / 방패외형 / 경험치배율 / 자동기본공격, and the retirement of
//! 데이터 관리), `autoatk` the auto basic attack the fourth of those rows
//! switches, `savexfer` the three save rows (세이브 내보내기 / 불러오기 /
//! 되돌리기) and their modal — the rows are drawn by the options hooks, so
//! the feature rides `CS_OPTIONS` as well as its own `CS_SAVEXFER`, and the
//! host half is `savexfer.rs`.
//!
//! What this module does at boot, after `elf::load` and before the host
//! patches in `patch.rs` are installed (so a host patch that verifies an
//! instruction word sees the hooked word and refuses, which is the conflict
//! check):
//!
//! 1. copies the blob to its base and zeroes its `.bss`;
//! 2. for each feature whose knob is on, verifies that **every** site still
//!    holds the word the manifest says it does, and only then writes the new
//!    words — a feature whose guest has moved under it is left out entirely,
//!    with a warning naming the first site that differs;
//! 3. writes the feature flags into the cave's `cave_cfg`;
//! 4. adds the cave's symbols to the guest's table, so `Vm::describe` names a
//!    cave function in a fault report and tests can `Vm::call` one by name.
//!
//! `CS_CAVE=0` skips all of it (the A/B knob for the whole mechanism);
//! `CS_BAG10=0` and the other per-feature knobs (`CS_SKIN`, `CS_OPTIONS`,
//! `CS_AUTOATK`) leave one feature out.
//!
//! One host patch reaches into the cave: `host_menu_arrow_hit` scales the
//! D-pad hit rects for `Check_Rect` calls made from the guest's sixteen menu
//! key handlers, and the cave's own key handlers (the options rows, the
//! blacksmith menu) call `Check_Rect` with the same four rects, so
//! `Vm::boot` adds the cave's range to that caller list.

use crate::patch::knob;
use crate::Vm;
use arm32_cpu::Memory;

const BLOB: &[u8] = include_bytes!("../cave/cave.bin");
const MANIFEST: &str = include_str!("../cave/cave.manifest");

/// One word the guest must change for a feature: where, what it holds now,
/// and what it becomes.
#[derive(Debug, Clone)]
pub struct Site {
    pub feature: &'static str,
    pub addr: u32,
    pub old: u32,
    pub new: u32,
    pub label: &'static str,
}

/// The parsed manifest.
pub struct Manifest {
    pub base: u32,
    pub size: u32,
    pub bss_end: u32,
    pub sites: Vec<Site>,
    pub symbols: Vec<(u32, &'static str)>,
}

/// Every feature the cave knows, in `cave_cfg` order (`cave/src/guest.h`).
pub const FEATURES: &[&str] = &["bag10", "skin", "options", "autoatk", "savexfer"];

/// A feature's knob, read with a literal name so the knob scan in `patch.rs`
/// (which keeps every knob read paired with an `option_env!` arm) can see it.
fn feature_on(feature: &str) -> bool {
    match feature {
        "bag10" => knob("CS_BAG10", 1) != 0,
        "skin" => knob("CS_SKIN", 1) != 0,
        "options" => knob("CS_OPTIONS", 1) != 0,
        "autoatk" => knob("CS_AUTOATK", 1) != 0,
        "savexfer" => knob("CS_SAVEXFER", 1) != 0 && knob("CS_OPTIONS", 1) != 0,
        _ => false,
    }
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

pub fn manifest() -> Result<Manifest, String> {
    let mut m = Manifest { base: 0, size: 0, bss_end: 0, sites: Vec::new(), symbols: Vec::new() };
    for (n, line) in MANIFEST.lines().enumerate() {
        let mut it = line.splitn(2, ' ');
        let key = it.next().unwrap_or("");
        let rest = it.next().unwrap_or("").trim();
        let bad = || format!("cave.manifest line {}: {line:?}", n + 1);
        match key {
            "base" => m.base = hex(rest).ok_or_else(bad)?,
            "size" => m.size = hex(rest).ok_or_else(bad)?,
            "bss_end" => m.bss_end = hex(rest).ok_or_else(bad)?,
            "patch" => {
                let mut f = rest.splitn(5, ' ');
                let feature = f.next().ok_or_else(bad)?;
                let addr = f.next().and_then(hex).ok_or_else(bad)?;
                let old = f.next().and_then(hex).ok_or_else(bad)?;
                let new = f.next().and_then(hex).ok_or_else(bad)?;
                let label = f.next().unwrap_or("");
                // The manifest is a compile-time constant, so its pieces can be
                // handed out as `&'static str` without copying.
                let feature: &'static str = leak(feature);
                m.sites.push(Site { feature, addr, old, new, label: leak(label) });
            }
            "sym" => {
                let mut f = rest.splitn(2, ' ');
                let addr = f.next().and_then(hex).ok_or_else(bad)?;
                let name = f.next().ok_or_else(bad)?;
                m.symbols.push((addr, leak(name)));
            }
            "" => {}
            _ => return Err(bad()),
        }
    }
    if m.base == 0 || m.size as usize != BLOB.len() {
        return Err(format!("cave.manifest disagrees with cave.bin ({} vs {} bytes)", m.size, BLOB.len()));
    }
    Ok(m)
}

/// The manifest lives in the binary; slices of it are already `'static`, but
/// the borrow checker cannot see through `lines()`, hence this identity.
fn leak(s: &str) -> &'static str {
    // SAFETY: `s` is a subslice of `MANIFEST`, a `&'static str`.
    unsafe { std::mem::transmute::<&str, &'static str>(s) }
}

/// Which features to install, from their knobs.
pub fn wanted() -> Vec<&'static str> {
    if knob("CS_CAVE", 1) == 0 {
        return Vec::new();
    }
    FEATURES.iter().copied().filter(|f| feature_on(f)).collect()
}

impl Vm {
    /// Copy the cave in and apply the hooks of every wanted feature. Returns
    /// the features that went in.
    pub fn install_cave(&mut self, want: &[&str]) -> Vec<&'static str> {
        let mut installed = Vec::new();
        if want.is_empty() {
            return installed;
        }
        let m = match manifest() {
            Ok(m) => m,
            Err(e) => {
                log::error!("cave: {e}; not installing");
                return installed;
            }
        };
        if m.base < self.elf.image_end || m.bss_end > crate::mem::HLE_BASE {
            log::error!(
                "cave: {:#x}..{:#x} does not fit between the image end {:#x} and the HLE window; not installing",
                m.base,
                m.bss_end,
                self.elf.image_end
            );
            return installed;
        }
        self.mem.write_bytes(m.base, BLOB);
        if let Some(bss) = self.mem.slice_mut(m.base + m.size, m.bss_end - m.base - m.size) {
            bss.fill(0);
        }
        self.cave_range = Some((m.base, m.bss_end));
        for (addr, name) in &m.symbols {
            self.elf.symbols.insert((*name).to_owned(), *addr);
        }

        for feature in FEATURES.iter().copied() {
            if !want.contains(&feature) {
                continue;
            }
            let sites: Vec<&Site> = m.sites.iter().filter(|s| s.feature == feature).collect();
            // Verify everything before writing anything: a half-applied
            // feature is the one outcome worse than a missing one.
            let moved = sites.iter().find(|s| self.mem.peek32(s.addr) != s.old);
            if let Some(s) = moved {
                log::warn!(
                    "cave: {feature} not installed — {} at {:#010x} holds {:#010x}, expected {:#010x}",
                    s.label,
                    s.addr,
                    self.mem.peek32(s.addr),
                    s.old
                );
                continue;
            }
            for s in &sites {
                self.mem.w32(s.addr, s.new);
            }
            installed.push(feature);
            log::info!("cave: {feature} installed ({} sites)", sites.len());
        }

        // Tell the cave which features are live.
        if let Some(cfg) = self.symbol("cave_cfg") {
            for (i, f) in FEATURES.iter().enumerate() {
                let on = installed.contains(f) as u8;
                self.mem.write_bytes(cfg + i as u32, &[on]);
            }
        }
        self.cave_installed = installed.clone();
        installed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::tests::loaded_guest;

    /// Run `f` with one hooked site holding its original word again, so the
    /// guest's own function can be called next to the cave's replacement.
    fn with_original<T>(vm: &mut Vm, site: &str, f: impl FnOnce(&mut Vm) -> T) -> T {
        let m = manifest().unwrap();
        let addr = vm.symbol(site).expect("guest symbol");
        let s = m.sites.iter().find(|s| s.addr == addr).expect("a hooked site");
        vm.mem.w32(addr, s.old);
        let r = f(vm);
        vm.mem.w32(addr, s.new);
        r
    }

    #[test]
    fn the_cave_installs_on_the_shipped_guest() {
        // Every word the manifest names still holds what cave/build.py saw when
        // it read payload/libchronosMian.so, the blob landed above the image,
        // and the guest's storage GOT slot now names the cave's copy.
        let vm = loaded_guest();
        assert_eq!(vm.cave_installed, FEATURES.to_vec());
        let m = manifest().unwrap();
        assert!(m.base >= vm.elf.image_end);
        assert!(m.sites.len() > 100);
        for s in &m.sites {
            assert_eq!(vm.mem.peek32(s.addr), s.new, "{} not applied", s.label);
        }
        let storage10 = vm.symbol("storage10").unwrap();
        let slot = m.sites.iter().find(|s| s.label.starts_with("GOT[_storage]")).unwrap();
        assert_eq!(vm.mem.peek32(slot.addr), storage10);
    }

    #[test]
    fn the_pool_is_the_same_for_everything_but_items() {
        // The four MP_* replacements restate the pool geometry of nine object
        // types. For every type but 7 (items, deliberately moved) the guest's
        // originals and the cave must name the same nodes and hand out free
        // positions in the same order.
        let mut vm = loaded_guest();
        let cave_init = vm.symbol("cave_MP_Initialize").unwrap();
        let cave_get = vm.symbol("cave_MP_getObj").unwrap();
        let cave_search = vm.symbol("cave_MP_search").unwrap();
        let orig_get = vm.symbol("MP_getObj").unwrap();
        let orig_search = vm.symbol("MP_search").unwrap();
        let scratch = vm.scratch;
        vm.call(cave_init, &[]).unwrap();

        // a position record is {ptr, s16 idx, u8 type, u8 free}
        let counts: &[(u32, u32)] =
            &[(1, 0x1e), (2, 3), (3, 0x41), (4, 0x32), (5, 0x32), (6, 0x168), (8, 0x32), (0x10, 0x96)];
        for &(ty, n) in counts {
            for idx in [0u32, n / 2, n - 1] {
                vm.mem.w32(scratch, 0);
                vm.mem.w32(scratch + 4, idx | (ty << 16) | (1 << 24));
                let a = vm.call(cave_get, &[scratch]).unwrap();
                let b = with_original(&mut vm, "MP_getObj", |vm| vm.call(orig_get, &[scratch]).unwrap());
                assert_eq!(a, b, "MP_getObj type {ty:#x} idx {idx}");
                assert_ne!(a, 0);
            }
        }

        // drain a type with the cave, re-initialise, drain with the original;
        // the two sequences of (ptr, idx|type) must match, and the answer past
        // the end must be type 0x12, "none"
        for &(ty, n) in counts {
            if ty == 2 {
                continue; // the original spins forever on a taken hero slot
            }
            let drain = |vm: &mut Vm, f: u32| -> Vec<(u32, u32)> {
                (0..n + 1)
                    .map(|_| {
                        vm.call(f, &[ty, scratch]).unwrap();
                        (vm.mem.peek32(scratch), vm.mem.peek32(scratch + 4))
                    })
                    .collect()
            };
            vm.call(cave_init, &[]).unwrap();
            let a = drain(&mut vm, cave_search);
            vm.call(cave_init, &[]).unwrap();
            let b = with_original(&mut vm, "MP_search", |vm| drain(vm, orig_search));
            assert_eq!(a, b, "MP_search sequence for type {ty:#x}");
            assert_eq!((a[n as usize].1 >> 16) & 0xff, 0x12);
        }
    }

    #[test]
    fn two_hundred_items_survive_the_sort() {
        // Ten bags of twenty synthetic items, worn ones and stacks among them:
        // after `bag10_sort` every item is still linked into some bag, the worn
        // one leads, stacks of the same countable item are merged to 99, and no
        // bag holds more than twenty.
        let mut vm = loaded_guest();
        let cave_init = vm.symbol("cave_MP_Initialize").unwrap();
        let new_item = vm.symbol("ITME_newItem").unwrap();
        let add = vm.symbol("LinkedList_addLastObject").unwrap();
        let sort = vm.symbol("bag10_sort").unwrap();
        let storage = vm.symbol("storage10").unwrap();
        vm.call(cave_init, &[]).unwrap();
        vm.mem.write_bytes(storage, &[0, 0, 10, 0]);

        // kind 0x1d (하급생명수) is countable; kind 0x07 (a weapon) is not
        for bag in 0..10u32 {
            let hdr = storage + 4 + bag * 0xc;
            for slot in 0..20u32 {
                let it = vm.call(new_item, &[]).unwrap();
                assert_ne!(it, 0, "pool ran out at bag {bag} slot {slot}");
                let (kind, idx, cnt, equip): (u8, u8, u8, u8) = if slot % 3 == 0 {
                    (0x1d, 0, 40, 0xff) // potions in stacks of 40
                } else {
                    (0x07, (slot % 5) as u8, 1, if bag == 0 && slot == 1 { 1 } else { 0xff })
                };
                vm.mem.write_bytes(it + 0x96, &[kind, cnt, equip]);
                vm.mem.write_bytes(it, &[idx]);
                vm.call(add, &[hdr, it + 0x9c]).unwrap();
                let c = (vm.mem.peek32(hdr + 8) & 0xff) as u8;
                vm.mem.write_bytes(hdr + 8, &[c + 1]);
            }
        }
        vm.call(sort, &[]).unwrap();

        let get = vm.symbol("ITEM_getItemFrom").unwrap();
        let mut seen = Vec::new();
        let mut potions = 0u32;
        for bag in 0..10u32 {
            let hdr = storage + 4 + bag * 0xc;
            let n = (vm.mem.peek32(hdr + 8) & 0xff) as u8;
            assert!(n <= 20, "bag {bag} holds {n}");
            for slot in 0..n as u32 {
                let it = vm.call(get, &[hdr, slot]).unwrap();
                assert_ne!(it, 0);
                let kind = (vm.mem.peek32(it + 0x94) >> 16) & 0xff;
                let cnt = (vm.mem.peek32(it + 0x94) >> 24) as u8;
                if kind == 0x1d {
                    potions += cnt as u32;
                }
                seen.push((bag, slot, kind, cnt));
            }
        }
        // 70 stacks of 40 = 2800 potions, now 28 stacks of 99 and one of 28
        assert_eq!(potions, 70 * 40);
        let stacks: Vec<u8> = seen.iter().filter(|s| s.2 == 0x1d).map(|s| s.3).collect();
        assert_eq!(stacks.len(), 29);
        assert!(stacks[..28].iter().all(|&c| c == 99));
        assert_eq!(stacks[28], 28);
        // the worn weapon leads
        let first = vm.call(get, &[storage + 4, 0]).unwrap();
        assert_eq!(vm.mem.peek32(first + 0x98) & 0xff, 1);
        // 130 weapons + 29 stacks = 159 items, packed twenty to a bag
        assert_eq!(seen.len(), 159);
        assert_eq!((vm.mem.peek32(storage + 4 + 7 * 0xc + 8) & 0xff) as u8, 19);
    }

    /// Every `orig_*` trampoline is the guest's displaced first word followed
    /// by a branch to its second instruction. The first version of
    /// `cave/build.py` wrote that branch as `b 0x10aacd8`, which the
    /// integrated assembler reads as a PC-relative *offset*, and the first
    /// key press in 환경설정 jumped into the void; this decodes the branch and
    /// checks where it lands.
    #[test]
    fn every_trampoline_returns_to_the_second_instruction() {
        let vm = loaded_guest();
        let m = manifest().unwrap();
        let mut seen = 0;
        for (addr, name) in &m.symbols {
            let Some(target) = name.strip_prefix("orig_") else { continue };
            let entry = vm.symbol(target).expect("the trampoline names a guest function");
            let site = m.sites.iter().find(|s| s.addr == entry).expect("the entry is hooked");
            assert_eq!(vm.mem.peek32(*addr), site.old, "{name}: displaced word");
            let b = vm.mem.peek32(*addr + 4);
            assert_eq!(b >> 24, 0xea, "{name}: second word is a plain b");
            let off = ((b & 0x00ff_ffff) << 8) as i32 >> 6; // sign-extended, times 4
            let dest = (*addr + 4).wrapping_add(8).wrapping_add(off as u32);
            assert_eq!(dest, entry + 4, "{name}: lands on the second instruction");
            seen += 1;
        }
        assert!(seen >= 4, "the options, smith and auto-attack hooks each have one");
    }

    /// 경험치배율 scales HERO_add_Exp's amount by the option byte and nothing
    /// else, and saturates rather than wrapping.
    #[test]
    fn the_experience_multiplier_is_the_option_byte() {
        let mut vm = loaded_guest();
        let scale = vm.symbol("opt_exp_scale").unwrap();
        let opt = vm.symbol("opt").unwrap();
        // pretend the record is loaded: magic 'C', version 1
        let loaded = vm.symbol("opt_loaded").unwrap();
        vm.mem.write_bytes(loaded, &[1]);
        for (mul, want) in [(0u8, 100u32), (1, 200), (2, 400), (3, 100)] {
            vm.mem.write_bytes(opt, &[b'C', 1, 0, 0, mul, 0, 0, 0]);
            assert_eq!(vm.call(scale, &[100]).unwrap(), want, "xpmul {mul}");
        }
        vm.mem.write_bytes(opt, &[b'C', 1, 0, 0, 2, 0, 0, 0]);
        assert_eq!(vm.call(scale, &[0x7000_0000]).unwrap(), 0x7fff_ffff, "saturates");
        assert_eq!(vm.call(scale, &[0]).unwrap(), 0, "nothing stays nothing");
    }

    /// The row count is nine shipped rows on **both** menus — 데이터 관리, the
    /// title menu's tenth, is retired — plus one row per feature that is in.
    #[test]
    fn the_option_rows_follow_the_features_that_are_in() {
        let mut vm = loaded_guest();
        let count = vm.symbol("opt_row_count").unwrap();
        let max = vm.symbol("opt_max_index").unwrap();
        let state = vm.symbol("_menuState").unwrap();
        let cfg = vm.symbol("cave_cfg").unwrap();
        // all in: 투구외형, 방패외형, 경험치배율, 자동기본공격, and the three
        // save rows — the same count under the title menu and in game
        vm.mem.w32(state, 4);
        assert_eq!(vm.call(count, &[]).unwrap(), 16);
        assert_eq!(vm.call(max, &[]).unwrap(), 15);
        vm.mem.w32(state, 0);
        assert_eq!(vm.call(count, &[]).unwrap(), 16);
        // without the skin feature the two visibility rows go, without
        // auto-attack its row goes, without savexfer the three save rows go;
        // 경험치배율 always stays
        vm.mem.write_bytes(cfg + 1, &[0]);
        assert_eq!(vm.call(count, &[]).unwrap(), 14);
        vm.mem.write_bytes(cfg + 3, &[0]);
        assert_eq!(vm.call(count, &[]).unwrap(), 13);
        vm.mem.write_bytes(cfg + 4, &[0]);
        assert_eq!(vm.call(count, &[]).unwrap(), 10);
        assert_eq!(vm.call(max, &[]).unwrap(), 9);
    }

    /// The options screen as a test sees it: a 400x240 LCD, the shipped
    /// record writer stubbed (the guest is not booted, so there is no storage
    /// to write to, and that write is the only side effect a row press has
    /// outside the menu globals), the options record marked loaded.
    struct OptScreen {
        key: u32,
        state: u32,
        sel: u32,
        point: u32,
        modal: u32,
    }

    fn options_screen(vm: &mut Vm) -> OptScreen {
        let lcd = vm.symbol("_lcdG").unwrap();
        vm.mem.w32(lcd + 0x4c, 400);
        vm.mem.w32(lcd + 0x50, 240);
        for stub in ["MX_sdSaveData", "MX_sdLoadData", "GameFrame_saveOptionData"] {
            let a = vm.symbol(stub).unwrap();
            vm.mem.w32(a, 0xe12f_ff1e); // bx lr
        }
        let loaded = vm.symbol("opt_loaded").unwrap();
        vm.mem.write_bytes(loaded, &[1]);
        OptScreen {
            key: vm.symbol("MainMenuOptionKey").unwrap(),
            state: vm.symbol("_menuState").unwrap(),
            sel: vm.symbol("_menuListKey").unwrap(),
            point: vm.symbol("_menuPoint").unwrap(),
            modal: vm.symbol("opt_modal").unwrap(),
        }
    }

    /// Row 9 selected, scrolled so it is on screen, no modal.
    fn select_row_nine(vm: &mut Vm, o: &OptScreen, menu_state: u32) {
        vm.mem.w32(o.state, menu_state);
        vm.mem.write_bytes(o.sel, &[9]);
        vm.mem.w32(o.point, 3);
        vm.mem.write_bytes(o.modal, &[0]);
    }

    /// 데이터 관리 is gone, and nothing on either menu can reach the screen it
    /// opened. The guest special-cases `_menuListKey == 9` in three places
    /// of `MainMenuOptionKey` (the key arm's jump table, the OK key, the touch
    /// arm's own row-9 block) and every one returns 0x64, the code for that
    /// screen; with row 9 now the cave's first row, every key the game has
    /// and every point on the screen goes through the hooked function with
    /// row 9 selected, and 0x64 never comes back. The control at the end
    /// puts the guest's own entry back and finds 0x64 at once — so the sweep
    /// is looking where the answer would be.
    #[test]
    fn row_nine_can_no_longer_open_the_data_manage_screen() {
        let mut vm = loaded_guest();
        let o = options_screen(&mut vm);
        // The host's own 0x64 blocker would also hide the answer; it is the
        // fallback for a build without the cave, not what is under test.
        vm.patches.retain(|(_, k)| !matches!(k, crate::patch::Patch::DataManageBlocked));
        let mut calls = 0u32;
        for menu_state in [4u32, 0] {
            for key in -20i32..=127 {
                select_row_nine(&mut vm, &o, menu_state);
                let r = vm.call(o.key, &[2, key as u32, 0]).unwrap();
                assert_ne!(r, 0x64, "key {key} with row 9 selected (menu state {menu_state})");
                calls += 1;
            }
            let mut y = 0;
            while y < 240 {
                let mut x = 0;
                while x < 400 {
                    select_row_nine(&mut vm, &o, menu_state);
                    let r = vm.call(o.key, &[0x13, x, y]).unwrap();
                    assert_ne!(r, 0x64, "touch ({x},{y}) with row 9 selected (menu state {menu_state})");
                    calls += 1;
                    x += 3;
                }
                y += 3;
            }
        }
        assert!(calls > 20_000, "{calls} calls");
        // The control: the shipped entry, the shipped OK on row 9.
        select_row_nine(&mut vm, &o, 4);
        let shipped =
            with_original(&mut vm, "MainMenuOptionKey", |vm| vm.call(o.key, &[2, (-5i32) as u32, 0]).unwrap());
        assert_eq!(shipped, 0x64, "the guest's own OK on row 9 is the data-manage screen");
        select_row_nine(&mut vm, &o, 4);
        assert_ne!(vm.call(o.key, &[2, (-5i32) as u32, 0]).unwrap(), 0x64, "and with the cave back, it is not");
    }

    /// The modal over the options list: raised only while a row of ours is
    /// selected, takes every key while it is up, answers 예/아니오 through the
    /// cave's `opt_xfer_answer`, and is dropped when the screen was left under
    /// it (leaving zeroes the selected row).
    #[test]
    fn the_modal_takes_every_key_and_hands_the_answer_back() {
        let mut vm = loaded_guest();
        let o = options_screen(&mut vm);
        let show = vm.symbol("opt_modal_show").unwrap();
        let answered = vm.symbol("opt_xfer_last_answer").unwrap();
        // The host's answer hook would take the answer first; the cave's own
        // body, which records it, is what this test reads.
        vm.patches.retain(|(_, k)| !matches!(k, crate::patch::Patch::XferAnswer));
        let text = vm.scratch;
        vm.mem.write_bytes(text, b"A;B;C\0");
        // Not on a row of ours: refused.
        vm.mem.w32(o.state, 4);
        vm.mem.write_bytes(o.sel, &[3]);
        assert_eq!(vm.call(show, &[2, text]).unwrap(), 0);
        assert_eq!(vm.mem.r8(o.modal), 0);
        // On the export row (9 + skin 2 + 경험치배율 + 자동기본공격): a question.
        vm.mem.write_bytes(o.sel, &[13]);
        vm.mem.w32(o.point, 7);
        assert_eq!(vm.call(show, &[2, text]).unwrap(), 1);
        assert_eq!((vm.mem.r8(o.modal), vm.mem.r8(o.modal + 1)), (2, 0), "kind, 예 selected");
        assert_eq!(vm.mem.cstr_bytes(o.modal + 4), b"A;B;C");
        // Every key is the modal's: the arrows move the selection, up/down and
        // the digits do nothing, and the row underneath never hears of them.
        for (key, sel) in [(-4i32, 1u8), (-3, 0), (b'6' as i32, 1), (-1, 1), (-2, 1), (b'4' as i32, 0), (b'1' as i32, 0)] {
            assert_eq!(vm.call(o.key, &[2, key as u32, 0]).unwrap(), 0, "key {key} is consumed");
            assert_eq!(vm.mem.r8(o.modal + 1), sel, "after key {key}");
            assert_eq!(vm.mem.r8(o.modal), 2, "still up after key {key}");
            assert_eq!(vm.mem.r8(o.sel), 13, "the row did not move");
        }
        // 아니오 by OK with the selection on it: down, and the cave heard "no".
        vm.mem.w32(answered, 7);
        vm.mem.write_bytes(o.modal + 1, &[1]);
        assert_eq!(vm.call(o.key, &[2, (-5i32) as u32, 0]).unwrap(), 0);
        assert_eq!(vm.mem.r8(o.modal), 0, "answered");
        assert_eq!(vm.mem.r32(answered), 0, "the answer reached opt_xfer_answer");
        // A tap on 예 (the left button, at h/2+0x1e) answers yes.
        assert_eq!(vm.call(show, &[2, text]).unwrap(), 1);
        let (w, h) = (400u32, 240u32);
        assert_eq!(vm.call(o.key, &[0x13, w / 2 - 0x40 - 4 + 8, h / 2 + 0x1e + 4]).unwrap(), 0);
        assert_eq!(vm.mem.r8(o.modal), 0, "a tap on 예 takes the modal down");
        assert_eq!(vm.mem.r32(answered), 1);
        // A message: 확인 in the middle, and OK closes it without an answer.
        vm.mem.w32(answered, 7);
        assert_eq!(vm.call(show, &[1, text]).unwrap(), 1);
        assert_eq!(vm.call(o.key, &[0x13, w / 2, h / 2 + 0x1e + 4]).unwrap(), 0);
        assert_eq!(vm.mem.r8(o.modal), 0);
        assert_eq!(vm.mem.r32(answered), 7, "a message has no answer to give");
        // Left under it: the next event finds the screen gone and drops it.
        // CLEAR is the event: the guest's own arm for it is "nothing", so the
        // unbooted guest is not asked to do anything after the drop.
        assert_eq!(vm.call(show, &[1, text]).unwrap(), 1);
        vm.mem.write_bytes(o.sel, &[0]);
        vm.mem.w32(o.point, 0);
        assert_eq!(vm.call(o.key, &[2, (-16i32) as u32, 0]).unwrap(), 0);
        assert_eq!(vm.mem.r8(o.modal), 0, "dropped with the screen");
    }

    /// The three save rows are presses: no value column, and only a step to
    /// the right (what OK and a tap on the selected row come in as) reaches
    /// the host's hook, with the row number in `r0`; a step to the left is
    /// nothing.
    #[test]
    fn the_save_rows_are_presses() {
        let mut vm = loaded_guest();
        let o = options_screen(&mut vm);
        let pressed = vm.symbol("opt_xfer_last_action").unwrap();
        // The host's hooks on the cave entries are installed by `Vm::new`;
        // take them out so the cave's own fallback body is what answers.
        vm.patches.retain(|(_, k)| !matches!(k, crate::patch::Patch::XferAction | crate::patch::Patch::XferAnswer));
        vm.mem.w32(o.state, 0);
        vm.mem.w32(o.point, 9);
        for (row, action) in [(13u8, 0u32), (14, 1), (15, 2)] {
            vm.mem.write_bytes(o.sel, &[row]);
            vm.mem.write_bytes(o.modal, &[0]);
            vm.mem.w32(pressed, 99);
            assert_eq!(vm.call(o.key, &[2, (-3i32) as u32, 0]).unwrap(), 0, "left on row {row}");
            assert_eq!(vm.mem.r8(o.modal), 0, "left on row {row} is nothing");
            assert_eq!(vm.mem.r32(pressed), 99);
            assert_eq!(vm.call(o.key, &[2, (-5i32) as u32, 0]).unwrap(), 0, "OK on row {row}");
            assert_eq!(vm.mem.r8(o.modal), 1, "OK on row {row} pressed it (the fallback says so)");
            assert_eq!(vm.mem.r32(pressed), action, "row {row} is action {action}");
        }
    }

    /// The draw-time shield hide only touches the hero's own sprite, and
    /// only turns a part into the "no part" marker the loop already skips.
    #[test]
    fn only_the_heros_shield_layer_is_dropped() {
        let mut vm = loaded_guest();
        let part = vm.symbol("skin_part_byte").unwrap();
        let get_hero = vm.symbol("MP_getHero").unwrap();
        let hero = vm.call(get_hero, &[0]).unwrap();
        assert_ne!(hero, 0);
        assert_eq!(vm.call(part, &[hero + 0x40, 0x85]).unwrap(), 0x7f, "the hero's shield part");
        assert_eq!(vm.call(part, &[hero + 0x40 + 0x100, 0x85]).unwrap(), 0x85, "another sprite");
        assert_eq!(vm.call(part, &[vm.scratch, 0x05]).unwrap(), 0x05, "not the hero");
    }
}
