//! prog_window: the first ring-3 app with an actual window, proving the
//! sys_win_create/sys_win_update/sys_recv path end to end. Creates a
//! small window, then loops forever polling sys_recv for keyboard events
//! the window manager forwards to it (only while its window is focused —
//! see `wm::encode_key_event`): each character key cycles the window's
//! background through a small palette and pushes the redraw via
//! sys_win_update. `.result` counts how many redraws happened, so a
//! headless selftest can confirm the whole round trip worked without a
//! real display.
//!
//! There's no font renderer in user space yet, so this only proves the
//! plumbing (create a window, receive input routed to *this* window
//! specifically, push pixels back, get composited) — an app that wants
//! to draw text still has to rasterize it itself.
//!
//! sys_recv never blocks (see src/syscall.rs), and ring 3 can't execute
//! `hlt` to idle cheaply either, so this just spins — acceptable for a
//! proof-of-concept, not for a real app once more processes exist.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_RECV: u64 = 5;
const SYS_WIN_CREATE: u64 = 6;
const SYS_WIN_UPDATE: u64 = 7;

const WIDTH: u32 = 48;
const HEIGHT: u32 = 32;
const PIXEL_COUNT: usize = (WIDTH * HEIGHT) as usize;

const PALETTE: [u32; 4] = [0x00_2F5C55, 0x00_B33A3A, 0x00_3C6EA8, 0x00_4A4A2E];

// Static rather than a stack-local array: this process's whole stack is
// 32 KiB (see USER_STACK_PAGES in process.rs), and 48*32*4 = 6 KiB is
// enough of a single frame to be worth not risking.
static mut PIXELS: [u32; PIXEL_COUNT] = [0; PIXEL_COUNT];

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let title = b"prog_window";
    unsafe {
        common::syscall(SYS_WIN_CREATE, WIDTH as u64, HEIGHT as u64, title.as_ptr() as u64, title.len() as u64);
    }

    let pixels = unsafe { &mut *core::ptr::addr_of_mut!(PIXELS) };
    let mut color_index: usize = 0;
    let mut redraws: u64 = 0;
    let mut buf = [0u8; 2];

    loop {
        let n = unsafe { common::syscall(SYS_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) };
        let is_char_event = n == buf.len() as u64 && buf[0] == 0; // see wm::encode_key_event's wire format
        if !is_char_event {
            continue;
        }
        color_index = (color_index + 1) % PALETTE.len();
        for pixel in pixels.iter_mut() {
            *pixel = PALETTE[color_index];
        }
        unsafe {
            common::syscall(SYS_WIN_UPDATE, pixels.as_ptr() as u64, PIXEL_COUNT as u64, 0, 0);
        }
        redraws += 1;
        common::RESULT.store(redraws, Ordering::Relaxed);
    }
}
