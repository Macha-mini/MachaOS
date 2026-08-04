//! A small four-function calculator app for the desktop. Deliberately
//! integer-only (`i64`): `task.rs`'s `context_switch` and the generated
//! ISR stubs only save general-purpose registers, not XMM/x87 state, so
//! introducing floating point here would plant a latent bug for whenever
//! this app's arithmetic runs concurrently with another task that also
//! touches the FPU. Integers sidestep the problem entirely.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::font;
use crate::gfx::{self, Surface};

const BUTTON_SIZE: u32 = 60;
const GRID_COLS: u32 = 4;
const GRID_ROWS: u32 = 4;
const DISPLAY_HEIGHT: u32 = 60;

const WIDTH: u32 = BUTTON_SIZE * GRID_COLS;
const HEIGHT: u32 = DISPLAY_HEIGHT + BUTTON_SIZE * GRID_ROWS;

const BG: u32 = 0x00_202020;
const BUTTON_BG: u32 = 0x00_333333;
const BUTTON_FG: u32 = 0x00_FFFFFF;
const DISPLAY_BG: u32 = 0x00_10161C;
const DISPLAY_FG: u32 = 0x00_9FCB6B;
const ERROR_FG: u32 = 0x00_E06060;

const BUTTONS: [(&str, u32, u32); 16] = [
    ("7", 0, 0), ("8", 1, 0), ("9", 2, 0), ("/", 3, 0),
    ("4", 0, 1), ("5", 1, 1), ("6", 2, 1), ("*", 3, 1),
    ("1", 0, 2), ("2", 1, 2), ("3", 2, 2), ("-", 3, 2),
    ("0", 0, 3), ("C", 1, 3), ("=", 2, 3), ("+", 3, 3),
];

const MAX_DIGITS: usize = 18;

pub struct CalculatorApp {
    buffer: Vec<u32>,
    display: String,
    accumulator: i64,
    pending_op: Option<u8>,
    fresh_entry: bool,
    error: bool,
}

impl CalculatorApp {
    pub fn new() -> Self {
        let mut app = Self {
            buffer: vec![BG; (WIDTH * HEIGHT) as usize],
            display: "0".into(),
            accumulator: 0,
            pending_op: None,
            fresh_entry: false,
            error: false,
        };
        app.render();
        app
    }

    pub fn width(&self) -> u32 {
        WIDTH
    }

    pub fn height(&self) -> u32 {
        HEIGHT
    }

    pub fn pixels(&self) -> &[u32] {
        &self.buffer
    }

    pub fn handle_click(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || y < DISPLAY_HEIGHT as i32 {
            return; // clicking the display itself does nothing
        }
        let col = x as u32 / BUTTON_SIZE;
        let row = (y as u32 - DISPLAY_HEIGHT) / BUTTON_SIZE;
        let Some((label, ..)) = BUTTONS.iter().find(|&&(_, c, r)| c == col && r == row) else {
            return;
        };
        self.press(label);
        self.render();
    }

    fn press(&mut self, label: &str) {
        match label {
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                if self.error || self.fresh_entry || self.display == "0" {
                    self.display.clear();
                    self.fresh_entry = false;
                    self.error = false;
                }
                if self.display.len() < MAX_DIGITS {
                    self.display.push_str(label);
                }
            }
            "C" => {
                self.display = "0".into();
                self.accumulator = 0;
                self.pending_op = None;
                self.fresh_entry = false;
                self.error = false;
            }
            "+" | "-" | "*" | "/" => {
                if !self.error {
                    self.apply_pending();
                    self.pending_op = Some(label.as_bytes()[0]);
                    self.fresh_entry = true;
                }
            }
            "=" => {
                if !self.error {
                    self.apply_pending();
                    self.pending_op = None;
                    self.fresh_entry = true;
                }
            }
            _ => {}
        }
    }

    fn apply_pending(&mut self) {
        let operand: i64 = self.display.parse().unwrap_or(0);
        match self.pending_op {
            None => self.accumulator = operand,
            Some(op) => match op {
                b'+' => self.accumulator = self.accumulator.wrapping_add(operand),
                b'-' => self.accumulator = self.accumulator.wrapping_sub(operand),
                b'*' => self.accumulator = self.accumulator.wrapping_mul(operand),
                b'/' => {
                    if operand == 0 {
                        self.error = true;
                        self.display = "Error".into();
                        return;
                    }
                    self.accumulator /= operand;
                }
                _ => {}
            },
        }
        self.display = self.accumulator.to_string();
    }

    fn render(&mut self) {
        gfx::fill_rect(self, 0, 0, WIDTH, HEIGHT, BG);
        gfx::fill_rect(self, 0, 0, WIDTH, DISPLAY_HEIGHT, DISPLAY_BG);
        let fg = if self.error { ERROR_FG } else { DISPLAY_FG };
        let text_w = (self.display.len() * font::GLYPH_WIDTH) as u32;
        let x = WIDTH.saturating_sub(text_w + 10);
        let y = (DISPLAY_HEIGHT - font::GLYPH_HEIGHT as u32) / 2;
        let display = self.display.clone();
        gfx::draw_string(self, x, y, &display, fg, None);

        for (label, col, row) in BUTTONS {
            let bx = col * BUTTON_SIZE;
            let by = DISPLAY_HEIGHT + row * BUTTON_SIZE;
            gfx::fill_rect(self, bx + 2, by + 2, BUTTON_SIZE - 4, BUTTON_SIZE - 4, BUTTON_BG);
            let lx = bx + (BUTTON_SIZE - font::GLYPH_WIDTH as u32) / 2;
            let ly = by + (BUTTON_SIZE - font::GLYPH_HEIGHT as u32) / 2;
            gfx::draw_string(self, lx, ly, label, BUTTON_FG, None);
        }
    }
}

impl Surface for CalculatorApp {
    fn width(&self) -> u32 {
        WIDTH
    }

    fn height(&self) -> u32 {
        HEIGHT
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < WIDTH && y < HEIGHT {
            let idx = (y * WIDTH + x) as usize;
            self.buffer[idx] = color;
        }
    }
}
