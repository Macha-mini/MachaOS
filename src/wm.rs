//! Window management: window rectangles, z-order, drag/resize/focus/close
//! hit testing, the taskbar, and the compositor that draws everything
//! into the framebuffer's back buffer each frame.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::calculator::CalculatorApp;
use crate::console::Console;
use crate::cpuid;
use crate::fat;
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
const MINIMIZE_BUTTON_SIZE: u32 = 14;
const MAXIMIZE_BUTTON_SIZE: u32 = 14;
const RESIZE_GRIP_SIZE: u32 = 12;
const MIN_COLS: usize = 20;
const MIN_ROWS: usize = 5;
const DESKTOP_BG_TOP: u32 = 0x00_3A5680;
const DESKTOP_BG_BOTTOM: u32 = 0x00_1B2A42;
const TITLE_FOCUSED: u32 = 0x00_2C5F9E;
const TITLE_UNFOCUSED: u32 = 0x00_45505C;
const TITLE_HIGHLIGHT: u32 = 0x00_5B8FD4;
const WINDOW_BORDER_FOCUSED: u32 = 0x00_7FB2E5;
const WINDOW_BORDER: u32 = 0x00_223143;
const WINDOW_SHADOW: u32 = 0x00_121D28;
const WINDOW_SHADOW_OFFSET: u32 = 6;
const TASKBAR_BG: u32 = 0x00_15202B;
const TASKBAR_TOP_LINE: u32 = 0x00_3E5878;
const TASKBAR_BUTTON_FOCUSED: u32 = 0x00_3C6EA8;
const TASKBAR_BUTTON: u32 = 0x00_263340;
const TASKBAR_BUTTON_MINIMIZED: u32 = 0x00_18222C;
const CLOSE_BUTTON_COLOR: u32 = 0x00_B33A3A;
const MINIMIZE_BUTTON_COLOR: u32 = 0x00_4A4A2E;
const MAXIMIZE_BUTTON_COLOR: u32 = 0x00_2F5C55;
const RESIZE_GRIP_COLOR: u32 = 0x00_6E7C8C;
const CONSOLE_FG: u32 = 0x00_E0E0E0;
const CONSOLE_BG: u32 = 0x00_10161C;
const EDITOR_CURSOR_COLOR: u32 = 0x00_FFCC66;
const EDITOR_STATUS_BG: u32 = 0x00_1A2430;
const EDITOR_STATUS_FG: u32 = 0x00_8FB8D8;

const START_BUTTON_WIDTH: u32 = 90;
const START_BUTTON_BG: u32 = 0x00_2F4A73;
const LAUNCHER_BG: u32 = 0x00_1C2733;
const LAUNCHER_ITEM_HOVER: u32 = 0x00_35506E;
const LAUNCHER_WIDTH: u32 = 280;
const LAUNCHER_ITEM_HEIGHT: u32 = 30;
// Where taskbar window buttons begin, past the Start button.
const WINDOW_BUTTONS_START_X: i32 = 8 + START_BUTTON_WIDTH as i32 + 10;

#[derive(Clone)]
enum LauncherAction {
    Terminal,
    Calculator,
    Notepad,
    SysInfo,
    RunElf(String),
}

/// Identifies one of the launcher's built-in, "singleton-ish" apps, so
/// closing one and reopening it from the launcher can restore where it
/// was last left. Windows spawned by `run <elf>` (dynamic titles) don't
/// get a slot here — there's no natural single "last position" for an
/// arbitrary disk program you might launch many copies of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppId {
    Terminal,
    Calculator,
    Notepad,
    SysInfo,
}

const APP_ID_COUNT: usize = 4;

#[derive(Clone, Copy)]
struct RememberedWindow {
    x: i32,
    y: i32,
    cols: usize,
    rows: usize,
    maximized: bool,
}

pub enum AppKind {
    Terminal { console: Console, editor: LineEditor },
    SysInfo { console: Console },
    Editor { console: Console, lines: Vec<String>, cursor_row: usize, cursor_col: usize, scroll_offset: usize, status: String },
    Calculator(CalculatorApp),
}

pub struct Window {
    pub title: &'static str,
    pub x: i32,
    pub y: i32,
    pub open: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub resizable: bool,
    app_id: Option<AppId>,
    // (x, y, cols, rows) from just before maximizing; restored on
    // un-maximize. Only ever `Some` while `maximized` is true.
    restore_geometry: Option<(i32, i32, usize, usize)>,
    pub kind: AppKind,
}

impl Window {
    fn content_size(&self) -> (u32, u32) {
        match &self.kind {
            AppKind::Terminal { console, .. }
            | AppKind::SysInfo { console }
            | AppKind::Editor { console, .. } => (console.width_px(), console.height_px()),
            AppKind::Calculator(app) => (app.width(), app.height()),
        }
    }

    fn content_pixels(&self) -> &[u32] {
        match &self.kind {
            AppKind::Terminal { console, .. }
            | AppKind::SysInfo { console }
            | AppKind::Editor { console, .. } => console.pixels(),
            AppKind::Calculator(app) => app.pixels(),
        }
    }

    /// Cell dimensions for the console-backed kinds; `(0, 0)` for
    /// `Calculator`, which has no grid (and is never resizable/maximizable
    /// anyway, so this is never used for it in practice).
    fn cols_rows(&self) -> (usize, usize) {
        match &self.kind {
            AppKind::Terminal { console, .. }
            | AppKind::SysInfo { console }
            | AppKind::Editor { console, .. } => (console.cols(), console.rows()),
            AppKind::Calculator(_) => (0, 0),
        }
    }
}

struct DragState {
    window_index: usize,
    offset_x: i32,
    offset_y: i32,
}

struct ResizeState {
    window_index: usize,
    last_cols: usize,
    last_rows: usize,
}

pub struct WindowManager {
    windows: Vec<Window>,
    focused: usize,
    cursor_x: i32,
    cursor_y: i32,
    screen_w: u32,
    screen_h: u32,
    dragging: Option<DragState>,
    resizing: Option<ResizeState>,
    left_was_down: bool,
    last_clock_secs: u64,
    launcher_open: bool,
    launcher_items: Vec<(String, LauncherAction)>,
    spawn_counter: u32,
    // Last known geometry of each built-in app, keyed by AppId, updated
    // whenever such a window is closed. `spawn_window` consults this
    // instead of the cascade position when relaunching that app.
    remembered: [Option<RememberedWindow>; APP_ID_COUNT],
    // Desktop background image, loaded from disk once at startup (see
    // `load_wallpaper`). `None` when there's no FAT32 volume mounted, no
    // /wallpaper.raw on it, or its size doesn't match the current mode —
    // composite() falls back to the plain gradient fill in that case.
    wallpaper: Option<Vec<u32>>,
}

impl WindowManager {
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        let margin = 40i32;
        let max_x = screen_w as i32 - 100;
        let max_y = screen_h as i32 - TASKBAR_HEIGHT as i32 - 100;
        let clamp = |x: i32, y: i32| (x.clamp(0, max_x), y.clamp(0, max_y));

        let sysinfo_window = Window {
            title: "System Info",
            x: clamp(880, margin).0,
            y: margin,
            open: true,
            minimized: false,
            maximized: false,
            resizable: false,
            app_id: Some(AppId::SysInfo),
            restore_geometry: None,
            kind: AppKind::SysInfo { console: build_sysinfo_console() },
        };

        let calculator_window = Window {
            title: "Calculator",
            x: clamp(1220, margin).0,
            y: margin,
            open: true,
            minimized: false,
            maximized: false,
            resizable: false,
            app_id: Some(AppId::Calculator),
            restore_geometry: None,
            kind: AppKind::Calculator(CalculatorApp::new()),
        };

        let mut editor_console = Console::new(100, 20, CONSOLE_FG, CONSOLE_BG);
        let editor_lines = vec![String::new()];
        let mut editor_scroll = 0usize;
        let mut editor_status = String::new();
        editor_render(&mut editor_console, &editor_lines, 0, 0, &mut editor_scroll, &mut editor_status);
        let (ex, ey) = clamp(margin, margin + 300);
        let editor_window = Window {
            title: "Notepad",
            x: ex,
            y: ey,
            open: true,
            minimized: false,
            maximized: false,
            resizable: true,
            app_id: Some(AppId::Notepad),
            restore_geometry: None,
            kind: AppKind::Editor {
                console: editor_console,
                lines: editor_lines,
                cursor_row: 0,
                cursor_col: 0,
                scroll_offset: editor_scroll,
                status: editor_status,
            },
        };

        let term_window = Window {
            title: "Terminal",
            x: margin,
            y: margin,
            open: true,
            minimized: false,
            maximized: false,
            resizable: true,
            app_id: Some(AppId::Terminal),
            restore_geometry: None,
            kind: AppKind::Terminal {
                console: Console::new(100, 30, CONSOLE_FG, CONSOLE_BG),
                editor: LineEditor::new(),
            },
        };

        let mut manager = Self {
            windows: vec![sysinfo_window, calculator_window, editor_window, term_window],
            focused: 3,
            cursor_x: (screen_w / 2) as i32,
            cursor_y: (screen_h / 2) as i32,
            screen_w,
            screen_h,
            dragging: None,
            resizing: None,
            left_was_down: false,
            last_clock_secs: u64::MAX,
            launcher_open: false,
            launcher_items: Vec::new(),
            spawn_counter: 0,
            remembered: [None; APP_ID_COUNT],
            wallpaper: load_wallpaper(screen_w, screen_h),
        };

        if let AppKind::Terminal { console, .. } = &mut manager.windows[3].kind {
            io::set_console_sink(Some(console));
            print!("{}", shell::prompt());
            io::set_console_sink(None);
        }

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

    /// Adds a new window (or reuses a closed one's slot, so repeatedly
    /// launching and closing apps from the launcher doesn't grow the
    /// window list forever), raises and focuses it, and returns its index.
    ///
    /// If `app_id` has a remembered geometry (it was open before, at some
    /// point, and got closed), the window reopens at that position/size
    /// instead of a fresh cascade position — nudged aside if something
    /// else already occupies that spot. Otherwise it cascades as before.
    fn spawn_window(&mut self, title: &'static str, resizable: bool, app_id: Option<AppId>, kind: AppKind) -> usize {
        let remembered = app_id.and_then(|id| self.remembered[id as usize]);
        let (base_x, base_y) = match remembered {
            Some(r) => (r.x, r.y),
            None => self.next_spawn_position(),
        };
        let (x, y) = self.avoid_overlap(base_x, base_y);
        let window = Window {
            title,
            x,
            y,
            open: true,
            minimized: false,
            maximized: false,
            resizable,
            app_id,
            restore_geometry: None,
            kind,
        };
        let index = if let Some(slot) = self.windows.iter().position(|w| !w.open) {
            self.windows[slot] = window;
            slot
        } else {
            self.windows.push(window);
            self.windows.len() - 1
        };
        self.raise(index);

        if let Some(r) = remembered {
            let cols = r.cols.max(MIN_COLS);
            let rows = r.rows.max(MIN_ROWS);
            resize_window(&mut self.windows[index], cols, rows);
            if r.maximized {
                self.maximize_window(index);
            }
        }
        index
    }

    /// Cascades new windows diagonally so they don't stack exactly on top
    /// of one another, wrapping back to the top-left once they'd run off
    /// the screen.
    fn next_spawn_position(&mut self) -> (i32, i32) {
        self.spawn_counter = self.spawn_counter.wrapping_add(1);
        let offset = (self.spawn_counter as i32 * 28) % 320;
        let x = (140 + offset).min(self.screen_w as i32 - 260);
        let y = (100 + offset).min(self.screen_h as i32 - TASKBAR_HEIGHT as i32 - 260);
        (x, y)
    }

    /// Nudges `(x, y)` diagonally while some other open, non-minimized
    /// window's top-left corner is within a few pixels of it, so a
    /// restored or cascaded window doesn't land exactly on top of one
    /// that's already there. Bounded so it can never loop forever.
    fn avoid_overlap(&self, mut x: i32, mut y: i32) -> (i32, i32) {
        let max_x = self.screen_w as i32 - 260;
        let max_y = self.screen_h as i32 - TASKBAR_HEIGHT as i32 - 260;
        for _ in 0..20 {
            let collides = self
                .windows
                .iter()
                .any(|w| w.open && !w.minimized && (w.x - x).abs() < 24 && (w.y - y).abs() < 24);
            if !collides {
                break;
            }
            x = (x + 28).clamp(0, max_x.max(0));
            y = (y + 28).clamp(0, max_y.max(0));
        }
        (x, y)
    }

    fn start_button_hit(&self) -> bool {
        let y = self.screen_h as i32 - TASKBAR_HEIGHT as i32;
        self.cursor_x >= 8
            && self.cursor_x < 8 + START_BUTTON_WIDTH as i32
            && self.cursor_y >= y + 4
            && self.cursor_y < y + TASKBAR_HEIGHT as i32 - 4
    }

    /// The cell grid a maximized window should fill: the whole screen
    /// width, minus the taskbar and title bar heights.
    fn max_content_cells(&self) -> (usize, usize) {
        let max_w = self.screen_w;
        let max_h = self.screen_h - TASKBAR_HEIGHT - TITLE_BAR_HEIGHT;
        ((max_w / font::GLYPH_WIDTH as u32) as usize, (max_h / font::GLYPH_HEIGHT as u32) as usize)
    }

    fn maximize_window(&mut self, index: usize) {
        let window = &self.windows[index];
        if !window.resizable || window.maximized {
            return;
        }
        let (cols, rows) = window.cols_rows();
        self.windows[index].restore_geometry = Some((window.x, window.y, cols, rows));
        self.windows[index].x = 0;
        self.windows[index].y = 0;
        self.windows[index].maximized = true;
        let (max_cols, max_rows) = self.max_content_cells();
        resize_window(&mut self.windows[index], max_cols, max_rows);
    }

    fn unmaximize_window(&mut self, index: usize) {
        let window = &mut self.windows[index];
        if !window.maximized {
            return;
        }
        window.maximized = false;
        if let Some((x, y, cols, rows)) = window.restore_geometry.take() {
            window.x = x;
            window.y = y;
            resize_window(window, cols, rows);
        }
    }

    /// Records a window's current geometry as "where this app was last
    /// left", so the launcher restores it there next time. No-op for
    /// windows without an `app_id` (e.g. `run`-launched programs, which
    /// have no single natural "last position"). If the window is
    /// currently maximized, its pre-maximize geometry is remembered
    /// instead (along with the fact that it was maximized), so reopening
    /// it maximizes it again rather than reopening at the stale
    /// full-screen bounds.
    fn remember_geometry(&mut self, index: usize) {
        let window = &self.windows[index];
        let Some(id) = window.app_id else { return };
        let remembered = if window.maximized {
            window
                .restore_geometry
                .map(|(x, y, cols, rows)| RememberedWindow { x, y, cols, rows, maximized: true })
        } else {
            let (cols, rows) = window.cols_rows();
            Some(RememberedWindow { x: window.x, y: window.y, cols, rows, maximized: false })
        };
        if let Some(r) = remembered {
            self.remembered[id as usize] = Some(r);
        }
    }

    fn launcher_popup_rect(&self) -> (i32, i32, u32, u32) {
        let popup_h = LAUNCHER_ITEM_HEIGHT * self.launcher_items.len().max(1) as u32 + 8;
        let popup_y = self.screen_h as i32 - TASKBAR_HEIGHT as i32 - popup_h as i32;
        (8, popup_y, LAUNCHER_WIDTH, popup_h)
    }

    fn launcher_hit_test(&self) -> Option<usize> {
        let (px, py, pw, ph) = self.launcher_popup_rect();
        if self.cursor_x < px
            || self.cursor_x >= px + pw as i32
            || self.cursor_y < py
            || self.cursor_y >= py + ph as i32
        {
            return None;
        }
        let rel_y = (self.cursor_y - py - 4).max(0) as u32;
        let idx = (rel_y / LAUNCHER_ITEM_HEIGHT) as usize;
        if idx < self.launcher_items.len() {
            Some(idx)
        } else {
            None
        }
    }

    /// Built-in apps first, then any `.elf` file found in `/bin` on the
    /// mounted FAT32 volume (if any) — rebuilt each time the launcher
    /// opens so newly written files show up without a reboot.
    fn build_launcher_items(&self) -> Vec<(String, LauncherAction)> {
        let mut items = vec![
            ("Terminal".to_string(), LauncherAction::Terminal),
            ("Calculator".to_string(), LauncherAction::Calculator),
            ("Notepad".to_string(), LauncherAction::Notepad),
            ("System Info".to_string(), LauncherAction::SysInfo),
        ];
        if fat::mounted() {
            if let Ok(entries) = fat::list_dir("/bin") {
                for entry in entries {
                    if !entry.is_dir && entry.name.to_lowercase().ends_with(".elf") {
                        let path = format!("/bin/{}", entry.name);
                        items.push((entry.name.clone(), LauncherAction::RunElf(path)));
                    }
                }
            }
        }
        items
    }

    fn run_launcher_action(&mut self, action: LauncherAction) {
        match action {
            LauncherAction::Terminal => {
                let idx = self.spawn_window(
                    "Terminal",
                    true,
                    Some(AppId::Terminal),
                    AppKind::Terminal {
                        console: Console::new(100, 30, CONSOLE_FG, CONSOLE_BG),
                        editor: LineEditor::new(),
                    },
                );
                if let AppKind::Terminal { console, .. } = &mut self.windows[idx].kind {
                    io::set_console_sink(Some(console));
                    print!("{}", shell::prompt());
                    io::set_console_sink(None);
                }
            }
            LauncherAction::Calculator => {
                self.spawn_window(
                    "Calculator",
                    false,
                    Some(AppId::Calculator),
                    AppKind::Calculator(CalculatorApp::new()),
                );
            }
            LauncherAction::Notepad => {
                let mut console = Console::new(100, 20, CONSOLE_FG, CONSOLE_BG);
                let lines = vec![String::new()];
                let mut scroll = 0usize;
                let mut status = String::new();
                editor_render(&mut console, &lines, 0, 0, &mut scroll, &mut status);
                self.spawn_window(
                    "Notepad",
                    true,
                    Some(AppId::Notepad),
                    AppKind::Editor { console, lines, cursor_row: 0, cursor_col: 0, scroll_offset: scroll, status },
                );
            }
            LauncherAction::SysInfo => {
                self.spawn_window(
                    "System Info",
                    false,
                    Some(AppId::SysInfo),
                    AppKind::SysInfo { console: build_sysinfo_console() },
                );
            }
            LauncherAction::RunElf(path) => {
                let title: &'static str = alloc::boxed::Box::leak(format!("Terminal: {}", path).into_boxed_str());
                let idx = self.spawn_window(
                    title,
                    true,
                    None,
                    AppKind::Terminal {
                        console: Console::new(100, 30, CONSOLE_FG, CONSOLE_BG),
                        editor: LineEditor::new(),
                    },
                );
                if let AppKind::Terminal { console, .. } = &mut self.windows[idx].kind {
                    io::set_console_sink(Some(console));
                    let command = format!("run {}", path);
                    println!("{}{}", shell::prompt(), command);
                    shell::execute(&command);
                    print!("{}", shell::prompt());
                    io::set_console_sink(None);
                }
            }
        }
    }

    fn draw_launcher(&self, surface: &mut dyn Surface) {
        let (px, py, pw, ph) = self.launcher_popup_rect();
        let bx = (px - 1).max(0) as u32;
        let by = (py - 1).max(0) as u32;
        gfx::fill_rect(surface, bx, by, pw + 2, ph + 2, WINDOW_BORDER_FOCUSED);
        gfx::fill_rect(surface, px as u32, py as u32, pw, ph, LAUNCHER_BG);
        for (i, (label, _)) in self.launcher_items.iter().enumerate() {
            let iy = py as u32 + 4 + i as u32 * LAUNCHER_ITEM_HEIGHT;
            let hovered = self.cursor_y >= iy as i32
                && self.cursor_y < (iy + LAUNCHER_ITEM_HEIGHT) as i32
                && self.cursor_x >= px
                && self.cursor_x < px + pw as i32;
            if hovered {
                gfx::fill_rect(surface, px as u32 + 2, iy, pw - 4, LAUNCHER_ITEM_HEIGHT - 2, LAUNCHER_ITEM_HOVER);
            }
            gfx::draw_string(surface, px as u32 + 10, iy + 8, label, 0x00_FFFFFF, None);
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) {
        if self.windows.is_empty() {
            return;
        }
        let window = &mut self.windows[self.focused];
        match &mut window.kind {
            AppKind::Terminal { console, editor } => {
                io::set_console_sink(Some(console));
                if let Feed::Line(line) = editor.feed(event) {
                    shell::execute(&line);
                    print!("{}", shell::prompt());
                }
                io::set_console_sink(None);
            }
            AppKind::Editor { console, lines, cursor_row, cursor_col, scroll_offset, status } => {
                editor_handle_key(console, lines, cursor_row, cursor_col, scroll_offset, status, event);
            }
            AppKind::SysInfo { .. } | AppKind::Calculator(_) => {}
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) {
        self.cursor_x = (self.cursor_x + event.dx).clamp(0, self.screen_w as i32 - 1);
        self.cursor_y = (self.cursor_y + event.dy).clamp(0, self.screen_h as i32 - 1);

        let just_pressed = event.left && !self.left_was_down;
        let just_released = !event.left && self.left_was_down;
        self.left_was_down = event.left;

        if let Some(resize) = &mut self.resizing {
            if event.left {
                let index = resize.window_index;
                let (wx, wy) = (self.windows[index].x, self.windows[index].y);
                let local_w = (self.cursor_x - wx).max(0) as u32;
                let local_h = (self.cursor_y - wy - TITLE_BAR_HEIGHT as i32).max(0) as u32;
                let cols = ((local_w / font::GLYPH_WIDTH as u32) as usize).max(MIN_COLS);
                let rows = ((local_h / font::GLYPH_HEIGHT as u32) as usize).max(MIN_ROWS);
                if cols != resize.last_cols || rows != resize.last_rows {
                    resize.last_cols = cols;
                    resize.last_rows = rows;
                    resize_window(&mut self.windows[index], cols, rows);
                }
            }
            if just_released {
                self.resizing = None;
            }
            return;
        }

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

    fn refocus_after_hide(&mut self, hidden_index: usize) {
        if self.focused == hidden_index {
            self.focused =
                self.windows.iter().rposition(|w| w.open && !w.minimized).unwrap_or(0);
        }
    }

    fn handle_click(&mut self) {
        if self.launcher_open {
            if let Some(idx) = self.launcher_hit_test() {
                let action = self.launcher_items[idx].1.clone();
                self.launcher_open = false;
                self.run_launcher_action(action);
            } else {
                // Clicking the Start button again, or anywhere else,
                // dismisses the popup (a second Start click shouldn't
                // reopen it in the same gesture).
                self.launcher_open = false;
            }
            return;
        }

        let taskbar_y = self.screen_h as i32 - TASKBAR_HEIGHT as i32;
        if self.cursor_y >= taskbar_y {
            if self.start_button_hit() {
                self.launcher_items = self.build_launcher_items();
                self.launcher_open = true;
                return;
            }
            let mut x = WINDOW_BUTTONS_START_X;
            for i in 0..self.windows.len() {
                if !self.windows[i].open {
                    continue;
                }
                let label_w = taskbar_label_width(self.windows[i].title);
                if self.cursor_x >= x && self.cursor_x < x + label_w {
                    self.windows[i].minimized = false;
                    self.raise(i);
                    return;
                }
                x += label_w + 6;
            }
            return;
        }

        for i in (0..self.windows.len()).rev() {
            let window = &self.windows[i];
            if !window.open || window.minimized {
                continue;
            }
            let (content_w, content_h) = window.content_size();
            let (wx, wy) = (window.x, window.y);
            let resizable = window.resizable;
            let maximized = window.maximized;

            if resizable && !maximized {
                let grip_x = wx + content_w as i32 - RESIZE_GRIP_SIZE as i32;
                let grip_y = wy + TITLE_BAR_HEIGHT as i32 + content_h as i32 - RESIZE_GRIP_SIZE as i32;
                if self.cursor_x >= grip_x
                    && self.cursor_x < grip_x + RESIZE_GRIP_SIZE as i32
                    && self.cursor_y >= grip_y
                    && self.cursor_y < grip_y + RESIZE_GRIP_SIZE as i32
                {
                    self.raise(i);
                    let focused_index = self.focused;
                    let cols = content_w as usize / font::GLYPH_WIDTH;
                    let rows = content_h as usize / font::GLYPH_HEIGHT;
                    self.resizing = Some(ResizeState { window_index: focused_index, last_cols: cols, last_rows: rows });
                    return;
                }
            }

            let in_title = self.cursor_x >= wx
                && self.cursor_x < wx + content_w as i32
                && self.cursor_y >= wy
                && self.cursor_y < wy + TITLE_BAR_HEIGHT as i32;
            if in_title {
                let (minimize_x, maximize_x, close_x) = title_button_positions(wx, content_w, resizable);

                if self.cursor_x >= close_x && self.cursor_x < close_x + CLOSE_BUTTON_SIZE as i32 {
                    self.remember_geometry(i);
                    self.windows[i].open = false;
                    self.refocus_after_hide(i);
                    return;
                }
                if let Some(max_x) = maximize_x {
                    if self.cursor_x >= max_x && self.cursor_x < max_x + MAXIMIZE_BUTTON_SIZE as i32 {
                        self.raise(i);
                        let focused_index = self.focused;
                        if maximized {
                            self.unmaximize_window(focused_index);
                        } else {
                            self.maximize_window(focused_index);
                        }
                        return;
                    }
                }
                if self.cursor_x >= minimize_x && self.cursor_x < minimize_x + MINIMIZE_BUTTON_SIZE as i32 {
                    self.windows[i].minimized = true;
                    self.refocus_after_hide(i);
                    return;
                }

                self.raise(i);
                let focused_index = self.focused;
                if maximized {
                    // Picking up a maximized window by its title bar
                    // restores it first, so the drag starts from (and
                    // follows) its normal size rather than dragging the
                    // whole full-screen window around.
                    self.unmaximize_window(focused_index);
                }
                let (wx2, wy2) = (self.windows[focused_index].x, self.windows[focused_index].y);
                self.dragging = Some(DragState {
                    window_index: focused_index,
                    offset_x: self.cursor_x - wx2,
                    offset_y: self.cursor_y - wy2,
                });
                return;
            }
            let in_content = self.cursor_x >= wx
                && self.cursor_x < wx + content_w as i32
                && self.cursor_y >= wy + TITLE_BAR_HEIGHT as i32
                && self.cursor_y < wy + TITLE_BAR_HEIGHT as i32 + content_h as i32;
            if in_content {
                let local_x = self.cursor_x - wx;
                let local_y = self.cursor_y - wy - TITLE_BAR_HEIGHT as i32;
                self.raise(i);
                if let AppKind::Calculator(app) = &mut self.windows[self.focused].kind {
                    app.handle_click(local_x, local_y);
                }
                return;
            }
        }
    }

    pub fn composite(&self) {
        fb::with_surface(|surface| {
            match &self.wallpaper {
                Some(pixels) => gfx::blit(surface, 0, 0, pixels, self.screen_w, self.screen_h),
                None => gfx::fill_rect_gradient_v(surface, 0, 0, self.screen_w, self.screen_h, DESKTOP_BG_TOP, DESKTOP_BG_BOTTOM),
            }
            for (i, window) in self.windows.iter().enumerate() {
                if window.open && !window.minimized {
                    draw_window(surface, window, i == self.focused);
                }
            }
            self.draw_taskbar(surface);
            if self.launcher_open {
                self.draw_launcher(surface);
            }
            draw_cursor(surface, self.cursor_x, self.cursor_y);
        });
        fb::present();
    }

    fn draw_taskbar(&self, surface: &mut dyn Surface) {
        let y = self.screen_h - TASKBAR_HEIGHT;
        gfx::fill_rect(surface, 0, y, self.screen_w, TASKBAR_HEIGHT, TASKBAR_BG);
        gfx::fill_rect(surface, 0, y, self.screen_w, 1, TASKBAR_TOP_LINE);

        let start_color = if self.launcher_open { TASKBAR_BUTTON_FOCUSED } else { START_BUTTON_BG };
        gfx::fill_rect(surface, 8, y + 4, START_BUTTON_WIDTH, TASKBAR_HEIGHT - 8, start_color);
        gfx::draw_string(surface, 8 + 6, y + 8, "Apps", 0x00_FFFFFF, None);

        let mut x = WINDOW_BUTTONS_START_X as u32;
        for (i, window) in self.windows.iter().enumerate() {
            if !window.open {
                continue;
            }
            let label_w = taskbar_label_width(window.title) as u32;
            let color = if i == self.focused {
                TASKBAR_BUTTON_FOCUSED
            } else if window.minimized {
                TASKBAR_BUTTON_MINIMIZED
            } else {
                TASKBAR_BUTTON
            };
            gfx::fill_rect(surface, x, y + 4, label_w, TASKBAR_HEIGHT - 8, color);
            gfx::draw_string(surface, x + 6, y + 8, window.title, 0x00_FFFFFF, None);
            x += label_w + 6;
        }

        let ticks = interrupts::ticks();
        let secs = ticks / 100;
        let now = crate::rtc::now();
        let clock = format!(
            "{:02}:{:02}  up {:02}:{:02}",
            now.hour,
            now.minute,
            secs / 60,
            secs % 60
        );
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

fn resize_window(window: &mut Window, cols: usize, rows: usize) {
    match &mut window.kind {
        AppKind::Terminal { console, editor } => {
            console.resize(cols, rows);
            io::set_console_sink(Some(console));
            print!("{}{}", shell::prompt(), editor.current_line());
            io::set_console_sink(None);
        }
        AppKind::Editor { console, lines, cursor_row, cursor_col, scroll_offset, status } => {
            console.resize(cols, rows);
            editor_render(console, lines, *cursor_row, *cursor_col, scroll_offset, status);
        }
        AppKind::SysInfo { .. } | AppKind::Calculator(_) => {} // not resizable
    }
}

fn byte_index(line: &str, char_index: usize) -> usize {
    line.char_indices().nth(char_index).map(|(i, _)| i).unwrap_or(line.len())
}

const NOTEPAD_PATH: &str = "/notepad.txt";

fn editor_handle_key(
    console: &mut Console,
    lines: &mut Vec<String>,
    cursor_row: &mut usize,
    cursor_col: &mut usize,
    scroll_offset: &mut usize,
    status: &mut String,
    event: keyboard::Event,
) {
    match event {
        keyboard::Event::Ctrl('s') => {
            let mut text = lines.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            match fat::write_file(NOTEPAD_PATH, text.as_bytes()) {
                Ok(()) => *status = format!("saved {} bytes to {}", text.len(), NOTEPAD_PATH),
                Err(e) => *status = format!("save failed: {}", e),
            }
        }
        keyboard::Event::Ctrl('o') => {
            match fat::read_file(NOTEPAD_PATH) {
                Ok(data) => {
                    let text = core::str::from_utf8(&data).unwrap_or("");
                    lines.clear();
                    lines.extend(text.split('\n').map(|l| l.to_string()));
                    if lines.last().map(String::is_empty) == Some(true) {
                        lines.pop();
                    }
                    *cursor_row = 0;
                    *cursor_col = 0;
                    *status = format!(
                        "opened {} ({} bytes)",
                        NOTEPAD_PATH,
                        data.len()
                    );
                }
                Err(e) => *status = format!("open failed: {}", e),
            }
        }
        keyboard::Event::Ctrl(_) => {} // Ctrl+other letters: not bound
        keyboard::Event::Char(c) => {
            let idx = byte_index(&lines[*cursor_row], *cursor_col);
            lines[*cursor_row].insert(idx, c);
            *cursor_col += 1;
        }
        keyboard::Event::Backspace => {
            if *cursor_col > 0 {
                let idx = byte_index(&lines[*cursor_row], *cursor_col - 1);
                lines[*cursor_row].remove(idx);
                *cursor_col -= 1;
            } else if *cursor_row > 0 {
                let current = lines.remove(*cursor_row);
                *cursor_row -= 1;
                *cursor_col = lines[*cursor_row].chars().count();
                lines[*cursor_row].push_str(&current);
            }
        }
        keyboard::Event::Enter => {
            let idx = byte_index(&lines[*cursor_row], *cursor_col);
            let rest = lines[*cursor_row].split_off(idx);
            lines.insert(*cursor_row + 1, rest);
            *cursor_row += 1;
            *cursor_col = 0;
        }
        keyboard::Event::Tab => {
            let idx = byte_index(&lines[*cursor_row], *cursor_col);
            lines[*cursor_row].insert_str(idx, "    ");
            *cursor_col += 4;
        }
        keyboard::Event::Left => {
            if *cursor_col > 0 {
                *cursor_col -= 1;
            } else if *cursor_row > 0 {
                *cursor_row -= 1;
                *cursor_col = lines[*cursor_row].chars().count();
            }
        }
        keyboard::Event::Right => {
            if *cursor_col < lines[*cursor_row].chars().count() {
                *cursor_col += 1;
            } else if *cursor_row + 1 < lines.len() {
                *cursor_row += 1;
                *cursor_col = 0;
            }
        }
        keyboard::Event::Up => {
            if *cursor_row > 0 {
                *cursor_row -= 1;
                *cursor_col = (*cursor_col).min(lines[*cursor_row].chars().count());
            }
        }
        keyboard::Event::Down => {
            if *cursor_row + 1 < lines.len() {
                *cursor_row += 1;
                *cursor_col = (*cursor_col).min(lines[*cursor_row].chars().count());
            }
        }
    }
    editor_render(console, lines, *cursor_row, *cursor_col, scroll_offset, status);
}

/// Re-renders the whole visible page from `lines` (the real document —
/// the console is just a display cache) and overlays a block cursor.
/// The bottom row is a status bar (save/open feedback), so only
/// `rows - 1` document lines are shown.
/// Called after every keystroke and after a resize, since both replace
/// the console's pixel buffer wholesale.
fn editor_render(
    console: &mut Console,
    lines: &[String],
    cursor_row: usize,
    cursor_col: usize,
    scroll_offset: &mut usize,
    status: &mut String,
) {
    let rows = console.rows();
    let doc_rows = rows.saturating_sub(1);
    if cursor_row < *scroll_offset {
        *scroll_offset = cursor_row;
    } else if cursor_row >= *scroll_offset + doc_rows {
        *scroll_offset = cursor_row + 1 - doc_rows;
    }
    console.clear();
    for line in lines.iter().skip(*scroll_offset).take(doc_rows) {
        let _ = writeln!(console, "{}", line);
    }
    let sx = font::GLYPH_WIDTH as u32;
    let sy = (doc_rows * font::GLYPH_HEIGHT) as u32;
    let cw = console.width_px();
    gfx::fill_rect(console, 0, sy, cw, font::GLYPH_HEIGHT as u32, EDITOR_STATUS_BG);
    gfx::draw_string(console, sx, sy, status, EDITOR_STATUS_FG, None);
    let visible = (cursor_row.saturating_sub(*scroll_offset)).min(doc_rows.saturating_sub(1));
    let cx = (cursor_col * font::GLYPH_WIDTH) as u32;
    let cy = (visible * font::GLYPH_HEIGHT) as u32;
    gfx::fill_rect(console, cx, cy, font::GLYPH_WIDTH as u32, font::GLYPH_HEIGHT as u32, EDITOR_CURSOR_COLOR);
}

fn taskbar_label_width(title: &str) -> i32 {
    (title.len() * font::GLYPH_WIDTH + 12) as i32
}

/// (minimize_x, maximize_x, close_x) in screen coordinates. `maximize_x`
/// is `None` for non-resizable windows, which don't get a maximize
/// button at all — shared between hit-testing and drawing so they can
/// never disagree about where the buttons are.
fn title_button_positions(wx: i32, content_w: u32, resizable: bool) -> (i32, Option<i32>, i32) {
    let close_x = wx + content_w as i32 - CLOSE_BUTTON_SIZE as i32 - 3;
    let maximize_x = if resizable { Some(close_x - MAXIMIZE_BUTTON_SIZE as i32 - 6) } else { None };
    let minimize_x = maximize_x.unwrap_or(close_x) - MINIMIZE_BUTTON_SIZE as i32 - 6;
    (minimize_x, maximize_x, close_x)
}

fn draw_window(surface: &mut dyn Surface, window: &Window, focused: bool) {
    let (content_w, content_h) = window.content_size();
    let x = window.x as u32;
    let y = window.y as u32;
    let total_h = TITLE_BAR_HEIGHT + content_h;

    // Drop shadow first, so the window itself paints over the part that
    // overlaps it; then a 1px border frame just outside the window edge.
    gfx::fill_rect(surface, x + WINDOW_SHADOW_OFFSET, y + WINDOW_SHADOW_OFFSET, content_w, total_h, WINDOW_SHADOW);
    let border_color = if focused { WINDOW_BORDER_FOCUSED } else { WINDOW_BORDER };
    let bx = x.saturating_sub(1);
    let by = y.saturating_sub(1);
    gfx::fill_rect(surface, bx, by, content_w + 2, total_h + 2, border_color);

    let title_color = if focused { TITLE_FOCUSED } else { TITLE_UNFOCUSED };
    gfx::fill_rect(surface, x, y, content_w, TITLE_BAR_HEIGHT, title_color);
    if focused {
        gfx::fill_rect(surface, x, y, content_w, 1, TITLE_HIGHLIGHT);
    }
    gfx::draw_string(surface, x + 4, y + 6, window.title, 0x00_FFFFFF, None);

    let (minimize_x, maximize_x, close_x) = title_button_positions(window.x, content_w, window.resizable);

    gfx::fill_rect(surface, close_x as u32, y + 3, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_COLOR);
    gfx::draw_string(surface, close_x as u32 + 3, y + 4, "x", 0x00_FFFFFF, None);

    if let Some(max_x) = maximize_x {
        let max_x = max_x as u32;
        gfx::fill_rect(surface, max_x, y + 3, MAXIMIZE_BUTTON_SIZE, CLOSE_BUTTON_SIZE, MAXIMIZE_BUTTON_COLOR);
        // A small square outline reads as "maximize" without needing a
        // dedicated glyph in the 8x8 font.
        let (ix, iy, is) = (max_x + 4, y + 6, 6u32);
        gfx::fill_rect(surface, ix, iy, is, 1, 0x00_FFFFFF);
        gfx::fill_rect(surface, ix, iy + is - 1, is, 1, 0x00_FFFFFF);
        gfx::fill_rect(surface, ix, iy, 1, is, 0x00_FFFFFF);
        gfx::fill_rect(surface, ix + is - 1, iy, 1, is, 0x00_FFFFFF);
    }

    let minimize_x = minimize_x as u32;
    gfx::fill_rect(surface, minimize_x, y + 3, MINIMIZE_BUTTON_SIZE, CLOSE_BUTTON_SIZE, MINIMIZE_BUTTON_COLOR);
    gfx::draw_string(surface, minimize_x + 3, y + 4, "_", 0x00_FFFFFF, None);

    gfx::blit(surface, x, y + TITLE_BAR_HEIGHT, window.content_pixels(), content_w, content_h);

    if window.resizable && !window.maximized {
        let grip_x = x + content_w - RESIZE_GRIP_SIZE;
        let grip_y = y + TITLE_BAR_HEIGHT + content_h - RESIZE_GRIP_SIZE;
        gfx::fill_rect(surface, grip_x, grip_y, RESIZE_GRIP_SIZE, RESIZE_GRIP_SIZE, RESIZE_GRIP_COLOR);
    }
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

/// Loads `/wallpaper.raw` (see `tools/gen_wallpaper.py`): `width*height`
/// little-endian u32 pixels, row-major, no header. Returns `None` on any
/// mismatch (no volume mounted, file missing, wrong size for the current
/// mode) so the caller can fall back to the plain gradient background
/// instead of showing a corrupted image.
fn load_wallpaper(screen_w: u32, screen_h: u32) -> Option<Vec<u32>> {
    if !fat::mounted() {
        return None;
    }
    let bytes = fat::read_file("/wallpaper.raw").ok()?;
    let expected_len = screen_w as usize * screen_h as usize * 4;
    if bytes.len() != expected_len {
        return None;
    }
    Some(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
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
