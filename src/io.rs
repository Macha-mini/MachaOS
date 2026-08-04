use core::fmt;
use core::fmt::Write as _;

use crate::serial;
use crate::vga;

macro_rules! print {
    ($($arg:tt)*) => {
        crate::io::print(core::format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        { crate::io::print(core::format_args!("\n")); }
    };
    ($($arg:tt)*) => {
        {
            crate::io::print(core::format_args!($($arg)*));
            crate::io::print(core::format_args!("\n"));
        }
    };
}

pub fn print(args: fmt::Arguments) {
    vga::write_fmt(args);
    let mut writer = serial::SerialWriter;
    let _ = writer.write_fmt(args);
}

/// Prints from exception/panic context: always to serial, best-effort to VGA
/// (avoids deadlock if the VGA writer is already locked).
pub fn exception_print(s: &str) {
    serial::write_str(s);
    if let Some(mut writer) = vga::try_lock_writer() {
        let _ = writer.write_str(s);
    }
}

/// Formats into a caller-provided stack buffer without allocating.
pub fn sprint<'a>(buf: &'a mut [u8], args: fmt::Arguments) -> &'a str {
    let mut writer = StackStr { buffer: buf, len: 0 };
    let _ = writer.write_fmt(args);
    // SAFETY: our formatter only copies utf-8 text into the buffer.
    unsafe { core::str::from_utf8_unchecked(&writer.buffer[..writer.len]) }
}

struct StackStr<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl fmt::Write for StackStr<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if self.len < self.buffer.len() {
                self.buffer[self.len] = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}
