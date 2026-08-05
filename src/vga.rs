use core::fmt;
use core::fmt::Write as _;

use crate::port;
use crate::sync::{SpinLock, SpinLockGuard};

pub const WIDTH: usize = 80;
pub const HEIGHT: usize = 25;
pub const VGA_ADDRESS: usize = 0xB8000;

pub mod colors {
    pub const BLACK: u8 = 0x00;
    pub const BLUE: u8 = 0x01;
    pub const GREEN: u8 = 0x02;
    pub const CYAN: u8 = 0x03;
    pub const RED: u8 = 0x04;
    pub const MAGENTA: u8 = 0x05;
    pub const BROWN: u8 = 0x06;
    pub const LIGHT_GRAY: u8 = 0x07;
    pub const DARK_GRAY: u8 = 0x08;
    pub const LIGHT_BLUE: u8 = 0x09;
    pub const LIGHT_GREEN: u8 = 0x0A;
    pub const LIGHT_CYAN: u8 = 0x0B;
    pub const LIGHT_RED: u8 = 0x0C;
    pub const LIGHT_MAGENTA: u8 = 0x0D;
    pub const YELLOW: u8 = 0x0E;
    pub const WHITE: u8 = 0x0F;
}

pub const fn color(fg: u8, bg: u8) -> u8 {
    (bg << 4) | fg
}

#[derive(Clone, Copy)]
#[repr(C)]
struct ScreenChar {
    ch: u8,
    color: u8,
}

pub struct Writer {
    row: usize,
    col: usize,
    current_color: u8,
    buffer_ptr: *mut ScreenChar,
    buffer_len: usize,
}

unsafe impl Send for Writer {}

impl Writer {
    pub const fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            current_color: color(colors::LIGHT_GRAY, colors::BLACK),
            buffer_ptr: core::ptr::null_mut(),
            buffer_len: 0,
        }
    }

    fn buffer(&mut self) -> &'static mut [ScreenChar] {
        if self.buffer_len == 0 {
            return &mut [];
        }
        unsafe { core::slice::from_raw_parts_mut(self.buffer_ptr, self.buffer_len) }
    }

    pub fn init_buffer(&mut self) {
        self.buffer_ptr = VGA_ADDRESS as *mut ScreenChar;
        self.buffer_len = WIDTH * HEIGHT;
    }

    pub fn clear_screen(&mut self) {
        let blank = ScreenChar {
            ch: b' ',
            color: self.current_color,
        };
        for cell in self.buffer().iter_mut() {
            *cell = blank;
        }
        self.row = 0;
        self.col = 0;
    }

    pub fn set_color(&mut self, color: u8) {
        self.current_color = color;
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.row += 1;
                self.col = 0;
                if self.row >= HEIGHT {
                    self.scroll();
                }
            }
            b'\r' => self.col = 0,
            b'\x08' => {
                if self.col > 0 {
                    self.col -= 1;
                    let idx = self.row * WIDTH + self.col;
                    self.buffer()[idx] = ScreenChar {
                        ch: b' ',
                        color: self.current_color,
                    };
                }
            }
            b'\t' => {
                let n = 4 - (self.col % 4);
                for _ in 0..n {
                    self.write_byte(b' ');
                }
            }
            byte => {
                if self.col >= WIDTH {
                    self.row += 1;
                    self.col = 0;
                    if self.row >= HEIGHT {
                        self.scroll();
                    }
                }
                let idx = self.row * WIDTH + self.col;
                self.buffer()[idx] = ScreenChar {
                    // VGA text mode has no Japanese glyphs; show a
                    // placeholder so multi-byte text doesn't smear.
                    ch: if byte < 0x80 { byte } else { b'?' },
                    color: self.current_color,
                };
                self.col += 1;
            }
        }
        self.update_cursor();
    }

    fn scroll(&mut self) {
        let buf = self.buffer();
        for r in 1..HEIGHT {
            buf.copy_within(r * WIDTH..(r + 1) * WIDTH, (r - 1) * WIDTH);
        }
        let blank = ScreenChar {
            ch: b' ',
            color: self.current_color,
        };
        for c in 0..WIDTH {
            buf[(HEIGHT - 1) * WIDTH + c] = blank;
        }
        self.row = HEIGHT - 1;
        self.col = 0;
    }

    fn update_cursor(&self) {
        let pos = (self.row * WIDTH + self.col) as u16;
        unsafe {
            port::outb(0x3D4, 14);
            port::outb(0x3D5, (pos >> 8) as u8);
            port::outb(0x3D4, 15);
            port::outb(0x3D5, (pos & 0xFF) as u8);
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
        Ok(())
    }
}

static WRITER: SpinLock<Writer> = SpinLock::new(Writer::new());

pub fn init() {
    let mut writer = WRITER.lock();
    writer.init_buffer();
    writer.set_color(color(colors::LIGHT_GRAY, colors::BLACK));
    writer.clear_screen();
    writer.update_cursor();
}

pub fn write_fmt(args: fmt::Arguments) {
    let mut writer = WRITER.lock();
    let _ = writer.write_fmt(args);
}

pub fn print_char(byte: u8) {
    let mut writer = WRITER.lock();
    writer.write_byte(byte);
}

pub fn erase_char() {
    let mut writer = WRITER.lock();
    writer.write_byte(b'\x08');
}

pub fn clear() {
    let mut writer = WRITER.lock();
    writer.clear_screen();
}

pub fn set_color(color: u8) {
    let mut writer = WRITER.lock();
    writer.set_color(color);
}

pub fn reset_color() {
    let mut writer = WRITER.lock();
    writer.set_color(color(colors::LIGHT_GRAY, colors::BLACK));
}

pub fn try_lock_writer() -> Option<SpinLockGuard<'static, Writer>> {
    WRITER.try_lock()
}
