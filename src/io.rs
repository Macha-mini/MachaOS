use core::fmt;
use core::fmt::Write as _;
use alloc::vec::Vec;

use crate::console;
use crate::serial;
use crate::vga;
use crate::sync::SpinLock;

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
static CAPTURE: SpinLock<Option<Vec<u8>>> = SpinLock::new(None);

pub fn start_capture() { *CAPTURE.lock() = Some(Vec::with_capacity(8192)); }

pub fn take_capture() -> Vec<u8> { CAPTURE.lock().take().unwrap_or_default() }

pub fn set_console_sink(target: Option<&mut console::Console>) {
    unsafe {
        ACTIVE_CONSOLE = match target {
            Some(console) => console as *mut console::Console,
            None => core::ptr::null_mut(),
        };
    }
}

pub fn print(args: fmt::Arguments) {
    let mut writer = serial::SerialWriter;
    let _ = writer.write_fmt(args);

    if let Some(buffer) = CAPTURE.lock().as_mut() {
        let mut capture = CaptureWriter { buffer };
        let _ = capture.write_fmt(args);
        return;
    }

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

struct CaptureWriter<'a> { buffer: &'a mut Vec<u8> }

impl fmt::Write for CaptureWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let remaining = 8192usize.saturating_sub(self.buffer.len());
        self.buffer.extend_from_slice(&s.as_bytes()[..s.len().min(remaining)]);
        Ok(())
    }
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
