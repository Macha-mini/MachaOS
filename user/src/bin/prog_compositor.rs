//! User-space compositor. The kernel owns only window surfaces, input capture,
//! and the protected framebuffer copy; z-order, hit-testing and composition
//! policy live here.

#![no_std]
#![no_main]

#[path = "../common.rs"] mod common;
#[path = "../font.rs"] mod font;
#[path = "../gfx.rs"] mod gfx;

use gfx::Surface;

const SYS_RECV: u64 = 5;
const SYS_WIN_LIST: u64 = 10;
const SYS_WIN_READ: u64 = 11;
const SYS_FB_INFO: u64 = 12;
const SYS_FB_PRESENT: u64 = 13;
const SYS_SEND: u64 = 4;
const SYS_WIN_FOCUS: u64 = 15;
const MAX_WINDOWS: usize = 16;
const RECORD_SIZE: usize = 32;
const MAX_WINDOW_PIXELS: usize = 800 * 480;
const SCREEN_W: usize = 1920;
const SCREEN_H: usize = 1080;
const SCREEN_PIXELS: usize = SCREEN_W * SCREEN_H;
const TITLE_H: u32 = 20;
const BG: u32 = 0x00_1B2A42;
const TITLE: u32 = 0x00_2C5F9E;
const TITLE_DIM: u32 = 0x00_45505C;
const BORDER: u32 = 0x00_7FB2E5;

static mut FRAME: [u32; SCREEN_PIXELS] = [0; SCREEN_PIXELS];
static mut WINDOW_PIXELS: [u32; MAX_WINDOW_PIXELS] = [0; MAX_WINDOW_PIXELS];

struct Screen { pixels: &'static mut [u32; SCREEN_PIXELS] }
impl Surface for Screen {
    fn width(&self) -> u32 { SCREEN_W as u32 }
    fn height(&self) -> u32 { SCREEN_H as u32 }
    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < SCREEN_W as u32 && y < SCREEN_H as u32 { self.pixels[(y as usize * SCREEN_W) + x as usize] = color; }
    }
}

#[derive(Clone, Copy)]
struct Window { pid: u64, x: i32, y: i32, width: u32, height: u32, focused: bool }

fn decode_windows(buf: &[u8; MAX_WINDOWS * RECORD_SIZE], count: usize, out: &mut [Window; MAX_WINDOWS]) {
    for i in 0..count {
        let r = &buf[i * RECORD_SIZE..(i + 1) * RECORD_SIZE];
        out[i] = Window {
            pid: u64::from_le_bytes(r[0..8].try_into().unwrap()),
            x: i32::from_le_bytes(r[8..12].try_into().unwrap()),
            y: i32::from_le_bytes(r[12..16].try_into().unwrap()),
            width: u32::from_le_bytes(r[16..20].try_into().unwrap()),
            height: u32::from_le_bytes(r[20..24].try_into().unwrap()),
            focused: u32::from_le_bytes(r[24..28].try_into().unwrap()) != 0,
        };
    }
}

fn compose(windows: &[Window; MAX_WINDOWS], count: usize) {
    let mut screen = Screen { pixels: unsafe { &mut *core::ptr::addr_of_mut!(FRAME) } };
    gfx::fill_rect(&mut screen, 0, 0, SCREEN_W as u32, SCREEN_H as u32, BG);
    for window in windows.iter().take(count) {
        let pixels = unsafe { &mut *core::ptr::addr_of_mut!(WINDOW_PIXELS) };
        let n = unsafe { common::syscall(SYS_WIN_READ, window.pid, pixels.as_mut_ptr() as u64, MAX_WINDOW_PIXELS as u64, 0) };
        if n == u64::MAX || n != window.width as u64 * window.height as u64 { continue; }
        let x = window.x.max(0) as u32;
        let y = window.y.max(0) as u32;
        let color = if window.focused { TITLE } else { TITLE_DIM };
        gfx::fill_rect(&mut screen, x.saturating_sub(1), y.saturating_sub(1), window.width + 2, window.height + TITLE_H + 2, BORDER);
        gfx::fill_rect(&mut screen, x, y, window.width, TITLE_H, color);
        gfx::blit(&mut screen, x, y + TITLE_H, pixels, window.width, window.height);
    }
    unsafe { common::syscall(SYS_FB_PRESENT, core::ptr::addr_of!(FRAME) as u64, SCREEN_PIXELS as u64, 0, 0); }
}

fn forward_key(event: &[u8; 6], windows: &[Window; MAX_WINDOWS], count: usize) {
    if let Some(window) = windows.iter().take(count).find(|window| window.focused) {
        unsafe { common::syscall(SYS_SEND, window.pid, event.as_ptr() as u64, event.len() as u64, 0); }
    }
}

fn handle_mouse(event: &[u8; 6], cursor: &mut (i32, i32), windows: &[Window; MAX_WINDOWS], count: usize) {
    let dx = i16::from_le_bytes([event[1], event[2]]) as i32;
    let dy = i16::from_le_bytes([event[3], event[4]]) as i32;
    cursor.0 = (cursor.0 + dx).clamp(0, SCREEN_W as i32 - 1);
    cursor.1 = (cursor.1 + dy).clamp(0, SCREEN_H as i32 - 1);
    if event[5] == 0 { return; }
    for window in windows.iter().take(count).rev() {
        if cursor.0 < window.x || cursor.0 >= window.x + window.width as i32 || cursor.1 < window.y + TITLE_H as i32 || cursor.1 >= window.y + TITLE_H as i32 + window.height as i32 { continue; }
        let local = [3, 0, (cursor.0 - window.x) as u16 as u8, ((cursor.0 - window.x) as u16 >> 8) as u8, (cursor.1 - window.y - TITLE_H as i32) as u16 as u8, ((cursor.1 - window.y - TITLE_H as i32) as u16 >> 8) as u8];
        unsafe { common::syscall(SYS_WIN_FOCUS, window.pid, 0, 0, 0); common::syscall(SYS_SEND, window.pid, local.as_ptr() as u64, 6, 0); }
        break;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let info = unsafe { common::syscall(SYS_FB_INFO, 0, 0, 0, 0) };
    if info != ((SCREEN_W as u64) << 32 | SCREEN_H as u64) { loop {} }
    let mut records = [0u8; MAX_WINDOWS * RECORD_SIZE];
    let mut windows = [Window { pid: 0, x: 0, y: 0, width: 0, height: 0, focused: false }; MAX_WINDOWS];
    let mut event = [0u8; 6];
    let mut cursor = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
    loop {
        let n = unsafe { common::syscall(SYS_RECV, event.as_mut_ptr() as u64, event.len() as u64, 0, 0) };
        let count = unsafe { common::syscall(SYS_WIN_LIST, records.as_mut_ptr() as u64, MAX_WINDOWS as u64, 0, 0) } as usize;
        if count > MAX_WINDOWS { continue; }
        decode_windows(&records, count, &mut windows);
        if n == 6 {
            if event[0] == 5 { handle_mouse(&event, &mut cursor, &windows, count); }
            else if event[0] == 0 || event[0] == 1 || event[0] == 2 || event[0] == 4 { forward_key(&event, &windows, count); }
        }
        compose(&windows, count);
    }
}
