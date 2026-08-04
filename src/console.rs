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
}

impl Console {
    pub fn new(cols: usize, rows: usize, fg: u32, bg: u32) -> Self {
        let width = cols * font::GLYPH_WIDTH;
        let height = rows * font::GLYPH_HEIGHT;
        Self {
            cols,
            rows,
            row: 0,
            col: 0,
            fg,
            bg,
            buffer: vec![bg; width * height],
        }
    }

    pub fn width_px(&self) -> u32 {
        (self.cols * font::GLYPH_WIDTH) as u32
    }

    pub fn height_px(&self) -> u32 {
        (self.rows * font::GLYPH_HEIGHT) as u32
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
        let width = cols * font::GLYPH_WIDTH;
        let height = rows * font::GLYPH_HEIGHT;
        self.buffer = vec![self.bg; width * height];
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.row += 1;
                self.col = 0;
                if self.row >= self.rows {
                    self.scroll();
                }
            }
            b'\r' => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    // Undo the line wrap: back up onto the end of the
                    // previous row rather than getting stuck at col 0.
                    self.row -= 1;
                    self.col = self.cols - 1;
                } else {
                    return;
                }
                self.draw_cell(self.col, self.row, b' ');
            }
            b'\t' => {
                let n = 4 - (self.col % 4);
                for _ in 0..n {
                    self.write_byte(b' ');
                }
            }
            byte => {
                if self.col >= self.cols {
                    self.row += 1;
                    self.col = 0;
                    if self.row >= self.rows {
                        self.scroll();
                    }
                }
                self.draw_cell(self.col, self.row, byte);
                self.col += 1;
            }
        }
    }

    fn draw_cell(&mut self, col: usize, row: usize, byte: u8) {
        let x = (col * font::GLYPH_WIDTH) as u32;
        let y = (row * font::GLYPH_HEIGHT) as u32;
        let fg = self.fg;
        let bg = self.bg;
        gfx::draw_char(self, x, y, byte, fg, Some(bg));
    }

    fn scroll(&mut self) {
        let width = self.width_px() as usize;
        let row_height = font::GLYPH_HEIGHT;
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
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}
