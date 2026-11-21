#!/usr/bin/env python3
"""Differential test: arm32_cpu (this fork) vs Unicorn (QEMU's ARM core).

Generates random ARM instructions with random register/memory state, runs one
instruction through both, and reports every disagreement.

    python tools_re/armdiff.py [--n 20000] [--seed 1] [--class smul|ldrd|all]

The interesting classes are the ones this fork implements by hand — the ARMv5TE
DSP multiplies and LDRD/STRD, which upstream (an ARMv4T core) does not have —
plus a broad random sweep to catch anything the decode table gets wrong.

Needs `pip install unicorn capstone` and the `armdiff` helper binary:

    cargo build --release -p cs_probe --bin armdiff
"""
import argparse
import random
import struct
import subprocess
import sys
from pathlib import Path

try:
    from unicorn import Uc, UC_ARCH_ARM, UC_MODE_ARM, UC_PROT_ALL
    from unicorn.arm_const import UC_ARM_REG_R0, UC_ARM_REG_CPSR, UC_ARM_REG_PC
except ImportError:
    sys.exit("pip install unicorn")

try:
    from capstone import Cs, CS_ARCH_ARM, CS_MODE_ARM
    MD = Cs(CS_ARCH_ARM, CS_MODE_ARM)
except ImportError:
    MD = None

SIZE = 0x2000
MEM = 256
DATA = 0x1000
ROOT = Path(__file__).resolve().parent.parent
HELPER = ROOT / "target" / "release" / "armdiff.exe"
if not HELPER.exists():
    HELPER = ROOT / "target" / "release" / "armdiff"


# Each generator returns (instruction, {reg: value}). The fixups pin the base
# register (and any offset register) so the access lands inside the scratch page
# and is correctly aligned — an UNPREDICTABLE encoding proves nothing, because
# the reference is free to do anything with it.


def gen_smul(rnd):
    """SMLA<x><y> / SMLAW<y> / SMULW<y> / SMLAL<x><y> / SMUL<x><y>."""
    op = rnd.randrange(4)
    x, y = rnd.randrange(2), rnd.randrange(2)
    rd, rs, rm = (rnd.randrange(12) for _ in range(3))
    rn = rnd.randrange(12)
    if op == 0b10:
        # SMLAL<x><y>: RdHi and RdLo must differ.
        while rn == rd:
            rn = rnd.randrange(12)
    inst = (
        0xE0000000 | (0b0001 << 24) | (op << 21) | (rd << 16) | (rn << 12)
        | (rs << 8) | (1 << 7) | (y << 6) | (x << 5) | rm
    )
    return inst, {}


def _addr_fixups(rnd, rn, align, rm=None):
    """Base (and index) values that keep the access in-window and aligned."""
    fix = {rn: DATA + rnd.randrange(0, 128, align)}
    if rm is not None and rm != rn:
        fix[rm] = rnd.randrange(0, 64, align)
    return fix


def gen_ldrd(rnd):
    """LDRD / STRD. Rd must be even; Rn must not overlap Rd/Rd+1 on writeback."""
    store = rnd.randrange(2)
    p, u = rnd.randrange(2), 1
    imm = rnd.randrange(2)
    rd = rnd.randrange(0, 10, 2)
    rn = rnd.choice([r for r in range(12) if r not in (rd, rd + 1)])
    w = rnd.randrange(2) if p else 0
    sh = 0b11 if store else 0b10
    base = 0xE0000000 | (p << 24) | (u << 23) | (imm << 22) | (w << 21) | (rn << 16) | (rd << 12)
    if imm:
        off = rnd.randrange(0, 64, 8)
        inst = base | ((off >> 4) << 8) | (1 << 7) | (sh << 5) | (1 << 4) | (off & 0xF)
        return inst, _addr_fixups(rnd, rn, 8)
    rm = rnd.choice([r for r in range(12) if r not in (rd, rd + 1, rn)])
    inst = base | (1 << 7) | (sh << 5) | (1 << 4) | rm
    return inst, _addr_fixups(rnd, rn, 8, rm)


def gen_halfword(rnd):
    """LDRH / STRH / LDRSB / LDRSH, aligned."""
    l = rnd.randrange(2)
    sh = rnd.choice([0b01, 0b10, 0b11]) if l else 0b01
    p, u, imm = rnd.randrange(2), 1, rnd.randrange(2)
    rd = rnd.randrange(12)
    rn = rnd.choice([r for r in range(12) if r != rd])
    w = rnd.randrange(2) if p else 1
    if w and rn == rd:
        w = 0
    base = (0xE0000000 | (p << 24) | (u << 23) | (imm << 22) | (w << 21) | (l << 20)
            | (rn << 16) | (rd << 12))
    if imm:
        off = rnd.randrange(0, 64, 2)
        inst = base | ((off >> 4) << 8) | (1 << 7) | (sh << 5) | (1 << 4) | (off & 0xF)
        return inst, _addr_fixups(rnd, rn, 2)
    rm = rnd.choice([r for r in range(12) if r not in (rd, rn)])
    inst = base | (1 << 7) | (sh << 5) | (1 << 4) | rm
    return inst, _addr_fixups(rnd, rn, 2, rm)


def gen_dataproc(rnd):
    """Data processing, all three operand forms, S bit on and off.

    Opcodes 8..11 with S clear are not data processing at all: on ARMv5 that
    encoding is the MSR/MRS space, and on ARMv6T2+ it is MOVW/MOVT. Unicorn
    models an ARMv7 core and executes them as MOVW/MOVT, so including them
    compares two different architectures rather than testing anything. This
    guest is Tag_CPU_arch = ARMv5TE and GCC 4.4 never emits them.
    """
    form = rnd.randrange(3)
    opcode = rnd.randrange(16)
    s = rnd.randrange(2)
    if 8 <= opcode <= 11:
        s = 1
    rn, rd = rnd.randrange(12), rnd.randrange(12)
    if form == 2:
        rot, imm = rnd.randrange(16), rnd.randrange(256)
        inst = (0xE0000000 | (1 << 25) | (opcode << 21) | (s << 20) | (rn << 16)
                | (rd << 12) | (rot << 8) | imm)
        return inst, {}
    rm, st = rnd.randrange(12), rnd.randrange(4)
    if form == 0:
        inst = (0xE0000000 | (opcode << 21) | (s << 20) | (rn << 16) | (rd << 12)
                | (rnd.randrange(32) << 7) | (st << 5) | rm)
        return inst, {}
    rs = rnd.randrange(12)
    inst = (0xE0000000 | (opcode << 21) | (s << 20) | (rn << 16) | (rd << 12)
            | (rs << 8) | (st << 5) | (1 << 4) | rm)
    return inst, {}


def gen_mullong(rnd):
    u, a, s = rnd.randrange(2), rnd.randrange(2), rnd.randrange(2)
    lo, rs, rm = (rnd.randrange(12) for _ in range(3))
    hi = rnd.choice([r for r in range(12) if r != lo])
    inst = (0xE0000000 | (1 << 23) | (u << 22) | (a << 21) | (s << 20) | (hi << 16)
            | (lo << 12) | (rs << 8) | (0b1001 << 4) | rm)
    return inst, {}


def gen_mul(rnd):
    a, s = rnd.randrange(2), rnd.randrange(2)
    rd, rn, rs, rm = (rnd.randrange(12) for _ in range(4))
    while rm == rd:
        rm = rnd.randrange(12)
    inst = (0xE0000000 | (a << 21) | (s << 20) | (rd << 16) | (rn << 12) | (rs << 8)
            | (0b1001 << 4) | rm)
    return inst, {}


def gen_block(rnd):
    """LDM / STM, word-aligned base, register list clear of PC and of the base
    on writeback."""
    p, u, w, l = rnd.randrange(2), rnd.randrange(2), rnd.randrange(2), rnd.randrange(2)
    rn = rnd.randrange(12)
    regs = rnd.randrange(1, 0x1000)
    if w:
        regs &= ~(1 << rn) & 0xFFF
        if regs == 0:
            regs = 1 if rn != 0 else 2
    inst = (0xE0000000 | (0b100 << 25) | (p << 24) | (u << 23) | (w << 21) | (l << 20)
            | (rn << 16) | regs)
    # Centre the base so both ascending and descending forms stay in-window.
    return inst, {rn: DATA + 128}


def gen_single(rnd):
    """LDR / STR / LDRB / STRB, aligned."""
    i, p, u, b = rnd.randrange(2), rnd.randrange(2), 1, rnd.randrange(2)
    l = rnd.randrange(2)
    rd = rnd.randrange(12)
    rn = rnd.choice([r for r in range(12) if r != rd])
    w = rnd.randrange(2) if p else 1
    if w and rn == rd:
        w = 0
    align = 1 if b else 4
    base = (0xE0000000 | (0b01 << 26) | (i << 25) | (p << 24) | (u << 23) | (b << 22)
            | (w << 21) | (l << 20) | (rn << 16) | (rd << 12))
    if i:
        rm = rnd.choice([r for r in range(12) if r not in (rd, rn)])
        inst = base | rm
        return inst, _addr_fixups(rnd, rn, align, rm)
    inst = base | rnd.randrange(0, 64, align)
    return inst, _addr_fixups(rnd, rn, align)


GENERATORS = {
    "smul": gen_smul,
    "ldrd": gen_ldrd,
    "half": gen_halfword,
    "dataproc": gen_dataproc,
    "mul": gen_mul,
    "mullong": gen_mullong,
    "block": gen_block,
    "single": gen_single,
}


def make_cases(n, seed, klass):
    rnd = random.Random(seed)
    gens = list(GENERATORS.values()) if klass == "all" else [GENERATORS[klass]]
    cases = []
    for _ in range(n):
        inst, fix = rnd.choice(gens)(rnd)
        regs = [rnd.randrange(1 << 32) for _ in range(15)]
        regs.append(0)
        for r, v in fix.items():
            regs[r] = v
        cpsr = 0x10 | (rnd.randrange(16) << 28)
        mem = bytes(rnd.randrange(256) for _ in range(MEM))
        cases.append((inst, regs, cpsr, mem))
    return cases


def run_ours(cases):
    blob = struct.pack("<I", len(cases))
    for inst, regs, cpsr, mem in cases:
        blob += struct.pack("<I", inst) + struct.pack("<16I", *regs) + struct.pack("<I", cpsr) + mem
    r = subprocess.run([str(HELPER)], input=blob, stdout=subprocess.PIPE, check=True)
    out, step = [], 16 * 4 + 4 + MEM
    for i in range(len(cases)):
        b = r.stdout[i * step : (i + 1) * step]
        out.append((list(struct.unpack("<16I", b[:64])), struct.unpack("<I", b[64:68])[0], b[68:]))
    return out


def run_unicorn(cases):
    out = []
    for inst, regs, cpsr, mem in cases:
        uc = Uc(UC_ARCH_ARM, UC_MODE_ARM)
        uc.mem_map(0, SIZE, UC_PROT_ALL)
        uc.mem_write(0, struct.pack("<I", inst))
        uc.mem_write(DATA, mem)
        for i in range(15):
            uc.reg_write(UC_ARM_REG_R0 + i, regs[i])
        uc.reg_write(UC_ARM_REG_CPSR, cpsr)
        try:
            uc.emu_start(0, SIZE, 0, 1)
        except Exception:
            out.append(None)
            continue
        r = [uc.reg_read(UC_ARM_REG_R0 + i) for i in range(15)]
        r.append(uc.reg_read(UC_ARM_REG_PC))
        out.append((r, uc.reg_read(UC_ARM_REG_CPSR), bytes(uc.mem_read(DATA, MEM))))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=20000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--class", dest="klass", default="all", choices=["all", *GENERATORS])
    ap.add_argument("--show", type=int, default=10)
    a = ap.parse_args()

    if not HELPER.exists():
        sys.exit(f"missing {HELPER}; cargo build --release -p cs_probe --bin armdiff")

    cases = make_cases(a.n, a.seed, a.klass)
    ours = run_ours(cases)
    theirs = run_unicorn(cases)

    bad = 0
    for i, (case, o, t) in enumerate(zip(cases, ours, theirs)):
        if t is None:
            continue  # unicorn refused the encoding; nothing to compare against
        inst = case[0]
        # r15 differs by pipeline convention, and the flags Unicorn keeps in the
        # high CPSR bits include ones no ARMv5 instruction here writes.
        dr = [j for j in range(15) if o[0][j] != t[0][j]]
        dflags = (o[1] >> 28) != (t[1] >> 28)
        dmem = o[2] != t[2]
        if dr or dflags or dmem:
            bad += 1
            if bad <= a.show:
                txt = ""
                if MD:
                    d = list(MD.disasm(struct.pack("<I", inst), 0))
                    txt = f"{d[0].mnemonic} {d[0].op_str}" if d else "?"
                print(f"\n#{i} inst={inst:#010x}  {txt}")
                print(f"  in regs = {[hex(x) for x in case[1][:15]]}  cpsr={case[2]:#x}")
                for j in dr:
                    print(f"  r{j}: ours={o[0][j]:#010x} unicorn={t[0][j]:#010x}")
                if dflags:
                    print(f"  NZCV: ours={o[1]>>28:04b} unicorn={t[1]>>28:04b}")
                if dmem:
                    for k in range(0, MEM, 4):
                        if o[2][k : k + 4] != t[2][k : k + 4]:
                            print(f"  mem+{k:#05x}: ours={o[2][k:k+4].hex()} unicorn={t[2][k:k+4].hex()}")
    checked = sum(1 for t in theirs if t is not None)
    print(f"\n{a.klass}: {checked} comparable of {a.n}, {bad} mismatches")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
