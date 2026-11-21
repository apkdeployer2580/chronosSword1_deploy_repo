//! A `printf` engine over an AAPCS `va_list`.
//!
//! `MC_knlSprintk` and `MC_knlPrintk` both funnel into the guest's imported
//! `vsnprintf`, so implementing it is the only way the game's own formatted
//! strings work. On ARM EABI a `va_list` is just a pointer into the caller's
//! argument area: 32-bit values are read in sequence, 64-bit values
//! (`long long`, `double`) are 8-byte aligned first.
//!
//! ## Why this works in bytes and never in `String`
//!
//! The guest's text is **EUC-KR**, not UTF-8 — there are 617 Korean literals in
//! its `.rodata` alone, and the save file and asset text are the same. Decoding
//! any of that as UTF-8 replaces every byte it does not understand with U+FFFD,
//! and re-encoding then emits three completely different bytes in its place. The
//! result still renders as Hangul, because the game's font maps any two bytes to
//! some syllable — just the wrong one. That is exactly the "썍몌예ㅃ 쏙옙"
//! corruption this replaced.
//!
//! So nothing here converts. Format string, arguments and output are all
//! `[u8]`, copied through unchanged, which is also precisely what C's
//! `vsnprintf` does.

pub struct VaList<'a> {
    pub ptr: u32,
    pub read_u32: &'a dyn Fn(u32) -> u32,
    /// The argument's bytes, up to its NUL. Raw — see the module note.
    pub cstr: &'a dyn Fn(u32) -> Vec<u8>,
}

impl VaList<'_> {
    fn u32(&mut self) -> u32 {
        let v = (self.read_u32)(self.ptr);
        self.ptr += 4;
        v
    }
    fn u64(&mut self) -> u64 {
        self.ptr = (self.ptr + 7) & !7;
        let lo = (self.read_u32)(self.ptr) as u64;
        let hi = (self.read_u32)(self.ptr + 4) as u64;
        self.ptr += 8;
        (hi << 32) | lo
    }
    fn f64(&mut self) -> f64 {
        f64::from_bits(self.u64())
    }
}

#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
    width: Option<usize>,
    prec: Option<usize>,
    long: u8,
}

/// Format `fmt` (raw guest bytes) into raw output bytes.
pub fn vsnprintf(fmt: &[u8], va: &mut VaList) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(fmt.len() + 32);
    let mut i = 0usize;

    while i < fmt.len() {
        // A byte >= 0x80 is the lead of a multi-byte character; copy it and
        // whatever follows verbatim. Only '%' is ever interpreted.
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt.len() {
            break;
        }
        if fmt[i] == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }

        let mut s = Spec::default();
        while i < fmt.len() {
            match fmt[i] {
                b'-' => s.left = true,
                b'0' => s.zero = true,
                b'+' => s.plus = true,
                b' ' => s.space = true,
                b'#' => s.alt = true,
                _ => break,
            }
            i += 1;
        }
        if i < fmt.len() && fmt[i] == b'*' {
            s.width = Some((va.u32() as i32).unsigned_abs() as usize);
            i += 1;
        } else {
            let start = i;
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                i += 1;
            }
            if i > start {
                s.width = std::str::from_utf8(&fmt[start..i]).ok().and_then(|t| t.parse().ok());
            }
        }
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            if i < fmt.len() && fmt[i] == b'*' {
                s.prec = Some(va.u32() as usize);
                i += 1;
            } else {
                let start = i;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    i += 1;
                }
                s.prec = Some(
                    std::str::from_utf8(&fmt[start..i])
                        .ok()
                        .and_then(|t| t.parse().ok())
                        .unwrap_or(0),
                );
            }
        }
        while i < fmt.len() && matches!(fmt[i], b'h' | b'l' | b'L' | b'z' | b'j' | b't') {
            if fmt[i] == b'l' {
                s.long += 1;
            }
            if fmt[i] == b'L' {
                s.long = 2;
            }
            i += 1;
        }
        if i >= fmt.len() {
            break;
        }
        let conv = fmt[i];
        i += 1;

        let body: Vec<u8> = match conv {
            b'd' | b'i' => {
                let v = if s.long >= 2 { va.u64() as i64 } else { va.u32() as i32 as i64 };
                let mut t = v.unsigned_abs().to_string();
                if let Some(p) = s.prec {
                    while t.len() < p {
                        t.insert(0, '0');
                    }
                }
                let sign = if v < 0 {
                    "-"
                } else if s.plus {
                    "+"
                } else if s.space {
                    " "
                } else {
                    ""
                };
                format!("{sign}{t}").into_bytes()
            }
            b'u' => {
                let v = if s.long >= 2 { va.u64() } else { va.u32() as u64 };
                v.to_string().into_bytes()
            }
            b'x' | b'X' | b'o' | b'p' => {
                let v = if s.long >= 2 { va.u64() } else { va.u32() as u64 };
                let t = match conv {
                    b'x' | b'p' => format!("{v:x}"),
                    b'X' => format!("{v:X}"),
                    _ => format!("{v:o}"),
                };
                let prefix = match (s.alt, conv) {
                    (true, b'x') => "0x",
                    (true, b'X') => "0X",
                    (_, b'p') => "0x",
                    _ => "",
                };
                format!("{prefix}{t}").into_bytes()
            }
            b'c' => vec![va.u32() as u8],
            b's' => {
                let p = va.u32();
                let mut t = if p == 0 { b"(null)".to_vec() } else { (va.cstr)(p) };
                if let Some(prec) = s.prec {
                    t.truncate(prec);
                }
                t
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = va.f64();
                let p = s.prec.unwrap_or(6);
                match conv {
                    b'e' => format!("{v:.p$e}"),
                    b'E' => format!("{v:.p$e}").to_uppercase(),
                    b'g' | b'G' => format!("{v}"),
                    _ => format!("{v:.p$}"),
                }
                .into_bytes()
            }
            other => {
                out.push(b'%');
                out.push(other);
                continue;
            }
        };

        // Width is counted in bytes, as C does. A multi-byte character is
        // therefore several columns of padding short — which is what the
        // original did too, so the game's layouts already account for it.
        match s.width {
            Some(w) if body.len() < w => {
                let pad = w - body.len();
                if s.left {
                    out.extend_from_slice(&body);
                    out.extend(std::iter::repeat_n(b' ', pad));
                } else if s.zero && s.prec.is_none() && conv != b's' && conv != b'c' {
                    // Zero padding goes after the sign, not before it.
                    let (sign, rest) = match body.first() {
                        Some(&c) if c == b'-' || c == b'+' => (&body[..1], &body[1..]),
                        _ => (&body[..0], &body[..]),
                    };
                    out.extend_from_slice(sign);
                    out.extend(std::iter::repeat_n(b'0', pad));
                    out.extend_from_slice(rest);
                } else {
                    out.extend(std::iter::repeat_n(b' ', pad));
                    out.extend_from_slice(&body);
                }
            }
            _ => out.extend_from_slice(&body),
        }
    }
    out
}
