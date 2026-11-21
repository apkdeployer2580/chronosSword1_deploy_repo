//! Differential test harness for the ARM interpreter.
//!
//! Reads a file of (instruction, register file, CPSR, memory) cases, executes
//! **one** instruction of each on `arm32_cpu`, and writes the resulting state
//! back out. `tools_re/armdiff.py` runs the identical cases through Unicorn
//! (QEMU's ARM core) and diffs the two.
//!
//! This exists because the fork carries hand-written implementations of the
//! ARMv5TE encodings upstream lacks (SMUL<x><y>, SMLA/SMLAW/SMLAL, LDRD/STRD)
//! and a table-driven decoder, and "the game renders wrong" is far too coarse a
//! signal to find an arithmetic slip in any of them. A per-instruction diff
//! against a reference implementation is exact and takes seconds.
//!
//! Layout, all little-endian:
//!   in:  u32 count, then per case: u32 inst, 16 x u32 regs, u32 cpsr, MEM bytes
//!   out: per case: 16 x u32 regs, u32 cpsr, MEM bytes

use arm32_cpu::{reg, Cpu, Memory, Mode};
use std::io::{Read, Write};

/// Guest window the cases run in: code at 0, scratch data at `DATA`.
const SIZE: usize = 0x2000;
const MEM: usize = 256;
const DATA: u32 = 0x1000;

struct Flat(Vec<u8>);

impl Memory for Flat {
    fn r8(&mut self, a: u32) -> u8 {
        *self.0.get(a as usize % SIZE).unwrap_or(&0)
    }
    fn r16(&mut self, a: u32) -> u16 {
        let o = a as usize % SIZE;
        u16::from_le_bytes([self.0[o], self.0[(o + 1) % SIZE]])
    }
    fn r32(&mut self, a: u32) -> u32 {
        let o = a as usize % SIZE;
        u32::from_le_bytes([
            self.0[o],
            self.0[(o + 1) % SIZE],
            self.0[(o + 2) % SIZE],
            self.0[(o + 3) % SIZE],
        ])
    }
    fn w8(&mut self, a: u32, v: u8) {
        let o = a as usize % SIZE;
        self.0[o] = v;
    }
    fn w16(&mut self, a: u32, v: u16) {
        let o = a as usize % SIZE;
        let b = v.to_le_bytes();
        self.0[o] = b[0];
        self.0[(o + 1) % SIZE] = b[1];
    }
    fn w32(&mut self, a: u32, v: u32) {
        let o = a as usize % SIZE;
        let b = v.to_le_bytes();
        for (i, x) in b.iter().enumerate() {
            self.0[(o + i) % SIZE] = *x;
        }
    }
}

fn main() {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).expect("read stdin");
    let count = u32::from_le_bytes(input[0..4].try_into().unwrap()) as usize;
    let case_len = 4 + 16 * 4 + 4 + MEM;
    let mut out = Vec::with_capacity(count * (16 * 4 + 4 + MEM));

    for c in 0..count {
        let base = 4 + c * case_len;
        let rd = |i: usize| u32::from_le_bytes(input[base + i..base + i + 4].try_into().unwrap());
        let inst = rd(0);

        let mut mem = Flat(vec![0u8; SIZE]);
        mem.0[0..4].copy_from_slice(&inst.to_le_bytes());
        mem.0[DATA as usize..DATA as usize + MEM]
            .copy_from_slice(&input[base + 4 + 68..base + 4 + 68 + MEM]);

        let mut cpu = Cpu::new();
        for r in 0..16u8 {
            cpu.reg_set(Mode::User, r, rd(4 + r as usize * 4));
        }
        let cpsr = rd(4 + 64);
        cpu.reg_set(Mode::User, reg::CPSR, cpsr);
        cpu.reg_set(Mode::User, reg::PC, 0);

        cpu.step(&mut mem);

        for r in 0..16u8 {
            out.extend_from_slice(&cpu.reg_get(Mode::User, r).to_le_bytes());
        }
        out.extend_from_slice(&cpu.reg_get(Mode::User, reg::CPSR).to_le_bytes());
        out.extend_from_slice(&mem.0[DATA as usize..DATA as usize + MEM]);
    }
    std::io::stdout().write_all(&out).expect("write stdout");
}
