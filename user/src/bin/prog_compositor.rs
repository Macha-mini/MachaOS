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
const SYS_FB_PRESENT_RECT: u64 = 17;
const SYS_SEND: u64 = 4;
const SYS_WIN_FOCUS: u64 = 15;
const MAX_WINDOWS: usize = 16;
const RECORD_SIZE: usize = 64;
const MAX_WINDOW_PIXELS: usize = 800 * 480;
const SCREEN_W: usize = 1920;
const SCREEN_H: usize = 1080;
const TILE_H: usize = 128;
const TILE_PIXELS: usize = SCREEN_W * TILE_H;
const TITLE_H: u32 = 20;
const BG: u32 = 0x00_1B2A42;
const TITLE: u32 = 0x00_2C5F9E;
const TITLE_DIM: u32 = 0x00_45505C;
const BORDER: u32 = 0x00_7FB2E5;
const TASKBAR_H: i32 = 28;
const TASKBAR_BG: u32 = 0x00_15202B;
const TASKBAR_BUTTON: u32 = 0x00_263340;
const TASKBAR_FOCUSED: u32 = 0x00_3C6EA8;
const START_BG: u32 = 0x00_2F4A73;

static mut FRAME: [u32; TILE_PIXELS] = [0; TILE_PIXELS];
static mut WINDOW_PIXELS: [u32; MAX_WINDOW_PIXELS] = [0; MAX_WINDOW_PIXELS];

struct Screen { pixels: &'static mut [u32; TILE_PIXELS] }
impl Surface for Screen {
    fn width(&self) -> u32 { SCREEN_W as u32 }
    fn height(&self) -> u32 { TILE_H as u32 }
    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < SCREEN_W as u32 && y < TILE_H as u32 { self.pixels[(y as usize * SCREEN_W) + x as usize] = color; }
    }
}

#[derive(Clone, Copy)]
struct Window { pid: u64, x: i32, y: i32, width: u32, height: u32, focused: bool, title: [u8; 32], title_len: usize }

fn fill_global(screen: &mut Screen, tile_y: i32, tile_height: i32, x: u32, y: i32, w: u32, h: u32, color: u32) {
    let start = y.max(tile_y);
    let end = (y + h as i32).min(tile_y + tile_height);
    if end > start { gfx::fill_rect(screen, x, (start - tile_y) as u32, w, (end - start) as u32, color); }
}

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
            title: { let mut title = [0; 32]; title.copy_from_slice(&r[32..64]); title },
            title_len: u32::from_le_bytes(r[28..32].try_into().unwrap()).min(32) as usize,
        };
    }
}

fn compose_tile(windows: &[Window; MAX_WINDOWS], count: usize, tile_y: usize, cursor: (i32, i32)) {
    // Initialize every pixel directly before drawing surfaces. This keeps a
    // dropped/empty window snapshot from exposing stale BSS pages in the
    // final framebuffer.
    unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(FRAME) as *mut u32, TILE_PIXELS).fill(BG); }
    let mut screen = Screen { pixels: unsafe { &mut *core::ptr::addr_of_mut!(FRAME) } };
    let tile_y = tile_y as i32;
    let tile_height = (SCREEN_H - tile_y as usize).min(TILE_H) as i32;
    for window in windows.iter().take(count) {
        let pixels = unsafe { &mut *core::ptr::addr_of_mut!(WINDOW_PIXELS) };
        let n = unsafe { common::syscall(SYS_WIN_READ, window.pid, pixels.as_mut_ptr() as u64, MAX_WINDOW_PIXELS as u64, 0) };
        if n == u64::MAX || n != window.width as u64 * window.height as u64 { continue; }
        let x = window.x.max(0) as u32;
        let y = window.y.max(0);
        let color = if window.focused { TITLE } else { TITLE_DIM };
        fill_global(&mut screen, tile_y, tile_height as i32, x.saturating_sub(1), y - 1, window.width + 2, window.height + TITLE_H + 2, BORDER);
        fill_global(&mut screen, tile_y, tile_height as i32, x, y, window.width, TITLE_H, color);
        let title = core::str::from_utf8(&window.title[..window.title_len]).unwrap_or("");
        if y + 6 >= tile_y && y + 6 < tile_y + tile_height as i32 { gfx::draw_string(&mut screen, x + 4, (y + 6 - tile_y) as u32, title, 0x00_FFFFFF, None); }
        fill_global(&mut screen, tile_y, tile_height as i32, x + window.width.saturating_sub(18), y + 3, 14, 14, 0x00_B33A3A);
        if y + 6 >= tile_y && y + 6 < tile_y + tile_height as i32 { gfx::draw_string(&mut screen, x + window.width.saturating_sub(15), (y + 6 - tile_y) as u32, "x", 0x00_FFFFFF, None); }
        let content_y = y + TITLE_H as i32;
        let start = content_y.max(tile_y);
        let end = (content_y + window.height as i32).min(tile_y + tile_height as i32);
        for gy in start..end { for xx in 0..window.width { screen.put_pixel(x + xx, (gy - tile_y) as u32, pixels[((gy - content_y) as u32 * window.width + xx) as usize]); } }
    }
    let taskbar_y = SCREEN_H as i32 - TASKBAR_H;
    fill_global(&mut screen, tile_y, tile_height as i32, 0, taskbar_y, SCREEN_W as u32, TASKBAR_H as u32, TASKBAR_BG);
    fill_global(&mut screen, tile_y, tile_height as i32, 8, taskbar_y + 4, 90, 20, START_BG);
    if taskbar_y + 8 >= tile_y && taskbar_y + 8 < tile_y + tile_height {
        gfx::draw_string(&mut screen, 14, (taskbar_y + 8 - tile_y) as u32, "Apps", 0x00_FFFFFF, None);
    }
    let mut button_x = 108u32;
    for window in windows.iter().take(count) {
        let width = (window.title_len * 8 + 12).min(220) as u32;
        fill_global(&mut screen, tile_y, tile_height as i32, button_x, taskbar_y + 4, width, 20, if window.focused { TASKBAR_FOCUSED } else { TASKBAR_BUTTON });
        if taskbar_y + 8 >= tile_y && taskbar_y + 8 < tile_y + tile_height {
            let title = core::str::from_utf8(&window.title[..window.title_len]).unwrap_or("");
            gfx::draw_string(&mut screen, button_x + 6, (taskbar_y + 8 - tile_y) as u32, title, 0x00_FFFFFF, None);
        }
        button_x += width + 6;
    }
    if cursor.1 >= tile_y && cursor.1 < tile_y + tile_height {
        let cy = cursor.1 - tile_y;
        for row in 0..16i32 { for col in 0..10i32 {
            if col <= row / 2 || (row > 8 && col <= 2) {
                screen.put_pixel((cursor.0 + col).max(0) as u32, (cy + row).min(tile_height - 1) as u32, 0x00_FFFFFF);
            }
        }}
    }
    unsafe { common::syscall(SYS_FB_PRESENT_RECT, core::ptr::addr_of!(FRAME) as u64, 0, tile_y as u64, (SCREEN_W as u64) << 32 | tile_height as u64); }
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
        if cursor.1 >= window.y && cursor.1 < window.y + TITLE_H as i32 {
            if cursor.0 >= window.x + window.width as i32 - 22 {
                let close = [6, 0, 0, 0, 0, 0];
                unsafe { common::syscall(SYS_SEND, window.pid, close.as_ptr() as u64, 6, 0); }
                return;
            }
            unsafe { common::syscall(SYS_WIN_FOCUS, window.pid, 0, 0, 0); }
            return;
        }
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
    let mut windows = [Window { pid: 0, x: 0, y: 0, width: 0, height: 0, focused: false, title: [0; 32], title_len: 0 }; MAX_WINDOWS];
    let mut event = [0u8; 6];
    let mut cursor = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
    loop {
        let n = unsafe { common::syscall(SYS_RECV, event.as_mut_ptr() as u64, event.len() as u64, 0, 0) };
        let count = unsafe { common::syscall(SYS_WIN_LIST, records.as_mut_ptr() as u64, MAX_WINDOWS as u64, 0, 0) } as usize;
        if count > MAX_WINDOWS { continue; }
        decode_windows(&records, count, &mut windows);
        if n == 6 {
            if event[0] == 5 { handle_mouse(&event, &mut cursor, &windows, count); }
            else if event[0] == 0 || event[0] == 1 || event[0] == 2 || event[0] == 4 || (7..=11).contains(&event[0]) { forward_key(&event, &windows, count); }
        }
        for tile_y in (0..SCREEN_H).step_by(TILE_H) { compose_tile(&windows, count, tile_y, cursor); }
    }
}
