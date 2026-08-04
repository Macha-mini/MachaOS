//! A small ring-3 notepad. The document and framebuffer have fixed limits so
//! this remains usable by the allocator-free user runtime.

#![no_std]
#![no_main]

#[path = "../common.rs"] mod common;
#[path = "../font.rs"] mod font;
#[path = "../gfx.rs"] mod gfx;

use gfx::Surface;

const SYS_RECV: u64 = 5;
const SYS_WIN_CREATE: u64 = 6;
const SYS_WIN_UPDATE: u64 = 7;
const SYS_FILE_READ: u64 = 8;
const SYS_FILE_WRITE: u64 = 9;
const WIDTH: u32 = 800;
const HEIGHT: u32 = 320;
const COLS: usize = 100;
const ROWS: usize = 40;
const DOC_CAPACITY: usize = 8192;
const PIXEL_COUNT: usize = (WIDTH * HEIGHT) as usize;
const BG: u32 = 0x00_10161C;
const FG: u32 = 0x00_E0E0E0;
const CURSOR: u32 = 0x00_FFCC66;
const STATUS_BG: u32 = 0x00_1A2430;
const STATUS_FG: u32 = 0x00_8FB8D8;

static mut PIXELS: [u32; PIXEL_COUNT] = [0; PIXEL_COUNT];

struct SurfaceBuf { pixels: &'static mut [u32; PIXEL_COUNT] }
impl Surface for SurfaceBuf {
    fn width(&self) -> u32 { WIDTH }
    fn height(&self) -> u32 { HEIGHT }
    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < WIDTH && y < HEIGHT { self.pixels[(y * WIDTH + x) as usize] = color; }
    }
}

struct Editor {
    doc: [u8; DOC_CAPACITY],
    len: usize,
    cursor: usize,
    status: [u8; 64],
    status_len: usize,
}

impl Editor {
    fn set_status(&mut self, text: &[u8]) {
        self.status_len = text.len().min(self.status.len());
        self.status[..self.status_len].copy_from_slice(&text[..self.status_len]);
    }

    fn insert(&mut self, byte: u8) {
        if self.len == DOC_CAPACITY { return; }
        self.doc.copy_within(self.cursor..self.len, self.cursor + 1);
        self.doc[self.cursor] = byte;
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 { return; }
        self.cursor -= 1;
        self.doc.copy_within(self.cursor + 1..self.len, self.cursor);
        self.len -= 1;
    }

    fn load(&mut self) {
        let path = b"/notepad.txt";
        let n = unsafe { common::syscall(SYS_FILE_READ, path.as_ptr() as u64, path.len() as u64, self.doc.as_mut_ptr() as u64, DOC_CAPACITY as u64) };
        if n == u64::MAX { self.set_status(b"open failed"); return; }
        self.len = n as usize;
        self.cursor = 0;
        self.set_status(b"opened /notepad.txt");
    }

    fn save(&mut self) {
        let path = b"/notepad.txt";
        let n = unsafe { common::syscall(SYS_FILE_WRITE, path.as_ptr() as u64, path.len() as u64, self.doc.as_ptr() as u64, self.len as u64) };
        if n == u64::MAX { self.set_status(b"save failed"); } else { self.set_status(b"saved /notepad.txt"); }
    }

    fn render(&mut self) {
        let mut surface = SurfaceBuf { pixels: unsafe { &mut *core::ptr::addr_of_mut!(PIXELS) } };
        gfx::fill_rect(&mut surface, 0, 0, WIDTH, HEIGHT, BG);
        let mut row = 0usize;
        let mut col = 0usize;
        for &byte in &self.doc[..self.len] {
            if row >= ROWS - 1 { break; }
            if byte == b'\n' { row += 1; col = 0; continue; }
            if col >= COLS { row += 1; col = 0; }
            if row >= ROWS - 1 { break; }
            gfx::draw_char(&mut surface, (col * font::GLYPH_WIDTH) as u32, (row * font::GLYPH_HEIGHT) as u32, byte, FG, Some(BG));
            col += 1;
        }
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        for &byte in &self.doc[..self.cursor] {
            if byte == b'\n' { cursor_row += 1; cursor_col = 0; } else if cursor_col + 1 < COLS { cursor_col += 1; }
        }
        if cursor_row < ROWS - 1 {
            gfx::fill_rect(&mut surface, (cursor_col * 8) as u32, (cursor_row * 8) as u32, 8, 8, CURSOR);
        }
        gfx::fill_rect(&mut surface, 0, ((ROWS - 1) * 8) as u32, WIDTH, 8, STATUS_BG);
        let status = core::str::from_utf8(&self.status[..self.status_len]).unwrap_or("");
        gfx::draw_string(&mut surface, 8, ((ROWS - 1) * 8) as u32, status, STATUS_FG, None);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let title = b"Notepad";
    unsafe { common::syscall(SYS_WIN_CREATE, WIDTH as u64, HEIGHT as u64, title.as_ptr() as u64, title.len() as u64); }
    let mut editor = Editor { doc: [0; DOC_CAPACITY], len: 0, cursor: 0, status: [0; 64], status_len: 0 };
    editor.set_status(b"Ctrl-S save  Ctrl-O open");
    editor.render();
    push_frame();
    let mut event = [0u8; 6];
    loop {
        let n = unsafe { common::syscall(SYS_RECV, event.as_mut_ptr() as u64, event.len() as u64, 0, 0) };
        if n != 6 { continue; }
        match event[0] {
            0 if event[1].is_ascii_graphic() || event[1] == b' ' => editor.insert(event[1]),
            1 => editor.backspace(),
            2 => editor.insert(b'\n'),
            4 if event[1] == b's' => editor.save(),
            4 if event[1] == b'o' => editor.load(),
            _ => continue,
        }
        editor.render();
        push_frame();
    }
}

fn push_frame() {
    unsafe { common::syscall(SYS_WIN_UPDATE, core::ptr::addr_of!(PIXELS) as u64, PIXEL_COUNT as u64, 0, 0); }
}
