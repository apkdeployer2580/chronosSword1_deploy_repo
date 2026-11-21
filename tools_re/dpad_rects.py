"""What would the menu D-pad's hit boxes swallow if we moved or grew them?

The four D-pad rects are copy-pasted into sixteen key handlers and are tested
**before** everything else on their screen, so enlarging the cluster
(`CS_DPAD_SCALE`) or pushing it off the corner (`CS_DPAD_DX`/`DY`) can make it
eat a button that belongs to something else. CLAUDE.md records one such find —
130% puts the down arrow over the left 15 px of `Compose_RecipeKey`'s 72x16
button — but that was measured by hand, once, for the scale axis only.

This recovers every `Check_Rect` rectangle in those sixteen functions by
walking them with a tiny symbolic interpreter (a register holds
`c + w*W + h*H`), then reports, for a grid of (scale, dx, dy), which
non-D-pad rectangle the moved cluster would first collide with.

    python tools_re/dpad_rects.py payload/libchronosMian.so            # the envelope
    python tools_re/dpad_rects.py payload/libchronosMian.so --at 125 4 4 [thick]
    python tools_re/dpad_rects.py payload/libchronosMian.so --dump     # every rect

`Check_Rect(x, y, rx, ry, [sp] rw, [sp+4] rh)` — the first two are the touch
point, so the rectangle is `r2, r3` plus the two stack words.
"""

import sys
from elftools.elf.elffile import ELFFile
from capstone import Cs, CS_ARCH_ARM, CS_MODE_ARM

# The cluster as the guest ships it, and the transform `patch.rs` applies.
DPAD_LEFT, DPAD_BOTTOM = 4, 2
DPAD_RECTS = [  # (rx, H - ry, rw, rh) as the guest writes them
    (0x1A, 0x53, 0x27, 0x17),  # up
    (0x1A, 0x17, 0x27, 0x17),  # down
    (0x00, 0x3C, 0x1A, 0x25),  # left
    (0x41, 0x3C, 0x1A, 0x25),  # right
]
HANDLERS = [
    "GameFrame_worldMapKeyPressed",
    "IGM_moveIconMtx",
    "GameMenuFrame_simpleBagKeyOnCube",
    "Compose_RecipeInputNumKey",
    "GameMenuCharPetKey",
    "Compose_RecipeKey",
    "GameMenuFrame_KeyPressed",
    "GameMenuQuest_KeyPressed",
    "ItemBagKey",
    "MainMenu_HelpKey",
    "MainMenuOptionKey",
    "NpcUI_tableKeyPressed",
    "NpcUI_skillTableKeyPressed",
    "NpcUI_skillMasterKeyPressed",
    "NpcUI_blackSmithKeyPressed",
    "NpcUI_merchantKeyPressed",
]
# The logical screen the numbers are evaluated at. 400x240 is the probe's and
# 520x240 is the S25's; width only shifts rects that are anchored to the right
# edge, so both are checked.
SCREENS = [(400, 240), (520, 240)]


def load(path):
    e = ELFFile(open(path, "rb"))
    st = e.get_section_by_name(".symtab")
    syms, sizes = {}, {}
    for s in st.iter_symbols():
        if s["st_shndx"] == "SHN_UNDEF" or not s.name:
            continue
        if s["st_info"]["type"] == "STT_FUNC":
            syms[s.name] = s["st_value"] & ~1
            sizes[s.name] = s["st_size"]
    data = open(path, "rb").read()
    segs = [
        (g["p_vaddr"], g["p_offset"], g["p_filesz"], g["p_memsz"])
        for g in e.iter_segments()
        if g["p_type"] == "PT_LOAD"
    ]

    def read(va, n):
        for v, o, fs, _ms in segs:
            if v <= va < v + fs:
                d = va - v
                return data[o + d : o + d + n]
        return b""

    return syms, sizes, read


# ---- the tiny interpreter ---------------------------------------------------
#
# A value is `(c, w, h)`, meaning `c + w*W + h*H`, or None when we lost track.
# That is enough because every rectangle in these handlers is built out of the
# screen size and constants: `w/2 - 0x41`, `H - 0x10`, `0x50`, and so on.

ZERO = (0, 0, 0)


def add(a, b, sign=1):
    if a is None or b is None:
        return None
    return (a[0] + sign * b[0], a[1] + sign * b[1], a[2] + sign * b[2])


def shift(a, n, arith=True):
    if a is None:
        return None
    # Only exact halving/doubling keeps the form; `asr #1` on `w` is `w/2`,
    # which the guest uses everywhere and which we carry as a fraction-free
    # coefficient by evaluating at a concrete screen later.
    if arith and n == 1:
        return (a[0] >> 1, a[1] / 2, a[2] / 2)
    return None


def rects_in(fn, base, size, read, check_rect):
    """Every `Check_Rect` rectangle reached in `fn`, as `(off, rect)`."""
    md = Cs(CS_ARCH_ARM, CS_MODE_ARM)
    md.detail = True
    code = read(base, size)
    reg = {}          # name -> (c, w, h)
    stack = {}        # sp offset -> (c, w, h)
    lcd = set()       # registers currently holding the _lcdG pointer
    out = []
    for i in md.disasm(code, base):
        m, ops = i.mnemonic, i.operands
        try:
            if m == "mov" and len(ops) == 2 and ops[1].type == 2:  # imm
                reg[i.reg_name(ops[0].reg)] = (ops[1].imm, 0, 0)
            elif m == "mvn" and len(ops) == 2 and ops[1].type == 2:
                reg[i.reg_name(ops[0].reg)] = (~ops[1].imm, 0, 0)
            elif m == "mov" and len(ops) == 2 and ops[1].type == 1:  # reg
                src = i.reg_name(ops[1].reg)
                reg[i.reg_name(ops[0].reg)] = reg.get(src)
                if src in lcd:
                    lcd.add(i.reg_name(ops[0].reg))
            elif m in ("add", "sub") and len(ops) == 3:
                d, a = i.reg_name(ops[0].reg), reg.get(i.reg_name(ops[1].reg))
                if ops[2].type == 2:
                    b = (ops[2].imm, 0, 0)
                else:
                    b = reg.get(i.reg_name(ops[2].reg))
                    if ops[2].shift.value:
                        b = None
                reg[d] = add(a, b, 1 if m == "add" else -1)
            elif m in ("asr", "lsr") and len(ops) == 3 and ops[2].type == 2:
                reg[i.reg_name(ops[0].reg)] = shift(
                    reg.get(i.reg_name(ops[1].reg)), ops[2].imm
                )
            elif m == "ldr" and len(ops) == 2 and ops[1].type == 3:  # [rn, #imm]
                rn = i.reg_name(ops[1].mem.base)
                d = i.reg_name(ops[0].reg)
                if rn == "pc":
                    # The literal pool. GCC parks constants here and reads them
                    # back at `pc + 8 + imm`; without this two thirds of the
                    # rectangles lose an argument.
                    lit = read(i.address + 8 + ops[1].mem.disp, 4)
                    reg[d] = (
                        (int.from_bytes(lit, "little", signed=False), 0, 0)
                        if len(lit) == 4
                        else None
                    )
                elif rn == "sp":
                    reg[d] = stack.get(ops[1].mem.disp)
                elif rn in lcd and ops[1].mem.disp == 0x4C:
                    reg[d] = (0, 1, 0)      # screen width
                elif rn in lcd and ops[1].mem.disp == 0x50:
                    reg[d] = (0, 0, 1)      # screen height
                else:
                    reg[d] = None
                    # `ldr rD, [r4, rN]` through the GOT lands the pointer to a
                    # global here; `_lcdG` is the one whose +0x4c/+0x50 matter,
                    # and it is the only global these handlers index that way
                    # before a Check_Rect. Treating every such load as a
                    # candidate is safe: a wrong guess yields a rect whose
                    # width is nonsense and those are dropped below.
            elif m == "str" and len(ops) == 2 and ops[1].type == 3:
                if i.reg_name(ops[1].mem.base) == "sp" and ops[1].mem.index == 0:
                    stack[ops[1].mem.disp] = reg.get(i.reg_name(ops[0].reg))
            elif m == "stm" and i.reg_name(ops[0].reg) == "sp":
                for n, o in enumerate(ops[1:]):
                    stack[4 * n] = reg.get(i.reg_name(o.reg))
            elif m == "bl":
                if ops[0].imm == check_rect:
                    out.append(
                        (
                            i.address - base,
                            (reg.get("r2"), reg.get("r3"), stack.get(0), stack.get(4)),
                        )
                    )
                # a call clobbers the scratch registers
                for r in ("r0", "r1", "r2", "r3", "r12", "ip"):
                    reg.pop(r, None)
                    lcd.discard(r)
            elif m == "ldr" and len(ops) == 2 and ops[1].type == 3:
                pass
        except Exception:
            pass
        # `ldr rD, [r4, rN]` — the GOT-indexed global load, marked after the
        # fact so the two branches above stay readable.
        if m == "ldr" and len(ops) == 2 and ops[1].type == 3 and ops[1].mem.index != 0:
            lcd.add(i.reg_name(ops[0].reg))
    return out


def at(v, w, h):
    return None if v is None else int(v[0] + v[1] * w + v[2] * h)


# The hexagon cluster (`patch.rs`: DPAD_ARM / DPAD_LONG / DPAD_SHORT), which
# replaces the shipped rects outright rather than scaling them. The four plates
# are one shape rotated -- up/down are LONG x SHORT, left/right SHORT x LONG --
# so the cluster is a `2*ARM + SHORT` square.
DPAD_ARM, DPAD_LONG, DPAD_SHORT = 27, 38, 23


# Where the 3x3 icon menu's left column starts answering, `W/2 - 65`, and the
# clamp `patch.rs::dpad_dx_for` puts on `dx` so the cluster never reaches it.
MENU_GRID_LEFT = 65


def dpad_dx_for(w, s, dx):
    span = 2 * (DPAD_ARM * s // 100) + DPAD_SHORT * s // 100
    return min(dx, max(w // 2 - MENU_GRID_LEFT - 1 - span, DPAD_LEFT))


def dpad_box(rx, ry_off, rw, rh, w, h, s, dx, dy, hexagons=True):
    """One of the four boxes after `patch.rs`, as (x0, y0, x1, y1)."""
    if hexagons:
        dx = dpad_dx_for(w, s, dx)
    if not hexagons:
        x = dx + (rx - DPAD_LEFT) * s // 100
        y0_src = h - ry_off
        y = h - dy - (h - y0_src - DPAD_BOTTOM) * s // 100
        return (x, y, x + rw * s // 100, y + rh * s // 100)
    # `DPAD_RECTS` is in up, down, left, right order and so is this.
    i = [r[:2] for r in DPAD_RECTS].index((rx, ry_off))
    arm = DPAD_ARM * s // 100
    lng, sht = DPAD_LONG * s // 100, DPAD_SHORT * s // 100
    span = 2 * arm + sht
    # x is anchored on the left edge, y on the bottom one, so an odd span puts
    # the extra pixel on opposite sides -- as in `patch.rs::dpad_plate`.
    cx, cy = dx + span // 2, h - dy - (span - span // 2)
    ox, oy, pw, ph = [
        (0, -arm, lng, sht),
        (0, arm, lng, sht),
        (-arm, 0, sht, lng),
        (arm, 0, sht, lng),
    ][i]
    x, y = cx + ox - pw // 2, cy + oy - ph // 2
    return (x, y, x + pw, y + ph)


def overlaps(a, b):
    return a[0] < b[2] and b[0] < a[2] and a[1] < b[3] and b[1] < a[3]


HEX = True


def main():
    global HEX
    path = sys.argv[1]
    syms, sizes, read = load(path)
    check_rect = syms["Check_Rect"]
    others = []   # (name, off, (x0,y0,x1,y1), screen)
    total_calls = n_dpad = n_lost = 0
    odd = []
    anchors = [(r[0], r[1]) for r in DPAD_RECTS]
    for name in HANDLERS:
        addr, size = syms[name], sizes[name]
        found = rects_in(name, addr, size, read, check_rect)
        total_calls += len(found)
        for off, (rx, ry, rw, rh) in found:
            # Classify once, at the first screen, so the counters mean call
            # sites rather than (site, screen) pairs.
            w0, h0 = SCREENS[0]
            xy = (at(rx, w0, h0), at(ry, w0, h0))
            if None not in xy and xy[0] is not None and (xy[0], h0 - xy[1]) in anchors:
                n_dpad += 1
            elif any(at(v, w0, h0) is None for v in (rx, ry, rw, rh)):
                n_lost += 1
            for w, h in SCREENS:
                vals = [at(v, w, h) for v in (rx, ry, rw, rh)]
                if any(v is None for v in vals):
                    continue
                x, y, ww, hh = vals
                if not (0 < ww <= w and 0 < hh <= h and -64 <= x <= w and -64 <= y <= h):
                    if (w, h) == SCREENS[0]:
                        odd.append((name, off, (x, y, ww, hh)))
                    continue
                # The cluster's own four, matched on **position only**. The
                # width or height of a copy is often set in a different basic
                # block and this linear walk loses it — `MainMenu_HelpKey+0x154`
                # comes back 1 px wide — so matching all four fields leaves the
                # arrows in the list and they then "collide" with themselves.
                # Nothing else in these handlers sits at either anchor; if
                # something did, it would collide in the shipped build too,
                # which is what the baseline below subtracts.
                if (x, h - y) in [(r[0], r[1]) for r in DPAD_RECTS]:
                    continue
                others.append((name, off, (x, y, x + ww, y + hh), (w, h)))

    kept = len(others) // len(SCREENS)
    print(
        f"{total_calls} Check_Rect sites in {len(HANDLERS)} handlers: "
        f"{n_dpad} are the cluster's own copies, {kept} other rectangles "
        f"recovered, {n_lost} lost (an argument is built in a block this "
        f"linear walk does not carry), {len(odd)} recovered but off the screen "
        f"(mis-read, shown by --dump). The lost ones are false negatives, so a "
        f"clean report is evidence and not proof.",
        file=sys.stderr,
    )
    if "--dump" in sys.argv:
        for name, off, r, sc in sorted(others):
            print(f"{name}+{off:#x} {sc[0]}x{sc[1]}: {r}")
        print(f"{len(others)} rectangles")
        print("--- recovered but implausible, so not checked ---")
        for name, off, r in sorted(odd):
            print(f"  {name}+{off:#x} x={r[0]} y={r[1]} w={r[2]} h={r[3]}")
        return

    def raw(s, dx, dy):
        bad = []
        for w, h in SCREENS:
            boxes = [dpad_box(*r, w, h, s, dx, dy, hexagons=HEX) for r in DPAD_RECTS]
            for name, off, r, sc in others:
                if sc != (w, h):
                    continue
                for bi, b in enumerate(boxes):
                    if overlaps(b, r):
                        bad.append((name, off, "udlr"[bi], r, b, sc))
        return bad

    # **The control.** Anything the shipped cluster already overlaps is not
    # something this feature broke, and it is also where a rect recovered
    # imperfectly shows up. Subtracting it means the answer is always "what
    # moving or growing the cluster costs", never "what the game already does".
    global HEX
    HEX = False
    base = {(c[0], c[1], c[5]) for c in raw(100, DPAD_LEFT, DPAD_BOTTOM)}
    HEX = "--sprite" not in sys.argv

    def collisions(s, dx, dy):
        return [c for c in raw(s, dx, dy) if (c[0], c[1], c[5]) not in base]

    if "--at" in sys.argv:
        k = sys.argv.index("--at")
        global DPAD_SHORT
        vals = [int(v) for v in sys.argv[k + 1 : k + 5] if v.lstrip("-").isdigit()]
        s, dx, dy = vals[:3]
        # A fourth number is `CS_DPAD_THICK`, which sets the plate thickness and
        # therefore the whole cluster (`2*ARM + thick` square).
        if len(vals) > 3:
            DPAD_SHORT = vals[3]
        bad = collisions(s, dx, dy)
        print(f"scale {s}%, dx {dx}, dy {dy}, thick {DPAD_SHORT}: {len(bad)} collision(s)")
        for name, off, arrow, r, b, sc in bad:
            print(f"  {arrow} {b} eats {name}+{off:#x} {r} at {sc[0]}x{sc[1]}")
        return

    print("largest clean margin at each scale (dx, dy grown independently)")
    for s in (100, 110, 115, 120, 125, 130):
        mx = max((d for d in range(4, 60) if not collisions(s, d, 4)), default=None)
        my = max((d for d in range(4, 60) if not collisions(s, 4, d)), default=None)
        both = max(
            (d for d in range(4, 60) if not collisions(s, d, d)), default=None
        )
        print(f"  {s:3}%   dx<={mx}   dy<={my}   dx=dy<={both}")


if __name__ == "__main__":
    main()
