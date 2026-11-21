//! Host port of the guest bitmap-font stack.
//!
//! After the rotate blitters moved to the host, profiling a title-screen frame
//! put **~88% of the remaining guest instructions in five font functions**:
//! `_midas_funcFntDrawString` (59.7%), `_midas_funcFntDrawPixelPreRGB` (15.6%),
//! `MC_grpGetRGBFromPixel` (6.9%) and `_midas_funcFntDrawPixel` (6.0%). The
//! stack is slow for the same structural reason the blitters were: every loop
//! counter, every cursor and every scratch value is a **GOT-indirect global**
//! (`_midas_drawx`, `_midas_r0`, `_midas_temp`, …), so the guest pays a
//! literal-pool load, a GOT load and a pointer load per access, per pixel, and
//! calls three functions per lit pixel on top.
//!
//! This module is a transcription, not a re-design: the same globals are read
//! at entry, mutated in the same order, and written back at exit, because the
//! guest reads some of them (`_midas_chX` carries the pen between calls of the
//! same draw, and the return value is `chX - x`). Anything the transcription
//! is not sure to reproduce bit for bit — a non-16bpp kernel, a hangul glyph
//! stride larger than the 32-byte compose buffer — **declines** back to the
//! interpreter, following the rule in `patch.rs`.
//!
//! Layout facts recovered from the shipped symbols (`tools_re/csdis_got.py`
//! resolves the GOT traffic to names):
//!
//! * The font object `F = *_mxfnt_prt`: `F[4]/F[8]` hangul cell w/h,
//!   `F[0xc]/F[0x10]` ASCII advance/rows, `F[0x14]` inter-char gap, `F[0x18]`
//!   space width, `F[0x1c]` y offset, `F[0x24]` alpha, `F[0x28]` draw mode
//!   (1 = double-strike, 2 = outline+body, 3 = outline only, ≤0 = body only),
//!   `F[0x30]` bytes per hangul component, `F[0x34]` size mode (8 selects the
//!   compact jamo tables), `F[0x3c]` ASCII glyph stride, `F[0x40]`/`F[0x44]`
//!   body/outline colors, `F[0x20]` the jamo-variant table block.
//! * `_midas_internalFont[0..4]` = cho / jung / jong / ASCII bitmap bases
//!   (seeded by `_midas_funcFntInvalidate`).
//! * A hangul syllable is composed by OR-ing three component bitmaps into the
//!   32-byte `_midas_abBuf` (16 rows x 2 bytes), then rasterised twice per
//!   row: `abBuf[row]` is the **right** half (drawn at `x+8`), `abBuf[row+1]`
//!   the left. EUC-KR → Johab conversion (`_midas_funcFntWanToJohab`) and the
//!   cho/jung/jong split (`_midas_funcFntGet3Syllable`) are table lookups.
//! * Blending is a 16x16x16 LUT: `_midas_internalFntOpacity[((A*16 +
//!   (src>>4))*16) + (dst>>4)] << 4` per channel, where the source channels
//!   live in the `_midas_rgb_{r,g,b}0` **accumulator globals** — they persist
//!   across calls, which is why the LUT slice here is copied 8 KB deep: an
//!   accumulator the guest left at `0xff0` indexes past the 4 KB table, and
//!   the guest would read whatever follows it.

use crate::mem::GuestMem;
use crate::Vm;
use arm32_cpu::{reg, Memory, Mode};

/// Guest addresses of everything the font stack touches, resolved from the
/// symbol table once at boot. `None` if any symbol is missing, in which case
/// every font patch declines.
#[derive(Clone, Copy)]
pub struct FontGlobals {
    mxfnt: u32,
    kernel_instance: u32,
    lcd: u32,
    lcd_w: u32,
    lcd_h: u32,
    lcd_rgb: u32,
    draw_dir: u32,
    clip: u32,
    opacity: u32,
    // pixel accumulators
    rgb_r0: u32,
    rgb_g0: u32,
    rgb_b0: u32,
    rgb_r1: u32,
    rgb_g1: u32,
    rgb_b1: u32,
    // DrawString state globals
    drawx: u32,
    drawy: u32,
    chx: u32,
    chwidth: u32,
    pi: u32,
    pbidx: u32,
    pub(crate) r0: u32,
    r1: u32,
    temp: u32,
    ch: u32,
    b1: u32,
    b2: u32,
    cho_code: u32,
    jung_code: u32,
    jong_code: u32,
    cho_n: u32,
    jung_n: u32,
    jong_n: u32,
    chk: u32,
    whan: u32,
    ab_buf: u32,
    cho_info: u32,
    jung_info: u32,
    jong_info: u32,
    pub(crate) internal_font: u32,
    // conversion tables
    han_code: u32,
    han_cjj: u32,
    only_first: u32,
    only_middle: u32,
}

impl FontGlobals {
    pub fn resolve(sym: impl Fn(&str) -> Option<u32>) -> Option<Self> {
        Some(Self {
            mxfnt: sym("_mxfnt_prt")?,
            kernel_instance: sym("_ZN18MIDASKernelManager8instanceE")?,
            lcd: sym("_midas_lcd")?,
            lcd_w: sym("_midas_lcdWidth")?,
            lcd_h: sym("_midas_lcdHeight")?,
            lcd_rgb: sym("_midas_lcdRGB")?,
            draw_dir: sym("_midas_drawDIR")?,
            clip: sym("_midas_internalFntClip")?,
            opacity: sym("_midas_internalFntOpacity")?,
            rgb_r0: sym("_midas_rgb_r0")?,
            rgb_g0: sym("_midas_rgb_g0")?,
            rgb_b0: sym("_midas_rgb_b0")?,
            rgb_r1: sym("_midas_rgb_r1")?,
            rgb_g1: sym("_midas_rgb_g1")?,
            rgb_b1: sym("_midas_rgb_b1")?,
            drawx: sym("_midas_drawx")?,
            drawy: sym("_midas_drawy")?,
            chx: sym("_midas_chX")?,
            chwidth: sym("_midas_chWidth")?,
            pi: sym("_midas_pi")?,
            pbidx: sym("_midas_pbidx")?,
            r0: sym("_midas_r0")?,
            r1: sym("_midas_r1")?,
            temp: sym("_midas_temp")?,
            ch: sym("_midas_ch")?,
            b1: sym("_midas_b1")?,
            b2: sym("_midas_b2")?,
            cho_code: sym("_midas_choCode")?,
            jung_code: sym("_midas_jungCode")?,
            jong_code: sym("_midas_jongCode")?,
            cho_n: sym("_midas_choN")?,
            jung_n: sym("_midas_jungN")?,
            jong_n: sym("_midas_jongN")?,
            chk: sym("_midas_chk")?,
            whan: sym("_midas_wHan")?,
            ab_buf: sym("_midas_abBuf")?,
            cho_info: sym("_midas_internalFntChoInfo")?,
            jung_info: sym("_midas_internalFntJungInfo")?,
            jong_info: sym("_midas_internalFntJongInfo")?,
            internal_font: sym("_midas_internalFont")?,
            han_code: sym("_midas_internalFntHanCodeTable")?,
            han_cjj: sym("_midas_internalFntHanCJJTable")?,
            only_first: sym("_midas_internalFntOnlyFirst")?,
            only_middle: sym("_midas_internalFntOnlyMiddle")?,
        })
    }
}

/// The six `_midas_rgb_*` accumulator words as locals. They are genuinely
/// stateful — `PreRGB` blends with whatever the last decompose left in the
/// `*0` set — so they are loaded at entry and stored at exit like the rest.
#[derive(Clone, Copy)]
struct Acc {
    r0: i32,
    g0: i32,
    b0: i32,
    r1: i32,
    g1: i32,
    b1: i32,
}

/// One rasteriser borrow: the framebuffer, the clip, and the accumulators.
/// Pixel routines are transcriptions of `_midas_funcFntDrawPixel*` +
/// `MC_grpGet{RGB,Pixel}From*` for the 16bpp kernel.
struct Pen<'a> {
    fb: &'a mut [u8],
    w: i32,
    h: i32,
    clip: [i32; 4],
    dir: i32,
    acc: &'a mut Acc,
    /// Opacity LUT copied 8 KB deep (see module docs). Empty when the call is
    /// fully opaque and no index can ever be formed.
    opa: &'a [u8],
}

impl Pen<'_> {
    /// `_midas_funcFntDrawPixelPreRGB(a, b, pix, alpha)`.
    fn px_pre(&mut self, a: i32, b: i32, pix: u32, alpha: i32) {
        // `_midas_drawDIR` remaps the coordinates before any test.
        let (x, y) = if self.dir == 0 {
            (a, b)
        } else if self.dir > 0 {
            (b, self.h - 1 - a)
        } else {
            (self.w - 1 - b, a)
        };
        if y < 0 || x < 0 || x >= self.w || y >= self.h {
            return;
        }
        if x < self.clip[0] || y < self.clip[1] || x >= self.clip[2] || y >= self.clip[3] {
            return;
        }
        let off = (self.w * y + x) as usize * 2;
        let mut out = pix;
        if alpha <= 0xfe {
            // GetRGBFromPixel(dst) into the *1 accumulators.
            let d = u16::from_le_bytes([self.fb[off], self.fb[off + 1]]) as i32;
            self.acc.r1 = (d >> 8) & 0xf8;
            self.acc.g1 = (d >> 3) & 0xfc;
            self.acc.b1 = (d << 3) & 0xff;
            // out_ch = OPACITY[(A*16 + src>>4)*16 + dst>>4] << 4, then the
            // result is written back into the *0 accumulators (the guest's
            // GOT globals double as the blend scratch).
            let a4 = alpha >> 4;
            let (sr, sg, sb) = (self.acc.r0, self.acc.g0, self.acc.b0);
            let idx = |s: i32, d: i32| ((((a4 << 4) + (s >> 4)) << 4) + (d >> 4)) as usize;
            let nr = (self.opa[idx(sr, self.acc.r1)] as i32) << 4;
            let ng = (self.opa[idx(sg, self.acc.g1)] as i32) << 4;
            let nb = (self.opa[idx(sb, self.acc.b1)] as i32) << 4;
            self.acc.r0 = nr;
            self.acc.g0 = ng;
            self.acc.b0 = nb;
            // GetPixelFromRGB, 16bpp: no masking — the guest relies on the
            // LUT producing 4-bit values.
            out = (((nr >> 3) << 11) | ((ng >> 2) << 5) | (nb >> 3)) as u32;
        }
        self.fb[off] = out as u8;
        self.fb[off + 1] = (out >> 8) as u8;
    }

    /// `_midas_funcFntDrawPixel`: decompose the color into the *0
    /// accumulators, then draw.
    fn px(&mut self, a: i32, b: i32, color: u32, alpha: i32) {
        self.acc.r0 = ((color >> 8) & 0xf8) as i32;
        self.acc.g0 = ((color >> 3) & 0xfc) as i32;
        self.acc.b0 = ((color << 3) & 0xff) as i32;
        self.px_pre(a, b, color, alpha);
    }

    fn px_w(&mut self, a: i32, b: i32, color: u32, alpha: i32) {
        self.px(a - 1, b, color, alpha);
        self.px(a, b, color, alpha);
        self.px(a + 1, b, color, alpha);
    }

    fn px_h(&mut self, a: i32, b: i32, color: u32, alpha: i32) {
        self.px(a, b + 1, color, alpha);
        self.px(a, b - 1, color, alpha);
    }

    fn px_2(&mut self, a: i32, b: i32, color: u32, alpha: i32) {
        self.px(a, b, color, alpha);
        self.px(a + 1, b, color, alpha);
    }
}

/// Read one guest byte, failing like a guest fault would if the address left
/// the window. The window is a single contiguous mapping, so this only fires
/// for addresses a real run would have died on anyway.
fn rd8(mem: &GuestMem, addr: u32) -> Result<u8, String> {
    mem.slice(addr, 1)
        .map(|b| b[0])
        .ok_or_else(|| format!("font: guest read outside the window at {addr:#x}"))
}

/// `_midas_funcFntWanToJohab`, on locals. In the shipped call the two pointer
/// arguments alias `_midas_b1`/`_midas_b2` themselves, so the net effect
/// collapses to final `(b1, b2)` values plus the `pbidx` side channel.
fn wan_to_johab(
    mem: &GuestMem,
    g: &FontGlobals,
    b1: &mut u32,
    b2: &mut u32,
    pbidx: &mut i32,
) -> Result<(), String> {
    *pbidx = 0;
    let (a0, b0) = (*b1, *b2);

    // ㄱ..ㅎ single consonant: index _midas_internalFntOnlyFirst.
    let consonant = |mem: &GuestMem, v: u32, b1: &mut u32, b2: &mut u32| -> Result<(), String> {
        *b2 = 0;
        let k = rd8(mem, g.only_first.wrapping_add(v).wrapping_sub(0xa1))? as u32;
        *b1 = ((k + 2) << 2) | 0x80;
        Ok(())
    };
    // ㅏ..ㅣ single vowel: index _midas_internalFntOnlyMiddle.
    let vowel = |mem: &GuestMem, v: u32, b1: &mut u32, b2: &mut u32| -> Result<(), String> {
        let k = rd8(mem, g.only_middle.wrapping_add(v).wrapping_sub(0xbf))? as u32 + 3;
        *b2 = (k << 5) & 0xff;
        *b1 = (k >> 3) | 0x80;
        Ok(())
    };
    // The A4-row split, shared with the 0xda continuation below: `v` is the
    // second byte, `b1` already holds 0xda on entry.
    let jamo = |mem: &GuestMem, v: u32, b1: &mut u32, b2: &mut u32| -> Result<(), String> {
        if v.wrapping_sub(0xa1) <= 0x1d {
            consonant(mem, v, b1, b2)
        } else if v.wrapping_sub(0xbf) <= 0x14 {
            vowel(mem, v, b1, b2)
        } else {
            Ok(()) // stays (0xda, v)
        }
    };

    if a0 == 0xa1 {
        if b0 == 0xa4 {
            *b1 = 0x83;
            *b2 = 0xc0;
            return Ok(());
        }
        if b0 == 0xa5 {
            *b1 = 0x83;
            *b2 = 0xe0;
            return Ok(());
        }
        // falls through to the syllable table with row 0xa1
    } else if a0 == 0xa4 {
        let sl = b0.wrapping_sub(0xa1);
        if sl <= 0x32 {
            *b1 = 0xda;
            return jamo(mem, b0, b1, b2);
        }
        // sl > 0x32 falls through to the syllable table with row 0xa4
    } else if a0 == 0xa3 && b0 == 0xbf {
        // full-width '?' — the guest rewrites it as the A1-row lookup 0xaf.
        return consonant(mem, 0xaf, b1, b2);
    }

    // Completed-syllable table. Row/column can go negative for the A1/A4
    // fall-throughs — the guest indexes before the table then; the wrapping
    // add reproduces exactly that read.
    let row = (a0 as i32).wrapping_sub(0xb0);
    let col = (b0 as i32).wrapping_sub(0xa1);
    let idx = row.wrapping_mul(0x5e).wrapping_add(col);
    *pbidx = idx.wrapping_shl(1);
    *b1 = row as u32;
    *b2 = col as u32;
    if idx > 0x92d {
        // The shipped code prints "ERROR : 0x%2X,0x%2X" through MC_knlPrintk
        // here (the per-frame intro noise). State-wise it only answers '?'.
        log::debug!("font: WanToJohab out of range ({:#x},{:#x})", a0, b0);
        *b1 = 0x3f;
        *b2 = 0x3f;
        return Ok(());
    }
    let at = g.han_code.wrapping_add((idx as u32).wrapping_mul(2));
    let c1 = rd8(mem, at)? as u32;
    let c2 = rd8(mem, at.wrapping_add(1))? as u32;
    *b1 = c1;
    *b2 = c2;
    if c1 == 0xda {
        return jamo(mem, c2, b1, b2);
    }
    Ok(())
}

/// `_midas_funcFntGet3Syllable(b1, b2)` — split a Johab code into the three
/// component indices via `_midas_internalFntHanCJJTable`, each minus one.
fn get3syllable(
    mem: &GuestMem,
    g: &FontGlobals,
    b1: u32,
    b2: u32,
) -> Result<(i32, i32, i32), String> {
    let code = ((b1 << 8) | b2) as i32;
    let cho = rd8(mem, g.han_cjj.wrapping_add(((code & 0x7c00) >> 10) as u32))? as i32 - 1;
    let jung = rd8(mem, g.han_cjj.wrapping_add(0x1f + ((code & 0x3e0) >> 5) as u32))? as i32 - 1;
    let jong = rd8(mem, g.han_cjj.wrapping_add(0x3e + (code & 0x1f) as u32))? as i32 - 1;
    Ok((cho, jung, jong))
}

/// Everything `host_fnt_draw_string` needs to have decided before it is
/// allowed to mutate anything.
struct Screen {
    lcd: u32,
    fb_len: u32,
    lw: i32,
    lh: i32,
    dir: i32,
    clip: [i32; 4],
}

impl Vm {
    /// Validate the kernel/LCD configuration the pixel path depends on.
    /// Returns `None` — meaning *decline, run the guest* — for anything other
    /// than the 16bpp RGB565 setup this game ships with.
    fn font_screen(&mut self, g: &FontGlobals) -> Option<Screen> {
        let kernel = self.mem.r32(g.kernel_instance);
        if kernel == 0 || self.mem.r32(kernel.wrapping_add(0x4f4)) != 0x10 {
            return None;
        }
        if self.mem.r32(g.lcd_rgb) != 2 {
            return None;
        }
        let lcd = self.mem.r32(g.lcd);
        let lw = self.mem.r32(g.lcd_w) as i32;
        let lh = self.mem.r32(g.lcd_h) as i32;
        if lcd == 0 || lw <= 0 || lh <= 0 {
            return None;
        }
        let fb_len = (lw as u32).checked_mul(lh as u32)?.checked_mul(2)?;
        self.mem.slice(lcd, fb_len)?;
        let clip = [
            self.mem.r32(g.clip) as i32,
            self.mem.r32(g.clip.wrapping_add(4)) as i32,
            self.mem.r32(g.clip.wrapping_add(8)) as i32,
            self.mem.r32(g.clip.wrapping_add(0xc)) as i32,
        ];
        Some(Screen { lcd, fb_len, lw, lh, dir: self.mem.r32(g.draw_dir) as i32, clip })
    }

    fn acc_load(&mut self, g: &FontGlobals) -> Acc {
        Acc {
            r0: self.mem.r32(g.rgb_r0) as i32,
            g0: self.mem.r32(g.rgb_g0) as i32,
            b0: self.mem.r32(g.rgb_b0) as i32,
            r1: self.mem.r32(g.rgb_r1) as i32,
            g1: self.mem.r32(g.rgb_g1) as i32,
            b1: self.mem.r32(g.rgb_b1) as i32,
        }
    }

    fn acc_store(&mut self, g: &FontGlobals, a: &Acc) {
        self.mem.w32(g.rgb_r0, a.r0 as u32);
        self.mem.w32(g.rgb_g0, a.g0 as u32);
        self.mem.w32(g.rgb_b0, a.b0 as u32);
        self.mem.w32(g.rgb_r1, a.r1 as u32);
        self.mem.w32(g.rgb_g1, a.g1 as u32);
        self.mem.w32(g.rgb_b1, a.b1 as u32);
    }

    /// The opacity LUT, copied 8 KB deep so any index the shipped arithmetic
    /// can form (accumulators reach 0xff0 → index 7935) reads the same bytes
    /// the guest would.
    fn opacity_copy(&mut self, g: &FontGlobals) -> Option<Vec<u8>> {
        self.mem.slice(g.opacity, 8192).map(|b| b.to_vec())
    }

    /// Host `_midas_funcFntDrawPixel{,W,H,2,PreRGB}` — the leaves keep their
    /// own patches so `_midas_funcFntGroup_Draw` (the other caller of PreRGB)
    /// and any guest-side fallback of DrawString still get the fast pixel.
    pub(crate) fn host_fnt_pixel(&mut self, kind: crate::patch::FntPixel) -> Result<bool, String> {
        use crate::patch::FntPixel;
        let Some(g) = self.font_globals else { return Ok(false) };
        let Some(scr) = self.font_screen(&g) else { return Ok(false) };
        let a = self.cpu.reg_get(Mode::User, 0) as i32;
        let b = self.cpu.reg_get(Mode::User, 1) as i32;
        let color = self.cpu.reg_get(Mode::User, 2);
        let alpha = self.cpu.reg_get(Mode::User, 3) as i32;
        if !(0..=0xff).contains(&alpha) {
            return Ok(false);
        }
        let mut acc = self.acc_load(&g);
        let blend = alpha <= 0xfe;
        if blend && matches!(kind, FntPixel::PreRgb) {
            // PreRGB blends with the accumulators as they stand; a value the
            // LUT arithmetic can't index safely means this call is not ours.
            for ch in [acc.r0, acc.g0, acc.b0] {
                if !(0..=0xff0).contains(&ch) {
                    return Ok(false);
                }
            }
        }
        let opa = if blend {
            match self.opacity_copy(&g) {
                Some(v) => v,
                None => return Ok(false),
            }
        } else {
            Vec::new()
        };

        {
            let fb = self.mem.slice_mut(scr.lcd, scr.fb_len).ok_or("font: framebuffer vanished")?;
            let mut pen = Pen {
                fb,
                w: scr.lw,
                h: scr.lh,
                clip: scr.clip,
                dir: scr.dir,
                acc: &mut acc,
                opa: &opa,
            };
            match kind {
                FntPixel::Plain => pen.px(a, b, color, alpha),
                FntPixel::W => pen.px_w(a, b, color, alpha),
                FntPixel::H => pen.px_h(a, b, color, alpha),
                FntPixel::Two => pen.px_2(a, b, color, alpha),
                FntPixel::PreRgb => pen.px_pre(a, b, color, alpha),
            }
        }
        self.acc_store(&g, &acc);
        self.fnt_host += 1;
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }

    /// Host `_midas_funcFntDrawString(x, y, str, len) -> drawn width`.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn host_fnt_draw_string(&mut self) -> Result<bool, String> {
        let Some(g) = self.font_globals else { return Ok(false) };
        let x = self.cpu.reg_get(Mode::User, 0) as i32;
        let y = self.cpu.reg_get(Mode::User, 1) as i32;
        let str_p = self.cpu.reg_get(Mode::User, 2);
        let mut len = self.cpu.reg_get(Mode::User, 3) as i32;

        // ---- decline checks, before any state is touched ------------------
        let f = self.mem.r32(g.mxfnt);
        if f == 0 || str_p == 0 {
            return Ok(false);
        }
        let Some(scr) = self.font_screen(&g) else { return Ok(false) };
        let alpha = self.mem.r32(f.wrapping_add(0x24)) as i32;
        if !(0..=0xff).contains(&alpha) {
            return Ok(false);
        }
        let hstride = self.mem.r32(f.wrapping_add(0x30)) as i32;
        let arows = self.mem.r32(f.wrapping_add(0x10)) as i32;
        let astride = self.mem.r32(f.wrapping_add(0x3c)) as i32;
        if hstride > 32 || arows > 256 {
            // hstride > 32 would overflow the guest's own 32-byte abBuf — an
            // UB path we make no attempt to mirror. arows caps the local
            // ASCII glyph copy.
            return Ok(false);
        }
        let opa = if alpha <= 0xfe {
            match self.opacity_copy(&g) {
                Some(v) => v,
                None => return Ok(false),
            }
        } else {
            Vec::new()
        };

        // ---- font fields ---------------------------------------------------
        let img = self.mem.r32(f.wrapping_add(0x20));
        let m34 = self.mem.r8(f.wrapping_add(0x34)) as i8 as i32;
        let f4 = self.mem.r32(f.wrapping_add(4)) as i32;
        let f8 = self.mem.r32(f.wrapping_add(8)) as i32;
        let f0c = self.mem.r32(f.wrapping_add(0xc)) as i32;
        let f14 = self.mem.r32(f.wrapping_add(0x14)) as i32;
        let f18 = self.mem.r32(f.wrapping_add(0x18)) as i32;
        let f1c = self.mem.r32(f.wrapping_add(0x1c)) as i32;
        let f28 = self.mem.r8(f.wrapping_add(0x28)) as u32;
        let mode = f28 as u8 as i8 as i32;
        let color = self.mem.r32(f.wrapping_add(0x40));
        let color2 = self.mem.r32(f.wrapping_add(0x44));
        let ifont = [
            self.mem.r32(g.internal_font),
            self.mem.r32(g.internal_font.wrapping_add(4)),
            self.mem.r32(g.internal_font.wrapping_add(8)),
            self.mem.r32(g.internal_font.wrapping_add(0xc)),
        ];
        // The jamo-variant tables hang off the font's own block.
        let cho_tab = img;
        let jung_tab = img.wrapping_add(0x2c);
        let jong_tab = img.wrapping_add(if m34 == 8 { 0x54 } else { 0x68 });

        // ---- committed: from here on the guest's globals are ours ---------
        self.mem.w32(g.cho_info, cho_tab);
        self.mem.w32(g.jung_info, jung_tab);
        self.mem.w32(g.jong_info, jong_tab);

        // Locals mirroring the globals; anything a path never touches keeps
        // its previous value, so seed them from the real state.
        let mut acc = self.acc_load(&g);
        let mut chb = self.mem.r8(g.ch);
        let (mut b1, mut b2) = (self.mem.r32(g.b1), self.mem.r32(g.b2));
        let (mut cho, mut jung, mut jong) = (
            self.mem.r32(g.cho_code) as i32,
            self.mem.r32(g.jung_code) as i32,
            self.mem.r32(g.jong_code) as i32,
        );
        let (mut cho_n, mut jung_n, mut jong_n) = (
            self.mem.r32(g.cho_n) as i32,
            self.mem.r32(g.jung_n) as i32,
            self.mem.r32(g.jong_n) as i32,
        );
        let mut chk = self.mem.r32(g.chk) as i32;
        let mut temp = self.mem.r32(g.temp) as i32;
        let (mut r0g, mut r1g) = (self.mem.r32(g.r0) as i32, self.mem.r32(g.r1) as i32);
        let mut chw = self.mem.r32(g.chwidth) as i32;
        let mut ab = [0u8; 32];
        if let Some(b) = self.mem.slice(g.ab_buf, 32) {
            ab.copy_from_slice(b);
        }

        let mut pbidx: i32 = 0;
        let mut pi: i32 = 0;
        let mut chx = x;
        let mut drawx = x;
        let mut drawy = y;

        if len == 0x7fff_ffff {
            len = self.mem.cstr_len(str_p) as i32;
        }

        // ASCII glyph rows for the current character, copied out so the draw
        // loops can hold the framebuffer borrow.
        let mut glyph = [0u8; 256];

        'chars: while len > pi {
            let ch = self.mem.r8(str_p.wrapping_add(pi as u32));
            chb = ch;
            if ch == 0 {
                break;
            }
            if chx != x {
                chx += f14;
            }
            chw = 0;

            if ch & 0x80 != 0 {
                // ---- hangul -----------------------------------------------
                b1 = self.mem.r8(str_p.wrapping_add(pi as u32)) as u32;
                pi += 1;
                b2 = self.mem.r8(str_p.wrapping_add(pi as u32)) as u32;
                pi += 1;
                wan_to_johab(&self.mem, &g, &mut b1, &mut b2, &mut pbidx)?;
                if b1 == 0x3f && b2 == 0x3f {
                    chx += f4;
                    continue 'chars;
                }
                ab = [0u8; 32];

                if b1 == 0x83 && (b2 == 0xc0 || b2 == 0xe0) {
                    // Compat jamo dots (ㆍ / ᆢ): set one or two bits straight
                    // in the compose buffer. C-truncating division/modulo,
                    // like the shipped sign dance.
                    let mut dot = |p: i32| {
                        let i = f8 + p / 8;
                        if (0..32).contains(&i) {
                            let r = p % 8;
                            if (0..8).contains(&r) {
                                ab[i as usize] |= 0x80u8 >> r;
                            }
                        }
                    };
                    if b2 == 0xc0 {
                        dot(f4 >> 1);
                    } else {
                        dot(f4 >> 2);
                        dot(3 * (f4 >> 2));
                    }
                    chw = f4 - 1;
                } else {
                    // Johab split, plus one on each component.
                    let (c, j, jo) = get3syllable(&self.mem, &g, b1, b2)?;
                    cho = c + 1;
                    jung = j + 1;
                    jong = jo + 1;
                    if m34 == 8 {
                        chk = (jong != 0) as i32;
                        jong_n = rd8(&self.mem, jong_tab.wrapping_add(jung as u32))? as i32;
                        jung_n = rd8(&self.mem, jung_tab.wrapping_add((0x14 * chk + cho) as u32))?
                            as i32;
                        cho_n = rd8(&self.mem, cho_tab.wrapping_add((0x16 * chk + jung) as u32))?
                            as i32;
                    } else {
                        let jsel = if cho == 6 || cho == 0x11 { 0x16 } else { 0 };
                        jong_n =
                            rd8(&self.mem, jong_tab.wrapping_add((jsel + jung) as u32))? as i32;
                        // Jung variant row: 0 without a final consonant, else
                        // one of two rows picked by a vowel-class bitmask.
                        let vsel = if jong != 0 {
                            if (jung as u32) <= 0x1b
                                && (1u32 << jung) & !0xf500_0000 & !0x7d_0000 & !0xff != 0
                            {
                                0x28
                            } else {
                                0x14
                            }
                        } else {
                            0
                        };
                        jung_n = rd8(&self.mem, jung_tab.wrapping_add((vsel + cho) as u32))? as i32;
                        chk = (jong != 0) as i32;
                        cho_n = rd8(&self.mem, cho_tab.wrapping_add((0x16 * chk + jung) as u32))?
                            as i32;
                    }

                    // Compose: cho overwrites, jung/jong OR in.
                    let n = hstride.clamp(0, 32) as usize;
                    let src =
                        ifont[0].wrapping_add((hstride.wrapping_mul(0x14 * cho_n + cho)) as u32);
                    for (i, slot) in ab.iter_mut().enumerate().take(n) {
                        *slot = rd8(&self.mem, src.wrapping_add(i as u32))?;
                    }
                    if jung != 0 {
                        pbidx = hstride.wrapping_mul(0x16 * jung_n + jung);
                        r0g = 0;
                        if hstride > 0 {
                            let src = ifont[1].wrapping_add(pbidx as u32);
                            for (i, slot) in ab.iter_mut().enumerate().take(n) {
                                *slot |= rd8(&self.mem, src.wrapping_add(i as u32))?;
                            }
                            r0g = hstride;
                        }
                    } else {
                        chw = f4 - 1;
                    }
                    if jong != 0 {
                        pbidx = hstride.wrapping_mul(0x1c * jong_n + jong);
                        r0g = 0;
                        if hstride > 0 {
                            let src = ifont[2].wrapping_add(pbidx as u32);
                            for (i, slot) in ab.iter_mut().enumerate().take(n) {
                                *slot |= rd8(&self.mem, src.wrapping_add(i as u32))?;
                            }
                            r0g = hstride;
                        }
                    }
                }

                // ---- hangul rasterise -------------------------------------
                let fb =
                    self.mem.slice_mut(scr.lcd, scr.fb_len).ok_or("font: framebuffer vanished")?;
                let mut pen = Pen {
                    fb,
                    w: scr.lw,
                    h: scr.lh,
                    clip: scr.clip,
                    dir: scr.dir,
                    acc: &mut acc,
                    opa: &opa,
                };
                // One pass over the 2-bytes-per-row buffer. `right` byte is
                // drawn at x+8, `left` at x — the row is a big-endian u16.
                let mut pass = |body: bool, one: bool| {
                    drawx = chx;
                    drawy = y;
                    r1g = 0;
                    if hstride <= 0 {
                        return;
                    }
                    loop {
                        r0g = 0;
                        let mut c = 0;
                        loop {
                            let bit_r = ((ab[(r1g & 31) as usize] as i32) << c) & 0x80;
                            temp = bit_r;
                            if bit_r != 0 {
                                if one {
                                    pen.px_2(drawx + 8, drawy, color, alpha);
                                    if r0g + 9 >= chw {
                                        chw = r0g + 9;
                                    }
                                } else if body {
                                    pen.px(drawx + 8, drawy, color, alpha);
                                    if r0g + 7 >= chw {
                                        chw = r0g + 8;
                                    }
                                } else {
                                    pen.px_w(drawx + 8, drawy, color2, alpha);
                                    pen.px_h(drawx + 8, drawy, color2, alpha);
                                    if r0g + 7 >= chw {
                                        chw = r0g + 8;
                                    }
                                }
                            }
                            let bit_l = ((ab[((r1g + 1) & 31) as usize] as i32) << r0g) & 0x80;
                            temp = bit_l;
                            if bit_l != 0 {
                                if one {
                                    pen.px_2(drawx, drawy, color, alpha);
                                    if r0g + 1 >= chw {
                                        chw = r0g + 1;
                                    }
                                } else if body {
                                    pen.px(drawx, drawy, color, alpha);
                                    if r0g >= chw {
                                        chw = r0g;
                                    }
                                } else {
                                    pen.px_w(drawx, drawy, color2, alpha);
                                    pen.px_h(drawx, drawy, color2, alpha);
                                    if r0g >= chw {
                                        chw = r0g;
                                    }
                                }
                            }
                            r0g += 1;
                            drawx += 1;
                            c = r0g;
                            if c > 7 {
                                break;
                            }
                        }
                        r1g += 2;
                        drawy += 1;
                        drawx = chx;
                        if hstride <= r1g {
                            break;
                        }
                    }
                };
                if mode == 1 {
                    pass(false, true);
                } else if mode >= 2 {
                    pass(false, false);
                }
                if f28 & 1 == 0 {
                    pass(true, false);
                }
                chx += chw + 1;
                continue 'chars;
            }

            if ch == 0x20 {
                chx += f18;
                pi += 1;
                continue 'chars;
            }

            // ---- ASCII glyph ----------------------------------------------
            pbidx = astride.wrapping_mul(ch as i32 - 0x20);
            pi += 1;
            let rows = arows.clamp(0, 256) as usize;
            let src = ifont[3].wrapping_add(pbidx as u32);
            for (i, slot) in glyph.iter_mut().enumerate().take(rows) {
                *slot = rd8(&self.mem, src.wrapping_add(i as u32))?;
            }

            let fb = self.mem.slice_mut(scr.lcd, scr.fb_len).ok_or("font: framebuffer vanished")?;
            let mut pen = Pen {
                fb,
                w: scr.lw,
                h: scr.lh,
                clip: scr.clip,
                dir: scr.dir,
                acc: &mut acc,
                opa: &opa,
            };
            let base_y = f1c + ((f8 - arows) >> 1) + y;
            let mut pass = |body: bool, one: bool| {
                drawy = base_y;
                drawx = chx;
                r1g = 0;
                if arows <= 0 {
                    return;
                }
                loop {
                    r0g = 0;
                    if astride > 0 {
                        let byte = glyph[(r1g as usize) & 255] as i32;
                        let mut c = 0;
                        loop {
                            let bit = (byte << c) & 0x80;
                            temp = bit;
                            if bit != 0 {
                                if one {
                                    pen.px_2(drawx, drawy, color, alpha);
                                    if r0g + 1 >= chw {
                                        chw = r0g + 1;
                                    }
                                } else if body {
                                    pen.px(drawx, drawy, color, alpha);
                                    if r0g >= chw {
                                        chw = r0g;
                                    }
                                } else {
                                    pen.px_w(drawx, drawy, color2, alpha);
                                    pen.px_h(drawx, drawy, color2, alpha);
                                    if r0g >= chw {
                                        chw = r0g;
                                    }
                                }
                            }
                            drawx += 1;
                            r0g += 1;
                            c = r0g;
                            if astride <= c {
                                break;
                            }
                        }
                    }
                    r1g += 1;
                    drawy += 1;
                    drawx = chx;
                    if arows <= r1g {
                        break;
                    }
                }
            };
            let adv;
            if mode == 1 {
                pass(false, true);
                adv = 1; // mode 1 is odd: no body pass, +1 advance
            } else {
                if mode >= 2 {
                    pass(false, false);
                }
                if f28 & 1 != 0 {
                    adv = 1;
                } else {
                    pass(true, false);
                    adv = 0;
                }
            }
            chx += f0c + adv;
        }

        // ---- write the final global state back ----------------------------
        self.mem.w32(g.chx, chx as u32);
        self.mem.w32(g.drawx, drawx as u32);
        self.mem.w32(g.drawy, drawy as u32);
        self.mem.w32(g.chwidth, chw as u32);
        self.mem.w32(g.pi, pi as u32);
        self.mem.w32(g.pbidx, pbidx as u32);
        self.mem.w32(g.r0, r0g as u32);
        self.mem.w32(g.r1, r1g as u32);
        self.mem.w32(g.temp, temp as u32);
        self.mem.w8(g.ch, chb);
        self.mem.w32(g.b1, b1);
        self.mem.w32(g.b2, b2);
        self.mem.w32(g.cho_code, cho as u32);
        self.mem.w32(g.jung_code, jung as u32);
        self.mem.w32(g.jong_code, jong as u32);
        self.mem.w32(g.cho_n, cho_n as u32);
        self.mem.w32(g.jung_n, jung_n as u32);
        self.mem.w32(g.jong_n, jong_n as u32);
        self.mem.w32(g.chk, chk as u32);
        self.mem.w32(g.whan, 0);
        self.mem.write_bytes(g.ab_buf, &ab);
        self.acc_store(&g, &acc);

        self.fnt_host += 1;
        self.cpu.reg_set(Mode::User, 0, (chx - x) as u32);
        let lr = self.cpu.reg_get(Mode::User, reg::LR);
        self.cpu.reg_set(Mode::User, reg::PC, lr);
        Ok(true)
    }
}
