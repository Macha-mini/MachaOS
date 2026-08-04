//! Window management: window rectangles, z-order, drag/focus/close hit
//! testing, the taskbar, and the compositor that draws everything into
//! the framebuffer's back buffer each frame.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::console::Console;
use crate::cpuid;
use crate::fb;
use crate::font;
use crate::gfx::{self, Surface};
use crate::interrupts;
use crate::io;
use crate::keyboard;
use crate::mouse::MouseEvent;
use crate::multiboot;
use crate::shell::{self, Feed, LineEditor};
use crate::task;

const TITLE_BAR_HEIGHT: u32 = 20;
const TASKBAR_HEIGHT: u32 = 28;
const CLOSE_BUTTON_SIZE: u32 = 14;
const DESKTOP_BG: u32 = 0x00_2B4570;
const TITLE_FOCUSED: u32 = 0x00_2C5F9E;
const TITLE_UNFOCUSED: u32 = 0x00_45505C;
const TASKBAR_BG: u32 = 0x00_15202B;
const TASKBAR_BUTTON_FOCUSED: u32 = 0x00_3C6EA8;
const TASKBAR_BUTTON: u32 = 0x00_263340;
const CLOSE_BUTTON_COLOR: u32 = 0x00_B33A3A;
const CONSOLE_FG: u32 = 0x00_E0E0E0;
const CONSOLE_BG: u32 = 0x00_10161C;

pub struct Window {
    pub title: &'static str,
    pub x: i32,
    pub y: i32,
    pub open: bool,
    pub console: Console,
    pub editor: Option<LineEditor>,
}

impl Window {
    fn content_size(&self) -> (u32, u32) {
        (self.console.width_px(), self.console.height_px())
    }
}

struct DragState {
    window_index: usize,
    offset_x: i32,
    offset_y: i32,
}

pub struct WindowManager {
    windows: Vec<Window>,
    focused: usize,
    cursor_x: i32,
    cursor_y: i32,
    screen_w: u32,
    screen_h: u32,
    dragging: Option<DragState>,
    left_was_down: bool,
    last_clock_secs: u64,
}

impl WindowManager {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        let sysinfo_window = Window {
            title: "System Info",
            x: 700,
            y: 40,
            open: true,
            console: build_sysinfo_console(),
            editor: None,
        };
        let term_window = Window {
            title: "Terminal",
            x: 40,
            y: 40,
            open: true,
            console: Console::new(80, 25, CONSOLE_FG, CONSOLE_BG),
            editor: Some(LineEditor::new()),
        };

        let mut manager = Self {
            windows: vec![sysinfo_window, term_window],
            focused: 1,
            cursor_x: (screen_w / 2) as i32,
            cursor_y: (screen_h / 2) as i32,
            screen_w,
            screen_h,
            dragging: None,
            left_was_down: false,
            last_clock_secs: u64::MAX,
        };

        let terminal = &mut manager.windows[1];
        io::set_console_sink(Some(&mut terminal.console));
        print!("machaos> ");
        io::set_console_sink(None);

        manager
    }

    pub fn clock_tick_due(&mut self) -> bool {
        let secs = interrupts::ticks() / 100;
        if secs != self.last_clock_secs {
            self.last_clock_secs = secs;
            true
        } else {
            false
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) {
        if self.windows.is_empty() {
            return;
        }
        let window = &mut self.windows[self.focused];
        let Window { console, editor, .. } = window;
        if let Some(editor) = editor {
            io::set_console_sink(Some(console));
            if let Feed::Line(line) = editor.feed(event) {
                shell::execute(&line);
                print!("machaos> ");
            }
            io::set_console_sink(None);
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        self.cursor_x = (self.cursor_x + event.dx).clamp(0, self.screen_w as i32 - 1);
        self.cursor_y = (self.cursor_y + event.dy).clamp(0, self.screen_h as i32 - 1);

        let just_pressed = event.left && !self.left_was_down;
        let just_released = !event.left && self.left_was_down;
        self.left_was_down = event.left;

        if let Some(drag) = &self.dragging {
            if event.left {
                let (max_x, max_y) = self.drag_bounds();
                let window = &mut self.windows[drag.window_index];
                window.x = (self.cursor_x - drag.offset_x).clamp(0, max_x);
                window.y = (self.cursor_y - drag.offset_y).clamp(0, max_y);
            }
        }
        if just_released {
            self.dragging = None;
        }
        if just_pressed {
            self.handle_click();
        }
    }

    fn drag_bounds(&self) -> (i32, i32) {
        (self.screen_w as i32 - 40, self.screen_h as i32 - TASKBAR_HEIGHT as i32 - TITLE_BAR_HEIGHT as i32)
    }

    fn raise(&mut self, index: usize) {
        let window = self.windows.remove(index);
        self.windows.push(window);
        self.focused = self.windows.len() - 1;
    }

    fn handle_click(&mut self) {
        let taskbar_y = self.screen_h as i32 - TASKBAR_HEIGHT as i32;
        if self.cursor_y >= taskbar_y {
            let mut x = 8i32;
            for i in 0..self.windows.len() {
                if !self.windows[i].open {
                    continue;
                }
                let label_w = taskbar_label_width(self.windows[i].title);
                if self.cursor_x >= x && self.cursor_x < x + label_w {
                    self.raise(i);
                    return;
                }
                x += label_w + 6;
            }
            return;
        }

        for i in (0..self.windows.len()).rev() {
            let window = &self.windows[i];
            if !window.open {
                continue;
            }
            let (content_w, content_h) = window.content_size();
            let (wx, wy) = (window.x, window.y);
            let in_title = self.cursor_x >= wx
                && self.cursor_x < wx + content_w as i32
                && self.cursor_y >= wy
                && self.cursor_y < wy + TITLE_BAR_HEIGHT as i32;
            if in_title {
                let close_x = wx + content_w as i32 - CLOSE_BUTTON_SIZE as i32 - 3;
                if self.cursor_x >= close_x && self.cursor_x < close_x + CLOSE_BUTTON_SIZE as i32 {
                    self.windows[i].open = false;
                    if self.focused == i {
                        self.focused = self.windows.iter().rposition(|w| w.open).unwrap_or(0);
                    }
                    return;
                }
                self.raise(i);
                let focused_index = self.focused;
                self.dragging = Some(DragState {
                    window_index: focused_index,
                    offset_x: self.cursor_x - wx,
                    offset_y: self.cursor_y - wy,
                });
                return;
            }
            let in_content = self.cursor_x >= wx
                && self.cursor_x < wx + content_w as i32
                && self.cursor_y >= wy + TITLE_BAR_HEIGHT as i32
                && self.cursor_y < wy + TITLE_BAR_HEIGHT as i32 + content_h as i32;
            if in_content {
                self.raise(i);
                return;
            }
        }
    }

    pub fn composite(&self) {
        fb::with_surface(|surface| {
            gfx::fill_rect(surface, 0, 0, self.screen_w, self.screen_h, DESKTOP_BG);
            for (i, window) in self.windows.iter().enumerate() {
                if window.open {
                    draw_window(surface, window, i == self.focused);
                }
            }
            self.draw_taskbar(surface);
            draw_cursor(surface, self.cursor_x, self.cursor_y);
        });
        fb::present();
    }

    fn draw_taskbar(&self, surface: &mut dyn Surface) {
        let y = self.screen_h - TASKBAR_HEIGHT;
        gfx::fill_rect(surface, 0, y, self.screen_w, TASKBAR_HEIGHT, TASKBAR_BG);

        let mut x = 8u32;
        for (i, window) in self.windows.iter().enumerate() {
            if !window.open {
                continue;
            }
            let label_w = taskbar_label_width(window.title) as u32;
            let color = if i == self.focused { TASKBAR_BUTTON_FOCUSED } else { TASKBAR_BUTTON };
            gfx::fill_rect(surface, x, y + 4, label_w, TASKBAR_HEIGHT - 8, color);
            gfx::draw_string(surface, x + 6, y + 8, window.title, 0x00_FFFFFF, None);
            x += label_w + 6;
        }

        let ticks = interrupts::ticks();
        let secs = ticks / 100;
        let clock = format!("up {:02}:{:02}", secs / 60, secs % 60);
        let clock_w = (clock.len() * font::GLYPH_WIDTH) as u32;
        gfx::draw_string(surface, self.screen_w - clock_w - 12, y + 8, &clock, 0x00_FFFFFF, None);

        // Background counter tasks keep running (preemptively, via the
        // scheduler in task.rs) whether or not anyone is looking at the
        // Terminal window — this is the visible proof of that.
        let counters = task::COUNTERS
            .iter()
            .map(|c| c.load(core::sync::atomic::Ordering::Relaxed))
            .collect::<Vec<_>>();
        let bg = format!("bg: {} {} {}", counters[0], counters[1], counters[2]);
        let bg_w = (bg.len() * font::GLYPH_WIDTH) as u32;
        gfx::draw_string(surface, self.screen_w - clock_w - bg_w - 24, y + 8, &bg, 0x00_9FCB6B, None);
    }
}

fn taskbar_label_width(title: &str) -> i32 {
    (title.len() * font::GLYPH_WIDTH + 12) as i32
}

fn draw_window(surface: &mut dyn Surface, window: &Window, focused: bool) {
    let (content_w, content_h) = window.content_size();
    let x = window.x as u32;
    let y = window.y as u32;

    let title_color = if focused { TITLE_FOCUSED } else { TITLE_UNFOCUSED };
    gfx::fill_rect(surface, x, y, content_w, TITLE_BAR_HEIGHT, title_color);
    gfx::draw_string(surface, x + 4, y + 6, window.title, 0x00_FFFFFF, None);

    let close_x = x + content_w - CLOSE_BUTTON_SIZE - 3;
    gfx::fill_rect(surface, close_x, y + 3, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_COLOR);
    gfx::draw_string(surface, close_x + 3, y + 4, "x", 0x00_FFFFFF, None);

    gfx::blit(surface, x, y + TITLE_BAR_HEIGHT, window.console.pixels(), content_w, content_h);
}

const CURSOR_W: u32 = 10;
const CURSOR_BITS: [u16; 16] = [
    0b1000000000,
    0b1100000000,
    0b1110000000,
    0b1111000000,
    0b1111100000,
    0b1111110000,
    0b1111111000,
    0b1111111100,
    0b1111111110,
    0b1111100000,
    0b1110110000,
    0b1100110000,
    0b1000011000,
    0b0000011000,
    0b0000001100,
    0b0000001100,
];

fn draw_cursor(surface: &mut dyn Surface, x: i32, y: i32) {
    for (row, bits) in CURSOR_BITS.iter().enumerate() {
        for col in 0..CURSOR_W {
            if bits & (1 << (CURSOR_W - 1 - col)) != 0 {
                let px = x + col as i32;
                let py = y + row as i32;
                if px >= 0 && py >= 0 {
                    surface.put_pixel(px as u32, py as u32, 0x00_FFFFFF);
                }
            }
        }
    }
}

fn build_sysinfo_console() -> Console {
    let mut console = Console::new(38, 16, CONSOLE_FG, CONSOLE_BG);
    let _ = writeln!(console, "MachaOS v0.1.0");
    let _ = writeln!(console);

    let vendor = cpuid::vendor_id();
    let _ = writeln!(console, "CPU: {}", core::str::from_utf8(&vendor).unwrap_or("unknown"));
    if let Some(brand) = cpuid::brand_string() {
        let _ = writeln!(console, "{}", core::str::from_utf8(&brand).unwrap_or("").trim_end());
    }
    let _ = writeln!(console, "cores: {}", cpuid::cores());
    let _ = writeln!(console);

    let info = multiboot::info();
    if let (Some(lower), Some(upper)) = (info.memory_lower_kb(), info.memory_upper_kb()) {
        let _ = writeln!(console, "memory: {} KiB", lower + upper);
    }
    let _ = writeln!(
        console,
        "heap: {} / {} KiB",
        crate::allocator::allocated_bytes() / 1024,
        crate::allocator::total_bytes() / 1024
    );
    let (fb_w, fb_h) = fb::dimensions();
    let _ = writeln!(console, "display: {}x{}x32", fb_w, fb_h);

    console
}
