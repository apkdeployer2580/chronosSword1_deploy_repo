//! The slice of OpenGL ES 1.x that `libchronosMian.so` actually links against.
//!
//! The binary imports 19 GL entry points and uses them for exactly one thing:
//! `MC_grpFlushLcd` pushes the WIPI framebuffer's dirty rectangle into a texture
//! with `glTexSubImage2D`, and `Texture2D::update` draws that texture as one
//! axis-aligned quad (plus up to two black letterbox bars). There is no
//! per-sprite GL traffic at all — the guest rasterises sprites itself, straight
//! into the framebuffer it got from `MC_grpGetScreenFrameBuffer`.
//!
//! So this is not an OpenGL implementation. It is a state machine that watches
//! those calls and performs the single blit they describe, on the CPU. That
//! keeps the whole renderer portable (the headless probe and the Android app run
//! the identical path) and costs nothing: one scaled copy of a 400x240 image per
//! frame.

pub const GL_TEXTURE_2D: u32 = 0x0DE1;
pub const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub const GL_UNSIGNED_SHORT_5_6_5: u32 = 0x8363;
pub const GL_UNSIGNED_SHORT_4_4_4_4: u32 = 0x8033;
pub const GL_UNSIGNED_SHORT_5_5_5_1: u32 = 0x8034;
pub const GL_RGB: u32 = 0x1907;
pub const GL_RGBA: u32 = 0x1908;
pub const GL_FLOAT: u32 = 0x1406;
pub const GL_TRIANGLE_STRIP: u32 = 0x0005;
pub const GL_NEAREST: f32 = 0x2600 as f32;

#[derive(Clone)]
pub struct Texture {
    pub width: i32,
    pub height: i32,
    pub format: u32,
    pub ty: u32,
    /// Decoded to RGBA8888 at upload time. Textures here are at most
    /// 512x256, so the extra memory is irrelevant next to not re-decoding
    /// 5:6:5 for every one of the ~96 000 pixels we blit each frame.
    pub rgba: Vec<u32>,
    pub mag_linear: bool,
}

#[derive(Clone, Copy, Default)]
pub struct Array {
    pub size: i32,
    pub ty: u32,
    pub stride: i32,
    pub ptr: u32,
}

pub struct Gl {
    pub viewport: (i32, i32, i32, i32),
    /// Left, right, bottom, top of the last `glOrthof`. The guest sets up a
    /// pixel-space projection with Y increasing downwards.
    pub ortho: (f32, f32, f32, f32),
    pub textures: Vec<Option<Texture>>,
    pub bound: u32,
    pub color: [f32; 4],
    pub vertex_array: Array,
    pub texcoord_array: Array,
    pub texcoord_enabled: bool,
    /// Destination surface, RGBA8888 (0xAABBGGRR little-endian in memory, i.e.
    /// the layout `softbuffer` and PNG writers both want as 0x00RRGGBB words).
    pub fb: Vec<u32>,
    pub fb_w: i32,
    pub fb_h: i32,
    /// Bumped by every `glDrawArrays`, so the host can tell a frame that drew
    /// something from one that did not.
    pub draws: u64,
    /// Scratch: destination column -> source texel column, rebuilt per draw.
    xmap: Vec<i32>,
    /// Optional horizontal stretch, as (origin, scale) in framebuffer pixels.
    ///
    /// The guest is told one uniform magnification, so it letterboxes a 400x240
    /// picture on a 20:9 phone exactly as the original did. Widening it to fill
    /// the screen cannot be expressed through that one number, so it is applied
    /// here instead: guest x is mapped from `[origin, origin + width)` onto the
    /// whole surface. Y is untouched, so this only ever stretches, never crops.
    pub stretch: Option<(f32, f32)>,
    /// Skip work whose output is already in `fb` (`CS_LAZY_GL`).
    ///
    /// The guest redraws its screen quad on **every** `nativeLoop` — 60 Hz on
    /// the device — but only uploads a new picture on its own ~21 Hz tick, and
    /// the quad's geometry, colour and texcoords are fixed at boot. So a frame
    /// with no upload repaints `fb` with the pixels it already holds, and the
    /// host then copies and presents them again: two full-surface passes of
    /// identical bytes, which on a phone is sustained memory traffic for
    /// nothing. With `lazy` on, [`Gl::draw_arrays`] skips its blit while
    /// `uploads` hasn't moved, and [`Gl::end_frame`] tells the host whether
    /// presenting is worth anything this frame.
    pub lazy: bool,
    /// Content generation: bumped by every texture mutation that could change
    /// what a draw produces. Never bumped by a draw — draws are pure functions
    /// of the textures and of geometry that is fixed after boot.
    pub uploads: u64,
    /// The generation the framebuffer was last brought up to date with.
    /// Starts distinct from `uploads` so the first frame always draws.
    drawn_uploads: u64,
    /// The last sub-image each texture received, so an identical re-upload can
    /// be recognised and skipped — decode included. A byte compare, not a
    /// hash: a hash collision would freeze a frame, a compare cannot. Only
    /// maintained while `lazy` is on.
    last_upload: Vec<Option<UploadMemo>>,
}

/// One remembered `glTexSubImage2D`, for [`Gl::tex_sub_image`]'s "is this the
/// same picture again" test. The rect matters as much as the bytes: the same
/// bytes at a different offset are a different picture.
struct UploadMemo {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    format: u32,
    ty: u32,
    pixels: Vec<u8>,
}

/// `CS_GL_DEBUG=1` traces the two texture calls `MC_grpFlushLcd` makes. The
/// pair matters together: a `glTexImage2D` zeroes the texture, so anything the
/// guest drew in an earlier frame and did not include in this frame's
/// `glTexSubImage2D` dirty rectangle disappears.
pub(crate) fn gl_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CS_GL_DEBUG").is_ok())
}

impl Gl {
    pub fn new(fb_w: i32, fb_h: i32) -> Self {
        Self {
            viewport: (0, 0, fb_w, fb_h),
            ortho: (0.0, fb_w as f32, fb_h as f32, 0.0),
            textures: vec![None],
            bound: 0,
            color: [1.0; 4],
            vertex_array: Array::default(),
            texcoord_array: Array::default(),
            texcoord_enabled: false,
            fb: vec![0xff00_0000; (fb_w * fb_h) as usize],
            fb_w,
            fb_h,
            draws: 0,
            xmap: Vec::new(),
            stretch: None,
            lazy: false,
            uploads: 0,
            drawn_uploads: u64::MAX,
            last_upload: Vec::new(),
        }
    }

    /// Close out one host frame: report whether anything uploaded since the
    /// last close, and remember the current generation as "shown". `true`
    /// means the framebuffer may differ from what the host last presented;
    /// `false` is a guarantee that it does not. With `lazy` off this always
    /// answers `true`, which is the shipped present-every-frame behaviour.
    pub fn end_frame(&mut self) -> bool {
        let dirty = self.uploads != self.drawn_uploads;
        self.drawn_uploads = self.uploads;
        dirty || !self.lazy
    }

    pub fn gen_texture(&mut self) -> u32 {
        self.textures.push(None);
        (self.textures.len() - 1) as u32
    }

    pub fn delete_texture(&mut self, id: u32) {
        if let Some(slot) = self.textures.get_mut(id as usize) {
            if slot.is_some() {
                self.uploads += 1;
            }
            *slot = None;
        }
        if let Some(memo) = self.last_upload.get_mut(id as usize) {
            *memo = None;
        }
    }

    pub fn tex_image(&mut self, w: i32, h: i32, format: u32, ty: u32) {
        let id = self.bound as usize;
        if id == 0 || id >= self.textures.len() || w <= 0 || h <= 0 {
            return;
        }
        if gl_debug() {
            log::info!("glTexImage2D id={id} {w}x{h} fmt={format:#x} ty={ty:#x} (texture reset)");
        }
        let mag_linear = self.textures[id].as_ref().is_some_and(|t| t.mag_linear);
        self.textures[id] = Some(Texture {
            width: w,
            height: h,
            format,
            ty,
            rgba: vec![0; (w * h) as usize],
            mag_linear,
        });
        // A reset is new content (everything the guest drew is now zero), and
        // it invalidates the sub-image memo — the next upload lands on a
        // zeroed texture, so "same bytes as last time" would no longer mean
        // "same result as last time".
        self.uploads += 1;
        if let Some(memo) = self.last_upload.get_mut(id) {
            *memo = None;
        }
    }

    pub fn tex_parameter(&mut self, pname: u32, value: f32) {
        // 0x2800 = GL_TEXTURE_MAG_FILTER, 0x2801 = GL_TEXTURE_MIN_FILTER.
        if pname != 0x2800 {
            return;
        }
        if let Some(Some(t)) = self.textures.get_mut(self.bound as usize) {
            t.mag_linear = value != GL_NEAREST;
        }
    }

    /// `pixels` is the raw guest sub-image, tightly packed, `w * h` texels of
    /// the texture's format.
    pub fn tex_sub_image(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        format: u32,
        ty: u32,
        pixels: &[u8],
    ) {
        let id = self.bound as usize;
        if !matches!(self.textures.get(id), Some(Some(_))) {
            return;
        }
        if w <= 0 || h <= 0 {
            return;
        }
        let bpp = bytes_per_texel(format, ty);
        let n = (w * h) as usize * bpp;
        if pixels.len() < n {
            return;
        }
        // A static screen re-uploads the same rectangle with the same bytes on
        // every game tick (the flush is unconditional in `timerCB`). Recognise
        // that and stop here: the decode below would reproduce what `rgba`
        // already holds, and `uploads` staying put is what lets the whole
        // frame's redraw, copy and present be skipped too. Sound because
        // `rgba` is only ever written here and in `tex_image`, and `tex_image`
        // clears this memo.
        if self.lazy {
            if self.last_upload.len() <= id {
                self.last_upload.resize_with(id + 1, || None);
            }
            let memo = &mut self.last_upload[id];
            if let Some(m) = memo {
                if m.x == x
                    && m.y == y
                    && m.w == w
                    && m.h == h
                    && m.format == format
                    && m.ty == ty
                    && m.pixels == pixels[..n]
                {
                    if gl_debug() {
                        log::info!("glTexSubImage2D {w}x{h}@{x},{y} (unchanged, skipped)");
                    }
                    return;
                }
            }
            *memo = Some(UploadMemo { x, y, w, h, format, ty, pixels: pixels[..n].to_vec() });
        }
        self.uploads += 1;
        let Some(Some(t)) = self.textures.get_mut(id) else {
            return;
        };
        if gl_debug() {
            log::info!("glTexSubImage2D {w}x{h}@{x},{y}");
        }
        for row in 0..h {
            let dy = y + row;
            if dy < 0 || dy >= t.height {
                continue;
            }
            for col in 0..w {
                let dx = x + col;
                if dx < 0 || dx >= t.width {
                    continue;
                }
                let src = ((row * w + col) as usize) * bpp;
                t.rgba[(dy * t.width + dx) as usize] = decode_texel(&pixels[src..], format, ty);
            }
        }
    }

    /// Map an ortho-space (pixel) coordinate to a framebuffer pixel.
    fn to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        let (l, r, b, t) = self.ortho;
        let (vx, vy, vw, vh) = self.viewport;
        // NDC in [-1, 1], then viewport. Y is flipped once by the ortho (the
        // guest passes bottom > top) and once by the viewport, so the net effect
        // is that guest Y and framebuffer Y run the same way.
        let nx = (x - l) / (r - l);
        let ny = (y - t) / (b - t);
        let sx = vx as f32 + nx * vw as f32;
        let sy = vy as f32 + ny * vh as f32;
        match self.stretch {
            Some((origin, scale)) => ((sx - origin) * scale, sy),
            None => (sx, sy),
        }
    }

    /// The only draw the guest ever issues: a 4-vertex strip forming an
    /// axis-aligned rectangle. Black-filled when the current colour is black
    /// (the letterbox bars — the guest leaves a stale texcoord array bound and
    /// relies on GL_MODULATE zeroing the texel), textured otherwise.
    pub fn draw_arrays(&mut self, mode: u32, first: i32, count: i32, read: &dyn Fn(u32) -> f32) {
        self.draws += 1;
        // No upload since the last completed frame means this draw writes the
        // pixels `fb` already holds: the quad's geometry, colour and texcoord
        // arrays are fixed at boot (`Texture2D` fields), so the textures are
        // the only input that moves. `draws` still counts — the probe prints
        // it, and skipping is not "did not draw", it is "drew the same thing
        // by leaving it there".
        if self.lazy && self.uploads == self.drawn_uploads {
            return;
        }
        if mode != GL_TRIANGLE_STRIP || count != 4 || self.vertex_array.ptr == 0 {
            return;
        }

        let vstride = if self.vertex_array.stride != 0 {
            self.vertex_array.stride
        } else {
            self.vertex_array.size * 4
        };
        let mut vx = [0f32; 4];
        let mut vy = [0f32; 4];
        for i in 0..4 {
            let p = self.vertex_array.ptr + ((first + i as i32) * vstride) as u32;
            vx[i] = read(p);
            vy[i] = read(p + 4);
        }

        let (x0, y0) = self.to_screen(
            vx.iter().cloned().fold(f32::INFINITY, f32::min),
            vy.iter().cloned().fold(f32::INFINITY, f32::min),
        );
        let (x1, y1) = self.to_screen(
            vx.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            vy.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        );
        let (dx0, dx1) = (x0.min(x1).round() as i32, x0.max(x1).round() as i32);
        let (dy0, dy1) = (y0.min(y1).round() as i32, y0.max(y1).round() as i32);
        if dx1 <= dx0 || dy1 <= dy0 {
            return;
        }

        let black = self.color[0] == 0.0 && self.color[1] == 0.0 && self.color[2] == 0.0;
        if black || !self.texcoord_enabled || self.texcoord_array.ptr == 0 {
            self.fill(dx0, dy0, dx1, dy1, 0xff00_0000);
            return;
        }

        let tstride = if self.texcoord_array.stride != 0 {
            self.texcoord_array.stride
        } else {
            self.texcoord_array.size * 4
        };
        let mut tu = [0f32; 4];
        let mut tv = [0f32; 4];
        for i in 0..4 {
            let p = self.texcoord_array.ptr + ((first + i as i32) * tstride) as u32;
            tu[i] = read(p);
            tv[i] = read(p + 4);
        }
        let u0 = tu.iter().cloned().fold(f32::INFINITY, f32::min);
        let u1 = tu.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let v0 = tv.iter().cloned().fold(f32::INFINITY, f32::min);
        let v1 = tv.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

        let Some(Some(tex)) = self.textures.get(self.bound as usize) else {
            return;
        };
        // Clone-free blit: the borrow of `tex` and the write to `self.fb` would
        // overlap, so pull out what we need first.
        let (tw, th) = (tex.width, tex.height);
        let texels = &tex.rgba as *const Vec<u32>;
        let (fb_w, fb_h) = (self.fb_w, self.fb_h);

        let dw = (dx1 - dx0) as f32;
        let dh = (dy1 - dy0) as f32;
        // SAFETY: `texels` points into `self.textures`, which this loop does not
        // touch; only `self.fb` is written.
        let texels = unsafe { &*texels };

        // The source column for a destination column does not depend on the row,
        // so resolve it once per frame instead of once per pixel. On a 1080p
        // landscape surface that is ~2.5 M pixels a frame, and the per-pixel form
        // cost a float divide and a clamp on every one of them.
        let (cx0, cx1) = (dx0.max(0), dx1.min(fb_w));
        if cx1 <= cx0 {
            return;
        }
        self.xmap.clear();
        self.xmap.reserve((cx1 - cx0) as usize);
        for x in cx0..cx1 {
            let u = u0 + (u1 - u0) * (((x - dx0) as f32 + 0.5) / dw);
            self.xmap.push(((u * tw as f32) as i32).clamp(0, tw - 1));
        }

        for y in dy0.max(0)..dy1.min(fb_h) {
            let v = v0 + (v1 - v0) * (((y - dy0) as f32 + 0.5) / dh);
            let sy = ((v * th as f32) as i32).clamp(0, th - 1);
            let src_row = (sy * tw) as usize;
            let dst_row = (y * fb_w) as usize;
            let dst = &mut self.fb[dst_row + cx0 as usize..dst_row + cx1 as usize];
            for (d, &sx) in dst.iter_mut().zip(self.xmap.iter()) {
                *d = texels[src_row + sx as usize];
            }
        }
    }

    fn fill(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, argb: u32) {
        for y in y0.max(0)..y1.min(self.fb_h) {
            let row = (y * self.fb_w) as usize;
            for x in x0.max(0)..x1.min(self.fb_w) {
                self.fb[row + x as usize] = argb;
            }
        }
    }
}

pub fn bytes_per_texel(format: u32, ty: u32) -> usize {
    match ty {
        GL_UNSIGNED_SHORT_5_6_5 | GL_UNSIGNED_SHORT_4_4_4_4 | GL_UNSIGNED_SHORT_5_5_5_1 => 2,
        GL_UNSIGNED_BYTE => {
            if format == GL_RGBA {
                4
            } else {
                3
            }
        }
        _ => 2,
    }
}

fn decode_texel(b: &[u8], format: u32, ty: u32) -> u32 {
    let argb = |r: u32, g: u32, b: u32, a: u32| (a << 24) | (r << 16) | (g << 8) | b;
    match ty {
        GL_UNSIGNED_SHORT_5_6_5 => {
            let p = u16::from_le_bytes([b[0], b[1]]) as u32;
            let r = (p >> 11) & 0x1f;
            let g = (p >> 5) & 0x3f;
            let bl = p & 0x1f;
            argb((r << 3) | (r >> 2), (g << 2) | (g >> 4), (bl << 3) | (bl >> 2), 0xff)
        }
        GL_UNSIGNED_SHORT_4_4_4_4 => {
            let p = u16::from_le_bytes([b[0], b[1]]) as u32;
            let r = (p >> 12) & 0xf;
            let g = (p >> 8) & 0xf;
            let bl = (p >> 4) & 0xf;
            let a = p & 0xf;
            argb(r * 17, g * 17, bl * 17, a * 17)
        }
        GL_UNSIGNED_SHORT_5_5_5_1 => {
            let p = u16::from_le_bytes([b[0], b[1]]) as u32;
            let r = (p >> 11) & 0x1f;
            let g = (p >> 6) & 0x1f;
            let bl = (p >> 1) & 0x1f;
            let a = if p & 1 != 0 { 0xff } else { 0 };
            argb((r << 3) | (r >> 2), (g << 3) | (g >> 2), (bl << 3) | (bl >> 2), a)
        }
        _ => {
            if format == GL_RGBA {
                argb(b[0] as u32, b[1] as u32, b[2] as u32, b[3] as u32)
            } else {
                argb(b[0] as u32, b[1] as u32, b[2] as u32, 0xff)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x4 lazy Gl with one 4x4 RGBA texture bound, plus the flat vertex
    /// and texcoord tables `draw_arrays` reads through its closure — the same
    /// full-screen quad `Texture2D::update` issues every frame.
    fn quad_gl() -> (Gl, Vec<f32>) {
        let mut gl = Gl::new(4, 4);
        gl.lazy = true;
        let id = gl.gen_texture();
        gl.bound = id;
        gl.tex_image(4, 4, GL_RGBA, GL_UNSIGNED_BYTE);
        // Addresses are word indexes into the table below: vertices at 0..8,
        // texcoords at 0x100.. — the closure is the "guest memory".
        gl.vertex_array = Array { size: 2, ty: GL_FLOAT, stride: 0, ptr: 4 };
        gl.texcoord_array = Array { size: 2, ty: GL_FLOAT, stride: 0, ptr: 0x104 };
        gl.texcoord_enabled = true;
        gl.color = [1.0; 4];
        let mut table = vec![0f32; 0x200];
        for (i, v) in [0.0, 0.0, 4.0, 0.0, 0.0, 4.0, 4.0, 4.0].iter().enumerate() {
            table[1 + i] = *v;
        }
        for (i, v) in [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0].iter().enumerate() {
            table[0x41 + i] = *v;
        }
        (gl, table)
    }

    fn draw(gl: &mut Gl, table: &[f32]) {
        let t: Vec<f32> = table.to_vec();
        gl.draw_arrays(GL_TRIANGLE_STRIP, 0, 4, &move |addr| t[(addr / 4) as usize]);
    }

    /// The flush is unconditional in the guest's `timerCB`, so a static screen
    /// re-uploads the same bytes every game tick. That must not count as a new
    /// picture — it is what lets a dialogue box on screen cost nothing — and
    /// anything that *does* change the texture must.
    #[test]
    fn an_identical_reupload_is_not_a_new_picture() {
        let (mut gl, _) = quad_gl();
        let px = [7u8; 4 * 4 * 4];
        gl.tex_sub_image(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, &px);
        let after_first = gl.uploads;
        gl.tex_sub_image(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, &px);
        assert_eq!(gl.uploads, after_first, "same rect, same bytes: not new");

        let mut other = px;
        other[0] ^= 1;
        gl.tex_sub_image(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, &other);
        assert_eq!(gl.uploads, after_first + 1, "one changed byte is a new picture");

        // Same bytes at a different offset are a different picture too.
        gl.tex_sub_image(0, 0, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE, &other[..16]);
        let n = gl.uploads;
        gl.tex_sub_image(1, 0, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE, &other[..16]);
        assert_eq!(gl.uploads, n + 1);

        // `glTexImage2D` zeroes the texture, so the memo must die with it:
        // the next upload lands on a different canvas even with equal bytes.
        gl.tex_image(4, 4, GL_RGBA, GL_UNSIGNED_BYTE);
        let n = gl.uploads;
        gl.tex_sub_image(1, 0, 2, 2, GL_RGBA, GL_UNSIGNED_BYTE, &other[..16]);
        assert_eq!(gl.uploads, n + 1, "a reset invalidates the memo");
    }

    /// The skipped redraw must be exactly the redraw that would have written
    /// the same bytes: a clean frame leaves the framebuffer untouched, an
    /// upload makes the next draw write again. The sentinel proves the skip
    /// really skipped — if the blit ran it would overwrite it with texels.
    #[test]
    fn a_clean_frame_skips_the_blit_and_a_dirty_one_does_not() {
        let (mut gl, table) = quad_gl();
        let px = [0x40u8; 4 * 4 * 4];
        gl.tex_sub_image(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, &px);
        draw(&mut gl, &table);
        let texel = gl.fb[0];
        assert_ne!(texel, 0xdead_beef, "the first draw must land");
        assert!(gl.end_frame(), "an upload makes the frame dirty");

        // No upload since: the draw is the same picture, so it may be skipped.
        gl.fb[0] = 0xdead_beef;
        draw(&mut gl, &table);
        assert_eq!(gl.fb[0], 0xdead_beef, "a clean redraw must not touch fb");
        assert!(!gl.end_frame(), "and the host need not present it");

        // A fresh upload un-skips the very next draw.
        let px2 = [0x80u8; 4 * 4 * 4];
        gl.tex_sub_image(0, 0, 4, 4, GL_RGBA, GL_UNSIGNED_BYTE, &px2);
        draw(&mut gl, &table);
        assert_ne!(gl.fb[0], 0xdead_beef, "a dirty redraw writes");
        assert!(gl.end_frame());

        // With the knob off, every frame presents — the shipped behaviour.
        gl.lazy = false;
        assert!(gl.end_frame(), "lazy off means present always");
    }
}
