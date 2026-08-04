//! prog_calculator: the four-function calculator (see src/calculator.rs)
//! ported to run as a real ring-3 process instead of kernel-resident
//! `AppKind::Calculator` state. Same arithmetic, same button layout,
//! same rendering (via the shared font.rs/gfx.rs copies) — the
//! difference is entirely in how it talks to the window manager: pixels
//! go out through sys_win_update, clicks come in through sys_recv,
//! instead of direct method calls into `wm::WindowManager`.
//!
//! No heap here (the user crate has no allocator), unlike the
//! kernel-resident version's `String`/`Vec` — `Display` is a fixed
//! MAX_DIGITS buffer with hand-rolled decimal formatting/parsing instead
//! of `to_string`/`str::parse`, and the pixel buffer is a static array
//! instead of a `Vec` (also avoids putting 288 KB on a 32 KiB stack).

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;
#[path = "../font.rs"]
mod font;
#[path = "../gfx.rs"]
mod gfx;

use gfx::Surface;

const SYS_RECV: u64 = 5;
const SYS_WIN_CREATE: u64 = 6;
const SYS_WIN_UPDATE: u64 = 7;

const BUTTON_SIZE: u32 = 60;
const GRID_COLS: u32 = 4;
const GRID_ROWS: u32 = 4;
const DISPLAY_HEIGHT: u32 = 60;

const WIDTH: u32 = BUTTON_SIZE * GRID_COLS;
const HEIGHT: u32 = DISPLAY_HEIGHT + BUTTON_SIZE * GRID_ROWS;
const PIXEL_COUNT: usize = (WIDTH * HEIGHT) as usize;

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

static mut PIXELS: [u32; PIXEL_COUNT] = [0; PIXEL_COUNT];

/// A fixed-capacity stand-in for the kernel version's `String` display.
struct Display {
    buf: [u8; MAX_DIGITS],
    len: usize,
}

impl Display {
    fn set(&mut self, s: &[u8]) {
        self.len = s.len().min(MAX_DIGITS);
        self.buf[..self.len].copy_from_slice(&s[..self.len]);
    }

    fn set_i64(&mut self, value: i64) {
        self.len = write_i64(&mut self.buf, value);
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn push_digit(&mut self, b: u8) {
        if self.len < MAX_DIGITS {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn is(&self, s: &str) -> bool {
        self.as_bytes() == s.as_bytes()
    }

    fn parse(&self) -> i64 {
        let bytes = self.as_bytes();
        let (neg, digits) = match bytes.first() {
            Some(b'-') => (true, &bytes[1..]),
            _ => (false, bytes),
        };
        let mut v: i64 = 0;
        for &b in digits {
            if !b.is_ascii_digit() {
                return 0; // matches the kernel version's `.parse().unwrap_or(0)`
            }
            v = v.wrapping_mul(10).wrapping_add((b - b'0') as i64);
        }
        if neg { -v } else { v }
    }
}

/// Formats `value` as decimal (with a leading `-` for negatives) into
/// `buf`, returning the length written. `i64::MIN` is handled via
/// `unsigned_abs` so negating it can't overflow.
fn write_i64(buf: &mut [u8; MAX_DIGITS], value: i64) -> usize {
    if value == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; MAX_DIGITS];
    let mut count = 0;
    let mut v = value.unsigned_abs();
    while v > 0 {
        digits[count] = b'0' + (v % 10) as u8;
        v /= 10;
        count += 1;
    }
    let mut len = 0;
    if value < 0 {
        buf[0] = b'-';
        len = 1;
    }
    for i in 0..count {
        buf[len] = digits[count - 1 - i];
        len += 1;
    }
    len
}

struct CalculatorApp {
    pixels: &'static mut [u32; PIXEL_COUNT],
    display: Display,
    accumulator: i64,
    pending_op: Option<u8>,
    fresh_entry: bool,
    error: bool,
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
            self.pixels[(y * WIDTH + x) as usize] = color;
        }
    }
}

impl CalculatorApp {
    fn handle_click(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || y < DISPLAY_HEIGHT as i32 {
            return;
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
                if self.error || self.fresh_entry || self.display.is("0") {
                    self.display.len = 0;
                    self.fresh_entry = false;
                    self.error = false;
                }
                self.display.push_digit(label.as_bytes()[0]);
            }
            "C" => {
                self.display.set(b"0");
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
        let operand = self.display.parse();
        match self.pending_op {
            None => self.accumulator = operand,
            Some(op) => match op {
                b'+' => self.accumulator = self.accumulator.wrapping_add(operand),
                b'-' => self.accumulator = self.accumulator.wrapping_sub(operand),
                b'*' => self.accumulator = self.accumulator.wrapping_mul(operand),
                b'/' => {
                    if operand == 0 {
                        self.error = true;
                        self.display.set(b"Error");
                        return;
                    }
                    self.accumulator /= operand;
                }
                _ => {}
            },
        }
        self.display.set_i64(self.accumulator);
    }

    fn render(&mut self) {
        gfx::fill_rect(self, 0, 0, WIDTH, HEIGHT, BG);
        gfx::fill_rect(self, 0, 0, WIDTH, DISPLAY_HEIGHT, DISPLAY_BG);
        let fg = if self.error { ERROR_FG } else { DISPLAY_FG };
        // Copied out first: `self.display.as_str()` borrows `self`
        // immutably, which can't coexist with the `&mut self` `gfx`
        // (drawing through `self` as the `Surface`) needs.
        let display_buf = self.display.buf;
        let display_len = self.display.len;
        let display_str = core::str::from_utf8(&display_buf[..display_len]).unwrap_or("0");
        let text_w = (display_len * font::GLYPH_WIDTH) as u32;
        let x = WIDTH.saturating_sub(text_w + 10);
        let y = (DISPLAY_HEIGHT - font::GLYPH_HEIGHT as u32) / 2;
        gfx::draw_string(self, x, y, display_str, fg, None);

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

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let title = b"Calculator";
    unsafe {
        common::syscall(SYS_WIN_CREATE, WIDTH as u64, HEIGHT as u64, title.as_ptr() as u64, title.len() as u64);
    }

    let mut app = CalculatorApp {
        pixels: unsafe { &mut *core::ptr::addr_of_mut!(PIXELS) },
        display: Display { buf: [0; MAX_DIGITS], len: 0 },
        accumulator: 0,
        pending_op: None,
        fresh_entry: false,
        error: false,
    };
    app.display.set(b"0");
    app.render();
    push_frame(&app);

    let mut buf = [0u8; 6];
    loop {
        let n = unsafe { common::syscall(SYS_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) };
        if n != buf.len() as u64 || buf[0] != 3 {
            continue; // only tag 3 (Click) is meaningful here; see wm::encode_click_event
        }
        let x = i32::from(u16::from_le_bytes([buf[2], buf[3]]));
        let y = i32::from(u16::from_le_bytes([buf[4], buf[5]]));
        app.handle_click(x, y);
        push_frame(&app);
    }
}

fn push_frame(app: &CalculatorApp) {
    unsafe {
        common::syscall(SYS_WIN_UPDATE, app.pixels.as_ptr() as u64, PIXEL_COUNT as u64, 0, 0);
    }
}
