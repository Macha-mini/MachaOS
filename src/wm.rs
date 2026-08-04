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

const TITLE_BAR_HEIGHT: u32 = 22;
const TASKBAR_HEIGHT: u32 = 30;
const CLOSE_BUTTON_SIZE: u32 = 16;
const MINIMIZE_BUTTON_SIZE: u32 = 16;
const MAXIMIZE_BUTTON_SIZE: u32 = 16;
const BUTTON_RADIUS: u32 = 4;
const RESIZE_GRIP_SIZE: u32 = 12;
const MIN_COLS: usize = 20;
const MIN_ROWS: usize = 5;
const DESKTOP_BG_TOP: u32 = 0x00_3A5680;
const DESKTOP_BG_BOTTOM: u32 = 0x00_1B2A42;

// Title bar
const TITLE_FOCUSED_TOP: u32 = 0x00_4481C8;
const TITLE_FOCUSED_BOTTOM: u32 = 0x00_1D4A7C;
const TITLE_UNFOCUSED_TOP: u32 = 0x00_4D5863;
const TITLE_UNFOCUSED_BOTTOM: u32 = 0x00_313A43;
const TITLE_HILITE: u32 = 0x00_9CC9F2;
const TITLE_TEXT: u32 = 0x00_FFFFFF;
const TITLE_TEXT_UNFOCUSED: u32 = 0x00_B6C2CD;
const TITLE_TEXT_SHADOW: u32 = 0x00_0F2A45;
const TITLE_UNDERLINE: u32 = 0x00_0F1E2E;

// Window frame and drop shadow (drawn as three nested offsets, darkest
// furthest from the window, so the edge fades out like a soft shadow)
const WINDOW_BORDER_FOCUSED: u32 = 0x00_74B3E9;
const WINDOW_BORDER: u32 = 0x00_2A3642;
const WINDOW_SHADOW_OUTER: u32 = 0x00_070C12;
const WINDOW_SHADOW_MID: u32 = 0x00_0E161F;
const WINDOW_SHADOW_INNER: u32 = 0x00_18232F;

// Title bar buttons
const BTN_CLOSE_BASE: u32 = 0x00_C04E45;
const BTN_CLOSE_HOVER: u32 = 0x00_E2685E;
const BTN_CLOSE_HILITE: u32 = 0x00_E2877E;
const BTN_NEUTRAL_BASE: u32 = 0x00_39454F;
const BTN_NEUTRAL_HOVER: u32 = 0x00_505D6B;
const BTN_NEUTRAL_HILITE: u32 = 0x00_55626F;
const BTN_GLYPH: u32 = 0x00_FFFFFF;
const BTN_GLYPH_SHADOW: u32 = 0x00_1A1A1A;
const RESIZE_GRIP_COLOR: u32 = 0x00_9AABBB;

// Taskbar
const TASKBAR_BG_TOP: u32 = 0x00_25313E;
const TASKBAR_BG_BOTTOM: u32 = 0x00_121B24;
const TASKBAR_TOP_LINE: u32 = 0x00_3E5878;
const TASKBAR_DIVIDER: u32 = 0x00_33414F;
const TASKBAR_TEXT: u32 = 0x00_DBE6F0;
const TASKBAR_TEXT_DIM: u32 = 0x00_9AA8B5;

// Start button
const START_BUTTON_WIDTH: u32 = 90;
const START_BTN_TOP: u32 = 0x00_3A6A9C;
const START_BTN_BOTTOM: u32 = 0x00_21456E;
const START_BTN_HOVER_TOP: u32 = 0x00_4478AE;
const START_BTN_HOVER_BOTTOM: u32 = 0x00_295180;
const START_BTN_EDGE: u32 = 0x00_12253C;
const START_LOGO_BG: u32 = 0x00_1C3A5E;
const START_LOGO_GLYPH: u32 = 0x00_8FD0FF;

// Taskbar window buttons
const TB_BTN_FOCUSED_TOP: u32 = 0x00_3C6EA8;
const TB_BTN_FOCUSED_BOTTOM: u32 = 0x00_244A78;
const TB_BTN_TOP: u32 = 0x00_2A3642;
const TB_BTN_BOTTOM: u32 = 0x00_1C262F;
const TB_BTN_HOVER_TOP: u32 = 0x00_35424F;
const TB_BTN_HOVER_BOTTOM: u32 = 0x00_25303A;
const TB_BTN_MIN_TOP: u32 = 0x00_1B242D;
const TB_BTN_MIN_BOTTOM: u32 = 0x00_131A22;
const TB_BTN_EDGE: u32 = 0x00_0E141B;
const TB_ACCENT: u32 = 0x00_6FB1E8;

// Launcher popup
const LAUNCHER_BG: u32 = 0x00_1B2530;
const LAUNCHER_BORDER: u32 = 0x00_74B3E9;
const LAUNCHER_HEADER_TOP: u32 = 0x00_2C4E7A;
const LAUNCHER_HEADER_BOTTOM: u32 = 0x00_1D3A5E;
const LAUNCHER_HEADER_TEXT: u32 = 0x00_FFFFFF;
const LAUNCHER_ITEM_HOVER_TOP: u32 = 0x00_2D4967;
const LAUNCHER_ITEM_HOVER_BOTTOM: u32 = 0x00_223B54;
const LAUNCHER_TEXT: u32 = 0x00_DAE5EE;
const LAUNCHER_TEXT_DIM: u32 = 0x00_93A2B0;
const LAUNCHER_ACCENT: u32 = 0x00_6FB1E8;
const LAUNCHER_WIDTH: u32 = 280;
const LAUNCHER_HEADER_H: u32 = 18;
const LAUNCHER_ITEM_HEIGHT: u32 = 30;
const LAUNCHER_SHADOW: u32 = 0x00_070C12;
// Where taskbar window buttons begin, past the Start button.
const WINDOW_BUTTONS_START_X: i32 = 8 + START_BUTTON_WIDTH as i32 + 10;

// Console / editor content colors
const CONSOLE_FG: u32 = 0x00_E0E0E0;
const CONSOLE_BG: u32 = 0x00_10161C;
const EDITOR_CURSOR_COLOR: u32 = 0x00_FFCC66;
const EDITOR_STATUS_BG: u32 = 0x00_1A2430;
const EDITOR_STATUS_FG: u32 = 0x00_8FB8D8;

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
        let items_h = LAUNCHER_ITEM_HEIGHT * self.launcher_items.len().max(1) as u32;
        let popup_h = LAUNCHER_HEADER_H + items_h + 6;
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
        let rel_y = self.cursor_y - py - LAUNCHER_HEADER_H as i32 - 2;
        if rel_y < 0 {
            return None;
        }
        let idx = (rel_y as u32 / LAUNCHER_ITEM_HEIGHT) as usize;
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
        let px = px as u32;
        let py = py as u32;

        gfx::fill_rect(surface, px + 4, py + 4, pw, ph, LAUNCHER_SHADOW);
        gfx::fill_rounded_rect(surface, px - 1, py - 1, pw + 2, ph + 2, 7, LAUNCHER_BORDER);
        gfx::fill_rounded_rect(surface, px, py, pw, ph, 6, LAUNCHER_BG);
        gfx::fill_rounded_rect_gradient_v(
            surface,
            px,
            py,
            pw,
            LAUNCHER_HEADER_H,
            6,
            LAUNCHER_HEADER_TOP,
            LAUNCHER_HEADER_BOTTOM,
        );
        gfx::fill_rect(surface, px, py + LAUNCHER_HEADER_H, pw, 1, 0x00_14263C);
        gfx::draw_string(surface, px + 10, py + 5, "Applications", LAUNCHER_HEADER_TEXT, None);

        let item_y0 = py + LAUNCHER_HEADER_H + 2;
        for (i, (label, _)) in self.launcher_items.iter().enumerate() {
            let iy = item_y0 + i as u32 * LAUNCHER_ITEM_HEIGHT;
            let hovered = self.cursor_y >= iy as i32
                && self.cursor_y < (iy + LAUNCHER_ITEM_HEIGHT) as i32
                && self.cursor_x >= px as i32 + 2
                && self.cursor_x < px as i32 + pw as i32 - 2;
            if hovered {
                gfx::fill_rect(surface, px + 3, iy + 3, 2, LAUNCHER_ITEM_HEIGHT - 6, LAUNCHER_ACCENT);
                gfx::fill_rect_gradient_v(
                    surface,
                    px + 2,
                    iy,
                    pw - 4,
                    LAUNCHER_ITEM_HEIGHT,
                    LAUNCHER_ITEM_HOVER_TOP,
                    LAUNCHER_ITEM_HOVER_BOTTOM,
                );
            }
            let text_color = if hovered { LAUNCHER_TEXT } else { LAUNCHER_TEXT_DIM };
            gfx::draw_string(surface, px + 12, iy + 11, label, text_color, None);
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
                    draw_window(surface, window, i == self.focused, self.cursor_x, self.cursor_y);
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
        gfx::fill_rect_gradient_v(surface, 0, y, self.screen_w, TASKBAR_HEIGHT, TASKBAR_BG_TOP, TASKBAR_BG_BOTTOM);
        gfx::fill_rect(surface, 0, y, self.screen_w, 1, TASKBAR_TOP_LINE);

        // Start button: gradient pill with a small logo tile.
        let start_hovered = self.start_button_hit();
        let (start_top, start_bottom) = if start_hovered || self.launcher_open {
            (START_BTN_HOVER_TOP, START_BTN_HOVER_BOTTOM)
        } else {
            (START_BTN_TOP, START_BTN_BOTTOM)
        };
        gfx::fill_rounded_rect_gradient_v(surface, 8, y + 3, START_BUTTON_WIDTH, TASKBAR_HEIGHT - 6, 5, start_top, start_bottom);
        gfx::fill_rect(surface, 9, y + TASKBAR_HEIGHT - 4, START_BUTTON_WIDTH - 2, 1, START_BTN_EDGE);
        gfx::fill_rounded_rect(surface, 8 + 5, y + 7, 16, 16, 3, START_LOGO_BG);
        gfx::draw_string(surface, 8 + 9, y + 11, "M", START_LOGO_GLYPH, None);
        gfx::draw_string(surface, 8 + 27, y + 11, "Apps", 0x00_FFFFFF, None);

        let btn_h = TASKBAR_HEIGHT - 6;
        let mut x = WINDOW_BUTTONS_START_X as u32;
        for (i, window) in self.windows.iter().enumerate() {
            if !window.open {
                continue;
            }
            let label_w = taskbar_label_width(window.title) as u32;
            let focused = i == self.focused;
            let hovered = self.cursor_x >= x as i32
                && self.cursor_x < (x + label_w) as i32
                && self.cursor_y >= y as i32 + 3
                && self.cursor_y < y as i32 + TASKBAR_HEIGHT as i32 - 3;
            let (top, bottom) = if focused {
                (TB_BTN_FOCUSED_TOP, TB_BTN_FOCUSED_BOTTOM)
            } else if window.minimized {
                (TB_BTN_MIN_TOP, TB_BTN_MIN_BOTTOM)
            } else if hovered {
                (TB_BTN_HOVER_TOP, TB_BTN_HOVER_BOTTOM)
            } else {
                (TB_BTN_TOP, TB_BTN_BOTTOM)
            };
            gfx::fill_rounded_rect_gradient_v(surface, x, y + 3, label_w, btn_h, BUTTON_RADIUS, top, bottom);
            gfx::fill_rect(surface, x + 1, y + 3 + btn_h - 1, label_w - 2, 1, TB_BTN_EDGE);
            if focused {
                // Small accent pill on the top edge marks the active window.
                gfx::fill_rounded_rect(surface, x + 3, y + 2, label_w - 6, 3, 2, TB_ACCENT);
            }
            let text_color = if window.minimized { TASKBAR_TEXT_DIM } else { TASKBAR_TEXT };
            gfx::draw_string(surface, x + 6, y + 11, window.title, text_color, None);
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
        let clock_x = self.screen_w - clock_w - 12;
        gfx::draw_string(surface, clock_x, y + 11, &clock, TASKBAR_TEXT, None);

        // Background counter tasks keep running (preemptively, via the
        // scheduler in task.rs) whether or not anyone is looking at the
        // Terminal window — this is the visible proof of that.
        let counters = task::COUNTERS
            .iter()
            .map(|c| c.load(core::sync::atomic::Ordering::Relaxed))
            .collect::<Vec<_>>();
        let bg = format!("bg: {} {} {}", counters[0], counters[1], counters[2]);
        let bg_w = (bg.len() * font::GLYPH_WIDTH) as u32;
        let bg_x = clock_x - bg_w - 16;
        gfx::draw_string(surface, bg_x, y + 11, &bg, 0x00_86C77B, None);
        gfx::fill_rect(surface, bg_x - 9, y + 4, 1, TASKBAR_HEIGHT - 8, TASKBAR_DIVIDER);
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

/// 1px-thick diagonal line between two endpoints (any direction),
/// inclusive of both endpoints. `Bresenham`-style step per pixel.
fn draw_diagonal(surface: &mut dyn Surface, x0: u32, y0: u32, x1: u32, y1: u32, color: u32) {
    let (mut x, mut y) = (x0 as i32, y0 as i32);
    let sx = if x1 >= x0 { 1 } else { -1 };
    let sy = if y1 >= y0 { 1 } else { -1 };
    let dx = (x1 as i32 - x0 as i32).abs();
    let dy = (y1 as i32 - y0 as i32).abs();
    let mut err = dx - dy;
    loop {
        if x >= 0 && y >= 0 {
            surface.put_pixel(x as u32, y as u32, color);
        }
        if x == x1 as i32 && y == y1 as i32 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }
}

/// Close glyph: a thick X filling an `s`x`s` box at (x, y).
fn draw_close_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    for i in 0..s {
        surface.put_pixel(x + i, y + i, color);
        surface.put_pixel(x + i + 1, y + i, color);
        surface.put_pixel(x + s - 1 - i, y + i, color);
        if i + 2 <= s {
            surface.put_pixel(x + s - 2 - i, y + i, color);
        }
    }
}

/// Minimize glyph: a thick bottom bar.
fn draw_minimize_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x, y + s - 2, s, 2, color);
}

/// Maximize glyph: a square outline with a thick top edge.
fn draw_maximize_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x, y, s, 2, color);
    gfx::fill_rect(surface, x, y, 1, s, color);
    gfx::fill_rect(surface, x + s - 1, y, 1, s, color);
    gfx::fill_rect(surface, x, y + s - 1, s, 1, color);
}

/// Restore glyph (shown on a maximized window's maximize button): a
/// small square behind a slightly larger one, offset to the bottom-right.
fn draw_restore_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x, y, s - 1, 2, color);
    gfx::fill_rect(surface, x, y, 1, s - 1, color);
    let (ox, oy) = (x + 2, y + 2);
    let os = s - 2;
    gfx::fill_rect(surface, ox, oy, os, 2, color);
    gfx::fill_rect(surface, ox, oy, 1, os, color);
    gfx::fill_rect(surface, ox + os - 1, oy, 1, os, color);
    gfx::fill_rect(surface, ox, oy + os - 1, os, 1, color);
}

/// One rounded title-bar button: gradient-free flat base with a 1px
/// inner top highlight, brightening on hover. The glyph is drawn by the
/// caller afterwards.
fn draw_title_button(surface: &mut dyn Surface, x: u32, y: u32, hovered: bool, base: u32, hover: u32, hilite: u32) {
    let color = if hovered { hover } else { base };
    gfx::fill_rounded_rect(surface, x, y, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_SIZE, BUTTON_RADIUS, color);
    gfx::fill_rect(surface, x + 1, y + 1, CLOSE_BUTTON_SIZE - 2, 1, hilite);
}

fn draw_window(surface: &mut dyn Surface, window: &Window, focused: bool, cursor_x: i32, cursor_y: i32) {
    let (content_w, content_h) = window.content_size();
    let x = window.x as u32;
    let y = window.y as u32;
    let total_h = TITLE_BAR_HEIGHT + content_h;

    // Soft drop shadow: three nested offsets, darkest furthest away.
    gfx::fill_rect(surface, x + 6, y + 6, content_w, total_h, WINDOW_SHADOW_OUTER);
    gfx::fill_rect(surface, x + 4, y + 4, content_w, total_h, WINDOW_SHADOW_MID);
    gfx::fill_rect(surface, x + 2, y + 2, content_w, total_h, WINDOW_SHADOW_INNER);
    let border_color = if focused { WINDOW_BORDER_FOCUSED } else { WINDOW_BORDER };
    let bx = x.saturating_sub(1);
    let by = y.saturating_sub(1);
    gfx::fill_rect(surface, bx, by, content_w + 2, total_h + 2, border_color);

    // Title bar: gradient, bright top edge when focused, text with a
    // soft drop shadow, dark underline separating it from the content.
    let (title_top, title_bottom) = if focused {
        (TITLE_FOCUSED_TOP, TITLE_FOCUSED_BOTTOM)
    } else {
        (TITLE_UNFOCUSED_TOP, TITLE_UNFOCUSED_BOTTOM)
    };
    gfx::fill_rect_gradient_v(surface, x, y, content_w, TITLE_BAR_HEIGHT, title_top, title_bottom);
    if focused {
        gfx::fill_rect(surface, x, y, content_w, 1, TITLE_HILITE);
    }
    let title_color = if focused { TITLE_TEXT } else { TITLE_TEXT_UNFOCUSED };
    let ty = y + (TITLE_BAR_HEIGHT - font::GLYPH_HEIGHT as u32) / 2;
    gfx::draw_string(surface, x + 5, ty + 1, window.title, TITLE_TEXT_SHADOW, None);
    gfx::draw_string(surface, x + 5, ty, window.title, title_color, None);
    gfx::fill_rect(surface, x, y + TITLE_BAR_HEIGHT - 1, content_w, 1, TITLE_UNDERLINE);

    let (minimize_x, maximize_x, close_x) = title_button_positions(window.x, content_w, window.resizable);

    // Buttons: hover state tracks the cursor so they brighten on mouseover.
    let in_button_row = cursor_y >= y as i32 + 3 && cursor_y < y as i32 + 3 + CLOSE_BUTTON_SIZE as i32;
    let hover_min = in_button_row && cursor_x >= minimize_x && cursor_x < minimize_x + MINIMIZE_BUTTON_SIZE as i32;
    let hover_close = in_button_row && cursor_x >= close_x && cursor_x < close_x + CLOSE_BUTTON_SIZE as i32;

    let minimize_x = minimize_x as u32;
    draw_title_button(surface, minimize_x, y + 3, hover_min, BTN_NEUTRAL_BASE, BTN_NEUTRAL_HOVER, BTN_NEUTRAL_HILITE);
    draw_minimize_icon(surface, minimize_x + 4, y + 6, 8, BTN_GLYPH_SHADOW);
    draw_minimize_icon(surface, minimize_x + 4, y + 5, 8, BTN_GLYPH);

    if let Some(max_x) = maximize_x {
        let max_x = max_x as u32;
        let hover_max = in_button_row && cursor_x >= max_x as i32 && cursor_x < max_x as i32 + MAXIMIZE_BUTTON_SIZE as i32;
        draw_title_button(surface, max_x, y + 3, hover_max, BTN_NEUTRAL_BASE, BTN_NEUTRAL_HOVER, BTN_NEUTRAL_HILITE);
        let icon_x = max_x + 4;
        if window.maximized {
            draw_restore_icon(surface, icon_x, y + 7, 8, BTN_GLYPH_SHADOW);
            draw_restore_icon(surface, icon_x, y + 6, 8, BTN_GLYPH);
        } else {
            draw_maximize_icon(surface, icon_x, y + 7, 8, BTN_GLYPH_SHADOW);
            draw_maximize_icon(surface, icon_x, y + 6, 8, BTN_GLYPH);
        }
    }

    let close_x = close_x as u32;
    draw_title_button(surface, close_x, y + 3, hover_close, BTN_CLOSE_BASE, BTN_CLOSE_HOVER, BTN_CLOSE_HILITE);
    draw_close_icon(surface, close_x + 4, y + 7, 8, BTN_GLYPH_SHADOW);
    draw_close_icon(surface, close_x + 4, y + 6, 8, BTN_GLYPH);

    gfx::blit(surface, x, y + TITLE_BAR_HEIGHT, window.content_pixels(), content_w, content_h);

    if window.resizable && !window.maximized {
        let grip_x = x + content_w - RESIZE_GRIP_SIZE;
        let grip_y = y + TITLE_BAR_HEIGHT + content_h - RESIZE_GRIP_SIZE;
        // A clearly visible resize handle: a shaded block with diagonal
        // stripe lines, so it reads as "grab this corner to resize".
        gfx::fill_rect(surface, grip_x, grip_y, RESIZE_GRIP_SIZE, RESIZE_GRIP_SIZE, 0x00_141B23);
        draw_diagonal(surface, grip_x, grip_y + 11, grip_x + 11, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 2, grip_y + 11, grip_x + 11, grip_y + 2, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 9, grip_x + 9, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 4, grip_y + 11, grip_x + 11, grip_y + 4, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 7, grip_x + 7, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 1, grip_x + 1, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 10, grip_y + 11, grip_x + 11, grip_y + 10, RESIZE_GRIP_COLOR);
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
    // Paint the shape eight times offset by one pixel in black first, so
    // the white cursor stays visible over bright parts of the wallpaper.
    for (dx, dy) in [
        (-1i32, -1i32),
        (0, -1),
        (1, -1),
        (-1, 0),
        (1, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
    ] {
        paint_cursor_shape(surface, x + dx, y + dy, 0x00_000000);
    }
    paint_cursor_shape(surface, x, y, 0x00_FFFFFF);
}

fn paint_cursor_shape(surface: &mut dyn Surface, x: i32, y: i32, color: u32) {
    for (row, bits) in CURSOR_BITS.iter().enumerate() {
        for col in 0..CURSOR_W {
            if bits & (1 << (CURSOR_W - 1 - col)) != 0 {
                let px = x + col as i32;
                let py = y + row as i32;
                if px >= 0 && py >= 0 {
                    surface.put_pixel(px as u32, py as u32, color);
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
