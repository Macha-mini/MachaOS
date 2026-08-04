//! Kernel-owned window handles and surfaces. Policy and composition live in
//! the user compositor; this module only validates storage and provides the
//! privileged framebuffer/window transport syscalls. Window surfaces are
//! copied through bounded snapshots rather than mapped into the compositor.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::SpinLock;

pub const MAX_WINDOWS: usize = 16;

pub struct WindowRecord {
    pub pid: usize,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub pixels: Vec<u32>,
    pub x: i32,
    pub y: i32,
}

static WINDOWS: SpinLock<Vec<WindowRecord>> = SpinLock::new(Vec::new());
static FOCUSED_PID: SpinLock<Option<usize>> = SpinLock::new(None);

pub fn create(pid: usize, width: u32, height: u32, title: String) -> bool {
    let count = (width as usize).checked_mul(height as usize).unwrap_or(usize::MAX);
    if count == 0 || count > 2048 * 2048 { return false; }
    let mut windows = WINDOWS.lock();
    if let Some(existing) = windows.iter_mut().find(|window| window.pid == pid) {
        existing.width = width;
        existing.height = height;
        existing.title = title;
        existing.pixels = vec![0; count];
        *FOCUSED_PID.lock() = Some(pid);
        return true;
    }
    if windows.len() >= MAX_WINDOWS { return false; }
    let slot = windows.len() as i32;
    windows.push(WindowRecord {
        pid,
        width,
        height,
        title,
        pixels: vec![0; count],
        x: 32 + (slot * 28) % 420,
        y: 32 + (slot * 28) % 260,
    });
    *FOCUSED_PID.lock() = Some(pid);
    true
}

pub fn update(pid: usize, pixels: Vec<u32>) -> bool {
    let mut windows = WINDOWS.lock();
    let Some(window) = windows.iter_mut().find(|window| window.pid == pid) else { return false };
    if pixels.len() != window.width as usize * window.height as usize { return false; }
    window.pixels = pixels;
    true
}

pub fn focus(pid: usize) {
    if WINDOWS.lock().iter().any(|window| window.pid == pid) {
        *FOCUSED_PID.lock() = Some(pid);
    }
}

pub fn focused() -> Option<usize> { *FOCUSED_PID.lock() }

pub fn with_windows<R>(f: impl FnOnce(&[WindowRecord]) -> R) -> R {
    let windows = WINDOWS.lock();
    f(&windows)
}

pub fn with_window<R>(pid: usize, f: impl FnOnce(Option<&WindowRecord>) -> R) -> R {
    let windows = WINDOWS.lock();
    f(windows.iter().find(|window| window.pid == pid))
}
