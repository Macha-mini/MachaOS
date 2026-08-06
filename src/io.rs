use core::fmt;
use core::fmt::Write as _;

use alloc::string::String;

use crate::console;
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

// Where `print!`/`println!` output goes on screen, in addition to always
// mirroring to serial. `desktop.rs` points this at whichever window's
// console currently owns keyboard focus before feeding it input; `None`
// (the default, and what non-GUI boots always use) falls back to the
// legacy VGA text writer. Single-core and never touched from interrupt
// context, so a bare static pointer is sufficient here.
static mut ACTIVE_CONSOLE: *mut console::Console = core::ptr::null_mut();

pub fn set_console_sink(target: Option<&mut console::Console>) {
    unsafe {
        ACTIVE_CONSOLE = match target {
            Some(console) => console as *mut console::Console,
            None => core::ptr::null_mut(),
        };
    }
}

// Shell pipe/redirection support: while this points at a `String`, every
// `print!`/`println!` goes into it instead of the screen/serial. The
// shell sets it around a command whose output is the input of the next
// pipeline stage or a `>` target. Same bare-pointer style as
// ACTIVE_CONSOLE (single-core, never from interrupt context); must be
// restored to null before the buffer is dropped.
static mut OUTPUT_CAPTURE: *mut String = core::ptr::null_mut();

/// Sets the capture target and returns the previous one (which may be
/// non-null when a pipe stage contains its own `>` redirection), so
/// callers can restore it afterwards instead of clobbering an outer
/// capture.
pub fn set_output_capture(target: Option<&mut String>) -> Option<*mut String> {
    unsafe {
        let prev = OUTPUT_CAPTURE;
        OUTPUT_CAPTURE = match target {
            Some(s) => s as *mut String,
            None => core::ptr::null_mut(),
        };
        if prev.is_null() {
            None
        } else {
            Some(prev)
        }
    }
}

/// Restores the capture target saved by `set_output_capture`.
pub fn restore_output_capture(prev: Option<*mut String>) {
    unsafe {
        OUTPUT_CAPTURE = prev.unwrap_or(core::ptr::null_mut());
    }
}

pub fn print(args: fmt::Arguments) {
    unsafe {
        if !OUTPUT_CAPTURE.is_null() {
            let _ = (*OUTPUT_CAPTURE).write_fmt(args);
            return;
        }
    }
    let mut writer = serial::SerialWriter;
    let _ = writer.write_fmt(args);

    unsafe {
        if let Some(console) = ACTIVE_CONSOLE.as_mut() {
            let _ = console.write_fmt(args);
            return;
        }
    }
    vga::write_fmt(args);
}

/// Clears whichever surface `print!`/`println!` currently target.
pub fn clear_active() {
    unsafe {
        if let Some(console) = ACTIVE_CONSOLE.as_mut() {
            console.clear();
            return;
        }
    }
    vga::clear();
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
