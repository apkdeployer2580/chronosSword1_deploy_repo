"""Who calls this guest function?

`xref.py` only answers that for PLT imports. Most of the interesting questions in
this binary are about *internal* functions — "does the load path re-run the equip
gate?", "is this hook's caller list complete?" — and those are direct `bl`/`b`
branches to a symbol's address, plus the occasional address taken as a literal
(a function pointer parked in the GOT, which is how the two screen blitters are
reached at all).

    python tools_re/callers.py payload/libchronosMian.so Check_Hand HERO_load_data
    python tools_re/callers.py payload/libchronosMian.so @0x65ea0

Prints, per target: every `bl`/`b`/`blx` to it with the enclosing function and
offset, then every word anywhere in the image whose value equals the address
(function-pointer references), also attributed to a symbol.
"""

import sys
import bisect
from elftools.elf.elffile import ELFFile
from capstone import Cs, CS_ARCH_ARM, CS_MODE_ARM


def load(path):
    e = ELFFile(open(path, "rb"))
    st = e.get_section_by_name(".symtab")
    funcs = sorted(
        (s["st_value"] & ~1, s.name)
        for s in st.iter_symbols()
        if s["st_info"]["type"] == "STT_FUNC" and s["st_shndx"] != "SHN_UNDEF"
    )
    byname = {}
    for s in st.iter_symbols():
        if s["st_shndx"] != "SHN_UNDEF" and s.name:
            byname.setdefault(s.name, s["st_value"] & ~1)
    return e, funcs, byname


def owner(funcs, addrs, a):
    i = bisect.bisect_right(addrs, a) - 1
    if i < 0:
        return "?", 0
    return funcs[i][1], a - funcs[i][0]


def main():
    path = sys.argv[1]
    e, funcs, byname = load(path)
    addrs = [f[0] for f in funcs]

    targets = {}
    for name in sys.argv[2:]:
        if name.startswith("@"):
            targets[int(name[1:], 0)] = name
        elif name in byname:
            targets[byname[name]] = name
        else:
            print(f"!! {name} not found")

    # Pass 1 — branches. Disassemble .text linearly; capstone stops at data, so
    # step over the bad word and keep going rather than giving up on the rest.
    text = e.get_section_by_name(".text")
    data, base = text.data(), text["sh_addr"]
    md = Cs(CS_ARCH_ARM, CS_MODE_ARM)
    hits = {t: [] for t in targets}
    off = 0
    while off < len(data):
        n = 0
        for i in md.disasm(data[off:], base + off):
            n += 1
            off = i.address - base + 4
            if i.mnemonic.startswith(("bl", "b")) and i.op_str.startswith("#"):
                try:
                    t = int(i.op_str[1:], 0)
                except ValueError:
                    continue
                if t in hits:
                    hits[t].append((i.address, i.mnemonic))
        if n == 0:
            off += 4

    # Pass 2 — the address parked as a literal word anywhere in the image. GOT
    # slots and jump tables both show up here.
    refs = {t: [] for t in targets}
    for seg in e.iter_segments():
        if seg["p_type"] != "PT_LOAD":
            continue
        blob, va = seg.data(), seg["p_vaddr"]
        for k in range(0, len(blob) - 3, 4):
            w = int.from_bytes(blob[k : k + 4], "little")
            if w in refs:
                refs[w].append(va + k)

    for t, name in targets.items():
        print(f"\n===== {name} @ {t:#x} =====")
        print(f"  branches: {len(hits[t])}")
        for a, m in hits[t]:
            o, d = owner(funcs, addrs, a)
            print(f"    {a:#010x}  {m:5s}  {o}+{d:#x}")
        if refs[t]:
            print(f"  address referenced as a word: {len(refs[t])}")
            for a in refs[t]:
                o, d = owner(funcs, addrs, a)
                where = f"{o}+{d:#x}" if d < 0x4000 else "(data)"
                print(f"    {a:#010x}  {where}")


if __name__ == "__main__":
    main()
