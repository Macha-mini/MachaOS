//! Minimal ring-3 terminal. Command execution starts with safe, local
//! commands; privileged services can be added behind the same syscall ABI.

#![no_std]
#![no_main]

#[path = "../common.rs"] mod common;
#[path = "../font.rs"] mod font;
#[path = "../gfx.rs"] mod gfx;

use gfx::Surface;

const SYS_RECV: u64 = 5;
const SYS_WIN_CREATE: u64 = 6;
const SYS_WIN_UPDATE: u64 = 7;
const SYS_EXIT: u64 = 1;
const SYS_SHELL_EXEC: u64 = 16;
const WIDTH: u32 = 800;
const HEIGHT: u32 = 240;
const COLS: usize = 100;
const ROWS: usize = 30;
const SCREEN_CAPACITY: usize = COLS * ROWS;
const LINE_CAPACITY: usize = 256;
const PIXEL_COUNT: usize = (WIDTH * HEIGHT) as usize;
const BG: u32 = 0x00_10161C;
const FG: u32 = 0x00_E0E0E0;
const PROMPT: &[u8] = b"machaos> ";

static mut PIXELS: [u32; PIXEL_COUNT] = [0; PIXEL_COUNT];

struct SurfaceBuf { pixels: &'static mut [u32; PIXEL_COUNT] }
impl Surface for SurfaceBuf {
    fn width(&self) -> u32 { WIDTH }
    fn height(&self) -> u32 { HEIGHT }
    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < WIDTH && y < HEIGHT { self.pixels[(y * WIDTH + x) as usize] = color; }
    }
}

struct Terminal { screen: [u8; SCREEN_CAPACITY], pos: usize, line: [u8; LINE_CAPACITY], len: usize }
impl Terminal {
    fn new() -> Self { Self { screen: [b' '; SCREEN_CAPACITY], pos: 0, line: [0; LINE_CAPACITY], len: 0 } }
    fn clear(&mut self) { self.screen.fill(b' '); self.pos = 0; }
    fn put(&mut self, byte: u8) {
        if byte == b'\n' { self.pos = (self.pos / COLS + 1) * COLS; }
        else { if self.pos >= SCREEN_CAPACITY { self.scroll(); } self.screen[self.pos] = byte; self.pos += 1; }
        if self.pos >= SCREEN_CAPACITY { self.scroll(); }
    }
    fn print(&mut self, text: &[u8]) { for &byte in text { self.put(byte); } }
    fn scroll(&mut self) { self.screen.copy_within(COLS.., 0); self.screen[SCREEN_CAPACITY - COLS..].fill(b' '); self.pos = SCREEN_CAPACITY - COLS; }
    fn prompt(&mut self) { self.print(PROMPT); }
    fn execute(&mut self) {
        self.put(b'\n');
        let line = &self.line[..self.len];
        if line == b"clear" || line == b"cls" { self.clear(); }
        else if !line.is_empty() {
            let mut output = [0u8; 8192];
            let n = unsafe { common::syscall(SYS_SHELL_EXEC, self.line.as_ptr() as u64, self.len as u64, output.as_mut_ptr() as u64, output.len() as u64) };
            if n != u64::MAX { self.print(&output[..(n as usize).min(output.len())]); }
        }
        self.len = 0;
        self.prompt();
    }
    fn render(&mut self) {
        let mut surface = SurfaceBuf { pixels: unsafe { &mut *core::ptr::addr_of_mut!(PIXELS) } };
        gfx::fill_rect(&mut surface, 0, 0, WIDTH, HEIGHT, BG);
        for row in 0..ROWS { for col in 0..COLS {
            let byte = self.screen[row * COLS + col];
            gfx::draw_char(&mut surface, (col * 8) as u32, (row * 8) as u32, byte, FG, Some(BG));
        }}
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let title = b"Terminal";
    unsafe { common::syscall(SYS_WIN_CREATE, WIDTH as u64, HEIGHT as u64, title.as_ptr() as u64, title.len() as u64); }
    let mut terminal = Terminal::new();
    terminal.prompt(); terminal.render(); push_frame();
    let mut event = [0u8; 6];
    loop {
        let n = unsafe { common::syscall(SYS_RECV, event.as_mut_ptr() as u64, event.len() as u64, 0, 0) };
        if n != 6 { continue; }
        match event[0] {
            6 => unsafe { common::syscall(SYS_EXIT, 0, 0, 0, 0); },
            0 if event[1].is_ascii() && terminal.len < LINE_CAPACITY => { terminal.line[terminal.len] = event[1]; terminal.len += 1; terminal.put(event[1]); }
            1 if terminal.len > 0 => {
                terminal.len -= 1;
                if terminal.pos > 0 { terminal.pos -= 1; terminal.screen[terminal.pos] = b' '; }
            }
            2 => terminal.execute(),
            4 if event[1] == b'l' => { terminal.clear(); terminal.prompt(); },
            _ => continue,
        }
        terminal.render(); push_frame();
    }
}

fn push_frame() { unsafe { common::syscall(SYS_WIN_UPDATE, core::ptr::addr_of!(PIXELS) as u64, PIXEL_COUNT as u64, 0, 0); } }
