//! Linear framebuffer access. Drawing goes into a heap-backed back buffer
//! (`present()` flushes it to the real MMIO framebuffer) to avoid flicker.
//! `emergency_*` functions bypass the lock and the back buffer entirely so
//! the panic handler can still put something on screen even if the
//! allocator or the lock holder is in a bad state.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::font;
use crate::gfx::Surface;
use crate::multiboot::MultibootInfo;
use crate::paging;
use crate::sync::SpinLock;

struct State {
    addr: u64,
    pitch: u32,
    width: u32,
    height: u32,
    back_buffer: Vec<u32>,
}

impl Surface for State {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            let idx = (y * self.width + x) as usize;
            self.back_buffer[idx] = color;
        }
    }
}

static STATE: SpinLock<Option<State>> = SpinLock::new(None);
static GRAPHICS_MODE: AtomicBool = AtomicBool::new(false);

// Duplicated out of the lock so the panic path can reach them without it.
static FB_ADDR: AtomicU64 = AtomicU64::new(0);
static FB_PITCH: AtomicU32 = AtomicU32::new(0);
static FB_WIDTH: AtomicU32 = AtomicU32::new(0);
static FB_HEIGHT: AtomicU32 = AtomicU32::new(0);

/// Only 32bpp linear RGB framebuffers are supported (the overwhelming
/// majority of VBE modes GRUB will hand back).
pub fn init(info: &MultibootInfo) -> bool {
    let fb = match info.framebuffer() {
        Some(fb) if fb.bpp == 32 => fb,
        _ => return false,
    };
    if !paging::is_identity_mapped(fb.addr, fb.pitch as u64 * fb.height as u64) {
        return false;
    }

    let back_buffer = vec![0u32; (fb.width * fb.height) as usize];
    *STATE.lock() = Some(State {
        addr: fb.addr,
        pitch: fb.pitch,
        width: fb.width,
        height: fb.height,
        back_buffer,
    });

    FB_ADDR.store(fb.addr, Ordering::Release);
    FB_PITCH.store(fb.pitch, Ordering::Release);
    FB_WIDTH.store(fb.width, Ordering::Release);
    FB_HEIGHT.store(fb.height, Ordering::Release);
    GRAPHICS_MODE.store(true, Ordering::Release);
    true
}

pub fn is_graphics_mode() -> bool {
    GRAPHICS_MODE.load(Ordering::Acquire)
}

pub fn dimensions() -> (u32, u32) {
    (
        FB_WIDTH.load(Ordering::Acquire),
        FB_HEIGHT.load(Ordering::Acquire),
    )
}

/// Runs `f` against the back buffer as a `gfx::Surface`. No-op if the
/// framebuffer was never initialized.
pub fn with_surface<F: FnOnce(&mut dyn Surface)>(f: F) {
    let mut guard = STATE.lock();
    if let Some(state) = guard.as_mut() {
        f(state);
    }
}

/// Reads back a pixel from the back buffer — used by `wayland.rs`'s
/// selftest hook to verify a client's shared-memory buffer actually made
/// it onto the real framebuffer, not just that the handshake completed.
pub fn get_pixel(x: u32, y: u32) -> Option<u32> {
    let guard = STATE.lock();
    let state = guard.as_ref()?;
    if x >= state.width || y >= state.height {
        return None;
    }
    Some(state.back_buffer[(y * state.width + x) as usize])
}

/// Copies the back buffer to the real MMIO framebuffer.
pub fn present() {
    let guard = STATE.lock();
    let Some(state) = guard.as_ref() else {
        return;
    };
    let dst = state.addr as *mut u32;
    for y in 0..state.height as usize {
        let row_offset = y * state.width as usize;
        let src_row = &state.back_buffer[row_offset..row_offset + state.width as usize];
        unsafe {
            let dst_row = dst.byte_add(y * state.pitch as usize);
            core::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, state.width as usize);
        }
    }
}

/// Lock-free, back-buffer-free pixel write straight to MMIO. Only safe to
/// call from the panic handler, where nothing else can be concurrently
/// touching the framebuffer.
pub fn emergency_put_pixel(x: u32, y: u32, color: u32) {
    let addr = FB_ADDR.load(Ordering::Acquire);
    if addr == 0 {
        return;
    }
    let pitch = FB_PITCH.load(Ordering::Acquire);
    let width = FB_WIDTH.load(Ordering::Acquire);
    let height = FB_HEIGHT.load(Ordering::Acquire);
    if x >= width || y >= height {
        return;
    }
    unsafe {
        let dst = (addr as *mut u32).byte_add(y as usize * pitch as usize);
        dst.add(x as usize).write_volatile(color);
    }
}

pub fn emergency_fill(color: u32) {
    let (width, height) = dimensions();
    for y in 0..height {
        for x in 0..width {
            emergency_put_pixel(x, y, color);
        }
    }
}

/// Heap-free string drawing for the panic path: reads glyphs straight out
/// of `font.rs`'s `.rodata` table and writes pixels directly to MMIO.
pub fn emergency_draw_string(x: u32, y: u32, s: &str, fg: u32) {
    let mut cursor_x = x;
    for byte in s.bytes() {
        let glyph = font::glyph(byte);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..font::GLYPH_WIDTH {
                if bits & (1 << col) != 0 {
                    emergency_put_pixel(cursor_x + col as u32, y + row as u32, fg);
                }
            }
        }
        cursor_x += font::GLYPH_WIDTH as u32;
    }
}
