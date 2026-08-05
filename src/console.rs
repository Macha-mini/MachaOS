//! A text-cell grid virtual console, the graphics-mode analogue of
//! `vga::Writer`. Instead of writing `ScreenChar`s into the fixed 0xB8000
//! VGA buffer, it blits glyphs (via `gfx::draw_char`) into its own pixel
//! buffer, which a `wm::Window` owns and the compositor blits onward.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::font;
use crate::gfx::{self, Surface};

pub struct Console {
    cols: usize,
    rows: usize,
    row: usize,
    col: usize,
    fg: u32,
    bg: u32,
    buffer: Vec<u32>,
    // UTF-8 sequence being accumulated by `write_byte` (multi-byte
    // Japanese chars arrive as separate byte writes from the shell).
    pending: [u8; 3],
    pending_len: u8,
    // Cells (1 or 2) occupied by the most recently written char; the
    // backspace handler needs it to know how far to step back.
    last_width: u8,
}

impl Console {
    pub fn new(cols: usize, rows: usize, fg: u32, bg: u32) -> Self {
        let width = cols * font::glyph_w();
        let height = rows * font::glyph_h();
        Self {
            cols,
            rows,
            row: 0,
            col: 0,
            fg,
            bg,
            buffer: vec![bg; width * height],
            pending: [0u8; 3],
            pending_len: 0,
            last_width: 1,
        }
    }

    pub fn width_px(&self) -> u32 {
        (self.cols * font::glyph_w()) as u32
    }

    pub fn height_px(&self) -> u32 {
        (self.rows * font::glyph_h()) as u32
    }

    pub fn pixels(&self) -> &[u32] {
        &self.buffer
    }

    pub fn clear(&mut self) {
        self.buffer.fill(self.bg);
        self.row = 0;
        self.col = 0;
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Reallocates the pixel buffer for a new cell grid size. Contents are
    /// not preserved (this is a scrolling teletype, not a document with a
    /// separate text model) — callers that own their own source of truth
    /// (a line buffer, an in-progress prompt string, ...) should re-print
    /// it after calling this.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.row = 0;
        self.col = 0;
        let width = cols * font::glyph_w();
        let height = rows * font::glyph_h();
        self.buffer = vec![self.bg; width * height];
    }

    /// Writes one character (ASCII or Japanese) at the current cursor,
    /// handling newline/CR/backspace/tab, cell wrapping for wide glyphs,
    /// and scrolling. The console is a text-cell grid: an 8x8 ASCII
    /// glyph fills one cell, a 16x16 Japanese glyph fills two.
    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => {
                self.row += 1;
                self.col = 0;
                if self.row >= self.rows {
                    self.scroll();
                }
            }
            '\r' => self.col = 0,
            '\u{8}' => {
                let w = self.last_width as usize;
                if self.col >= w {
                    self.col -= w;
                } else if self.row > 0 {
                    // Undo the line wrap: back up onto the end of the
                    // previous row rather than getting stuck at col 0.
                    self.row -= 1;
                    self.col = self.cols.saturating_sub(w);
                } else {
                    return;
                }
                for i in 0..w {
                    self.draw_cell(self.col + i, self.row, b' ');
                }
            }
            '\t' => {
                let n = 4 - (self.col % 4);
                for _ in 0..n {
                    self.write_char(' ');
                }
            }
            c => {
                let cells = if c.is_ascii() { 1 } else { 2 };
                if self.col + cells > self.cols {
                    self.row += 1;
                    self.col = 0;
                    if self.row >= self.rows {
                        self.scroll();
                    }
                }
                if c.is_ascii() {
                    self.draw_cell(self.col, self.row, c as u8);
                } else {
                    let x = (self.col * font::glyph_w()) as u32;
                    let y = (self.row * font::glyph_h()) as u32;
                    let fg = self.fg;
                    let bg = self.bg;
                    gfx::draw_cp(self, x, y, c, fg, Some(bg));
                }
                self.col += cells;
                self.last_width = cells as u8;
            }
        }
    }

    /// Writes one byte, transparently reassembling multi-byte UTF-8
    /// sequences (the shell feeds `print!` output one byte at a time
    /// through this path).
    pub fn write_byte(&mut self, byte: u8) {
        if byte < 0x80 {
            self.write_char(byte as char);
            return;
        }
        if byte & 0xC0 == 0x80 && self.pending_len > 0 {
            // Continuation byte of an in-progress sequence.
            self.pending[self.pending_len as usize] = byte;
            self.pending_len += 1;
            let total = utf8_len(self.pending[0]);
            if self.pending_len as usize == total {
                let cp = decode_utf8(&self.pending, total);
                self.write_char(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                self.pending_len = 0;
            }
        } else if byte & 0xC0 != 0x80 {
            // Lead byte. 4-byte (astral) sequences aren't in the
            // embedded font, so render a placeholder instead.
            self.pending_len = 0;
            let total = utf8_len(byte);
            if total == 2 || total == 3 {
                self.pending[0] = byte;
                self.pending_len = 1;
            } else {
                self.write_char('\u{FFFD}');
            }
        } else {
            // Stray continuation byte.
            self.pending_len = 0;
            self.write_char('\u{FFFD}');
        }
    }

    fn draw_cell(&mut self, col: usize, row: usize, byte: u8) {
        let x = (col * font::glyph_w()) as u32;
        let y = (row * font::glyph_h()) as u32;
        let fg = self.fg;
        let bg = self.bg;
        gfx::draw_char(self, x, y, byte, fg, Some(bg));
    }

    fn scroll(&mut self) {
        let width = self.width_px() as usize;
        let row_height = font::glyph_h();
        let total_rows_px = self.rows * row_height;
        self.buffer.copy_within(width * row_height..width * total_rows_px, 0);
        let last_row_start = width * (total_rows_px - row_height);
        for pixel in &mut self.buffer[last_row_start..] {
            *pixel = self.bg;
        }
        self.row = self.rows - 1;
        self.col = 0;
    }
}

impl Surface for Console {
    fn width(&self) -> u32 {
        self.width_px()
    }

    fn height(&self) -> u32 {
        self.height_px()
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width() && y < self.height() {
            let idx = y as usize * self.width_px() as usize + x as usize;
            self.buffer[idx] = color;
        }
    }

    // Fast paths over the contiguous `buffer` (see `fb::State`); the
    // console's own pixel buffer is the window content the compositor
    // blits every frame.

    fn blit_span(&mut self, x: u32, y: u32, src: &[u32], offset: usize, len: u32) {
        let w = self.width_px() as usize;
        if y as usize >= self.buffer.len() / w.max(1) || x as usize >= w || len == 0 {
            return;
        }
        let n = (len as usize).min(w - x as usize);
        let row_start = y as usize * w + x as usize;
        self.buffer[row_start..row_start + n].copy_from_slice(&src[offset..offset + n]);
    }

    fn fill_span(&mut self, x: u32, y: u32, len: u32, color: u32) {
        let w = self.width_px() as usize;
        if y as usize >= self.buffer.len() / w.max(1) || x as usize >= w || len == 0 {
            return;
        }
        let n = (len as usize).min(w - x as usize);
        let row_start = y as usize * w + x as usize;
        self.buffer[row_start..row_start + n].fill(color);
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

/// Byte length of the UTF-8 sequence a lead byte announces.
fn utf8_len(lead: u8) -> usize {
    if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

/// Decodes a 2- or 3-byte UTF-8 sequence (BMP only — that's all the
/// embedded font covers).
fn decode_utf8(b: &[u8], len: usize) -> u32 {
    match len {
        2 => (((b[0] & 0x1F) as u32) << 6) | ((b[1] & 0x3F) as u32),
        3 => {
            (((b[0] & 0x0F) as u32) << 12)
                | (((b[1] & 0x3F) as u32) << 6)
                | ((b[2] & 0x3F) as u32)
        }
        _ => 0xFFFD,
    }
}
