//! Window management: window rectangles, z-order, drag/resize/focus/close
//! hit testing, the taskbar, and the compositor that draws everything
//! into the framebuffer's back buffer each frame. The chrome follows the
//! Windows 11 design language: rounded windows over a translucent,
//! centered taskbar, a centered start-menu popup, and flat dark surfaces.

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
use crate::file_explorer::{DragInfo, FileExplorer, FsAction};
use crate::font;
use crate::gfx::{self, Surface};
use crate::image_viewer::ImageViewerApp;
use crate::interrupts;
use crate::io;
use crate::keyboard;
use crate::mouse::MouseEvent;
use crate::multiboot;
use crate::paint::{PaintAction, PaintApp};
use crate::settings::{Settings, SettingsApp, SettingsChange};
use crate::shell::{self, Feed, LineEditor};
use crate::task;

const TITLE_BAR_HEIGHT: u32 = 32;
const TASKBAR_HEIGHT: u32 = 48;
const WINDOW_RADIUS: u32 = 8;
const TITLE_BTN_W: u32 = 40;
const TITLE_BTN_H: u32 = 28;
const TITLE_BTN_GAP: u32 = 2;
const TITLE_BTN_MARGIN_RIGHT: u32 = 6;
const RESIZE_GRIP_SIZE: u32 = 12;
const MIN_COLS: usize = 20;
const MIN_ROWS: usize = 5;
const MAX_TERMINAL_COLS: usize = 100;
const MAX_TERMINAL_ROWS: usize = 30;
const MAX_EDITOR_COLS: usize = 100;
const MAX_EDITOR_ROWS: usize = 20;
const DESKTOP_BG_TOP: u32 = 0x00_0F2E5F;
const DESKTOP_BG_BOTTOM: u32 = 0x00_061630;

/// Console cell counts for a new window, sized so the window fits the
/// current virtual screen at the current font scale — a 100x30 console
/// that was comfortable at 8px glyphs overflows a scaled-down desktop.
/// Terminals take roughly 3/4 of the screen width, like Windows' own.
fn fit_console_cells(screen_w: u32, screen_h: u32, max_cols: usize, max_rows: usize) -> (usize, usize) {
    let avail_w = (screen_w * 3 / 4).saturating_sub(40) / font::glyph_w() as u32;
    let avail_h = screen_h.saturating_sub(TASKBAR_HEIGHT + 80) / font::glyph_h() as u32;
    (
        (avail_w as usize).clamp(MIN_COLS, max_cols),
        (avail_h as usize).clamp(MIN_ROWS, max_rows),
    )
}

// Windows: flat dark surfaces with subtle borders, like Win11 dark mode.
const WIN_BG: u32 = 0x00_202020;
const WIN_BORDER: u32 = 0x00_2B2B2B;
const WIN_BORDER_FOCUSED: u32 = 0x00_4A4A4A;
const TITLE_TEXT: u32 = 0x00_FFFFFF;
const TITLE_TEXT_UNFOCUSED: u32 = 0x00_8A8A8A;
const TITLE_DIVIDER: u32 = 0x00_2A2A2A;

// Title bar buttons: invisible until hover, close turns red.
const BTN_HOVER: u32 = 0x00_2C2C2C;
const BTN_CLOSE_HOVER: u32 = 0x00_C42B1C;
const BTN_GLYPH: u32 = 0x00_FFFFFF;
const BTN_GLYPH_DIM: u32 = 0x00_9A9A9A;
const RESIZE_GRIP_COLOR: u32 = 0x00_4A4A4A;

// Taskbar: translucent acrylic bar, centered icon group.
const TASKBAR_BG: u32 = 0x00_1F1F1F;
const TASKBAR_BG_ALPHA: u32 = 235;
const TASKBAR_TOP_LINE: u32 = 0x00_2E2E2E;
const TASKBAR_TEXT: u32 = 0x00_FFFFFF;
const TASKBAR_TEXT_DIM: u32 = 0x00_8A8A8A;
const TB_BTN_HOVER: u32 = 0x00_2C2C2C;
const TB_BTN_ACTIVE: u32 = 0x00_3A3A3A;
const TB_DIVIDER: u32 = 0x00_2E2E2E;
const START_BTN_W: u32 = 40;
const START_BTN_H: u32 = 36;
const TASKBAR_GAP: i32 = 4;
// Left-edge padding for the taskbar button group and the start menu.
const TASKBAR_PAD: i32 = 12;
// Start-button four-pane logo color (Windows yellow).
const START_LOGO_COLOR: u32 = 0x00_FFB900;

// Start-menu popup (Win11 style: search box + pinned tile grid).
const LAUNCHER_BG: u32 = 0x00_262626;
const LAUNCHER_BG_ALPHA: u32 = 235;
const LAUNCHER_BORDER: u32 = 0x00_3F3F3F;
const LAUNCHER_SEARCH_BG: u32 = 0x00_3A3A3A;
const LAUNCHER_TEXT: u32 = 0x00_FFFFFF;
const LAUNCHER_TEXT_DIM: u32 = 0x00_8A8A8A;
const LAUNCHER_TILE_HOVER: u32 = 0x00_2C2C2C;
const LAUNCHER_W: u32 = 408;
const LAUNCHER_COLS: usize = 4;
const LAUNCHER_TILE_W: u32 = 92;
const LAUNCHER_TILE_H: u32 = 84;
const LAUNCHER_ICON: u32 = 56;
const LAUNCHER_TILES_Y: u32 = 64;

// Right-click context menu (Win11 style: small rounded popup).
const MENU_ITEM_H: u32 = 26;
const MENU_PAD: u32 = 6;
const MENU_W: u32 = 170;
const MENU_BG: u32 = 0x00_252525;
const MENU_BORDER: u32 = 0x00_3F3F3F;
const MENU_HOVER: u32 = 0x00_2C2C2C;
const MENU_TEXT: u32 = 0x00_FFFFFF;

// Console / editor content colors
const CONSOLE_FG: u32 = 0x00_E6E6E6;
const CONSOLE_BG: u32 = 0x00_202020;
const EDITOR_CURSOR_COLOR: u32 = 0x00_FFCC66;
const EDITOR_STATUS_BG: u32 = 0x00_262626;
const EDITOR_STATUS_FG: u32 = 0x00_8FB8D8;

#[derive(Clone)]
enum LauncherAction {
    Terminal,
    Calculator,
    Files,
    Notepad,
    SysInfo,
    Settings,
    Paint,
    ImageViewer,
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
    Files,
    Settings,
    Paint,
    ImageViewer,
}

const APP_ID_COUNT: usize = 8;

impl AppId {
    /// The stable name this app is written as in the desktop session
    /// file (see `session`). Renaming an entry here is a format change.
    fn name(&self) -> &'static str {
        match self {
            AppId::Terminal => "terminal",
            AppId::Calculator => "calculator",
            AppId::Notepad => "notepad",
            AppId::SysInfo => "sysinfo",
            AppId::Files => "files",
            AppId::Settings => "settings",
            AppId::Paint => "paint",
            AppId::ImageViewer => "imageviewer",
        }
    }

    fn from_name(name: &str) -> Option<AppId> {
        match name {
            "terminal" => Some(AppId::Terminal),
            "calculator" => Some(AppId::Calculator),
            "notepad" => Some(AppId::Notepad),
            "sysinfo" => Some(AppId::SysInfo),
            "files" => Some(AppId::Files),
            "settings" => Some(AppId::Settings),
            "paint" => Some(AppId::Paint),
            "imageviewer" => Some(AppId::ImageViewer),
            _ => None,
        }
    }

    fn from_index(index: usize) -> Option<AppId> {
        match index {
            0 => Some(AppId::Terminal),
            1 => Some(AppId::Calculator),
            2 => Some(AppId::Notepad),
            3 => Some(AppId::SysInfo),
            4 => Some(AppId::Files),
            5 => Some(AppId::Settings),
            6 => Some(AppId::Paint),
            7 => Some(AppId::ImageViewer),
            _ => None,
        }
    }
}

/// The launcher action that opens a built-in app, one per `AppId`.
fn builtin_action(id: AppId) -> LauncherAction {
    match id {
        AppId::Terminal => LauncherAction::Terminal,
        AppId::Calculator => LauncherAction::Calculator,
        AppId::Notepad => LauncherAction::Notepad,
        AppId::SysInfo => LauncherAction::SysInfo,
        AppId::Files => LauncherAction::Files,
        AppId::Settings => LauncherAction::Settings,
        AppId::Paint => LauncherAction::Paint,
        AppId::ImageViewer => LauncherAction::ImageViewer,
    }
}

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
    Editor { console: Console, lines: Vec<String>, cursor_row: usize, cursor_col: usize, scroll_offset: usize, status: String, path: String },
    Calculator(CalculatorApp),
    FileExplorer(FileExplorer),
    Settings(SettingsApp),
    Paint(PaintApp),
    ImageViewer(ImageViewerApp),
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
            AppKind::FileExplorer(app) => (app.width(), app.height()),
            AppKind::Settings(app) => (app.width(), app.height()),
            AppKind::Paint(app) => (app.width(), app.height()),
            AppKind::ImageViewer(app) => (app.width(), app.height()),
        }
    }

    fn content_pixels(&self) -> &[u32] {
        match &self.kind {
            AppKind::Terminal { console, .. }
            | AppKind::SysInfo { console }
            | AppKind::Editor { console, .. } => console.pixels(),
            AppKind::Calculator(app) => app.pixels(),
            AppKind::FileExplorer(app) => app.pixels(),
            AppKind::Settings(app) => app.pixels(),
            AppKind::Paint(app) => app.pixels(),
            AppKind::ImageViewer(app) => app.pixels(),
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
            AppKind::Calculator(_)
            | AppKind::FileExplorer(_)
            | AppKind::Settings(_)
            | AppKind::Paint(_)
            | AppKind::ImageViewer(_) => (0, 0),
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

struct FileDragState {
    source_window: usize,
    path: String,
    is_dir: bool,
}

struct PressState {
    window_index: usize,
    item: usize,
    screen_x: i32,
    screen_y: i32,
}

/// A right-click context menu: a small popup listing actions at a
/// screen position. `target_window` is the File Explorer window the
/// menu was opened over (`None` for the desktop menu).
struct ContextMenu {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    target_window: Option<usize>,
    items: Vec<ContextItem>,
}

struct ContextItem {
    label: &'static str,
    action: ContextAction,
}

enum ContextAction {
    /// Run an explorer action on the menu's target window.
    Explorer(crate::file_explorer::ExplorerMenuAction),
    /// Launch a built-in app (desktop menu).
    Launch(LauncherAction),
    /// Reload settings from disk (desktop "Refresh").
    ReloadSettings,
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
    file_drag: Option<FileDragState>,
    press_state: Option<PressState>,
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
    // /system/wallpaper.raw on it, or its size doesn't match the current mode —
    // composite() falls back to the plain gradient fill in that case.
    wallpaper: Option<Vec<u32>>,
    // Persistent system settings loaded from /system/settings.conf.
    settings: Settings,
    // Taskbar clock cache: (uptime second the string was rendered for,
    // formatted "HH:MM  up MM:SS"). Reading the CMOS RTC is slow port
    // I/O, so the clock is only re-read when the displayed second
    // actually changes — never on every composite (mouse moves trigger
    // composites constantly).
    clock_cache: (u64, String),
    // Snapshot of the pixels under the cursor, taken right before the
    // cursor was drawn (plus the position it was taken at), so a
    // cursor-only frame can erase the old cursor by restoring this
    // region and draw the cursor at its new position.
    cursor_save: Option<(i32, i32, Vec<u32>)>,
    // Right-click context menu popup, `None` when closed.
    context_menu: Option<ContextMenu>,
    // Previous right-button state, for press/release edge detection.
    right_was_down: bool,
}

impl WindowManager {
    /// Starts with an empty desktop: no apps are launched at boot — the
    /// start menu opens whatever the user picks, and window geometry is
    /// remembered per app across font-size changes. If a desktop session
    /// was persisted on disk (see `session`), the apps that were open
    /// are restored to where the user left them.
    pub fn new(screen_w: u32, screen_h: u32) -> Self {
        let mut manager = Self {
            windows: Vec::new(),
            focused: 0,
            cursor_x: (screen_w / 2) as i32,
            cursor_y: (screen_h / 2) as i32,
            screen_w,
            screen_h,
            dragging: None,
            resizing: None,
            file_drag: None,
            press_state: None,
            left_was_down: false,
            last_clock_secs: u64::MAX,
            launcher_open: false,
            launcher_items: Vec::new(),
            spawn_counter: 0,
            remembered: [None; APP_ID_COUNT],
            wallpaper: load_wallpaper(screen_w, screen_h),
            settings: Settings::load(),
            clock_cache: (u64::MAX, String::new()),
            cursor_save: None,
            context_menu: None,
            right_was_down: false,
        };
        manager.restore_session();
        manager
    }

    /// Restores the persisted desktop session (see `session`): reopens
    /// the apps that were open when the last session was saved, at their
    /// saved positions/sizes, and repopulates the remembered-geometry
    /// table so closed apps reopen where they were left too. Runs once
    /// at boot, after settings are applied; on any failure (no volume,
    /// no file, unknown app names) the desktop just stays empty.
    fn restore_session(&mut self) {
        let data = crate::session::load_from_disk();
        if data.windows.is_empty() && data.remembered.is_empty() {
            return;
        }
        for entry in &data.remembered {
            let Some(id) = AppId::from_name(&entry.app) else {
                continue;
            };
            self.remembered[id as usize] = Some(RememberedWindow {
                x: entry.x,
                y: entry.y,
                cols: entry.cols,
                rows: entry.rows,
                maximized: entry.maximized,
            });
        }
        let max_x = (self.screen_w as i32 - 40).max(0);
        let max_y = (self.screen_h as i32 - TASKBAR_HEIGHT as i32 - TITLE_BAR_HEIGHT as i32).max(0);
        for entry in &data.windows {
            let Some(id) = AppId::from_name(&entry.app) else {
                continue;
            };
            if self.windows.iter().any(|w| w.open && w.app_id == Some(id)) {
                continue;
            }
            self.run_launcher_action(builtin_action(id));
            let idx = self.focused;
            self.windows[idx].x = entry.x.clamp(0, max_x);
            self.windows[idx].y = entry.y.clamp(0, max_y);
            if !entry.maximized && entry.cols > 0 {
                let cols = entry.cols.max(MIN_COLS);
                let rows = entry.rows.max(MIN_ROWS);
                resize_window(&mut self.windows[idx], cols, rows);
            }
            if entry.maximized {
                self.maximize_window(idx);
            }
            self.windows[idx].minimized = entry.minimized;
        }
        // Rewrite the file so a stale session (apps that no longer exist)
        // doesn't come back on the next boot.
        self.persist_session();
    }

    /// Persists the current desktop state (open windows and remembered
    /// geometry) to `/system/desktop.session`, so the next boot can
    /// restore it. Called after any window opens, closes, moves, resizes,
    /// or changes minimized/maximized state; a failed disk write is
    /// silently ignored (the in-memory session stays authoritative).
    fn persist_session(&self) {
        let mut data = crate::session::SessionData::default();
        for window in &self.windows {
            if !window.open {
                continue;
            }
            let Some(id) = window.app_id else {
                continue;
            };
            let (cols, rows) = window.cols_rows();
            data.windows.push(crate::session::SessionWindow {
                app: id.name().to_string(),
                x: window.x,
                y: window.y,
                cols,
                rows,
                minimized: window.minimized,
                maximized: window.maximized,
            });
        }
        for (index, remembered) in self.remembered.iter().enumerate() {
            let Some(remembered) = remembered else {
                continue;
            };
            let Some(id) = AppId::from_index(index) else {
                continue;
            };
            if data.windows.iter().any(|w| w.app == id.name()) {
                continue;
            }
            data.remembered.push(crate::session::SessionWindow {
                app: id.name().to_string(),
                x: remembered.x,
                y: remembered.y,
                cols: remembered.cols,
                rows: remembered.rows,
                minimized: false,
                maximized: remembered.maximized,
            });
        }
        crate::session::save_to_disk(&data);
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
        let start_x = self.taskbar_layout().0;
        self.cursor_x >= start_x
            && self.cursor_x < start_x + START_BTN_W as i32
            && self.cursor_y >= y + 6
            && self.cursor_y < y + 6 + START_BTN_H as i32
    }

    /// The left-aligned taskbar group: returns (start-button x, then one
    /// (window_index, x, width) per open window). Shared by hit-testing
    /// and drawing so they can never disagree about button placement.
    fn taskbar_layout(&self) -> (i32, Vec<(usize, i32, i32)>) {
        let mut buttons = Vec::new();
        for (i, window) in self.windows.iter().enumerate() {
            if !window.open {
                continue;
            }
            let w = taskbar_label_width(window.title);
            buttons.push((i, 0, w));
        }
        let group_x = TASKBAR_PAD;
        let mut x = group_x + START_BTN_W as i32 + TASKBAR_GAP;
        for b in &mut buttons {
            b.1 = x;
            x += b.2 + TASKBAR_GAP;
        }
        (group_x, buttons)
    }

    /// The cell grid a maximized window should fill: the whole screen
    /// width, minus the taskbar and title bar heights.
    fn max_content_cells(&self) -> (usize, usize) {
        let max_w = self.screen_w;
        let max_h = self.screen_h - TASKBAR_HEIGHT - TITLE_BAR_HEIGHT;
        ((max_w / font::glyph_w() as u32) as usize, (max_h / font::glyph_h() as u32) as usize)
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

    /// Applies a display-resolution change requested by the Settings
    /// app: rescales the virtual framebuffer, re-clamps every window
    /// onto the new screen, and reloads the wallpaper at the new size.
    fn apply_resolution(&mut self, w: u32, h: u32) {
        if !fb::set_virtual_resolution(w, h) {
            return;
        }
        self.screen_w = w;
        self.screen_h = h;
        self.wallpaper = None;
        self.wallpaper = load_wallpaper(w, h);
        self.cursor_x = self.cursor_x.clamp(0, w as i32 - 1);
        self.cursor_y = self.cursor_y.clamp(0, h as i32 - 1);
        let max_x = (w as i32 - 40).max(0);
        let max_y = (h as i32 - TASKBAR_HEIGHT as i32 - TITLE_BAR_HEIGHT as i32).max(0);
        let (max_cols, max_rows) = self.max_content_cells();
        for window in &mut self.windows {
            if window.open && !window.minimized {
                window.x = window.x.clamp(0, max_x);
                window.y = window.y.clamp(0, max_y);
                if window.maximized {
                    let (cols, rows) = window.cols_rows();
                    if cols != max_cols || rows != max_rows {
                        resize_window(window, max_cols, max_rows);
                    }
                }
            }
        }
        self.settings = Settings::load();
        self.persist_session();
    }

    /// Reflows the desktop to a new font scale in place, keeping every
    /// open window (its contents, position, size and minimized/maximized
    /// state) instead of rebuilding the whole desktop from scratch.
    /// Cell-counted windows keep their cell dimensions scaled up by the
    /// larger glyphs (Windows-style), clamped so they still fit on
    /// screen; fixed-pixel apps are left untouched.
    fn apply_font_scale(&mut self, scale: u8) {
        font::set_scale(scale);
        let max_x = (self.screen_w as i32 - 40).max(0);
        let max_y = (self.screen_h as i32 - TASKBAR_HEIGHT as i32 - TITLE_BAR_HEIGHT as i32).max(0);
        let (max_cols, max_rows) = self.max_content_cells();
        let max_cols = max_cols.max(1);
        let max_rows = max_rows.max(1);
        for window in &mut self.windows {
            if !window.open {
                continue;
            }
            window.x = window.x.clamp(0, max_x);
            window.y = window.y.clamp(0, max_y);
            let (cols, rows) = window.cols_rows();
            if cols == 0 {
                // Fixed-pixel apps (Calculator, Settings, ...): their
                // own layouts already adapt to the glyph size.
                continue;
            }
            let (target_cols, target_rows) = if window.maximized {
                (max_cols, max_rows)
            } else {
                (cols.clamp(1, max_cols), rows.clamp(1, max_rows))
            };
            // Always rebuild: the console's pixel buffer is sized at the
            // glyph metrics in effect when it was created, so every
            // console-backed window must be re-rendered at the new size
            // even when its cell count didn't change.
            resize_window(window, target_cols, target_rows);
        }
        self.persist_session();
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
        let rows = (self.launcher_items.len().max(1) + LAUNCHER_COLS - 1) / LAUNCHER_COLS;
        let popup_h = LAUNCHER_TILES_Y + rows as u32 * LAUNCHER_TILE_H + 10;
        let popup_y = self.screen_h as i32 - TASKBAR_HEIGHT as i32 - popup_h as i32 - 10;
        let popup_x = TASKBAR_PAD;
        (popup_x, popup_y, LAUNCHER_W, popup_h)
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
        let base_x = px + 20;
        let base_y = py + LAUNCHER_TILES_Y as i32;
        for (i, _) in self.launcher_items.iter().enumerate() {
            let col = (i % LAUNCHER_COLS) as i32;
            let row = (i / LAUNCHER_COLS) as i32;
            let tx = base_x + col * LAUNCHER_TILE_W as i32;
            let ty = base_y + row * LAUNCHER_TILE_H as i32;
            if self.cursor_x >= tx
                && self.cursor_x < tx + LAUNCHER_TILE_W as i32
                && self.cursor_y >= ty
                && self.cursor_y < ty + LAUNCHER_TILE_H as i32
            {
                return Some(i);
            }
        }
        None
    }

    /// Built-in apps first, then any `.elf` file found in `/bin` on the
    /// mounted FAT32 volume (if any) — rebuilt each time the launcher
    /// opens so newly written files show up without a reboot.
    fn build_launcher_items(&self) -> Vec<(String, LauncherAction)> {
        let mut items = vec![
            ("Terminal".to_string(), LauncherAction::Terminal),
            ("Calculator".to_string(), LauncherAction::Calculator),
            ("File Explorer".to_string(), LauncherAction::Files),
            ("Notepad".to_string(), LauncherAction::Notepad),
            ("Paint".to_string(), LauncherAction::Paint),
            ("Image Viewer".to_string(), LauncherAction::ImageViewer),
            ("System Info".to_string(), LauncherAction::SysInfo),
            ("Settings".to_string(), LauncherAction::Settings),
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
                let (cols, rows) = fit_console_cells(self.screen_w, self.screen_h, MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS);
                let idx = self.spawn_window(
                    "Terminal",
                    true,
                    Some(AppId::Terminal),
                    AppKind::Terminal {
                        console: Console::new(cols, rows, CONSOLE_FG, CONSOLE_BG),
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
            LauncherAction::Files => {
                self.spawn_window(
                    "File Explorer",
                    false,
                    Some(AppId::Files),
                    AppKind::FileExplorer(FileExplorer::new()),
                );
            }
            LauncherAction::Notepad => {
                let (cols, rows) = fit_console_cells(self.screen_w, self.screen_h, MAX_EDITOR_COLS, MAX_EDITOR_ROWS);
                let mut console = Console::new(cols, rows, CONSOLE_FG, CONSOLE_BG);
                let lines = vec![String::new()];
                let mut scroll = 0usize;
                let mut status = String::new();
                editor_render(&mut console, &lines, 0, 0, &mut scroll, &mut status);
                self.spawn_window(
                    "Notepad",
                    true,
                    Some(AppId::Notepad),
                    AppKind::Editor { console, lines, cursor_row: 0, cursor_col: 0, scroll_offset: scroll, status, path: String::new() },
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
            LauncherAction::Settings => {
                let settings_copy = Settings {
                    accent_color: self.settings.accent_color.clone(),
                    wallpaper: self.settings.wallpaper,
                    show_bg_tasks: self.settings.show_bg_tasks,
                    resolution: self.settings.resolution,
                    font_scale_percent: self.settings.font_scale_percent,
                };
                self.spawn_window(
                    "Settings",
                    false,
                    Some(AppId::Settings),
                    AppKind::Settings(SettingsApp::new(settings_copy)),
                );
            }
            LauncherAction::Paint => {
                self.spawn_window(
                    "Paint",
                    false,
                    Some(AppId::Paint),
                    AppKind::Paint(PaintApp::new()),
                );
            }
            LauncherAction::ImageViewer => {
                self.spawn_window(
                    "Image Viewer",
                    false,
                    Some(AppId::ImageViewer),
                    AppKind::ImageViewer(ImageViewerApp::new()),
                );
            }
            LauncherAction::RunElf(path) => {
                self.launch_elf_in_terminal(path);
            }
        }
    }

    /// Spawns a terminal window running `run <path>` (used by the
    /// launcher's `/bin` entries and by the file explorer for `.elf`).
    fn launch_elf_in_terminal(&mut self, path: String) {
        let title: &'static str = alloc::boxed::Box::leak(format!("Terminal: {}", path).into_boxed_str());
        let (cols, rows) = fit_console_cells(self.screen_w, self.screen_h, MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS);
        let idx = self.spawn_window(
            title,
            true,
            None,
            AppKind::Terminal {
                console: Console::new(cols, rows, CONSOLE_FG, CONSOLE_BG),
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

    /// Spawns a Notepad window preloaded with a text file (used by the
    /// file explorer when opening `.txt`/`.md`/... files).
    fn open_notepad_with(&mut self, path: String, content: &[u8]) {
        let (cols, rows) = fit_console_cells(self.screen_w, self.screen_h, MAX_EDITOR_COLS, MAX_EDITOR_ROWS);
        let mut console = Console::new(cols, rows, CONSOLE_FG, CONSOLE_BG);
        let mut lines: Vec<String> = core::str::from_utf8(content)
            .unwrap_or("")
            .split('\n')
            .map(|l| l.to_string())
            .collect();
        if lines.last().map(String::is_empty) == Some(true) {
            lines.pop();
        }
        if lines.is_empty() {
            // An empty (or newline-only) file still gets one empty line
            // to type into.
            lines.push(String::new());
        }
        let mut scroll = 0usize;
        let mut status = format!("opened {} ({} bytes)", path, content.len());
        editor_render(&mut console, &lines, 0, 0, &mut scroll, &mut status);
        let base = path.rsplit('/').next().unwrap_or(&path);
        let title: &'static str = alloc::boxed::Box::leak(format!("Notepad: {}", base).into_boxed_str());
        self.spawn_window(
            title,
            true,
            None,
            AppKind::Editor { console, lines, cursor_row: 0, cursor_col: 0, scroll_offset: scroll, status, path },
        );
    }

    /// Spawns an Image Viewer window preloaded with a `.raw` file's bytes
    /// (used by the file explorer — see `file_explorer::is_raw`). The raw
    /// format (`tools/gen_wallpaper.py`) has no width/height header, so an
    /// arbitrary file opened this way is guessed as square; a non-square
    /// dump just shows `ImageViewerApp::load_image`'s existing "invalid
    /// size" message instead of a wrong picture.
    fn open_image_viewer_with(&mut self, path: String, content: Vec<u8>) {
        let side = isqrt((content.len() / 4) as u32);
        let mut app = ImageViewerApp::new();
        app.load_image(&path, &content, side, side);
        let base = path.rsplit('/').next().unwrap_or(&path);
        let title: &'static str = alloc::boxed::Box::leak(format!("Image Viewer: {}", base).into_boxed_str());
        self.spawn_window(title, true, None, AppKind::ImageViewer(app));
    }

    /// Handles a `FsAction` reported by an app (the file explorer).
    fn dispatch_action(&mut self, action: FsAction) {
        match action {
            FsAction::RunTerminal(path) => self.launch_elf_in_terminal(path),
            FsAction::OpenText(path, content) => self.open_notepad_with(path, &content),
            FsAction::OpenImage(path, content) => self.open_image_viewer_with(path, content),
        }
    }

    /// Win11-style start menu: acrylic panel centered above the taskbar
    /// with a search box and a grid of pinned app tiles.
    fn draw_launcher(&self, surface: &mut dyn Surface) {
        let (px, py, pw, ph) = self.launcher_popup_rect();
        let (px, py) = (px as u32, py as u32);

        gfx::fill_rounded_rect_blend(surface, px + 6, py + 6, pw, ph, 12, 0x00_000000, 90);
        gfx::fill_rounded_rect(surface, px - 1, py - 1, pw + 2, ph + 2, 13, LAUNCHER_BORDER);
        gfx::fill_rounded_rect_blend(surface, px, py, pw, ph, 12, LAUNCHER_BG, LAUNCHER_BG_ALPHA);

        // Search box (decorative for now).
        let search_h = 28;
        gfx::fill_rounded_rect(surface, px + 16, py + 12, pw - 32, search_h, 8, LAUNCHER_SEARCH_BG);
        gfx::draw_string(surface, px + 28, py + 12 + (search_h - font::glyph_h() as u32) / 2, "Search", LAUNCHER_TEXT_DIM, None);

        // "Pinned" header.
        gfx::draw_string(surface, px + 20, py + 48, "Pinned", LAUNCHER_TEXT_DIM, None);

        // App tiles: colored rounded square + letter, label underneath.
        let base_x = px + 20;
        let base_y = py + LAUNCHER_TILES_Y;
        for (i, (label, _)) in self.launcher_items.iter().enumerate() {
            let col = (i % LAUNCHER_COLS) as u32;
            let row = (i / LAUNCHER_COLS) as u32;
            let tx = base_x + col * LAUNCHER_TILE_W;
            let ty = base_y + row * LAUNCHER_TILE_H;
            let hovered = self.cursor_x >= tx as i32
                && self.cursor_x < (tx + LAUNCHER_TILE_W) as i32
                && self.cursor_y >= ty as i32
                && self.cursor_y < (ty + LAUNCHER_TILE_H) as i32;
            if hovered {
                gfx::fill_rounded_rect(surface, tx, ty, LAUNCHER_TILE_W, LAUNCHER_TILE_H, 8, LAUNCHER_TILE_HOVER);
            }
            let icon_x = tx + (LAUNCHER_TILE_W - LAUNCHER_ICON) / 2;
            let icon_y = ty + 6;
            gfx::fill_rounded_rect(surface, icon_x, icon_y, LAUNCHER_ICON, LAUNCHER_ICON, 12, launcher_icon_color(label));
            let letter = label.bytes().next().unwrap_or(b'?');
            let lx = icon_x + (LAUNCHER_ICON - font::glyph_w() as u32) / 2;
            let ly = icon_y + (LAUNCHER_ICON - font::glyph_h() as u32) / 2;
            gfx::draw_char(surface, lx, ly, letter, 0x00_FFFFFF, None);

            let label_text = truncate_label(label, 10);
            let label_w = (label_text.len() as u32 * font::glyph_w() as u32) as u32;
            let lx = tx + (LAUNCHER_TILE_W - label_w) / 2;
            let ly = ty + 6 + LAUNCHER_ICON + 6;
            gfx::draw_string(surface, lx, ly, &label_text, LAUNCHER_TEXT, None);
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) {
        // Esc closes a right-click context menu before anything else
        // sees the key.
        if let keyboard::Event::Escape = event {
            if self.context_menu.take().is_some() {
                return;
            }
        }
        if self.windows.is_empty() {
            return;
        }
        // The file explorer reports "open this file" actions that need
        // the window manager, so collect it inside the borrow and act
        // on it after `window` is released.
        let action = {
            let window = &mut self.windows[self.focused];
            match &mut window.kind {
                AppKind::Terminal { console, editor } => {
                    io::set_console_sink(Some(console));
                    if let Feed::Line(line) = editor.feed(event) {
                        shell::execute(&line);
                        print!("{}", shell::prompt());
                    }
                    io::set_console_sink(None);
                    None
                }
                AppKind::Editor { console, lines, cursor_row, cursor_col, scroll_offset, status, path } => {
                    editor_handle_key(console, lines, cursor_row, cursor_col, scroll_offset, status, path, event);
                    None
                }
                AppKind::FileExplorer(app) => app.handle_key(event),
                AppKind::Settings(_) => None,
                AppKind::Paint(app) => {
                    let action = app.handle_key(event);
                    if let Some(PaintAction::Load) = action {
                        // TODO: Open file picker
                        None
                    } else {
                        None
                    }
                }
                AppKind::ImageViewer(app) => {
                    app.handle_key(event);
                    None
                }
                AppKind::SysInfo { .. } | AppKind::Calculator(_) => None,
            }
        };
        if let Some(action) = action {
            self.dispatch_action(action);
        }
    }

    /// Handles one mouse event. Returns `true` when the event changed
    /// something that requires a full recomposite (a click, a drag, a
    /// resize, hovering over chrome with hover states, ...), and
    /// `false` for a pure cursor move over inert surfaces — in which
    /// case the desktop loop can use the cheap cursor-only composite
    /// path instead of redrawing the whole desktop.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let old_x = self.cursor_x;
        let old_y = self.cursor_y;
        self.cursor_x = (self.cursor_x + event.dx).clamp(0, self.screen_w as i32 - 1);
        self.cursor_y = (self.cursor_y + event.dy).clamp(0, self.screen_h as i32 - 1);
        let moved = old_x != self.cursor_x || old_y != self.cursor_y;

        let just_pressed = event.left && !self.left_was_down;
        let just_released = !event.left && self.left_was_down;
        self.left_was_down = event.left;

        let just_pressed_right = event.right && !self.right_was_down;
        self.right_was_down = event.right;

        // Right-click: open (or reposition) the context menu at the
        // cursor. Always needs a full recomposite to draw the popup.
        if just_pressed_right {
            self.open_context_menu();
            return true;
        }

        // Left-click with the context menu open routes to the menu
        // first: an item click runs its action, a click outside closes
        // the menu and falls through to the normal click handling.
        if just_pressed && self.context_menu.is_some() {
            if self.click_context_menu() {
                return true;
            }
        }

        if let Some(resize) = &mut self.resizing {
            if event.left {
                let index = resize.window_index;
                let (wx, wy) = (self.windows[index].x, self.windows[index].y);
                let local_w = (self.cursor_x - wx).max(0) as u32;
                let local_h = (self.cursor_y - wy - TITLE_BAR_HEIGHT as i32).max(0) as u32;
                let cols = ((local_w / font::glyph_w() as u32) as usize).max(MIN_COLS);
                let rows = ((local_h / font::glyph_h() as u32) as usize).max(MIN_ROWS);
                if cols != resize.last_cols || rows != resize.last_rows {
                    resize.last_cols = cols;
                    resize.last_rows = rows;
                    resize_window(&mut self.windows[index], cols, rows);
                }
            }
            if just_released {
                self.resizing = None;
                self.persist_session();
            }
            return true;
        }

        // A file drag owns the mouse until release; it must not be
        // mistaken for moving the whole window underneath it.
        if self.file_drag.is_some() {
            if just_released {
                self.finish_file_drag();
            }
            return true;
        }

        // A normal list click becomes a file drag only after the cursor
        // moves a few pixels, preserving single- and double-click behavior.
        if let Some(press) = &self.press_state {
            if event.left
                && (self.cursor_x - press.screen_x).abs() + (self.cursor_y - press.screen_y).abs() >= 6
            {
                self.begin_file_drag();
                if self.file_drag.is_some() {
                    return true;
                }
            }
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
            if self.dragging.is_some() {
                self.persist_session();
            }
            self.dragging = None;
            self.press_state = None;
        }
        if just_pressed {
            self.handle_click();
            return true;
        }

        // A pure cursor move over inert surfaces (no button, no drag,
        // no hover-rendering chrome under the cursor) only needs the
        // cursor redrawn, not the whole desktop. The *old* cursor
        // position counts too: when the cursor just left a hoverable
        // area, one more full composite is needed to erase the stale
        // hover highlight it left behind.
        if moved && !event.left && self.dragging.is_none() && self.resizing.is_none() {
            !(self.cursor_over_hoverable_at(self.cursor_x, self.cursor_y)
                || self.cursor_over_hoverable_at(old_x, old_y))
        } else {
            true
        }
    }

    /// Whether the cursor sits over anything whose hover state must be
    /// re-rendered when the cursor moves: window title bars
    /// (min/max/close buttons), File Explorer and Settings content
    /// (hover-highlighted rows), the taskbar, and the launcher.
    /// When the cursor is over plain desktop/window content instead, a
    /// cursor-only redraw is sufficient.
    fn cursor_over_hoverable_at(&self, cx: i32, cy: i32) -> bool {
        // The context menu highlights items under the cursor.
        if let Some(menu) = &self.context_menu {
            if cx >= menu.x
                && cx < menu.x + menu.w as i32
                && cy >= menu.y
                && cy < menu.y + menu.h as i32
            {
                return true;
            }
        }
        // Taskbar row (buttons + start button light up on hover).
        if cy >= (self.screen_h - TASKBAR_HEIGHT) as i32 {
            return true;
        }
        // Launcher tiles.
        if self.launcher_open {
            let (lx, ly, lw, lh) = self.launcher_popup_rect();
            if cx >= lx && cx < lx + lw as i32 && cy >= ly && cy < ly + lh as i32 {
                return true;
            }
        }
        for window in &self.windows {
            if !window.open || window.minimized {
                continue;
            }
            let (cw, ch) = window.content_size();
            let in_title = cy >= window.y && cy < window.y + TITLE_BAR_HEIGHT as i32;
            let in_content = cy >= window.y + TITLE_BAR_HEIGHT as i32
                && cy < window.y + TITLE_BAR_HEIGHT as i32 + ch as i32;
            let in_x = cx >= window.x && cx < window.x + cw as i32;
            if in_x && (in_title || in_content) {
                // Title bars always have hover states; among the apps,
                // only File Explorer and Settings re-render on cursor
                // moves (hover highlights). Terminals, Calculator,
                // Paint, and the Image Viewer don't.
                if in_title {
                    return true;
                }
                if matches!(window.kind, AppKind::FileExplorer(_) | AppKind::Settings(_)) {
                    return true;
                }
            }
        }
        false
    }

    /// Opens the right-click context menu at the cursor: a File
    /// Explorer menu when the click is over an open explorer's content
    /// area (selecting the item under the cursor), otherwise the
    /// desktop menu. Closing any previously open menu first.
    fn open_context_menu(&mut self) {
        self.context_menu = None;
        let (cx, cy) = (self.cursor_x, self.cursor_y);

        // Topmost open explorer window whose content area contains the
        // cursor (checked in z-order, so an overlapping explorer wins).
        let mut target: Option<usize> = None;
        for i in (0..self.windows.len()).rev() {
            let window = &self.windows[i];
            if !window.open || window.minimized {
                continue;
            }
            let (cw, ch) = window.content_size();
            let x0 = window.x;
            let y0 = window.y + TITLE_BAR_HEIGHT as i32;
            if cx >= x0 && cx < x0 + cw as i32 && cy >= y0 && cy < y0 + ch as i32 {
                if matches!(window.kind, AppKind::FileExplorer(_)) {
                    target = Some(i);
                    break;
                }
                // The cursor is over some other app's content: no menu
                // (only the explorer and the desktop have one).
                return;
            }
        }

        let items: Vec<ContextItem> = match target {
            Some(index) => {
                // Select the entry under the cursor so menu actions
                // target it.
                let local_x = cx - self.windows[index].x;
                let local_y = cy - self.windows[index].y - TITLE_BAR_HEIGHT as i32;
                if let AppKind::FileExplorer(app) = &mut self.windows[index].kind {
                    app.select_at(local_x, local_y);
                }
                vec![
                    ContextItem { label: "Open", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::Open) },
                    ContextItem { label: "Rename", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::Rename) },
                    ContextItem { label: "Delete", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::Delete) },
                    ContextItem { label: "New Folder", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::NewFolder) },
                    ContextItem { label: "New File", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::NewFile) },
                    ContextItem { label: "Refresh", action: ContextAction::Explorer(crate::file_explorer::ExplorerMenuAction::Refresh) },
                ]
            }
            None => vec![
                ContextItem { label: "Terminal", action: ContextAction::Launch(LauncherAction::Terminal) },
                ContextItem { label: "File Explorer", action: ContextAction::Launch(LauncherAction::Files) },
                ContextItem { label: "Calculator", action: ContextAction::Launch(LauncherAction::Calculator) },
                ContextItem { label: "Settings", action: ContextAction::Launch(LauncherAction::Settings) },
                ContextItem { label: "Refresh", action: ContextAction::ReloadSettings },
            ],
        };

        let w = MENU_W;
        let h = MENU_PAD * 2 + items.len() as u32 * MENU_ITEM_H;
        // Open down-right from the cursor, flipped up when it would
        // overflow the bottom of the screen, clamped to the edges.
        let x = (cx + 4).min(self.screen_w as i32 - w as i32).max(0);
        let y = if cy + 4 + h as i32 <= self.screen_h as i32 {
            (cy + 4).max(0)
        } else {
            (cy - h as i32 - 4).max(0)
        };
        self.context_menu = Some(ContextMenu {
            x,
            y,
            w,
            h,
            target_window: target,
            items,
        });
    }

    /// Handles a left-click while the context menu is open. Returns
    /// `true` when the click was consumed by the menu (an item was
    /// activated); `false` when it landed outside (the menu is closed
    /// and the click should fall through to the normal handler).
    fn click_context_menu(&mut self) -> bool {
        let Some(menu) = self.context_menu.take() else {
            return false;
        };
        let (cx, cy) = (self.cursor_x, self.cursor_y);
        let inside = cx >= menu.x && cx < menu.x + menu.w as i32 && cy >= menu.y && cy < menu.y + menu.h as i32;
        if !inside {
            return false;
        }
        let local_y = cy - menu.y;
        if local_y < MENU_PAD as i32 {
            return true;
        }
        let idx = ((local_y - MENU_PAD as i32) / MENU_ITEM_H as i32) as usize;
        let action = menu.items.get(idx).map(|item| &item.action);
        if let Some(action) = action {
            self.run_context_action(action, menu.target_window);
        }
        true
    }

    fn run_context_action(&mut self, action: &ContextAction, target_window: Option<usize>) {
        match action {
            ContextAction::Explorer(explorer_action) => {
                let Some(index) = target_window else {
                    return;
                };
                let Some(window) = self.windows.get_mut(index) else {
                    return;
                };
                let fs_action = match &mut window.kind {
                    AppKind::FileExplorer(app) => app.context_action(*explorer_action),
                    _ => None,
                };
                if let Some(fs_action) = fs_action {
                    self.dispatch_action(fs_action);
                }
            }
            ContextAction::Launch(launcher_action) => {
                let action = launcher_action.clone();
                self.run_launcher_action(action);
            }
            ContextAction::ReloadSettings => {
                self.settings = Settings::load();
            }
        }
    }

    fn set_explorer_drag(&mut self, drag: Option<DragInfo>) {
        for window in &mut self.windows {
            if let AppKind::FileExplorer(app) = &mut window.kind {
                app.set_drag(drag.clone());
            }
        }
    }

    fn begin_file_drag(&mut self) {
        let Some(press) = self.press_state.take() else { return };
        let Some(window) = self.windows.get(press.window_index) else { return };
        let source = match &window.kind {
            AppKind::FileExplorer(app) => app.entry_path(press.item),
            _ => None,
        };
        let Some((path, is_dir)) = source else { return };
        self.file_drag = Some(FileDragState {
            source_window: press.window_index,
            path: path.clone(),
            is_dir,
        });
        self.set_explorer_drag(Some(DragInfo { path, is_dir }));
    }

    fn finish_file_drag(&mut self) {
        let Some(drag) = self.file_drag.take() else { return };
        let mut target_index = None;
        for i in (0..self.windows.len()).rev() {
            let window = &self.windows[i];
            if !window.open || window.minimized {
                continue;
            }
            let (content_w, content_h) = window.content_size();
            let in_content = self.cursor_x >= window.x
                && self.cursor_x < window.x + content_w as i32
                && self.cursor_y >= window.y + TITLE_BAR_HEIGHT as i32
                && self.cursor_y < window.y + TITLE_BAR_HEIGHT as i32 + content_h as i32;
            if in_content {
                target_index = Some(i);
                break;
            }
        }

        let mut handled = false;
        if let Some(index) = target_index {
            let (wx, wy) = (self.windows[index].x, self.windows[index].y);
            let local_x = self.cursor_x - wx;
            let local_y = self.cursor_y - wy - TITLE_BAR_HEIGHT as i32;
            handled = match &mut self.windows[index].kind {
                AppKind::FileExplorer(app) => app.handle_drop(local_x, local_y, &drag.path, drag.is_dir),
                _ => false,
            };
            if handled && index != drag.source_window {
                if let Some(AppKind::FileExplorer(app)) = self.windows.get_mut(drag.source_window).map(|w| &mut w.kind) {
                    app.refresh_now();
                }
            }
        }
        self.press_state = None;
        self.set_explorer_drag(None);
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
                self.persist_session();
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
            let (_, buttons) = self.taskbar_layout();
            for (i, x, w) in buttons {
                if self.cursor_x >= x && self.cursor_x < x + w {
                    self.windows[i].minimized = false;
                    self.raise(i);
                    self.persist_session();
                    return;
                }
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
                    let cols = content_w as usize / font::glyph_w();
                    let rows = content_h as usize / font::glyph_h();
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

                let in_button_row = self.cursor_y >= wy + 2 && self.cursor_y < wy + 2 + TITLE_BTN_H as i32;
                if in_button_row && self.cursor_x >= close_x && self.cursor_x < close_x + TITLE_BTN_W as i32 {
                    self.remember_geometry(i);
                    self.windows[i].open = false;
                    self.refocus_after_hide(i);
                    self.persist_session();
                    return;
                }
                if let Some(max_x) = maximize_x {
                    if in_button_row && self.cursor_x >= max_x && self.cursor_x < max_x + TITLE_BTN_W as i32 {
                        self.raise(i);
                        let focused_index = self.focused;
                        if maximized {
                            self.unmaximize_window(focused_index);
                        } else {
                            self.maximize_window(focused_index);
                        }
                        self.persist_session();
                        return;
                    }
                }
                if in_button_row && self.cursor_x >= minimize_x && self.cursor_x < minimize_x + TITLE_BTN_W as i32 {
                    self.windows[i].minimized = true;
                    self.refocus_after_hide(i);
                    self.persist_session();
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
                let (action, item, settings_change) = {
                    let window = &mut self.windows[self.focused];
                    let mut settings_change = SettingsChange::None;
                    let (action, item) = match &mut window.kind {
                        AppKind::Calculator(app) => {
                            app.handle_click(local_x, local_y);
                            (None, None)
                        }
                        AppKind::FileExplorer(app) => {
                            let item = app.item_at(local_x, local_y);
                            let action = app.handle_click(local_x, local_y);
                            let item = app.drag_candidate(item);
                            (action, item)
                        }
                        AppKind::Settings(app) => {
                            settings_change = app.handle_click(local_x, local_y);
                            (None, None)
                        }
                        AppKind::Paint(app) => {
                            app.handle_click(local_x, local_y);
                            (None, None)
                        }
                        AppKind::ImageViewer(app) => {
                            app.handle_click(local_x, local_y);
                            (None, None)
                        }
                        _ => (None, None),
                    };
                    (action, item, settings_change)
                };
                if let Some(action) = action {
                    self.press_state = None;
                    self.dispatch_action(action);
                } else if let Some(item) = item {
                    self.press_state = Some(PressState {
                        window_index: self.focused,
                        item,
                        screen_x: self.cursor_x,
                        screen_y: self.cursor_y,
                    });
                } else {
                    self.press_state = None;
                }
                match settings_change {
                    SettingsChange::None => {}
                    SettingsChange::Reload => self.settings = Settings::load(),
                    SettingsChange::Resolution(w, h) => self.apply_resolution(w, h),
                    SettingsChange::FontScale(scale) => self.apply_font_scale(scale),
                }
                return;
            }
        }
    }

    pub fn composite(&mut self) {
        // Refresh the taskbar clock cache when the displayed second
        // changes. CMOS RTC reads are slow port I/O, so this runs at
        // most once per second, not on every composite (mouse moves
        // trigger composites constantly).
        let secs = interrupts::ticks() / 100;
        if secs != self.clock_cache.0 {
            self.clock_cache.0 = secs;
            let now = crate::rtc::now();
            self.clock_cache.1 = format!(
                "{:02}:{:02}  up {:02}:{:02}",
                now.hour,
                now.minute,
                secs / 60,
                secs % 60
            );
        }
        fb::with_surface(|surface| {
            // Use wallpaper only if settings enable it and it's available.
            if self.settings.wallpaper {
                if let Some(pixels) = &self.wallpaper {
                    gfx::blit(surface, 0, 0, pixels, self.screen_w, self.screen_h);
                } else {
                    gfx::fill_rect_gradient_v(surface, 0, 0, self.screen_w, self.screen_h, DESKTOP_BG_TOP, DESKTOP_BG_BOTTOM);
                }
            } else {
                gfx::fill_rect_gradient_v(surface, 0, 0, self.screen_w, self.screen_h, DESKTOP_BG_TOP, DESKTOP_BG_BOTTOM);
            }
            for (i, window) in self.windows.iter_mut().enumerate() {
                if window.open && !window.minimized {
                    // The file explorer re-renders on cursor moves so
                    // its hover highlights follow the mouse. The cursor
                    // position is clamped to a sentinel when it's
                    // outside the window's content area, so moving the
                    // mouse elsewhere on the desktop doesn't re-render
                    // the whole 700x440 buffer on every event.
                    if let AppKind::FileExplorer(app) = &mut window.kind {
                        let (lx, ly) = content_cursor(
                            self.cursor_x - window.x,
                            self.cursor_y - window.y - TITLE_BAR_HEIGHT as i32,
                            app.width(),
                            app.height(),
                        );
                        app.render_hover(lx, ly);
                    }
                    // Settings app also re-renders on cursor moves for hover.
                    if let AppKind::Settings(app) = &mut window.kind {
                        let (lx, ly) = content_cursor(
                            self.cursor_x - window.x,
                            self.cursor_y - window.y - TITLE_BAR_HEIGHT as i32,
                            app.width(),
                            app.height(),
                        );
                        app.render_hover(lx, ly);
                    }
                    // Paint app re-renders on mouse moves for drawing.
                    if let AppKind::Paint(app) = &mut window.kind {
                        app.handle_mouse_move(
                            self.cursor_x - window.x,
                            self.cursor_y - window.y - TITLE_BAR_HEIGHT as i32,
                            self.left_was_down,
                        );
                    }
                    draw_window(surface, window, i == self.focused, self.cursor_x, self.cursor_y);
                }
            }
            self.draw_taskbar(surface);
            if self.launcher_open {
                self.draw_launcher(surface);
            }
            if let Some(menu) = &self.context_menu {
                draw_context_menu(surface, menu, self.cursor_x, self.cursor_y);
            }
            if let Some(drag) = &self.file_drag {
                draw_file_drag_tag(surface, self.cursor_x, self.cursor_y, &drag.path, drag.is_dir);
            }
        });
        // Snapshot the region under the cursor *before* drawing it, so a
        // later cursor-only frame can erase it by restoring this region.
        self.cursor_save = Some((self.cursor_x, self.cursor_y, fb::snapshot_region(
            (self.cursor_x - CURSOR_SAVE_MARGIN).max(0) as u32,
            (self.cursor_y - CURSOR_SAVE_MARGIN).max(0) as u32,
            CURSOR_SAVE_W,
            CURSOR_SAVE_H,
        )));
        fb::with_surface(|surface| {
            draw_cursor(surface, self.cursor_x, self.cursor_y);
        });
        fb::present();
    }

    /// Cheap path for a pure cursor move: erase the old cursor by
    /// restoring the pixels saved under it during the last composite,
    /// draw the cursor at its new position, and push only the affected
    /// region to the framebuffer — instead of recompositing the whole
    /// desktop. Only valid when nothing but the cursor moved; the
    /// desktop loop guarantees that (see `handle_mouse`'s return value).
    pub fn composite_cursor_only(&mut self) {
        let (old_x, old_y, saved) = match self.cursor_save.take() {
            Some(saved) => saved,
            // No prior snapshot (e.g. first frame); just do a full
            // composite instead.
            None => return self.composite(),
        };
        // Erase the old cursor.
        fb::restore_region(
            (old_x - CURSOR_SAVE_MARGIN).max(0) as u32,
            (old_y - CURSOR_SAVE_MARGIN).max(0) as u32,
            CURSOR_SAVE_W,
            CURSOR_SAVE_H,
            &saved,
        );
        // The old and new cursor positions may both need updating on
        // screen: restore_region changed the old one, and the cursor
        // will be drawn at the new one.
        let min_x = (old_x.min(self.cursor_x) - CURSOR_SAVE_MARGIN).max(0) as u32;
        let min_y = (old_y.min(self.cursor_y) - CURSOR_SAVE_MARGIN).max(0) as u32;
        let max_x = (old_x.max(self.cursor_x) + CURSOR_SAVE_W as i32 - CURSOR_SAVE_MARGIN) as u32;
        let max_y = (old_y.max(self.cursor_y) + CURSOR_SAVE_H as i32 - CURSOR_SAVE_MARGIN) as u32;
        let region_w = max_x.saturating_sub(min_x).min(self.screen_w - min_x.min(self.screen_w));
        let region_h = max_y.saturating_sub(min_y).min(self.screen_h - min_y.min(self.screen_h));

        // Snapshot under the new cursor position, then draw it.
        self.cursor_save = Some((self.cursor_x, self.cursor_y, fb::snapshot_region(
            (self.cursor_x - CURSOR_SAVE_MARGIN).max(0) as u32,
            (self.cursor_y - CURSOR_SAVE_MARGIN).max(0) as u32,
            CURSOR_SAVE_W,
            CURSOR_SAVE_H,
        )));
        fb::with_surface(|surface| {
            draw_cursor(surface, self.cursor_x, self.cursor_y);
        });
        fb::present_region(min_x, min_y, region_w, region_h);
    }

    /// Win11-style taskbar: a translucent acrylic bar with a centered
    /// group of rounded buttons, the start button at the front of the
    /// group, and the clock in a tray on the right.
    fn draw_taskbar(&self, surface: &mut dyn Surface) {
        let y = self.screen_h - TASKBAR_HEIGHT;
        gfx::fill_rect_blend(surface, 0, y, self.screen_w, TASKBAR_HEIGHT, TASKBAR_BG, TASKBAR_BG_ALPHA);
        gfx::fill_rect(surface, 0, y, self.screen_w, 1, TASKBAR_TOP_LINE);

        let accent = self.settings.accent_color_rgb();
        let (start_x, buttons) = self.taskbar_layout();
        let start_hovered = self.start_button_hit() || self.launcher_open;

        // Start button: Win11 four-pane logo in a hover pill.
        if start_hovered {
            gfx::fill_rounded_rect(surface, start_x as u32, y + 6, START_BTN_W, START_BTN_H, 8, TB_BTN_HOVER);
        }
        let pane = 4 + font::scale() as u32 * 2;
        let logo = 2 * pane + 2;
        let lx = start_x as u32 + (START_BTN_W - logo) / 2;
        let ly = y + 6 + (START_BTN_H - logo) / 2;
        gfx::fill_rounded_rect(surface, lx, ly, pane, pane, 2, START_LOGO_COLOR);
        gfx::fill_rounded_rect(surface, lx + pane + 2, ly, pane, pane, 2, START_LOGO_COLOR);
        gfx::fill_rounded_rect(surface, lx, ly + pane + 2, pane, pane, 2, START_LOGO_COLOR);
        gfx::fill_rounded_rect(surface, lx + pane + 2, ly + pane + 2, pane, pane, 2, START_LOGO_COLOR);

        // Centered app buttons: icon tile + label.
        for (i, x, w) in buttons {
            let window = &self.windows[i];
            let focused = i == self.focused;
            let hovered = self.cursor_x >= x
                && self.cursor_x < x + w
                && self.cursor_y >= y as i32 + 6
                && self.cursor_y < y as i32 + 6 + START_BTN_H as i32;
            let (bx, bw) = (x as u32, w as u32);
            let bg = if focused {
                TB_BTN_ACTIVE
            } else if hovered {
                TB_BTN_HOVER
            } else {
                TASKBAR_BG
            };
            gfx::fill_rounded_rect(surface, bx, y + 6, bw, START_BTN_H, 6, bg);
            let icon = icon_size(16);
            let ix = bx + 8;
            let iy = y + 6 + (START_BTN_H - icon) / 2;
            draw_app_icon(surface, ix, iy, icon, &window.kind, window.title);
            let text_color = if window.minimized { TASKBAR_TEXT_DIM } else { TASKBAR_TEXT };
            let ty = y + 6 + (START_BTN_H - font::glyph_h() as u32) / 2;
            gfx::draw_string(surface, bx + 8 + icon + 6, ty, window.title, text_color, None);
            if focused {
                // Active indicator pill under the icon.
                gfx::fill_rounded_rect(surface, ix, y + 42, icon, 3, 2, accent);
            }
        }

        // System tray: clock on the right, optional bg-task counters.
        // The clock string is cached and only refreshed (via the slow
        // CMOS RTC port reads) when the displayed second changes.
        let clock = &self.clock_cache.1;
        let clock_w = (clock.len() * font::glyph_w()) as u32;
        let clock_x = self.screen_w - clock_w - 12;
        gfx::draw_string(surface, clock_x, y + (TASKBAR_HEIGHT - font::glyph_h() as u32) / 2, &clock, TASKBAR_TEXT, None);

        // Background counter tasks keep running (preemptively, via the
        // scheduler in task.rs) whether or not anyone is looking at the
        // Terminal window — this is the visible proof of that.
        if self.settings.show_bg_tasks {
            let counters = task::COUNTERS
                .iter()
                .map(|c| c.load(core::sync::atomic::Ordering::Relaxed))
                .collect::<Vec<_>>();
            let bg = format!("bg: {} {} {}", counters[0], counters[1], counters[2]);
            let bg_w = (bg.len() * font::glyph_w()) as u32;
            let bg_x = clock_x - bg_w - 16;
            gfx::draw_string(surface, bg_x, y + (TASKBAR_HEIGHT - font::glyph_h() as u32) / 2, &bg, 0x00_86C77B, None);
            gfx::fill_rect(surface, bg_x - 9, y + 6, 1, TASKBAR_HEIGHT - 12, TB_DIVIDER);
        }
    }
}

/// Content-local cursor coordinates for hover-rendering apps, clamped
/// to a sentinel outside the app's buffer so a window whose cursor is
/// elsewhere doesn't re-render its whole buffer on every mouse event.
fn content_cursor(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
        (x, y)
    } else {
        (i32::MAX, i32::MAX)
    }
}

/// Win11-style right-click context menu: a small rounded popup listing
/// the menu's actions, with a hover highlight under the cursor.
fn draw_context_menu(surface: &mut dyn Surface, menu: &ContextMenu, cx: i32, cy: i32) {
    let (x, y) = (menu.x as u32, menu.y as u32);
    // Soft drop shadow, then the border + surface.
    gfx::fill_rounded_rect_blend(surface, x + 3, y + 3, menu.w, menu.h, 8, 0x00_000000, 70);
    gfx::fill_rounded_rect(surface, x - 1, y - 1, menu.w + 2, menu.h + 2, 9, MENU_BORDER);
    gfx::fill_rounded_rect(surface, x, y, menu.w, menu.h, 8, MENU_BG);
    for (i, item) in menu.items.iter().enumerate() {
        let iy = y + MENU_PAD + i as u32 * MENU_ITEM_H;
        let hovered = cx >= x as i32
            && cx < (x + menu.w) as i32
            && cy >= iy as i32
            && cy < (iy + MENU_ITEM_H) as i32;
        if hovered {
            gfx::fill_rounded_rect(surface, x + 3, iy, menu.w - 6, MENU_ITEM_H, 5, MENU_HOVER);
        }
        let ty = iy + (MENU_ITEM_H - font::glyph_h() as u32) / 2;
        gfx::draw_string(surface, x + 14, ty, item.label, MENU_TEXT, None);
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
        AppKind::Editor { console, lines, cursor_row, cursor_col, scroll_offset, status, .. } => {
            console.resize(cols, rows);
            editor_render(console, lines, *cursor_row, *cursor_col, scroll_offset, status);
        }
        AppKind::SysInfo { console } => {
            *console = build_sysinfo_console();
        }
        AppKind::Calculator(_)
        | AppKind::FileExplorer(_)
        | AppKind::Settings(_)
        | AppKind::Paint(_)
        | AppKind::ImageViewer(_) => {} // fixed pixel size; not resizable
    }
}

fn byte_index(line: &str, char_index: usize) -> usize {
    line.char_indices().nth(char_index).map(|(i, _)| i).unwrap_or(line.len())
}

const NOTEPAD_PATH: &str = "/users/macha/Documents/notepad.txt";

fn editor_handle_key(
    console: &mut Console,
    lines: &mut Vec<String>,
    cursor_row: &mut usize,
    cursor_col: &mut usize,
    scroll_offset: &mut usize,
    status: &mut String,
    path: &str,
    event: keyboard::Event,
) {
    // An editor opened from the file explorer saves to (and reloads
    // from) the file it was opened with; the launcher's plain Notepad
    // keeps the classic fixed default.
    let save_path = if path.is_empty() { NOTEPAD_PATH } else { path };
    match event {
        keyboard::Event::Ctrl('s') => {
            let mut text = lines.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            match fat::write_file(save_path, text.as_bytes()) {
                Ok(()) => *status = format!("saved {} bytes to {}", text.len(), save_path),
                Err(e) => *status = format!("save failed: {}", e),
            }
        }
        keyboard::Event::Ctrl('o') => {
            match fat::read_file(save_path) {
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
                        save_path,
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
        keyboard::Event::Escape | keyboard::Event::F2 => {}
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
    let sx = font::glyph_w() as u32;
    let sy = (doc_rows * font::glyph_h()) as u32;
    let cw = console.width_px();
    gfx::fill_rect(console, 0, sy, cw, font::glyph_h() as u32, EDITOR_STATUS_BG);
    gfx::draw_string(console, sx, sy, status, EDITOR_STATUS_FG, None);
    let visible = (cursor_row.saturating_sub(*scroll_offset)).min(doc_rows.saturating_sub(1));
    let cx = (cursor_col * font::glyph_w()) as u32;
    let cy = (visible * font::glyph_h()) as u32;
    gfx::fill_rect(console, cx, cy, font::glyph_w() as u32, font::glyph_h() as u32, EDITOR_CURSOR_COLOR);
}

fn taskbar_label_width(title: &str) -> i32 {
    (icon_size(16) + 6 + title.len() as u32 * font::glyph_w() as u32 + 20) as i32
}

/// An app-tile size that grows a little with the font scale.
fn icon_size(base: u32) -> u32 {
    base + (font::scale() as u32 - 1) * 8
}

/// Win11-ish color per built-in app (used for title-bar and taskbar
/// tiles); `run`-launched programs get a neutral gray.
fn app_icon_color(kind: &AppKind) -> u32 {
    match kind {
        AppKind::Terminal { .. } => 0x00_2D7D46,
        AppKind::Calculator(_) => 0x00_4A6EE0,
        AppKind::FileExplorer(_) => 0x00_E8A33D,
        AppKind::Editor { .. } => 0x00_0078D4,
        AppKind::Paint(_) => 0x00_C5423B,
        AppKind::ImageViewer(_) => 0x00_7B5CD6,
        AppKind::SysInfo { .. } => 0x00_3E5C76,
        AppKind::Settings(_) => 0x00_5C6BC0,
    }
}

fn launcher_icon_color(label: &str) -> u32 {
    if label.starts_with("Terminal") {
        0x00_2D7D46
    } else if label.starts_with("Calculator") {
        0x00_4A6EE0
    } else if label.starts_with("File Explorer") {
        0x00_E8A33D
    } else if label.starts_with("Notepad") {
        0x00_0078D4
    } else if label.starts_with("Paint") {
        0x00_C5423B
    } else if label.starts_with("Image Viewer") {
        0x00_7B5CD6
    } else if label.starts_with("System Info") {
        0x00_3E5C76
    } else if label.starts_with("Settings") {
        0x00_5C6BC0
    } else {
        0x00_4A4A4A
    }
}

/// A rounded colored tile with the app's first letter inside, the
/// Win11 "pinned app" look for our text-only renderer.
fn draw_app_icon(surface: &mut dyn Surface, x: u32, y: u32, size: u32, kind: &AppKind, title: &str) {
    let color = app_icon_color(kind);
    gfx::fill_rounded_rect(surface, x, y, size, size, size / 4, color);
    let letter = title.bytes().next().unwrap_or(b'?');
    let lx = x + (size - font::glyph_w() as u32) / 2;
    let ly = y + (size - font::glyph_h() as u32) / 2;
    gfx::draw_char(surface, lx, ly, letter, 0x00_FFFFFF, None);
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() > max_chars {
        let mut shortened: String = label.chars().take(max_chars - 1).collect();
        shortened.push_str("..");
        shortened
    } else {
        label.to_string()
    }
}

/// (minimize_x, maximize_x, close_x) in screen coordinates. `maximize_x`
/// is `None` for non-resizable windows, which don't get a maximize
/// button at all — shared between hit-testing and drawing so they can
/// never disagree about where the buttons are.
fn title_button_positions(wx: i32, content_w: u32, resizable: bool) -> (i32, Option<i32>, i32) {
    let close_x = wx + content_w as i32 - TITLE_BTN_W as i32 - TITLE_BTN_MARGIN_RIGHT as i32;
    let maximize_x = if resizable {
        Some(close_x - TITLE_BTN_W as i32 - TITLE_BTN_GAP as i32)
    } else {
        None
    };
    let minimize_x = maximize_x.unwrap_or(close_x) - TITLE_BTN_W as i32 - TITLE_BTN_GAP as i32;
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

/// Close glyph: a 45-degree X in the center of a title button.
fn draw_close_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    draw_diagonal(surface, x, y, x + s - 1, y + s - 1, color);
    draw_diagonal(surface, x + s - 1, y, x, y + s - 1, color);
}

/// Minimize glyph: a short horizontal bar.
fn draw_minimize_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x, y + s - 1, s, 2, color);
}

/// Maximize glyph: a square outline with a thick top edge.
fn draw_maximize_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x, y, s, 2, color);
    gfx::fill_rect(surface, x, y, 2, s, color);
    gfx::fill_rect(surface, x + s - 2, y, 2, s, color);
    gfx::fill_rect(surface, x, y + s - 2, s, 2, color);
}

/// Restore glyph (shown on a maximized window's maximize button): a
/// small square behind a slightly larger one, offset to the bottom-right.
fn draw_restore_icon(surface: &mut dyn Surface, x: u32, y: u32, s: u32, color: u32) {
    gfx::fill_rect(surface, x + 3, y, s, 2, color);
    gfx::fill_rect(surface, x + 3, y, 2, s, color);
    gfx::fill_rect(surface, x + 3, y + s - 2, s, 2, color);
    let (ox, oy) = (x, y + 3);
    gfx::fill_rect(surface, ox, oy, s - 3, 2, color);
    gfx::fill_rect(surface, ox, oy, 2, s - 3, color);
    gfx::fill_rect(surface, ox + s - 3 - 2, oy, 2, s - 3, color);
    gfx::fill_rect(surface, ox, oy + s - 3 - 2, s - 3, 2, color);
}

/// One title-bar button: a flat hover pill (red for close) behind the
/// glyph, drawn by the caller afterwards.
fn draw_title_button(surface: &mut dyn Surface, x: u32, y: u32, hovered: bool, close: bool) {
    if hovered {
        let bg = if close { BTN_CLOSE_HOVER } else { BTN_HOVER };
        gfx::fill_rounded_rect(surface, x, y, TITLE_BTN_W, TITLE_BTN_H, 4, bg);
    }
}

fn draw_window(surface: &mut dyn Surface, window: &Window, focused: bool, cursor_x: i32, cursor_y: i32) {
    let (content_w, content_h) = window.content_size();
    let x = window.x as u32;
    let y = window.y as u32;
    let total_h = TITLE_BAR_HEIGHT + content_h;

    if window.maximized {
        // Maximized windows bleed edge to edge: no shadow, no rounding.
        gfx::fill_rect(surface, x, y, content_w, total_h, WIN_BG);
    } else {
        // Soft drop shadow (three nested offsets, darkest furthest out),
        // collapsed into a single darkening pass by `draw_window_shadow`.
        gfx::draw_window_shadow(surface, x, y, content_w, total_h, WINDOW_RADIUS);
        let border_color = if focused { WIN_BORDER_FOCUSED } else { WIN_BORDER };
        gfx::fill_rounded_rect(surface, x - 1, y - 1, content_w + 2, total_h + 2, WINDOW_RADIUS + 1, border_color);
        gfx::fill_rounded_rect(surface, x, y, content_w, total_h, WINDOW_RADIUS, WIN_BG);
    }

    // Title bar: flat, same surface as the window body, with a thin
    // divider above the content and an app tile + title on the left.
    let title_color = if focused { TITLE_TEXT } else { TITLE_TEXT_UNFOCUSED };
    let ty = y + (TITLE_BAR_HEIGHT - font::glyph_h() as u32) / 2;
    let icon = icon_size(16);
    draw_app_icon(surface, x + 8, y + (TITLE_BAR_HEIGHT - icon) / 2, icon, &window.kind, window.title);
    let title_x = x + 8 + icon + 6;
    gfx::draw_string(surface, title_x, ty, window.title, title_color, None);
    gfx::fill_rect(surface, x, y + TITLE_BAR_HEIGHT - 1, content_w, 1, TITLE_DIVIDER);

    let (minimize_x, maximize_x, close_x) = title_button_positions(window.x, content_w, window.resizable);
    let button_y = y + 2;

    // Hover states track the cursor so the buttons light up on mouseover.
    let in_button_row = cursor_y >= button_y as i32 && cursor_y < (button_y + TITLE_BTN_H) as i32;
    let hover_min = in_button_row && cursor_x >= minimize_x && cursor_x < minimize_x + TITLE_BTN_W as i32;
    let hover_close = in_button_row && cursor_x >= close_x && cursor_x < close_x + TITLE_BTN_W as i32;
    let glyph_color = if focused { BTN_GLYPH } else { BTN_GLYPH_DIM };

    let minimize_x = minimize_x as u32;
    draw_title_button(surface, minimize_x, button_y, hover_min, false);
    draw_minimize_icon(surface, minimize_x + 12, button_y + 8, 16, glyph_color);

    if let Some(max_x) = maximize_x {
        let max_x = max_x as u32;
        let hover_max = in_button_row && cursor_x >= max_x as i32 && cursor_x < max_x as i32 + TITLE_BTN_W as i32;
        draw_title_button(surface, max_x, button_y, hover_max, false);
        let icon_x = max_x + 14;
        if window.maximized {
            draw_restore_icon(surface, icon_x, button_y + 8, 12, glyph_color);
        } else {
            draw_maximize_icon(surface, icon_x, button_y + 8, 12, glyph_color);
        }
    }

    let close_x = close_x as u32;
    draw_title_button(surface, close_x, button_y, hover_close, true);
    draw_close_icon(surface, close_x + 14, button_y + 9, 12, glyph_color);

    gfx::blit_rounded(surface, x, y + TITLE_BAR_HEIGHT, window.content_pixels(), content_w, content_h, WINDOW_RADIUS);

    if window.resizable && !window.maximized {
        let grip_x = x + content_w - RESIZE_GRIP_SIZE;
        let grip_y = y + TITLE_BAR_HEIGHT + content_h - RESIZE_GRIP_SIZE;
        gfx::fill_rounded_rect(surface, grip_x, grip_y, RESIZE_GRIP_SIZE, RESIZE_GRIP_SIZE, 3, 0x00_141414);
        draw_diagonal(surface, grip_x, grip_y + 11, grip_x + 11, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 2, grip_y + 11, grip_x + 11, grip_y + 2, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 9, grip_x + 9, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 4, grip_y + 11, grip_x + 11, grip_y + 4, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 7, grip_x + 7, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x, grip_y + 1, grip_x + 1, grip_y, RESIZE_GRIP_COLOR);
        draw_diagonal(surface, grip_x + 10, grip_y + 11, grip_x + 11, grip_y + 10, RESIZE_GRIP_COLOR);
    }
}

// The cursor is a 10x16 bitmap drawn with a 1px black outline around
// it; `composite_cursor_only` snapshots a slightly larger region around
// it so restoring that region cleanly erases the whole cursor.
const CURSOR_SAVE_W: u32 = 14;
const CURSOR_SAVE_H: u32 = 20;
const CURSOR_SAVE_MARGIN: i32 = 2;

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

fn draw_file_drag_tag(surface: &mut dyn Surface, cursor_x: i32, cursor_y: i32, path: &str, is_dir: bool) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let max_chars = 24usize;
    let label = if name.chars().count() > max_chars {
        let mut shortened: String = name.chars().take(max_chars - 2).collect();
        shortened.push_str("..");
        shortened
    } else {
        name.to_string()
    };
    let tag_w = (label.len() as u32 * font::glyph_w() as u32 + 30).min(260);
    let tag_h = 20u32;
    let x = (cursor_x + 14).max(0) as u32;
    let y = (cursor_y + 16).max(0) as u32;
    gfx::fill_rounded_rect(surface, x + 2, y + 2, tag_w, tag_h, 5, 0x00_070C12);
    gfx::fill_rounded_rect(surface, x, y, tag_w, tag_h, 5, 0x00_2B3A49);
    gfx::fill_rect(surface, x, y, tag_w, 1, 0x00_6FB1E8);
    let icon = if is_dir { 0x00_F0C070 } else { 0x00_BBC8D4 };
    gfx::fill_rect(surface, x + 6, y + 6, 10, 8, icon);
    gfx::draw_string(surface, x + 22, y + 6, &label, 0x00_FFFFFF, None);
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

/// Loads `/system/wallpaper.raw` (see `tools/gen_wallpaper.py`):
/// `width*height` little-endian u32 pixels, row-major, no header. The
/// file is generated at the physical resolution, so if the virtual
/// resolution differs it is scaled down to fit (bilinear, fixed-point,
/// without materializing a full-size intermediate copy). Returns `None`
/// on any mismatch (no volume mounted, file missing, wrong size) so the
/// caller can fall back to the plain gradient background instead of
/// showing a corrupted image.
fn load_wallpaper(screen_w: u32, screen_h: u32) -> Option<Vec<u32>> {
    if !fat::mounted() {
        return None;
    }
    let bytes = fat::read_file("/system/wallpaper.raw").ok()?;
    let virt_len = (screen_w as usize * screen_h as usize) * 4;
    if bytes.len() == virt_len {
        return Some(raw_bytes_to_pixels(bytes));
    }
    let (phys_w, phys_h) = fb::dimensions();
    let phys_len = (phys_w as usize * phys_h as usize) * 4;
    if bytes.len() == phys_len {
        return Some(gfx::scale_bytes(&bytes, phys_w, phys_h, screen_w, screen_h));
    }
    None
}

/// Reinterprets a freshly-read little-endian `0x00RRGGBB` `.raw` file
/// buffer as pixels **without copying**, halving the transient heap need
/// when the desktop reloads its wallpaper (a second full-size buffer is
/// what previously exhausted the heap under fragmentation). Safe because
/// `fat::read_file` allocates exactly `size` bytes and the allocator
/// hands out 16-byte-aligned buffers (u32 needs 4). Falls back to a
/// per-pixel copy if the capacity ever differs from the length.
fn raw_bytes_to_pixels(bytes: Vec<u8>) -> Vec<u32> {
    let len = bytes.len() / 4;
    if len * 4 == bytes.len() && bytes.capacity() == bytes.len() {
        let ptr = bytes.as_ptr() as *mut u32;
        core::mem::forget(bytes);
        unsafe { Vec::from_raw_parts(ptr, len, len) }
    } else {
        bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}

/// Integer square root (Newton's method), used to guess square dimensions
/// for a headerless `.raw` image opened from the file explorer.
fn isqrt(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
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
    let (fb_w, fb_h) = fb::virtual_dimensions();
    let _ = writeln!(console, "display: {}x{}x32", fb_w, fb_h);

    console
}
