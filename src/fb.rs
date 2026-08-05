//! Linear framebuffer access. The window manager composites at a
//! *virtual* resolution (chosen by the user in Settings; defaults to the
//! physical mode) into a heap-backed back buffer, then `present()`
//! scales it onto the real MMIO framebuffer when the virtual and
//! physical resolutions differ. This is how "changing the OS
//! resolution" works: GRUB picks the physical VBE mode at boot, and the
//! virtual desktop can be any supported size below it.
//! `emergency_*` functions bypass the lock and the back buffer entirely so
//! the panic handler can still put something on screen even if the
//! allocator or the lock holder is in a bad state.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::font;
use crate::gfx::{self, Surface};
use crate::multiboot::MultibootInfo;
use crate::paging;
use crate::sync::SpinLock;

/// Resolutions the Settings app offers. Must stay <= the physical
/// framebuffer in both axes; anything else is rejected.
pub const SUPPORTED_RESOLUTIONS: [(u32, u32); 6] = [
    (1920, 1080),
    (1600, 900),
    (1366, 768),
    (1280, 720),
    (1024, 576),
    (800, 600),
];

struct State {
    addr: u64,
    pitch: u32,
    width: u32,
    height: u32,
    // Virtual desktop resolution: the compositor draws into
    // `vback_buffer` at this size, and present() scales it to the
    // physical buffer.
    vwidth: u32,
    vheight: u32,
    vback_buffer: Vec<u32>,
    // Physical back buffer: `vback_buffer` scaled to the real mode.
    back_buffer: Vec<u32>,
}

impl Surface for State {
    fn width(&self) -> u32 {
        self.vwidth
    }

    fn height(&self) -> u32 {
        self.vheight
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < self.vwidth && y < self.vheight {
            let idx = (y * self.vwidth + x) as usize;
            self.vback_buffer[idx] = color;
        }
    }

    fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        if x < self.vwidth && y < self.vheight {
            Some(self.vback_buffer[(y * self.vwidth + x) as usize])
        } else {
            None
        }
    }

    // Fast paths over the contiguous `vback_buffer`: one slice copy /
    // fill / blend per row instead of one virtual `put_pixel` call per
    // pixel. This is the hot path — the compositor blits the full
    // wallpaper and every window's pixels through here each frame.

    fn blit_span(&mut self, x: u32, y: u32, src: &[u32], offset: usize, len: u32) {
        if y >= self.vheight || x >= self.vwidth || len == 0 {
            return;
        }
        let n = len.min(self.vwidth - x) as usize;
        let row_start = (y * self.vwidth + x) as usize;
        self.vback_buffer[row_start..row_start + n].copy_from_slice(&src[offset..offset + n]);
    }

    fn fill_span(&mut self, x: u32, y: u32, len: u32, color: u32) {
        if y >= self.vheight || x >= self.vwidth || len == 0 {
            return;
        }
        let n = len.min(self.vwidth - x) as usize;
        let row_start = (y * self.vwidth + x) as usize;
        self.vback_buffer[row_start..row_start + n].fill(color);
    }

    fn blend_span(&mut self, x: u32, y: u32, len: u32, color: u32, alpha: u32) {
        if y >= self.vheight || x >= self.vwidth || len == 0 {
            return;
        }
        if alpha >= 255 {
            return self.fill_span(x, y, len, color);
        }
        if alpha == 0 {
            return;
        }
        let n = len.min(self.vwidth - x) as usize;
        let row_start = (y * self.vwidth + x) as usize;
        let row = &mut self.vback_buffer[row_start..row_start + n];
        let inv = 255 - alpha;
        for px in row {
            let mut out = 0u32;
            for shift in [16u32, 8, 0] {
                let f = (color >> shift) & 0xFF;
                let b = (*px >> shift) & 0xFF;
                let v = (f * alpha + b * inv) / 255;
                out |= (v & 0xFF) << shift;
            }
            *px = out;
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
/// majority of VBE modes GRUB will hand back). The virtual resolution
/// starts out equal to the physical one.
pub fn init(info: &MultibootInfo) -> bool {
    let fb = match info.framebuffer() {
        Some(fb) if fb.bpp == 32 => fb,
        _ => return false,
    };
    if !paging::is_identity_mapped(fb.addr, fb.pitch as u64 * fb.height as u64) {
        return false;
    }

    let vback_buffer = vec![0u32; (fb.width * fb.height) as usize];
    let back_buffer = vec![0u32; (fb.width * fb.height) as usize];
    *STATE.lock() = Some(State {
        addr: fb.addr,
        pitch: fb.pitch,
        width: fb.width,
        height: fb.height,
        vwidth: fb.width,
        vheight: fb.height,
        vback_buffer,
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

/// The physical (VBE) framebuffer size, as chosen by GRUB at boot.
pub fn dimensions() -> (u32, u32) {
    (
        FB_WIDTH.load(Ordering::Acquire),
        FB_HEIGHT.load(Ordering::Acquire),
    )
}

/// The virtual desktop resolution — what the window manager lays out
/// against. Differs from `dimensions()` while a non-native resolution
/// is active.
pub fn virtual_dimensions() -> (u32, u32) {
    let guard = STATE.lock();
    let state = guard.as_ref();
    match state {
        Some(state) => (state.vwidth, state.vheight),
        None => dimensions(),
    }
}

/// Changes the virtual desktop resolution. The back buffer is
/// reallocated (cleared to black); `present()` rescales from here on.
/// The new size must be no larger than the physical framebuffer.
pub fn set_virtual_resolution(w: u32, h: u32) -> bool {
    let mut guard = STATE.lock();
    let Some(state) = guard.as_mut() else {
        return false;
    };
    if w == 0 || h == 0 || w > state.width || h > state.height {
        return false;
    }
    state.vwidth = w;
    state.vheight = h;
    state.vback_buffer = Vec::new();
    state.vback_buffer = vec![0u32; (w * h) as usize];
    true
}

/// Runs `f` against the virtual back buffer as a `gfx::Surface`. No-op
/// if the framebuffer was never initialized.
pub fn with_surface<F: FnOnce(&mut dyn Surface)>(f: F) {
    let mut guard = STATE.lock();
    if let Some(state) = guard.as_mut() {
        f(state);
    }
}

/// Reads back a pixel from the *physical* back buffer (after scaling) —
/// used by `wayland.rs`'s selftest hook to verify a client's
/// shared-memory buffer actually made it onto the real framebuffer, not
/// just that the handshake completed.
pub fn get_pixel(x: u32, y: u32) -> Option<u32> {
    let guard = STATE.lock();
    let state = guard.as_ref()?;
    if x >= state.width || y >= state.height {
        return None;
    }
    Some(state.back_buffer[(y * state.width + x) as usize])
}

/// Copies the virtual back buffer's region [x, x+w) x [y, y+h) out, for
/// the window manager's cursor-restore trick: before drawing the cursor
/// the compositor snapshots the (small) rectangle under it, so a later
/// cursor-only frame can restore that rectangle and draw the cursor at
/// its new position without recompositing the whole desktop.
/// Out-of-bounds parts are clamped; the returned buffer is always
/// `w`x`h` (zero-filled where the source is out of range).
pub fn snapshot_region(x: u32, y: u32, w: u32, h: u32) -> alloc::vec::Vec<u32> {
    let mut out = alloc::vec![0u32; (w * h) as usize];
    let guard = STATE.lock();
    let Some(state) = guard.as_ref() else {
        return out;
    };
    if w == 0 || h == 0 {
        return out;
    }
    let x = x.min(state.vwidth);
    let y = y.min(state.vheight);
    let x_end = (x + w).min(state.vwidth);
    for yy in 0..h {
        let sy = y + yy;
        if sy >= state.vheight {
            break;
        }
        let n = (x_end - x) as usize;
        let src = &state.vback_buffer[(sy * state.vwidth + x) as usize..(sy * state.vwidth + x_end) as usize];
        out[(yy as usize * w as usize)..(yy as usize * w as usize + n)].copy_from_slice(src);
    }
    out
}

/// Writes a region saved by `snapshot_region` back into the virtual
/// back buffer (clamped to bounds).
pub fn restore_region(x: u32, y: u32, w: u32, h: u32, pixels: &[u32]) {
    let mut guard = STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    if w == 0 || h == 0 {
        return;
    }
    let x = x.min(state.vwidth);
    let y = y.min(state.vheight);
    let x_end = (x + w).min(state.vwidth);
    for yy in 0..h {
        let sy = y + yy;
        if sy >= state.vheight {
            break;
        }
        let n = (x_end - x) as usize;
        let dst = &mut state.vback_buffer[(sy * state.vwidth + x) as usize..(sy * state.vwidth + x_end) as usize];
        dst.copy_from_slice(&pixels[(yy as usize * w as usize)..(yy as usize * w as usize + n)]);
    }
}

/// Pushes only the given region of the virtual back buffer to the real
/// framebuffer (used by the cursor-only fast path; a full `present()`
/// is still issued when the virtual and physical resolutions differ,
/// since a scale is active then).
pub fn present_region(x: u32, y: u32, w: u32, h: u32) {
    let mut guard = STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    if state.vwidth != state.width || state.vheight != state.height {
        drop(guard);
        return present();
    }
    if w == 0 || h == 0 {
        return;
    }
    let x = x.min(state.width);
    let y = y.min(state.height);
    let x_end = (x + w).min(state.width);
    let y_end = (y + h).min(state.height);
    if x_end <= x || y_end <= y {
        return;
    }
    // Mirror the region into the physical back buffer too, so
    // `get_pixel` (the wayland selftest hook) stays accurate.
    for yy in y..y_end {
        let n = (x_end - x) as usize;
        let src = &state.vback_buffer[(yy * state.vwidth + x) as usize..(yy * state.vwidth + x_end) as usize];
        let dst = &mut state.back_buffer[(yy * state.width + x) as usize..(yy * state.width + x_end) as usize];
        dst.copy_from_slice(src);
    }
    let dst = state.addr as *mut u32;
    for yy in y..y_end {
        let n = (x_end - x) as usize;
        let src_row = &state.back_buffer[(yy * state.width + x) as usize..(yy * state.width + x_end) as usize];
        unsafe {
            let dst_row = dst.byte_add(yy as usize * state.pitch as usize).add(x as usize);
            core::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, n);
        }
    }
}

/// Scales the virtual back buffer to the physical one and copies it to
/// the real MMIO framebuffer.
pub fn present() {
    let mut guard = STATE.lock();
    let Some(state) = guard.as_mut() else {
        return;
    };
    if state.vwidth != state.width || state.vheight != state.height {
        gfx::scale_into(
            &state.vback_buffer,
            state.vwidth,
            state.vheight,
            &mut state.back_buffer,
            state.width,
            state.height,
        );
    } else {
        state.back_buffer.copy_from_slice(&state.vback_buffer);
    }
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
