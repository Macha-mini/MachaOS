//! Surface-agnostic drawing primitives (rectangles, glyphs, strings).
//! Anything that can plot a pixel (the real framebuffer's back buffer, a
//! window's private pixel buffer, ...) can implement `Surface` and reuse
//! these routines.

use crate::font;

pub trait Surface {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn put_pixel(&mut self, x: u32, y: u32, color: u32);
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

pub fn draw_char(surface: &mut dyn Surface, x: u32, y: u32, ch: u8, fg: u32, bg: Option<u32>) {
    let glyph = font::glyph(ch);
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
}

pub fn draw_string(surface: &mut dyn Surface, x: u32, y: u32, s: &str, fg: u32, bg: Option<u32>) {
    let mut cursor_x = x;
    for byte in s.bytes() {
        draw_char(surface, cursor_x, y, byte, fg, bg);
        cursor_x += font::GLYPH_WIDTH as u32;
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
