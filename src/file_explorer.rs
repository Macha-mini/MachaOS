//! A GUI file explorer for the desktop: browse the mounted FAT32
//! volume with a scrollable list, navigate folders, open text files in
//! Notepad, launch `.elf` programs, and delete files. Renders into its
//! own pixel buffer like the calculator, and reports "open this file"
//! actions back to the window manager (which owns window spawning).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::fat::{self, DirEntry};
use crate::font;
use crate::gfx::{self, Surface};
use crate::interrupts;
use crate::keyboard;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 400;
const PATH_BAR_H: u32 = 24;
const STATUS_H: u32 = 16;
const ROW_H: u32 = 18;
const LIST_Y: u32 = PATH_BAR_H;
const SCROLLBAR_W: u32 = 10;
const LIST_W: u32 = WIDTH - SCROLLBAR_W;
const UP_BUTTON_W: u32 = 40;
const DOUBLE_CLICK_TICKS: u64 = 30; // ~300 ms at the 100 Hz clock
const DELETE_ARM_TICKS: u64 = 120; // ~1.2 s to confirm a delete

const BG: u32 = 0x00_141B23;
const LIST_BG: u32 = 0x00_10161C;
const PATH_BG_TOP: u32 = 0x00_22303E;
const PATH_BG_BOTTOM: u32 = 0x00_1A2632;
const PATH_TEXT: u32 = 0x00_FFFFFF;
const STATUS_BG: u32 = 0x00_1A2430;
const STATUS_TEXT: u32 = 0x00_8FB8D8;
const STATUS_TEXT_BAD: u32 = 0x00_E06060;
const TEXT: u32 = 0x00_DBE6F0;
const TEXT_DIM: u32 = 0x00_9AA8B5;
const DIR_TEXT: u32 = 0x00_F0C070;
const ELF_TEXT: u32 = 0x00_86C77B;
const SELECTED_TOP: u32 = 0x00_3C6EA8;
const SELECTED_BOTTOM: u32 = 0x00_244A78;
const SCROLL_TRACK: u32 = 0x00_1B2632;
const SCROLL_THUMB: u32 = 0x00_3A4753;
const BUTTON_BG_TOP: u32 = 0x00_35424F;
const BUTTON_BG_BOTTOM: u32 = 0x00_232C35;
const BUTTON_EDGE: u32 = 0x00_0E141B;
const ICON_FOLDER: u32 = 0x00_F0C070;
const ICON_FILE: u32 = 0x00_BBC8D4;
const ICON_ELF: u32 = 0x00_86C77B;

/// What the explorer asks the window manager to do after a click or key.
/// Owned values only, so the WM can act on it after the borrow ends.
pub enum FsAction {
    /// Launch `path` (a `.elf`) in a fresh terminal window.
    RunTerminal(String),
    /// Open `content` (a text file's bytes) in a fresh Notepad window.
    OpenText(String, Vec<u8>),
}

pub struct FileExplorer {
    buffer: Vec<u32>,
    path: String,
    entries: Vec<DirEntry>,
    selection: usize,
    scroll: usize,
    // Double-click detection: (row, tick) of the previous press.
    last_click_row: usize,
    last_click_ticks: u64,
    // Two-step delete confirmation: (selection at press time, tick).
    pending_delete: Option<(usize, u64)>,
    status: String,
}

fn is_elf(name: &str) -> bool {
    name.to_lowercase().ends_with(".elf")
}

fn is_text(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".log") || lower.ends_with(".cfg")
}

/// Joins `child` onto `path` (handling the root directory).
fn join(path: &str, child: &str) -> String {
    if path == "/" {
        format!("/{}", child)
    } else {
        format!("{}/{}", path, child)
    }
}

/// Parent of `path`; stays "/" at the root.
fn parent_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => path[..i].to_string(),
        None => "/".to_string(),
    }
}

/// Directories first (case-insensitive), then by name.
fn sort_entries(entries: &mut [DirEntry]) {
    let mut i = 1;
    while i < entries.len() {
        let mut j = i;
        while j > 0 {
            let a = &entries[j - 1];
            let b = &entries[j];
            let a_dir = a.is_dir;
            let b_dir = b.is_dir;
            let a_lower = a.name.to_lowercase();
            let b_lower = b.name.to_lowercase();
            // `b` should move before `a` when it's a directory and `a`
            // isn't, or (same kind) its name sorts earlier.
            let b_before_a = (b_dir && !a_dir) || (b_dir == a_dir && b_lower < a_lower);
            if !b_before_a {
                break;
            }
            entries.swap(j - 1, j);
            j -= 1;
        }
        i += 1;
    }
}

impl FileExplorer {
    pub fn new() -> Self {
        let mut app = Self {
            buffer: vec![BG; (WIDTH * HEIGHT) as usize],
            path: "/".to_string(),
            entries: Vec::new(),
            selection: 0,
            scroll: 0,
            last_click_row: usize::MAX,
            last_click_ticks: 0,
            pending_delete: None,
            status: String::new(),
        };
        app.navigate("/".to_string());
        app
    }

    pub fn width(&self) -> u32 {
        WIDTH
    }

    pub fn height(&self) -> u32 {
        HEIGHT
    }

    pub fn pixels(&self) -> &[u32] {
        &self.buffer
    }

    fn visible_rows(&self) -> usize {
        ((HEIGHT - PATH_BAR_H - STATUS_H) / ROW_H) as usize
    }

    fn ensure_selection_visible(&mut self) {
        let rows = self.visible_rows();
        let max_scroll = self.entries.len().saturating_sub(rows);
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.selection >= self.scroll + rows {
            self.scroll = self.selection + 1 - rows;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Reloads the entry list for the current path (keeps selection).
    fn refresh(&mut self) {
        self.entries = match fat::list_dir(&self.path) {
            Ok(mut entries) => {
                sort_entries(&mut entries);
                entries
            }
            Err(e) => {
                self.status = format!("cannot read {}: {}", self.path, e);
                Vec::new()
            }
        };
        if self.selection >= self.entries.len() {
            self.selection = 0;
        }
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(self.visible_rows()));
        self.render();
    }

    fn navigate(&mut self, path: String) {
        self.path = path;
        self.selection = 0;
        self.scroll = 0;
        self.pending_delete = None;
        self.status = String::new();
        self.refresh();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let new = if delta < 0 {
            self.selection.saturating_sub((-delta) as usize)
        } else {
            self.selection.saturating_add(delta as usize).min(self.entries.len() - 1)
        };
        self.selection = new;
        self.ensure_selection_visible();
        self.render();
    }

    fn open_selected(&mut self) -> Option<FsAction> {
        let entry = self.entries.get(self.selection).cloned()?;
        let path = join(&self.path, &entry.name);
        if entry.is_dir {
            self.navigate(path);
            None
        } else if is_elf(&entry.name) {
            self.status = format!("launching {}", entry.name);
            self.render();
            Some(FsAction::RunTerminal(path))
        } else if entry.size > 0 && is_text(&entry.name) {
            match fat::read_file(&path) {
                Ok(content) => {
                    self.status = format!("opened {} ({} bytes)", entry.name, content.len());
                    self.render();
                    Some(FsAction::OpenText(path, content))
                }
                Err(e) => {
                    self.status = format!("cannot read {}: {}", entry.name, e);
                    self.render();
                    None
                }
            }
        } else {
            self.status = format!("{}: no viewer ({:.1} KiB)", entry.name, entry.size as f32 / 1024.0);
            self.render();
            None
        }
    }

    /// Delete armed on the first press, executed on a second press while
    /// still armed. Only removes regular files and empty directories.
    fn delete_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selection).cloned() else {
            return;
        };
        let now = interrupts::ticks();
        if let Some((arm_sel, arm_tick)) = self.pending_delete {
            if arm_sel == self.selection && now.saturating_sub(arm_tick) <= DELETE_ARM_TICKS {
                let path = join(&self.path, &entry.name);
                self.pending_delete = None;
                match fat::remove(&path) {
                    Ok(()) => {
                        self.status = format!("deleted {}", entry.name);
                        self.refresh();
                    }
                    Err(e) => {
                        self.status = format!("delete failed: {}", e);
                        self.render();
                    }
                }
                return;
            }
        }
        self.pending_delete = Some((self.selection, now));
        self.status = format!("delete {}? press Del again to confirm", entry.name);
        self.render();
    }

    fn scroll_click(&mut self, _x: u32, y: u32) {
        let rows = self.visible_rows();
        let max_scroll = self.entries.len().saturating_sub(rows);
        if max_scroll == 0 {
            return;
        }
        let track_h = HEIGHT - PATH_BAR_H - STATUS_H;
        let thumb_h = (track_h * rows as u32 / self.entries.len() as u32).max(12);
        let thumb_y = LIST_Y + (track_h - thumb_h) * self.scroll as u32 / max_scroll as u32;
        if y < thumb_y {
            self.scroll = self.scroll.saturating_sub(rows / 2);
        } else if y >= thumb_y + thumb_h {
            self.scroll = (self.scroll + rows / 2).min(max_scroll);
        }
        self.render();
    }

    /// Returns an action (open a file in a new window) or `None`; the
    /// caller (window manager) acts on it.
    pub fn handle_click(&mut self, x: i32, y: i32) -> Option<FsAction> {
        if x < 0 || y < 0 {
            return None;
        }
        let (x, y) = (x as u32, y as u32);

        if y < PATH_BAR_H {
            if x >= WIDTH - UP_BUTTON_W - 6 && x < WIDTH - 6 {
                self.navigate(parent_path(&self.path));
            }
            return None;
        }
        if y >= HEIGHT - STATUS_H {
            return None;
        }
        if x >= LIST_W {
            self.scroll_click(x, y);
            return None;
        }
        let row = ((y - LIST_Y) / ROW_H) as usize + self.scroll;
        if row >= self.entries.len() {
            self.pending_delete = None;
            return None;
        }
        let now = interrupts::ticks();
        let is_double =
            self.last_click_row == row && now.saturating_sub(self.last_click_ticks) <= DOUBLE_CLICK_TICKS;
        self.last_click_row = row;
        self.last_click_ticks = now;
        self.selection = row;
        self.ensure_selection_visible();
        self.pending_delete = None;
        if is_double {
            self.open_selected()
        } else {
            let entry = &self.entries[row];
            if entry.is_dir {
                self.status = format!("{} (dir, {} items)", entry.name, self.entries.len().saturating_sub(1));
            } else {
                self.status = format!("{:.1} KiB", entry.size as f32 / 1024.0);
            }
            self.render();
            None
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) -> Option<FsAction> {
        match event {
            keyboard::Event::Up => self.move_selection(-1),
            keyboard::Event::Down => self.move_selection(1),
            keyboard::Event::Enter => return self.open_selected(),
            keyboard::Event::Backspace => self.navigate(parent_path(&self.path)),
            // The PS/2 driver doesn't map a Delete key; 'd' (pressed
            // twice, armed first) is the explorer's delete gesture.
            keyboard::Event::Char('d') => self.delete_selected(),
            keyboard::Event::Char('r') => {
                self.status = String::new();
                self.refresh();
            }
            _ => {}
        }
        None
    }

    fn render(&mut self) {
        let rows = self.visible_rows();
        gfx::fill_rect(self, 0, 0, WIDTH, HEIGHT, BG);
        gfx::fill_rect(self, 0, LIST_Y, LIST_W, HEIGHT - PATH_BAR_H - STATUS_H, LIST_BG);
        gfx::fill_rect_gradient_v(self, 0, 0, WIDTH, PATH_BAR_H, PATH_BG_TOP, PATH_BG_BOTTOM);
        gfx::fill_rect(self, 0, PATH_BAR_H - 1, WIDTH, 1, 0x00_0E141B);
        gfx::fill_rect(self, 0, HEIGHT - STATUS_H, WIDTH, STATUS_H, STATUS_BG);
        gfx::fill_rect(self, 0, HEIGHT - STATUS_H - 1, WIDTH, 1, 0x00_0E141B);

        // Path bar: current path, then an "Up" button on the right.
        let path = self.path.clone();
        gfx::draw_string(self, 8, 8, &path, PATH_TEXT, None);
        let btn_x = WIDTH - UP_BUTTON_W - 6;
        gfx::fill_rounded_rect_gradient_v(self, btn_x, 4, UP_BUTTON_W, PATH_BAR_H - 8, 4, BUTTON_BG_TOP, BUTTON_BG_BOTTOM);
        gfx::fill_rect(self, btn_x + 1, PATH_BAR_H - 5, UP_BUTTON_W - 2, 1, BUTTON_EDGE);
        let label = if self.path == "/" { "Root" } else { "Up" };
        let lx = btn_x + (UP_BUTTON_W - (label.len() as u32 * font::GLYPH_WIDTH as u32)) / 2;
        gfx::draw_string(self, lx, 8, label, TEXT, None);

        // Scrollbar.
        let track_h = HEIGHT - PATH_BAR_H - STATUS_H;
        gfx::fill_rect(self, LIST_W, LIST_Y, SCROLLBAR_W, track_h, SCROLL_TRACK);
        if rows < self.entries.len() {
            let max_scroll = self.entries.len() - rows;
            let thumb_h = (track_h * rows as u32 / self.entries.len() as u32).max(12);
            let thumb_y = LIST_Y + (track_h - thumb_h) * self.scroll as u32 / max_scroll as u32;
            gfx::fill_rect(self, LIST_W + 2, thumb_y, SCROLLBAR_W - 4, thumb_h, SCROLL_THUMB);
        }

        // Entry list.
        for r in 0..rows {
            let index = r + self.scroll;
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            let (name, is_dir, size) = (entry.name.clone(), entry.is_dir, entry.size);
            let ry = LIST_Y + r as u32 * ROW_H;
            let selected = index == self.selection;
            if selected {
                gfx::fill_rect_gradient_v(self, 0, ry, LIST_W, ROW_H, SELECTED_TOP, SELECTED_BOTTOM);
            }
            draw_entry_icon(self, 8, ry + 4, &name, is_dir);
            let (name_color, size_color) = if selected {
                (0x00_FFFFFF, 0x00_C9DAE8)
            } else if is_dir {
                (DIR_TEXT, TEXT_DIM)
            } else if is_elf(&name) {
                (ELF_TEXT, TEXT_DIM)
            } else {
                (TEXT, TEXT_DIM)
            };
            gfx::draw_string(self, 24, ry + 5, &name, name_color, None);
            if !is_dir {
                let size = format!("{} B", size);
                let sw = (size.len() * font::GLYPH_WIDTH) as u32;
                gfx::draw_string(self, LIST_W - sw - 14, ry + 5, &size, size_color, None);
            }
        }

        // Status bar.
        let status = if self.status.is_empty() {
            format!("{} items in {}", self.entries.len(), self.path)
        } else {
            self.status.clone()
        };
        let is_error = status.starts_with("cannot")
            || status.starts_with("error")
            || status.starts_with("delete failed")
            || status.starts_with("no viewer");
        let status_color = if is_error { STATUS_TEXT_BAD } else { STATUS_TEXT };
        gfx::draw_string(self, 6, HEIGHT - STATUS_H + 4, &status, status_color, None);
    }
}

/// Small glyph for a folder / document / executable, drawn with rects
/// since the 8x8 font has no box-drawing characters.
fn draw_entry_icon(surface: &mut dyn Surface, x: u32, y: u32, name: &str, is_dir: bool) {
    if is_dir {
        // Folder: a short tab on top of a taller body.
        gfx::fill_rect(surface, x, y + 1, 5, 2, ICON_FOLDER);
        gfx::fill_rect(surface, x + 5, y, 5, 2, ICON_FOLDER);
        gfx::fill_rect(surface, x + 10, y + 1, 2, 1, ICON_FOLDER);
        gfx::fill_rect(surface, x, y + 3, 14, 7, ICON_FOLDER);
        gfx::fill_rect(surface, x + 1, y + 4, 12, 5, BG);
    } else {
        // Document: page outline with a folded corner and two text lines.
        let color = if is_elf(name) { ICON_ELF } else { ICON_FILE };
        gfx::fill_rect(surface, x + 2, y, 9, 10, color);
        gfx::fill_rect(surface, x + 3, y + 1, 7, 8, BG);
        gfx::fill_rect(surface, x + 4, y + 3, 5, 1, color);
        gfx::fill_rect(surface, x + 4, y + 5, 5, 1, color);
        gfx::fill_rect(surface, x + 4, y + 7, 3, 1, color);
    }
}

impl Surface for FileExplorer {
    fn width(&self) -> u32 {
        WIDTH
    }

    fn height(&self) -> u32 {
        HEIGHT
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < WIDTH && y < HEIGHT {
            let idx = (y * WIDTH + x) as usize;
            self.buffer[idx] = color;
        }
    }
}
