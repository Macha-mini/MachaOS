//! Surface-agnostic drawing primitives (rectangles, glyphs, strings).
//! Anything that can plot a pixel (the real framebuffer's back buffer, a
//! window's private pixel buffer, ...) can implement `Surface` and reuse
//! these routines.

use alloc::vec;
use alloc::vec::Vec;

use crate::font;

pub trait Surface {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn put_pixel(&mut self, x: u32, y: u32, color: u32);
    /// Reads back a pixel already painted onto this surface, used by the
    /// alpha-blending helpers to composite over whatever is underneath.
    /// Returns `None` (degrading the blend to a plain fill) for surfaces
    /// that can't read back.
    fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        let _ = (x, y);
        None
    }

    /// Copies `len` pixels from `src[offset..offset+len]` onto this
    /// surface's row `y` starting at column `x`. The default walks
    /// `put_pixel`; surfaces backed by a contiguous pixel buffer
    /// override this with a slice copy (the hot path for full-screen
    /// blits, window content, and wallpaper).
    fn blit_span(&mut self, x: u32, y: u32, src: &[u32], offset: usize, len: u32) {
        for i in 0..len {
            self.put_pixel(x + i, y, src[offset + i as usize]);
        }
    }

    /// Fills `len` pixels on row `y` starting at column `x` with `color`.
    /// Same override story as `blit_span`.
    fn fill_span(&mut self, x: u32, y: u32, len: u32, color: u32) {
        for i in 0..len {
            self.put_pixel(x + i, y, color);
        }
    }

    /// Blends `color` over `len` pixels on row `y` starting at column `x`
    /// with opacity `alpha` (0-255). Same override story as `blit_span`.
    fn blend_span(&mut self, x: u32, y: u32, len: u32, color: u32, alpha: u32) {
        if alpha >= 255 {
            return self.fill_span(x, y, len, color);
        }
        if alpha == 0 {
            return;
        }
        for i in 0..len {
            if let Some(base) = self.get_pixel(x + i, y) {
                self.put_pixel(x + i, y, blend(color, base, alpha));
            } else {
                self.put_pixel(x + i, y, color);
            }
        }
    }
}

pub fn fill_rect(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let x_end = (x + w).min(surface.width());
    let y_end = (y + h).min(surface.height());
    if x_end <= x || y_end <= y {
        return;
    }
    for yy in y..y_end {
        surface.fill_span(x, yy, x_end - x, color);
    }
}

/// Same as `fill_rect`, but interpolates linearly from `top` to `bottom`
/// (each 0x00RRGGBB) one scanline at a time, for a soft vertical gradient
/// instead of a flat fill.
pub fn fill_rect_gradient_v(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, top: u32, bottom: u32) {
    if h == 0 {
        return;
    }
    let denom = (h - 1).max(1) as i32;
    let channel = |c: u32, shift: u32| ((c >> shift) & 0xFF) as i32;
    for row in 0..h {
        let mut color = 0u32;
        for shift in [16u32, 8, 0] {
            let a = channel(top, shift);
            let b = channel(bottom, shift);
            let v = a + (b - a) * row as i32 / denom;
            color |= (v as u32 & 0xFF) << shift;
        }
        fill_span_clipped(surface, x, y + row, w, color);
    }
}

/// `fill_rect`'s row loop for a single scanline, clipped to the surface.
fn fill_span_clipped(surface: &mut dyn Surface, x: u32, y: u32, w: u32, color: u32) {
    let x_end = (x + w).min(surface.width());
    if y >= surface.height() || x_end <= x {
        return;
    }
    surface.fill_span(x, y, x_end - x, color);
}

/// Integer square root (floor) via Newton's method. `r` is always small
/// here (window/panel corner radii, at most a few dozen px), so this
/// converges in a handful of iterations — cheap enough to call per row
/// in `rounded_inset` with no FPU involved.
fn isqrt(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Horizontal inset for a given scanline of a rectangle with rounded
/// corners of radius `r`: `0` in the middle rows, growing toward the
/// corners along a true quarter-circle arc (not a linear chamfer) so
/// the painted width narrows smoothly there instead of faceting into
/// an octagon.
fn rounded_inset(row: u32, h: u32, r: u32) -> u32 {
    if r == 0 {
        return 0;
    }
    // Vertical distance from the corner's outer edge, 0..r.
    let dy = if row < r {
        r - 1 - row
    } else if row >= h - r {
        row - (h - r)
    } else {
        return 0;
    };
    let dx = isqrt(r * r - dy * dy);
    r - dx.min(r)
}

/// Like `fill_rect`, but with the four corners clipped to a circular arc
/// of radius `r` (which reveals whatever is behind the rect there, so
/// buttons look "pill-shaped" over any background).
pub fn fill_rounded_rect(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, r: u32, color: u32) {
    if h == 0 {
        return;
    }
    for row in 0..h {
        let inset = rounded_inset(row, h, r);
        let inner = w.saturating_sub(2 * inset);
        if inner >= 2 {
            fill_span_clipped(surface, x + inset, y + row, inner, color);
        }
    }
}

/// `fill_rounded_rect` with the vertical-gradient coloring of
/// `fill_rect_gradient_v`.
pub fn fill_rounded_rect_gradient_v(
    surface: &mut dyn Surface,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    r: u32,
    top: u32,
    bottom: u32,
) {
    if h == 0 {
        return;
    }
    let denom = (h - 1).max(1) as i32;
    let channel = |c: u32, shift: u32| ((c >> shift) & 0xFF) as i32;
    for row in 0..h {
        let mut color = 0u32;
        for shift in [16u32, 8, 0] {
            let a = channel(top, shift);
            let b = channel(bottom, shift);
            let v = a + (b - a) * row as i32 / denom;
            color |= (v as u32 & 0xFF) << shift;
        }
        let inset = rounded_inset(row, h, r);
        let inner = w.saturating_sub(2 * inset);
        if inner >= 2 {
            fill_span_clipped(surface, x + inset, y + row, inner, color);
        }
    }
}

/// Blends one 0x00RRGGBB color over another with the given foreground
/// opacity (0-255), all in integer math (the kernel never touches the
/// FPU). Used for the Win11-style "acrylic" translucent surfaces.
pub fn blend(fg: u32, bg: u32, alpha: u32) -> u32 {
    if alpha >= 255 {
        return fg;
    }
    if alpha == 0 {
        return bg;
    }
    let inv = 255 - alpha;
    let mut out = 0u32;
    for shift in [16u32, 8, 0] {
        let f = (fg >> shift) & 0xFF;
        let b = (bg >> shift) & 0xFF;
        let v = (f * alpha + b * inv) / 255;
        out |= (v & 0xFF) << shift;
    }
    out
}

/// `fill_rect_blend`'s row loop for a single scanline, clipped to the surface.
fn blend_span_clipped(surface: &mut dyn Surface, x: u32, y: u32, w: u32, color: u32, alpha: u32) {
    let x_end = (x + w).min(surface.width());
    if y >= surface.height() || x_end <= x {
        return;
    }
    surface.blend_span(x, y, x_end - x, color, alpha);
}

/// `fill_rect` with translucency: blends `color` over whatever is
/// already on the surface (which must implement `get_pixel` for the
/// blend to do anything; otherwise it degrades to a solid fill).
pub fn fill_rect_blend(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, color: u32, alpha: u32) {
    if alpha >= 255 {
        return fill_rect(surface, x, y, w, h, color);
    }
    if alpha == 0 {
        return;
    }
    let y_end = (y + h).min(surface.height());
    if y_end <= y {
        return;
    }
    for yy in y..y_end {
        blend_span_clipped(surface, x, yy, w, color, alpha);
    }
}

/// `fill_rounded_rect` with translucency (see `fill_rect_blend`).
pub fn fill_rounded_rect_blend(
    surface: &mut dyn Surface,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    r: u32,
    color: u32,
    alpha: u32,
) {
    if h == 0 {
        return;
    }
    for row in 0..h {
        let inset = rounded_inset(row, h, r);
        let inner = w.saturating_sub(2 * inset);
        if inner >= 2 {
            blend_span_clipped(surface, x + inset, y + row, inner, color, alpha);
        }
    }
}

/// Softens whatever is already composited under a translucent panel
/// (taskbar, launcher, context menu) so the panel's alpha tint reads as
/// frosted glass instead of a flat dark rectangle over a sharp
/// backdrop. Cheap approximation rather than a true per-pixel blur:
/// reads the backdrop back at a coarse stride (`DOWNSAMPLE`), box-blurs
/// that small buffer, then bilinear-upsamples it back to full size with
/// `scale_pixels` (the same scaler the desktop compositor already uses
/// for wallpaper/present scaling) — a full-resolution box blur over,
/// say, the whole taskbar width was measurably heavier, and this panel
/// redraws on most desktop events (mouse move, blink, clock tick), so
/// it has to stay cheap. Call this immediately before the panel's own
/// `fill_rect_blend`/`fill_rounded_rect_blend` tint pass, over the same
/// footprint. Requires the surface's `get_pixel` to return real data
/// (the desktop back buffer does); on a surface that can't read back,
/// this is a no-op since there's nothing to blur.
pub fn blur_backdrop(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, radius: u32) {
    if radius == 0 {
        return;
    }
    let x_end = (x + w).min(surface.width());
    let y_end = (y + h).min(surface.height());
    if x_end <= x || y_end <= y {
        return;
    }
    let w = x_end - x;
    let h = y_end - y;
    if w < 2 || h < 2 {
        return;
    }

    const DOWNSAMPLE: u32 = 4;
    let dw = (w / DOWNSAMPLE).max(1);
    let dh = (h / DOWNSAMPLE).max(1);
    let mut small = vec![0u32; (dw * dh) as usize];
    for sy in 0..dh {
        let src_y = y + (sy * h) / dh;
        for sx in 0..dw {
            let src_x = x + (sx * w) / dw;
            small[(sy * dw + sx) as usize] = surface.get_pixel(src_x, src_y).unwrap_or(0);
        }
    }
    let radius_small = radius.div_ceil(DOWNSAMPLE).max(1);
    let mut tmp = vec![0u32; (dw * dh) as usize];
    box_blur_pass(&small, &mut tmp, dw, dh, radius_small, true);
    let mut blurred = vec![0u32; (dw * dh) as usize];
    box_blur_pass(&tmp, &mut blurred, dw, dh, radius_small, false);

    let mut out = vec![0u32; (w * h) as usize];
    scale_pixels(|sx, sy| blurred[(sy * dw + sx) as usize], dw, dh, &mut out, w, h, None);
    for row in 0..h {
        for col in 0..w {
            surface.put_pixel(x + col, y + row, out[(row * w + col) as usize]);
        }
    }
}

/// One pass of `blur_backdrop`'s separable box blur: averages a
/// `2*radius+1`-wide window of neighbors along one axis (rows when
/// `horizontal`, columns otherwise), clamped at the buffer edges.
fn box_blur_pass(src: &[u32], dst: &mut [u32], w: u32, h: u32, radius: u32, horizontal: bool) {
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    for o in 0..outer {
        for i in 0..inner {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius).min(inner - 1);
            let mut sum = [0u32; 3];
            for j in lo..=hi {
                let (row, col) = if horizontal { (o, j) } else { (j, o) };
                let p = src[(row * w + col) as usize];
                sum[0] += (p >> 16) & 0xFF;
                sum[1] += (p >> 8) & 0xFF;
                sum[2] += p & 0xFF;
            }
            let n = hi - lo + 1;
            let (row, col) = if horizontal { (o, i) } else { (i, o) };
            dst[(row * w + col) as usize] = ((sum[0] / n) << 16) | ((sum[1] / n) << 8) | (sum[2] / n);
        }
    }
}

/// For a rounded rectangle with top-left (`rx`, `ry`), size `w`x`h`,
/// corner radius `r`, returns the closed x-range `[start, end)` that
/// the rectangle covers on scanline `py` (empty when the scanline
/// misses the rect entirely). Computing this once per row instead of
/// testing every pixel keeps the window-shadow pass cheap.
fn rounded_row_range(py: u32, rx: u32, ry: u32, w: u32, h: u32, r: u32) -> (u32, u32) {
    if py < ry || py >= ry + h {
        return (0, 0);
    }
    let inset = rounded_inset(py - ry, h, r);
    let inner = w.saturating_sub(2 * inset);
    if inner < 2 {
        return (0, 0);
    }
    (rx + inset, rx + inset + inner)
}

/// The classic three-layer window drop shadow, drawn in one pass.
///
/// `draw_window` used to emit three nested `fill_rounded_rect_blend`s
/// (offsets 7/5/3, radii r+2/r+1/r, alphas 70/50/30). Because every
/// layer is pure black, blending is just darkening: a pixel covered by
/// `k` layers ends up multiplied by `prod((255 - alpha) / 255)` over
/// the covering layers — no per-layer channel math, and pixels under
/// the window body (painted over afterwards anyway) are skipped
/// entirely. Same visuals, a fraction of the work.
///
/// `(x, y)` is the window's top-left, `w`x`h` its body size. The
/// window's own opaque border rect (`x-1, y-1, w+2, h+2, r+1`) is
/// drawn after this and is skipped here since it fully covers whatever
/// is beneath it.
pub fn draw_window_shadow(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, r: u32) {
    let x0 = x + 3;
    let y0 = y + 3;
    let x1 = x + w + 9;
    let y1 = y + h + 9;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // The window's border rect, drawn after the shadow, covers its own
    // area; the shadow is only visible outside it.
    let bx = x.saturating_sub(1);
    let by = y.saturating_sub(1);
    let bw = w + 2;
    let bh = h + 2;
    let br = r + 1;
    let layers = [
        (7u32, r + 2, 70u32),
        (5, r + 1, 50),
        (3, r, 30),
    ];
    for py in y0..y1 {
        if py >= surface.height() {
            break;
        }
        // Precompute this scanline's coverage ranges once per row: the
        // window's border (skip) and each shadow layer (darken), then
        // the pixel loop is pure integer range checks.
        let (b_start, b_end) = rounded_row_range(py, bx, by, bw, bh, br);
        let mut layer_ranges = [(0u32, 0u32); 3];
        for (i, (offset, radius, _)) in layers.iter().enumerate() {
            layer_ranges[i] = rounded_row_range(py, x + offset, y + offset, w + 2, h + 2, *radius);
        }
        for px in x0..x1 {
            if px >= surface.width() {
                break;
            }
            if px >= b_start && px < b_end {
                continue;
            }
            // Darken by each covering layer's (255 - alpha) / 255.
            let mut scale = 255u32;
            for (i, (_, _, alpha)) in layers.iter().enumerate() {
                let (ls, le) = layer_ranges[i];
                if px >= ls && px < le {
                    scale = scale * (255 - alpha) / 255;
                }
            }
            if scale == 255 {
                continue;
            }
            let Some(base) = surface.get_pixel(px, py) else {
                continue;
            };
            let mut out = 0u32;
            for shift in [16u32, 8, 0] {
                let v = (((base >> shift) & 0xFF) * scale + 127) / 255;
                out |= (v & 0xFF) << shift;
            }
            surface.put_pixel(px, py, out);
        }
    }
}

pub fn draw_char(surface: &mut dyn Surface, x: u32, y: u32, ch: u8, fg: u32, bg: Option<u32>) {
    let glyph = font::glyph(ch);
    let s = font::scale() as u32;
    if s == 1 {
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..font::GLYPH_WIDTH {
                let set = bits & (1 << col) != 0;
                if set {
                    surface.put_pixel(x + col as u32, y + row as u32, fg);
                } else if let Some(bg) = bg {
                    surface.put_pixel(x + col as u32, y + row as u32, bg);
                }
            }
        }
    } else {
        // Scale the 8x8 glyph up by filling s x s blocks per set pixel.
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..font::GLYPH_WIDTH {
                let set = bits & (1 << col) != 0;
                if set {
                    fill_rect(surface, x + col as u32 * s, y + row as u32 * s, s, s, fg);
                } else if let Some(bg) = bg {
                    fill_rect(surface, x + col as u32 * s, y + row as u32 * s, s, s, bg);
                }
            }
        }
    }
}

/// Draws `c` (ASCII or Japanese) at (`x`, `y`); returns the pixel
/// advance. See `font::draw_cp`.
pub fn draw_cp(surface: &mut dyn Surface, x: u32, y: u32, c: char, fg: u32, bg: Option<u32>) -> u32 {
    font::draw_cp(surface, x, y, c, fg, bg)
}

pub fn draw_string(surface: &mut dyn Surface, x: u32, y: u32, s: &str, fg: u32, bg: Option<u32>) {
    let mut cursor_x = x;
    for c in s.chars() {
        cursor_x += font::draw_cp(surface, cursor_x, y, c, fg, bg);
    }
}

/// Pixel width of `s` at the current font scale. Char-aware: Japanese
/// glyphs count double-width, so label sizing matches `draw_string`.
pub fn text_width(s: &str) -> u32 {
    s.chars().map(font::char_width).sum()
}

/// Copies a `src_w`x`src_h` pixel buffer onto `surface` at (`dst_x`, `dst_y`).
pub fn blit(surface: &mut dyn Surface, dst_x: u32, dst_y: u32, src: &[u32], src_w: u32, src_h: u32) {
    let x_end = (dst_x + src_w).min(surface.width());
    let y_end = (dst_y + src_h).min(surface.height());
    if x_end <= dst_x || y_end <= dst_y {
        return;
    }
    let span = x_end - dst_x;
    for y in 0..(y_end - dst_y) {
        surface.blit_span(dst_x, dst_y + y, src, (y * src_w) as usize, span);
    }
}

/// `blit` with the four corners clipped to a circular arc of radius `r`,
/// so content pasted into a rounded window frame doesn't paint over the
/// frame's corner pixels (those stay the window background instead).
pub fn blit_rounded(
    surface: &mut dyn Surface,
    dst_x: u32,
    dst_y: u32,
    src: &[u32],
    src_w: u32,
    src_h: u32,
    r: u32,
) {
    for row in 0..src_h {
        let inset = rounded_inset(row, src_h, r);
        let inner = src_w.saturating_sub(2 * inset);
        if inner == 0 {
            continue;
        }
        surface.blit_span(
            dst_x + inset,
            dst_y + row,
            src,
            (row as usize * src_w as usize) + inset as usize,
            inner,
        );
    }
}

/// Scales a `src_w`x`src_h` pixel buffer to `dst_w`x`dst_h` with bilinear
/// interpolation in pure fixed-point integer math (the kernel doesn't
/// use the FPU). Used to downscale the virtual desktop's back buffer to
/// the physical framebuffer and to fit the wallpaper to the current
/// virtual resolution.
pub fn scale_image(src: &[u32], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u32> {
    let mut dst = vec![0u32; (dst_w * dst_h) as usize];
    scale_into(src, src_w, src_h, &mut dst, dst_w, dst_h);
    dst
}

/// `scale_image` writing into a preallocated `dst_w`x`dst_h` buffer
/// (avoiding an allocation on the hot composite path).
pub fn scale_into(src: &[u32], src_w: u32, src_h: u32, dst: &mut [u32], dst_w: u32, dst_h: u32) {
    scale_pixels(
        |x, y| src[(y * src_w + x) as usize],
        src_w,
        src_h,
        dst,
        dst_w,
        dst_h,
        None,
    );
}

/// `scale_into` with caller-owned row scratch buffers, so the bilinear
/// filter's two working rows aren't reallocated on every frame (the
/// present path calls this once per composite at non-native
/// resolutions).
pub fn scale_into_scratch(
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    scratch: &mut [u32],
) {
    let (top_row, bot_row) = scratch.split_at_mut(dst_w as usize);
    scale_pixels(
        |x, y| src[(y * src_w + x) as usize],
        src_w,
        src_h,
        dst,
        dst_w,
        dst_h,
        Some((top_row, bot_row)),
    );
}

/// Scales raw little-endian 0x00RRGGBB pixel *bytes* (the wallpaper.raw
/// on-disk format) straight into a `dst_w`x`dst_h` buffer of u32 pixels,
/// without materializing the full-size source as a `Vec<u32>` first —
/// that intermediate copy is what pushes the 40 MiB kernel heap over
/// budget at non-native resolutions.
pub fn scale_bytes(
    bytes: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u32> {
    let mut dst = vec![0u32; (dst_w * dst_h) as usize];
    scale_pixels(
        |x, y| {
            let idx = (y * src_w + x) as usize * 4;
            u32::from_le_bytes([bytes[idx], bytes[idx + 1], bytes[idx + 2], bytes[idx + 3]])
        },
        src_w,
        src_h,
        &mut dst,
        dst_w,
        dst_h,
        None,
    );
    dst
}

/// Bilinear fixed-point scaling with pluggable source pixel access.
/// When `scratch` is `Some((top, bot))` those two `dst_w`-sized buffers
/// are used as the row caches instead of allocating fresh ones (see
/// `scale_into_scratch`).
fn scale_pixels<F: Fn(u32, u32) -> u32>(
    get: F,
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
    scratch: Option<(&mut [u32], &mut [u32])>,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    // Fixed-point (16.16) mapping of destination pixels to source
    // coordinates, matching the visual centers so the image isn't skewed.
    let xmap = |dx: u32| ((dx as u64) << 16) * (src_w as u64 - 1) / (dst_w as u64 - 1);
    let ymap = |dy: u32| ((dy as u64) << 16) * (src_h as u64 - 1) / (dst_h as u64 - 1);

    let lerp = |a: u32, b: u32, f: u32| {
        let mut out = 0u32;
        for shift in [16u32, 8, 0] {
            let av = (a >> shift) & 0xFF;
            let bv = (b >> shift) & 0xFF;
            let v = av as i32 + ((bv as i32 - av as i32) * f as i32 >> 16);
            out |= (v.clamp(0, 255) as u32 & 0xFF) << shift;
        }
        out
    };

    // Caller-provided scratch rows when available; otherwise keep the
    // freshly allocated ones alive for the duration of the loop.
    let mut owned_top: Option<Vec<u32>> = None;
    let mut owned_bot: Option<Vec<u32>> = None;
    let (top_row, bot_row): (&mut [u32], &mut [u32]) = match scratch {
        Some((top, bot)) => (top, bot),
        None => {
            owned_top = Some(alloc::vec![0u32; dst_w as usize]);
            owned_bot = Some(alloc::vec![0u32; dst_w as usize]);
            (
                owned_top.as_mut().unwrap().as_mut_slice(),
                owned_bot.as_mut().unwrap().as_mut_slice(),
            )
        }
    };
    let top_row = &mut top_row[..dst_w as usize];
    let bot_row = &mut bot_row[..dst_w as usize];

    let mut prev_y: Option<(u32, u32)> = None; // (y0, y1) of the last row
    for dy in 0..dst_h {
        let sy = ymap(dy);
        let y0 = (sy >> 16) as usize;
        let y1 = (y0 + 1).min(src_h as usize - 1);
        let fy = (sy & 0xFFFF) as u32;
        if prev_y != Some((y0 as u32, y1 as u32)) {
            prev_y = Some((y0 as u32, y1 as u32));
            for dx in 0..dst_w {
                let sx = xmap(dx);
                let x0 = (sx >> 16) as usize;
                let x1 = (x0 + 1).min(src_w as usize - 1);
                let fx = (sx & 0xFFFF) as u32;
                let p00 = get(x0 as u32, y0 as u32);
                let p10 = get(x1 as u32, y0 as u32);
                let p01 = get(x0 as u32, y1 as u32);
                let p11 = get(x1 as u32, y1 as u32);
                top_row[dx as usize] = lerp(p00, p10, fx);
                bot_row[dx as usize] = lerp(p01, p11, fx);
            }
        }
        let row = &mut dst[(dy as usize) * dst_w as usize..(dy as usize + 1) * dst_w as usize];
        for dx in 0..dst_w as usize {
            row[dx] = lerp(top_row[dx], bot_row[dx], fy);
        }
    }
}
