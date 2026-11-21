# cave — guest-side code for features too big for host patches

The Chronos Sword 2 mod (`chronosSword/크로노소드2.apk`) that these features
are ported from is C compiled for the guest and dropped into a `.bag10` section
above `.bss`, with branches from the original functions into it. This directory
does the same for Chronos Sword 1, with one difference that matters: the `.so`
on disk is never modified. `cs_core/src/cave.rs` copies the blob into guest
memory at boot and applies the hooks there, after checking every site.

```
cave/
  build.py        NDK clang -> cs_core/cave/cave.bin + cave.manifest   (python cave/build.py)
  cave.ld         the blob is linked at 0x011b0000, the hole above the image
  hooks.py        every guest word that changes, by feature, with the word expected there
  src/guest.h     guest functions, globals and struct offsets the cave relies on
  src/entry.c     cave_cfg (feature flags the host writes), memcpy/memset
  src/rt.S        __aeabi_* division shims onto the guest's libgcc
  src/mp.c        the object pool with a 256-item sub-pool          (bag10)
  src/bag10.c     storage struct, sort, save buffer, load wrapper     (bag10)
  src/bag10.S     tab strip / key detours                             (bag10)
  src/cave.h      what the cave's own files share (the options record)
  src/skin.c      외형변경: record table, sprite substitution, smith UI (skin)
  src/skin.S      smith entry/draw/reset, bag dimming, shield hide   (skin)
  src/opt.c       the extra 환경설정 rows and their record, the retirement
                  of 데이터 관리, the save rows and their modal (options, savexfer)
  src/opt.S       drawOption / OptionKey / ListKey / add_Exp / overlay detours
  src/autoatk.c   자동기본공격 world tick                              (autoatk)
  src/autoatk.S   HERO_Check_Autotarget override                     (autoatk)
```

The generated `cs_core/cave/cave.bin` and `cave.manifest` are committed, so
`cargo build` never needs the NDK. After editing anything here run
`python cave/build.py` and commit the two outputs; `python cave/build.py --check`
fails if they are stale.

## Rules that keep this safe

* **Every hook names the word it expects.** `build.py` reads the guest and
  refuses to build if a site does not hold it; the host checks again at boot
  and drops the whole feature (never half of it) if a word differs.
* **Displaced instructions are re-stated verbatim** at the top of each detour
  in `*.S`, and the branch back goes to the instruction after them. A hook on
  a function entry that still needs the original (`orig=`) gets a generated
  trampoline, and `build.py` rejects a first instruction that reads PC.
* **Whole-function replacements** (`b cave_fn` at the entry) are preferred to
  mid-function detours where the function is small enough to restate in C
  — the pool (`mp.c`), the sort, the free-slot count, the hero save.
* **Call-site hooks** (`bl` retargeted, checked with `expect_call`) are
  preferred to both: no displaced instruction at all.
* **No `_storage` in C.** The guest reaches it through its GOT slot, which the
  hook table points at `storage10`; C code uses `storage10` directly so both
  sides see the same struct.
* Compile for ARMv5TE in ARM state (`-marm -march=armv5te`), never Thumb: the
  interpreter decodes what the game itself is built from.
* **Branches to guest addresses are written as `Symbol+offset`, never as a
  bare number.** The integrated assembler reads `b 0x10aacd8` as a PC-relative
  offset, not an address; the first `orig=` trampoline was generated that way
  and the first key press in 환경설정 jumped into the void. `build.py` now emits
  `b Sym+off`, and `cave.rs` has a test that decodes every trampoline's branch.
* **Detours that call C push an even number of registers**, so the stack stays
  8-aligned at the call (AAPCS; clang may use `strd`/`ldrd`).
* The host scales the four D-pad hit rects for `Check_Rect` calls that come
  from the guest's menu key handlers *and from the cave* (`Vm::boot` adds the
  cave's range to that list), so cave code may test those rects with the
  shipped values and nothing else — the match is by value, and a stray call
  with the same values would be moved too.

## A/B

`CS_CAVE=0` leaves the whole mechanism out; `CS_BAG10=0`, `CS_SKIN=0`,
`CS_OPTIONS=0`, `CS_AUTOATK=0` and `CS_SAVEXFER=0` leave one feature out
(`savexfer` also needs `CS_OPTIONS`: its rows are drawn by the options hooks,
and its host half — files, vault, the phone's document screens — is
`cs_core/src/savexfer.rs`, which hooks the cave's `opt_xfer_action` /
`opt_xfer_answer` entries). All are build-time
knobs like every other `CS_*` (`patch.rs::knob`), and the probe reads them from
the environment too.

The probe can reach what a tap sequence cannot: `--smith <frame>` opens the
blacksmith (input then goes straight to `NpcUI_KeyPressed`), `--give
<frame>:worn:<slot>:<idx>` puts a transmog donor in the bag, `--monster
<frame>:<id>[:dx:dy]` spawns a monster next to the hero for 자동기본공격.
