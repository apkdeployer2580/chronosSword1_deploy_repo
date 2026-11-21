#!/usr/bin/env python3
"""Build the guest-side code cave: C/asm compiled for the guest, linked against
the guest's own symbol table, plus the table of hook sites the host applies.

    python cave/build.py            # writes cs_core/cave/cave.bin + cave.manifest
    python cave/build.py --check    # rebuild to a temp dir and diff against the committed output

Why this exists at all is in cave/README.md. In one line: the features ported
from the Chronos Sword 2 mod are far too large to express as one host patch per
instruction, and that mod was itself C compiled for the guest and dropped into
the hole above .bss - so that is what this does too, except the .so on disk is
never touched: the host copies the blob in and applies the hooks at boot, after
verifying that every site still holds the word we expect.

Toolchain: the Android NDK's clang (r25+), targeting ARMv5TE in ARM state so the
interpreter's decoder sees the same instruction set the game itself uses.
"""
import argparse
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
GUEST_SO = REPO / "payload" / "libchronosMian.so"
OUT_DIR = REPO / "cs_core" / "cave"
IMAGE_BASE = 0x01000000
CAVE_BASE = 0x011B0000  # first page above .bss (the CS1 image ends at 0x0117d288)
CAVE_LIMIT = 0x01400000  # hard ceiling; the HLE window starts at 0x01800000

from elftools.elf.elffile import ELFFile  # noqa: E402
from capstone import Cs, CS_ARCH_ARM, CS_MODE_ARM  # noqa: E402


def find_ndk():
    cands = []
    for env in ("ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "NDK_HOME"):
        if os.environ.get(env):
            cands.append(Path(os.environ[env]))
    sdk = os.environ.get("ANDROID_SDK_ROOT") or os.environ.get("ANDROID_HOME")
    if not sdk and os.environ.get("LOCALAPPDATA"):
        sdk = Path(os.environ["LOCALAPPDATA"]) / "Android" / "Sdk"
    if sdk:
        cands += sorted(Path(sdk).glob("ndk/*"), reverse=True)
    for c in cands:
        for host in ("windows-x86_64", "linux-x86_64", "darwin-x86_64"):
            b = c / "toolchains" / "llvm" / "prebuilt" / host / "bin"
            if b.exists():
                return b
    sys.exit("no Android NDK found (set ANDROID_NDK_HOME)")


SYM_SPEC = r"\s*([A-Za-z_][\w.$]*)\s*(?:\+\s*(0x[0-9a-fA-F]+|\d+))?\s*"  # 'Sym' or 'Sym+0x10'


class Guest:
    """The guest ELF: symbols, bytes at an address, and the runtime value of a
    GOT/data word after our loader has applied the relocations."""

    def __init__(self, path):
        self.e = ELFFile(open(path, "rb"))
        st = self.e.get_section_by_name(".symtab")
        self.syms = {}  # name -> (value, size, type)
        self.by_addr = []  # (value, size, name) sorted
        for s in st.iter_symbols():
            if s["st_shndx"] == "SHN_UNDEF" or not s.name:
                continue
            t = s["st_info"]["type"]
            if t not in ("STT_FUNC", "STT_OBJECT", "STT_NOTYPE"):
                continue
            # Bit 0 of a function's value is the Thumb bit; on an object it is
            # just an odd address (`_selectSlotNumber` lives at ...bd1).
            v = s["st_value"] & ~1 if t == "STT_FUNC" else s["st_value"]
            if s.name not in self.syms:
                self.syms[s.name] = (v, s["st_size"], t)
            if t in ("STT_FUNC", "STT_OBJECT"):
                self.by_addr.append((v, s["st_size"], s.name))
        self.by_addr.sort()
        self.segs = [
            (p["p_vaddr"], p["p_vaddr"] + p["p_filesz"], p.data())
            for p in self.e.iter_segments()
            if p["p_type"] == "PT_LOAD"
        ]
        self.relative = set()
        sec = self.e.get_section_by_name(".rel.dyn")
        if sec is not None:
            for r in sec.iter_relocations():
                if r["r_info_type"] == 23:  # R_ARM_RELATIVE
                    self.relative.add(r["r_offset"])
        got = self.e.get_section_by_name(".got")
        self.got_lo, self.got_hi = got["sh_addr"], got["sh_addr"] + got["sh_size"]

    def addr(self, spec):
        """'Sym', 'Sym+0x10', or '0x...' (image-relative, no base)."""
        m = re.fullmatch(r"\s*(0x[0-9a-fA-F]+)\s*", spec)
        if m:
            return int(m.group(1), 16)
        m = re.fullmatch(SYM_SPEC, spec)
        if not m:
            raise ValueError(f"bad address spec {spec!r}")
        if m.group(1) not in self.syms:
            raise KeyError(f"guest has no symbol {m.group(1)!r}")
        return self.syms[m.group(1)][0] + (int(m.group(2), 0) if m.group(2) else 0)

    def bytes_at(self, va, n):
        for lo, hi, d in self.segs:
            if lo <= va and va + n <= hi:
                return d[va - lo : va - lo + n]
        raise KeyError(f"{va:#x} is not in a loaded segment")

    def word(self, va):
        return struct.unpack("<I", self.bytes_at(va, 4))[0]

    def runtime_word(self, va):
        """What the host sees at `va` once `elf::load` has run: the file bytes
        plus the image base for every R_ARM_RELATIVE slot."""
        w = self.word(va)
        return (w + IMAGE_BASE) & 0xFFFFFFFF if va in self.relative else w

    def got_slot_of(self, sym):
        """The .got word holding `sym`'s address (PIC code reads globals through
        it). Exactly one such slot is expected."""
        target = self.syms[sym][0]
        hits = [a for a in range(self.got_lo, self.got_hi, 4) if self.word(a) == target]
        if len(hits) != 1:
            raise KeyError(f"expected one GOT slot for {sym}, found {len(hits)}")
        return hits[0]


# ---- ARM encoding helpers ---------------------------------------------------


def enc_branch(src, dst, link, cond=0xE):
    off = (dst - (src + 8)) >> 2
    if not -0x800000 <= off < 0x800000:
        raise ValueError(f"branch {src:#x} -> {dst:#x} out of range")
    return (cond << 28) | (0x0A000000 | (0x01000000 if link else 0)) | (off & 0x00FFFFFF)


def is_branch(word):
    return (word & 0x0E000000) == 0x0A000000


def branch_target(src, word):
    off = word & 0x00FFFFFF
    if off & 0x800000:
        off -= 0x1000000
    return src + 8 + off * 4


def relocatable_first_insn(word, va):
    """Can this instruction be moved into a trampoline unchanged? Anything that
    reads PC (literal loads, adr, branches) cannot."""
    md = Cs(CS_ARCH_ARM, CS_MODE_ARM)
    ins = next(md.disasm(struct.pack("<I", word), va), None)
    if ins is None:
        return False, "undecodable"
    if is_branch(word) or ins.mnemonic in ("bx", "blx"):
        return False, f"branch: {ins.mnemonic} {ins.op_str}"
    if "pc" in ins.op_str:
        return False, f"pc-relative: {ins.mnemonic} {ins.op_str}"
    return True, f"{ins.mnemonic} {ins.op_str}"


def set_ldr_str_imm(word, new_imm):
    """Rewrite the 12-bit immediate of an LDR/STR/LDRB/STRB (imm form), or the
    split 8-bit immediate of LDRH/STRH/LDRSB/LDRSH/LDRD/STRD."""
    if (word & 0x0E000000) == 0x04000000:  # single data transfer, imm12
        if new_imm > 0xFFF:
            raise ValueError("imm12 overflow")
        return (word & ~0xFFF) | new_imm
    if (word & 0x0E400090) == 0x00400090:  # halfword/signed, imm8 split
        if new_imm > 0xFF:
            raise ValueError("imm8 overflow")
        return (word & ~0xF0F) | ((new_imm & 0xF0) << 4) | (new_imm & 0xF)
    raise ValueError(f"not an immediate-offset load/store: {word:#010x}")


# ---- the build ----------------------------------------------------------------


def run(cmd):
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"command failed: {' '.join(map(str, cmd))}\n{r.stdout}\n{r.stderr}")
    if r.stderr.strip():
        print(r.stderr, file=sys.stderr)
    return r.stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="build to a temp dir and compare with the committed output")
    ap.add_argument("--keep", help="keep intermediate files in this directory")
    args = ap.parse_args()

    ndk = find_ndk()
    clang, lld, nm = ndk / "clang", ndk / "ld.lld", ndk / "llvm-nm"
    g = Guest(GUEST_SO)

    sys.path.insert(0, str(HERE))
    import hooks as H  # the hook table lives next to the sources

    work = Path(args.keep) if args.keep else Path(tempfile.mkdtemp(prefix="cave_"))
    work.mkdir(parents=True, exist_ok=True)

    # 1. Every guest symbol, for the linker. Names the cave defines itself are
    #    left out so the cave can override them.
    own = set()
    for src in sorted((HERE / "src").glob("*.[cS]")):
        own |= set(re.findall(r"^\s*\.globa?l\s+(\w+)", src.read_text(encoding="utf-8"), re.M))
        own |= set(re.findall(r"^\w[\w\s*]*?\b(\w+)\s*\([^;]*\)\s*\{", src.read_text(encoding="utf-8"), re.M))
    lines = []
    for name, (v, sz, t) in sorted(g.syms.items()):
        if name in own or not re.fullmatch(r"[A-Za-z_][\w.$]*", name):
            continue
        lines.append(f"PROVIDE({name} = {IMAGE_BASE + v:#x});")
    (work / "guest_syms.ld").write_text("\n".join(lines) + "\n")

    # 2. Trampolines for every "call the original" hook: the displaced first
    #    word followed by a branch to the second instruction. The branch is
    #    written against the guest symbol, never as a bare number: the
    #    integrated assembler reads `b 0x10aacd8` as a PC-relative offset, not
    #    an address, and the trampoline lands in the void.
    tramp = ["\t.syntax unified", "\t.arm", "\t.text"]
    for h in H.HOOKS:
        if h.get("orig"):
            m = re.fullmatch(SYM_SPEC, h["site"])
            if not m:
                sys.exit(f"{h['site']}: an `orig` trampoline needs a symbolic site")
            site = g.addr(h["site"])
            w = g.word(site)
            ok, why = relocatable_first_insn(w, site)
            if not ok:
                sys.exit(f"{h['site']}: first instruction cannot be displaced ({why})")
            back = (int(m.group(2), 0) if m.group(2) else 0) + 4
            tramp += [
                f"\t.global {h['orig']}",
                f"{h['orig']}:",
                f"\t.word {w:#010x}\t@ {why}",
                f"\tb {m.group(1)}+{back:#x}",
            ]
    (work / "trampolines.S").write_text("\n".join(tramp) + "\n")

    # 3. Compile.
    cflags = [
        "--target=armv5te-none-eabi", "-march=armv5te", "-marm", "-mfloat-abi=soft",
        "-O2", "-ffreestanding", "-fno-builtin", "-nostdlib", "-fno-pic", "-fno-pie",
        "-fno-stack-protector", "-fno-jump-tables", "-fomit-frame-pointer",
        "-fno-asynchronous-unwind-tables", "-fno-unwind-tables", "-fno-exceptions",
        "-Wall", "-Wextra", "-Wno-unused-parameter", "-Wno-unused-function",
        "-I", str(HERE / "src"),
    ]
    objs = []
    for src in sorted((HERE / "src").glob("*.[cS]")) + [work / "trampolines.S"]:
        obj = work / (src.name + ".o")  # bag10.c and bag10.S must not collide
        run([clang] + cflags + ["-c", str(src), "-o", str(obj)])
        objs.append(obj)

    # 4. Link at the cave base against the guest's addresses.
    elf_out = work / "cave.elf"
    run([lld, "-T", HERE / "cave.ld", "-T", work / "guest_syms.ld", "--no-dynamic-linker", "-o", elf_out] + objs)

    ce = ELFFile(open(elf_out, "rb"))
    blob = bytearray()
    bss_end = CAVE_BASE
    for p in ce.iter_segments():
        if p["p_type"] != "PT_LOAD":
            continue
        va, fsz, msz = p["p_vaddr"], p["p_filesz"], p["p_memsz"]
        if va < CAVE_BASE + len(blob):
            sys.exit("overlapping segments")
        blob += b"\0" * (va - CAVE_BASE - len(blob))
        blob += p.data()[:fsz]
        bss_end = max(bss_end, va + msz)
    if bss_end >= CAVE_LIMIT:
        sys.exit(f"cave too large: ends at {bss_end:#x}")

    csyms = {}
    for line in run([nm, "--defined-only", elf_out]).splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[1] in "TtDdBbRr":
            csyms[parts[2]] = int(parts[0], 16)

    def cave_addr(spec):
        m = re.fullmatch(r"\s*([A-Za-z_]\w*)\s*(?:\+\s*(0x[0-9a-fA-F]+|\d+))?\s*", spec)
        if not m or m.group(1) not in csyms:
            raise KeyError(f"cave has no symbol {spec!r}")
        return csyms[m.group(1)] + (int(m.group(2), 0) if m.group(2) else 0)

    # 5. The hook table.
    out = [f"base {CAVE_BASE:#x}", f"size {len(blob):#x}", f"bss_end {bss_end:#x}"]
    seen = {}
    for h in H.HOOKS:
        feat = h["feature"]
        kind = h["kind"]
        if kind == "got":
            site = g.got_slot_of(h["site"])
        else:
            site = g.addr(h["site"])
        gsite = IMAGE_BASE + site
        if kind in ("b", "bl"):
            old = g.word(site)
            if "expect" in h and old != h["expect"]:
                sys.exit(f"{h['site']}: expected {h['expect']:#010x} at site, found {old:#010x}")
            if kind == "bl" and "expect_call" in h:
                if not is_branch(old) or branch_target(site, old) != g.addr(h["expect_call"]):
                    sys.exit(f"{h['site']}: expected a call to {h['expect_call']}, found {old:#010x}")
            new = enc_branch(gsite, cave_addr(h["target"]), link=(kind == "bl"), cond=h.get("cond", 0xE))
            label = f"{h['site']} -> {h['target']}"
        elif kind == "word":
            old = g.runtime_word(site)
            if "expect" in h and old != h["expect"]:
                sys.exit(f"{h['site']}: expected {h['expect']:#010x}, found {old:#010x}")
            new = h["value"] if isinstance(h["value"], int) else cave_addr(h["value"])
            label = f"{h['site']} := {h['value']}"
        elif kind == "got":
            old = g.runtime_word(site)
            new = cave_addr(h["value"])
            label = f"GOT[{h['site']}] := {h['value']}"
        elif kind == "imm":
            old = g.word(site)
            if "expect" in h and old != h["expect"]:
                sys.exit(f"{h['site']}: expected {h['expect']:#010x}, found {old:#010x}")
            new = set_ldr_str_imm(old, h["value"])
            label = f"{h.get('label', h['site'])} imm -> {h['value']:#x}"
        else:
            sys.exit(f"unknown hook kind {kind}")
        if gsite in seen:
            sys.exit(f"two hooks at {gsite:#x}: {seen[gsite]} and {label}")
        seen[gsite] = label
        out.append(f"patch {feat} {gsite:#010x} {old:#010x} {new:#010x} {label}")
    for name, a in sorted(csyms.items(), key=lambda kv: kv[1]):
        if not name.startswith((".", "$")):
            out.append(f"sym {a:#010x} {name}")
    manifest = "\n".join(out) + "\n"

    if args.check:
        same = (OUT_DIR / "cave.bin").read_bytes() == bytes(blob) and (OUT_DIR / "cave.manifest").read_text() == manifest
        print("up to date" if same else "OUT OF DATE: run cave/build.py")
        sys.exit(0 if same else 1)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    (OUT_DIR / "cave.bin").write_bytes(bytes(blob))
    (OUT_DIR / "cave.manifest").write_text(manifest)
    print(f"cave: {len(blob)} bytes at {CAVE_BASE:#x}, bss to {bss_end:#x}, {len(H.HOOKS)} hooks, {len(csyms)} symbols")
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
