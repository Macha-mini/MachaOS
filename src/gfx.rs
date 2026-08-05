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
}

pub fn fill_rect(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let x_end = (x + w).min(surface.width());
    let y_end = (y + h).min(surface.height());
    for yy in y..y_end {
        for xx in x..x_end {
            surface.put_pixel(xx, yy, color);
        }
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
        fill_rect(surface, x, y + row, w, 1, color);
    }
}

/// Horizontal inset for a given scanline of a rectangle with rounded
/// corners of radius `r`: `0` in the middle rows, growing toward the
/// corners so the painted width narrows to an arc there.
fn rounded_inset(row: u32, h: u32, r: u32) -> u32 {
    if r == 0 {
        0
    } else if row < r {
        r - 1 - row
    } else if row >= h - r {
        row - (h - r)
    } else {
        0
    }
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
            fill_rect(surface, x + inset, y + row, inner, 1, color);
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
            fill_rect(surface, x + inset, y + row, inner, 1, color);
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

/// `fill_rect` with translucency: blends `color` over whatever is
/// already on the surface (which must implement `get_pixel` for the
/// blend to do anything; otherwise it degrades to a solid fill).
pub fn fill_rect_blend(surface: &mut dyn Surface, x: u32, y: u32, w: u32, h: u32, color: u32, alpha: u32) {
    if alpha >= 255 {
        return fill_rect(surface, x, y, w, h, color);
    }
    let x_end = (x + w).min(surface.width());
    let y_end = (y + h).min(surface.height());
    for yy in y..y_end {
        for xx in x..x_end {
            if let Some(base) = surface.get_pixel(xx, yy) {
                surface.put_pixel(xx, yy, blend(color, base, alpha));
            } else {
                surface.put_pixel(xx, yy, color);
            }
        }
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
            fill_rect_blend(surface, x + inset, y + row, inner, 1, color, alpha);
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

pub fn draw_string(surface: &mut dyn Surface, x: u32, y: u32, s: &str, fg: u32, bg: Option<u32>) {
    let mut cursor_x = x;
    for byte in s.bytes() {
        draw_char(surface, cursor_x, y, byte, fg, bg);
        cursor_x += font::glyph_w() as u32;
    }
}

/// Copies a `src_w`x`src_h` pixel buffer onto `surface` at (`dst_x`, `dst_y`).
pub fn blit(surface: &mut dyn Surface, dst_x: u32, dst_y: u32, src: &[u32], src_w: u32, src_h: u32) {
    for y in 0..src_h {
        for x in 0..src_w {
            let color = src[(y * src_w + x) as usize];
            surface.put_pixel(dst_x + x, dst_y + y, color);
        }
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
        let src_row = &src[row as usize * src_w as usize..(row as usize + 1) * src_w as usize];
        for col in inset..inset + inner {
            surface.put_pixel(dst_x + col, dst_y + row, src_row[col as usize]);
        }
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
    scale_pixels(|x, y| src[(y * src_w + x) as usize], src_w, src_h, dst, dst_w, dst_h);
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
    );
    dst
}

/// Bilinear fixed-point scaling with pluggable source pixel access.
fn scale_pixels<F: Fn(u32, u32) -> u32>(
    get: F,
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    dst_h: u32,
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

    let mut prev_y: Option<(u32, u32)> = None; // (y0, y1) of the last row
    let mut top_row = vec![0u32; dst_w as usize];
    let mut bot_row = vec![0u32; dst_w as usize];

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
