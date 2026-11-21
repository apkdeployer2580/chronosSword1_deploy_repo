//! Host `startDes` — the DES driver behind every save and every monster load.
//!
//! This is the last big guest-side cost, and unlike the blitters it is not a
//! rendering cost: after the four blitters moved to the host, the only frame in
//! a 2600-frame replay still over the 33 ms budget was a single
//! `MX_desDecrypt`. One 1832-byte decrypt is **6.4 M guest instructions**
//! (~3500 per byte, measured on `desRound` alone), which is ~139 ms on a
//! Galaxy S21.
//!
//! It costs that because the shipped implementation works on **bit arrays** —
//! one bit per 32-bit word — so a single block is 64 words in, 64 out, with
//! 16 rounds of 48-word expansion, XOR and permutation in between, all of it
//! `ldr`/`str` through the interpreter.
//!
//! And it runs often:
//!
//! * `GameFrame_autoSave` -> `GameFrame_save_hero_data` + `GameFrame_saveGame`
//!   -> `SaveGameBase` -> `MX_desEncrypt`, with the lengths baked in
//!   (`0x1807` = 6151 and `0xc00` = 3072). That is 9216 bytes encrypted in one
//!   frame, ~32 M instructions, **~0.7 s of stall**. This is the reported
//!   "it hitches after a boss fight".
//! * `MONSTER_addBody` -> `MONSTER_dataLoad` -> `MD5_LoadData` ->
//!   `MX_desDecrypt`, with **no cache** (0xd0bd4 builds the filename, reads,
//!   decrypts, `MC_knlFree`s). So every monster that spawns pays it again.
//!
//! # Why this is not "just DES"
//!
//! It encrypts the player's save file, so a host port that differs by one bit
//! destroys saves irreversibly. Three things make a textbook DES the wrong
//! answer here, all confirmed against the shipped image:
//!
//! 1. **`MAT_SBOX` is corrupt.** S1 row 3 column 10 is `2`; FIPS 46-3 says `3`.
//!    That row is not a permutation of 0..15 at all — two 2s, no 3 — so it is a
//!    typo baked into the binary rather than an alternate box, and it is the
//!    only one of the 32 rows that fails the test. It fires whenever S1 sees
//!    `0b110101`, which is often enough to change roughly **one block in four**.
//! 2. **The final swap is omitted.** The output is `IP⁻¹(L16 ‖ R16)`, not the
//!    standard `IP⁻¹(R16 ‖ L16)`. The scheme is self-consistent — its own
//!    decrypt inverts its own encrypt — but it is not FIPS DES.
//! 3. **The block count truncates**: `blocks = byteLen / 8`, so the trailing
//!    `byteLen % 8` bytes are left in cleartext. `0x1807` is 768 blocks and
//!    seven cleartext bytes. A host that rounded up would silently rewrite the
//!    tail of every save.
//!
//! So every table is read out of guest memory by symbol name, never hardcoded,
//! and the algorithm is transcribed from the instruction stream rather than
//! recalled. On top of that the host **proves itself against the guest's own
//! code** before it is trusted with a single byte — see [`Vm::des_selftest`].
//!
//! # Why hooking `startDes` is the safe place
//!
//! The direction-dependent state is not here. `MX_desEncrypt` (0x2ed20) and
//! `MX_desDecrypt` (0x2ec7c) reverse `KEY4REAL`'s sixteen 48-byte round keys in
//! place — the loops at 0x2ed50 and 0x2ecb0 — guarded by the `KEY4ENCRYPT` flag,
//! **before** they call `startDes`; `desRound` itself always walks rounds 0..15
//! forward. Hooking the shared driver therefore inherits whatever order the guest
//! just established and needs no model of the flag at all.
//!
//! It also inherits the truncating `byteLen / 8`, which is the other trap that
//! lives in the wrappers, and it makes the self-check provably side-effect-free:
//! `startDes` *reads* `KEY4REAL` (through `desRound`, `ldrb` only) and never
//! touches `KEY4ENCRYPT`. Its GOT literal pool holds only the two `MAT_IP*`
//! offsets. So calling it on a host-owned scratch block needs no save/restore of
//! anything. Hooking the wrappers instead would have required reproducing the
//! reversal *and* restoring all 768 bytes plus the flag around every check,
//! in the right order, on every error path.

use crate::mem::GuestMem;
use crate::Vm;
use arm32_cpu::{reg, Memory, Mode};

/// `startDes(mode, blocks, buf)`: `MX_desDecrypt` passes 0 (`mov r0,#0` at
/// 0x2ed04), `MX_desEncrypt` passes 1 (0x2eda4), and `startDes` only ever tests
/// it against zero (`cmp r0,#0 / beq 0x2ec2c` at 0x2eb00).
const DECRYPT: u32 = 0;

/// The tables, read from guest memory every call.
///
/// The permutations are in `.data`; `KEY4REAL` is in `.bss` (`SHT_NOBITS`, so it
/// has **no file image** and is meaningless until `MX_desInit` has run). Re-read
/// every call rather than cached, and `KEY4REAL` especially: the wrappers reverse
/// it in place, so its order is call-history state. A host that snapshotted it at
/// boot would decrypt with the encrypt schedule forever. The cost is a few
/// hundred bytes against millions of interpreter steps.
struct Tables {
    /// Initial permutation, 64 entries, **1-based**.
    ip: [u8; 64],
    /// Inverse initial permutation, 64 entries, 1-based.
    ip_inv: [u8; 64],
    /// E expansion, 48 entries, 1-based.
    exp: [u8; 48],
    /// P permutation, 32 entries, 1-based.
    permu: [u8; 32],
    /// Eight 4x16 S-boxes, row-major. **Not** the FIPS tables — see the module
    /// comment.
    sbox: [u8; 512],
    /// Sixteen 48-byte round keys, one bit per byte, in whatever order the
    /// calling wrapper last left them.
    key: [u8; 768],
}

impl Tables {
    fn read(vm: &mut Vm) -> Option<Self> {
        let mut t = Tables {
            ip: [0; 64],
            ip_inv: [0; 64],
            exp: [0; 48],
            permu: [0; 32],
            sbox: [0; 512],
            key: [0; 768],
        };
        // By name, not by address: the binary ships its `.symtab`, and a
        // differently built guest then declines instead of reading rubbish.
        fn slurp(mem: &mut GuestMem, addr: Option<u32>, out: &mut [u8]) -> bool {
            match addr.and_then(|a| mem.slice(a, out.len() as u32)) {
                Some(b) => {
                    out.copy_from_slice(b);
                    true
                }
                None => false,
            }
        }
        let addrs: Vec<Option<u32>> =
            ["MAT_IP", "MAT_IP_1", "MAT_EXP", "MAT_PERMU", "MAT_SBOX", "KEY4REAL"]
                .iter()
                .map(|s| vm.symbol(s))
                .collect();
        let ok = slurp(&mut vm.mem, addrs[0], &mut t.ip)
            && slurp(&mut vm.mem, addrs[1], &mut t.ip_inv)
            && slurp(&mut vm.mem, addrs[2], &mut t.exp)
            && slurp(&mut vm.mem, addrs[3], &mut t.permu)
            && slurp(&mut vm.mem, addrs[4], &mut t.sbox)
            && slurp(&mut vm.mem, addrs[5], &mut t.key);
        ok.then_some(t)
    }
}

/// One 64-bit block as the guest holds it: 64 bytes, one bit per byte.
type Bits = [u8; 64];

/// `char2binary` (0x2bb8c): `dst[i*8 + (7-b)] = (src[i] >> b) & 1`. MSB first.
fn to_bits(block: &[u8]) -> Bits {
    let mut out = [0u8; 64];
    for (i, &byte) in block.iter().enumerate().take(8) {
        for b in 0..8 {
            out[i * 8 + (7 - b)] = (byte >> b) & 1;
        }
    }
    out
}

/// `binary2char` (0x2bbd8): accumulates `1 << (7 - i)` and treats **any**
/// non-zero word as a set bit. Every value here is already 0 or 1, but the
/// guest's rule is reproduced rather than assumed.
fn from_bits(bits: &Bits) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        for b in 0..8 {
            if bits[i * 8 + b] != 0 {
                *byte |= 1 << (7 - b);
            }
        }
    }
    out
}

/// `desRound` (0x2b8f4). All sixteen rounds, in place on two 32-bit-array
/// halves. There is no direction argument: the subkeys are always walked
/// forward, and the caller has already put them in the right order.
fn rounds(l: &mut [u8; 32], r: &mut [u8; 32], t: &Tables) {
    let mut expanded = [0u8; 48];
    let mut sboxout = [0u8; 32];
    let mut permout = [0u8; 32];
    for round in 0..16 {
        let l_next = *r;
        // E(R) ^ K, the subkey being 48 bytes at round * 0x30.
        for i in 0..48 {
            expanded[i] = r[t.exp[i] as usize - 1] ^ t.key[round * 48 + i];
        }
        // Eight 6-bit groups. Row is `2*b1 + b6`, column `b2 b3 b4 b5` — the
        // standard decomposition, recovered from the index arithmetic at
        // 0x2ba3c..0x2ba58 rather than assumed.
        for n in 0..8 {
            let x = &expanded[n * 6..n * 6 + 6];
            let row = 2 * x[0] as usize + x[5] as usize;
            let col = 8 * x[1] as usize + 4 * x[2] as usize + 2 * x[3] as usize + x[4] as usize;
            let v = t.sbox[64 * n + 16 * row + col];
            // The nibble unpacks MSB first into four consecutive words.
            for b in 0..4 {
                sboxout[n * 4 + b] = (v >> (3 - b)) & 1;
            }
        }
        for i in 0..32 {
            permout[i] = sboxout[t.permu[i] as usize - 1];
        }
        // R is written from the *old* L before L is replaced. Safe only because
        // the guest keeps them in separate buffers, which is why `l_next` is a
        // snapshot taken at the top.
        for i in 0..32 {
            r[i] = permout[i] ^ l[i];
        }
        *l = l_next;
    }
}

/// One block, mode included. Returns the eight bytes to write back.
fn block(input: &[u8], mode: u32, t: &Tables) -> [u8; 8] {
    let bits = to_bits(input);
    // IP, 1-based (0x2eadc..0x2eaec).
    let mut x = [0u8; 64];
    for i in 0..64 {
        x[i] = bits[t.ip[i] as usize - 1];
    }

    let encrypt = mode != DECRYPT;
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    if encrypt {
        // 0x2eb14: A = X[0..32], B = X[32..64].
        a.copy_from_slice(&x[..32]);
        b.copy_from_slice(&x[32..]);
    } else {
        // 0x2ec38: crossed. This *is* the standard decryption entry, and it is
        // what makes the shipped scheme invert itself despite the missing final
        // swap.
        b.copy_from_slice(&x[..32]);
        a.copy_from_slice(&x[32..]);
    }
    rounds(&mut a, &mut b, t);
    if encrypt {
        // 0x2eb48 `swap32bit(A, B, tmp)` — a real three-pass swap. Decrypt
        // branches straight past it (0x2ec60).
        std::mem::swap(&mut a, &mut b);
    }

    // 0x2eb50: the tail always emits B ‖ A.
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&b);
    out[32..].copy_from_slice(&a);
    // IP-inverse, 1-based (0x2eb8c..0x2eba0).
    let mut y = [0u8; 64];
    for i in 0..64 {
        y[i] = out[t.ip_inv[i] as usize - 1];
    }
    from_bits(&y)
}

/// Whether the host implementation has been proven against the guest's own code
/// for a given direction, per process.
#[derive(Default)]
pub struct Des {
    checked: [bool; 2],
    trusted: [bool; 2],
}

impl Vm {
    /// Host `startDes(mode, blocks, buf)`.
    ///
    /// Declines — leaving the shipped code to run — on anything unexpected, and
    /// on the first call for each direction it declines *after* proving itself:
    /// see [`Vm::des_verify`].
    pub(crate) fn host_start_des(&mut self) -> Result<bool, String> {
        if !des_enabled() {
            return Ok(false);
        }
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        // The self-check calls the guest's own `startDes` through `Vm::call`,
        // which enters with this LR. Without this the host would answer its own
        // reference call and the check would be vacuous.
        if lr == crate::RETURN_MAGIC {
            return Ok(false);
        }
        let mode = self.cpu.reg_get(Mode::User, 0);
        let blocks = self.cpu.reg_get(Mode::User, 1) as i32;
        let buf = self.cpu.reg_get(Mode::User, 2);
        // `cmp r1,#0 / ble` at 0x2e994: nothing to do, and nothing is written.
        if blocks <= 0 || buf == 0 {
            return Ok(false);
        }
        let dir = usize::from(mode != DECRYPT);
        if !self.des_verify(mode, dir)? {
            return Ok(false);
        }
        let Some(t) = Tables::read(self) else { return Ok(false) };
        let Some(len) = (blocks as u32).checked_mul(8) else { return Ok(false) };
        let Some(src) = self.mem.slice(buf, len) else { return Ok(false) };

        let mut out = Vec::with_capacity(len as usize);
        for i in 0..blocks as usize {
            out.extend_from_slice(&block(&src[i * 8..i * 8 + 8], mode, &t));
        }
        // The guest `memcpy`s each block back over the input as it goes
        // (0x2ec04); one write of the whole span is the same bytes, because no
        // block reads another block.
        self.mem.write_bytes(buf, &out);

        self.des_host += 1;
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Run the self-check for both directions now, rather than waiting for the
    /// first real call.
    ///
    /// Called at the end of `boot`, which matters for the *encrypt* direction:
    /// the first encrypt in a session is a real save, and "verify, then decline
    /// if wrong" is only comforting if the verification cannot itself be the
    /// thing that goes wrong under a deadline. Doing it at boot means a mismatch
    /// is a log line before the player has a save to lose.
    ///
    /// `MX_desInit` (from `onStartApp`) must have run, or `KEY4REAL` is still
    /// zeroed and the check would compare two identical-but-meaningless results.
    /// `KEY4ENCRYPT` ships as 0xff in `.data` and `MX_desInit` sets it to 1, so
    /// that byte is the flag for "the key schedule exists".
    pub fn des_selftest(&mut self) {
        if !des_enabled() {
            return;
        }
        match self.symbol("KEY4ENCRYPT").map(|a| self.mem.r8(a)) {
            Some(0xff) | None => {
                log::warn!("des: MX_desInit has not run; host DES stays off until first use");
                return;
            }
            Some(_) => {}
        }
        for mode in [DECRYPT, 1] {
            let dir = usize::from(mode != DECRYPT);
            if let Err(e) = self.des_verify(mode, dir) {
                log::warn!("des: self-check for mode {mode} could not run: {e}");
                self.des.trusted[dir] = false;
            }
        }
    }

    /// Prove the host DES against the guest's own `startDes`, once per
    /// direction, and refuse to serve that direction ever again if they differ.
    ///
    /// This is not belt-and-braces. The host is about to rewrite the bytes of
    /// the player's save file, and the only authority on what those bytes should
    /// be is the code being replaced — so it is asked, with the real key
    /// schedule, in the real process, at the moment of first use.
    ///
    /// Eight blocks of a fixed pattern cost ~224 K guest instructions, about
    /// 5 ms on the device, once. DES has no data-dependent control flow, so
    /// agreement on 64 bytes exercises every table lookup shape there is.
    ///
    /// Nothing the game depends on is disturbed: `startDes` reads `KEY4REAL`
    /// and writes only its own stack and the buffer it was handed, which here
    /// is a host-owned heap block. The `KEY4ENCRYPT` flag lives in the
    /// *wrappers*, which are not called.
    fn des_verify(&mut self, mode: u32, dir: usize) -> Result<bool, String> {
        if self.des.checked[dir] {
            return Ok(self.des.trusted[dir]);
        }
        self.des.checked[dir] = true;
        let (Some(func), Some(scratch)) = (self.symbol("startDes"), self.des_scratch()) else {
            return Ok(false);
        };
        // Deterministic, and deliberately not all-distinct: repeated bytes catch
        // a host that indexes by position where the guest indexes by value.
        // Counted in `usize` and cast per byte, not `0..DES_PROBE as u8`: a probe
        // of 256 would truncate to an empty range and pass vacuously.
        let probe: Vec<u8> =
            (0..DES_PROBE).map(|i| (i as u8).wrapping_mul(37).wrapping_add(11)).collect();

        let Some(t) = Tables::read(self) else { return Ok(false) };
        let mut want = Vec::with_capacity(DES_PROBE);
        for i in 0..DES_PROBE / 8 {
            want.extend_from_slice(&block(&probe[i * 8..i * 8 + 8], mode, &t));
        }

        self.mem.write_bytes(scratch, &probe);
        self.call(func, &[mode, (DES_PROBE / 8) as u32, scratch])?;
        let got = match self.mem.slice(scratch, DES_PROBE as u32) {
            Some(b) => b.to_vec(),
            None => return Ok(false),
        };

        self.des.trusted[dir] = got == want;
        if self.des.trusted[dir] {
            log::info!(
                "des: host {} verified against the guest ({DES_PROBE} bytes)",
                if mode == DECRYPT { "decrypt" } else { "encrypt" }
            );
        } else {
            // Loud, because the alternative to noticing is a corrupted save.
            // The corrupt S1 entry only fires for one 6-bit input, so the first
            // block often matches — report the first block that does not, or the
            // difference reads as "identical bytes disagree".
            let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0) / 8 * 8;
            log::error!(
                "des: host {} DISAGREES with the guest; leaving DES to the interpreter. \
                 block {} of {}: guest {:02x?} host {:02x?}",
                if mode == DECRYPT { "decrypt" } else { "encrypt" },
                at / 8,
                DES_PROBE / 8,
                &got[at..at + 8],
                &want[at..at + 8]
            );
        }
        Ok(self.des.trusted[dir])
    }

    /// A guest buffer for the self-check, allocated on first use so the feature
    /// costs nothing when it never runs.
    fn des_scratch(&mut self) -> Option<u32> {
        if self.des_buf == 0 {
            self.des_buf = self.heap.alloc(DES_PROBE as u32)?;
        }
        Some(self.des_buf)
    }
}

/// Bytes the self-check round-trips.
///
/// Sixteen blocks rather than eight, because one deviation is only caught
/// *probabilistically*. Everything else — a bit-order flip, a 1-based slip on any
/// of the five permutations, the missing final swap, the crossed decrypt entry, a
/// wrong subkey order — changes essentially every block. But the corrupt
/// `MAT_SBOX` entry fires on exactly one of S1's 64 inputs, so a block exercises
/// it with probability `1 - (63/64)^16 ≈ 22%`, and a 64-byte probe misses it
/// entirely `(63/64)^128 ≈ 13%` of the time. 128 bytes takes that to ~1.8%.
///
/// The shipped host reads that table out of guest memory and so cannot hold the
/// wrong value — this is insurance against a future edit that hardcodes it, at
/// ~500 K guest instructions per direction, once.
const DES_PROBE: usize = 128;

/// `CS_NO_DES=1` leaves every save and every monster load to the interpreter.
fn des_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var("CS_NO_DES").is_ok_and(|v| v != "0"))
}
