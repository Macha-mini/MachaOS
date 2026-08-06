//! A modern, Windows-style GUI file explorer for the desktop: sidebar
//! with quick access and root-folder shortcuts, a toolbar, clickable
//! breadcrumbs, a sortable details view with column headers, and an
//! icon (grid) view. Renders into its own pixel buffer like the
//! calculator, and reports "open this file" actions back to the window
//! manager (which owns window spawning).
//!
//! The window manager calls `render_hover` with the cursor position on
//! every composite, so hover highlights follow the mouse; everything
//! else redraws on demand. All arithmetic is integer-only (no floats):
//! the scheduler doesn't preserve FPU state across task switches.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::fat::{self, DirEntry};
use crate::font;
use crate::gfx::{self, Surface};
use crate::interrupts;
use crate::keyboard;
use crate::users;

const WIDTH: u32 = 700;
const HEIGHT: u32 = 440;
const PATH_H: u32 = 24; // breadcrumb bar
const TOOLBAR_H: u32 = 26;
const HEADER_H: u32 = 24; // details-view column headers
const STATUS_H: u32 = 16;
const LIST_Y: u32 = PATH_H + TOOLBAR_H;
const SIDEBAR_W: u32 = 128;
const SCROLLBAR_W: u32 = 10;
const LIST_W: u32 = WIDTH - SIDEBAR_W - SCROLLBAR_W;
const ROW_H: u32 = 18;
const GRID_TILE_W: u32 = 88;
const GRID_TILE_H: u32 = 54;
const DOUBLE_CLICK_TICKS: u64 = 30; // ~300 ms at the 100 Hz clock
const DELETE_ARM_TICKS: u64 = 120; // ~1.2 s to confirm a delete

const BG: u32 = 0x00_1F1F1F;
const LIST_BG: u32 = 0x00_1C1C1C;
const SIDEBAR_BG: u32 = 0x00_1A1A1A;
const PATH_BG: u32 = 0x00_252525;
const TOOLBAR_BG: u32 = 0x00_232323;
const STATUS_BG: u32 = 0x00_242424;
const HEADER_BG: u32 = 0x00_242424;
const TEXT: u32 = 0x00_FFFFFF;
const TEXT_DIM: u32 = 0x00_9A9A9A;
const TEXT_FAINT: u32 = 0x00_6A6A6A;
const DIR_TEXT: u32 = 0x00_F0C070;
const ELF_TEXT: u32 = 0x00_86C77B;
const SELECTED_TOP: u32 = 0x00_3A3A3A;
const SELECTED_BOTTOM: u32 = 0x00_333333;
const HOVER_BG: u32 = 0x00_2A2A2A;
const CRUMB_BG: u32 = 0x00_2C2C2C;
const ACCENT: u32 = 0x00_4CC2FF;
const SCROLL_TRACK: u32 = 0x00_202020;
const SCROLL_THUMB: u32 = 0x00_3A3A3A;
const BTN_TOP: u32 = 0x00_2C2C2C;
const BTN_BOTTOM: u32 = 0x00_2C2C2C;
const BTN_HOVER_TOP: u32 = 0x00_343434;
const BTN_HOVER_BOTTOM: u32 = 0x00_343434;
const BTN_EDGE: u32 = 0x00_1A1A1A;
const ICON_FOLDER: u32 = 0x00_F0C070;
const ICON_FILE: u32 = 0x00_BBC8D4;
const ICON_ELF: u32 = 0x00_86C77B;
const STATUS_TEXT: u32 = 0x00_9AB8D8;
const STATUS_TEXT_BAD: u32 = 0x00_E06060;

/// What the explorer asks the window manager to do after a click or key.
/// Owned values only, so the WM can act on it after the borrow ends.
pub enum FsAction {
    /// Launch `path` (a `.elf`) in a fresh terminal window.
    RunTerminal(String),
    /// Open `content` (a text file's bytes) in a fresh Notepad window.
    OpenText(String, Vec<u8>),
    /// Open `content` (a raw image's bytes) in a fresh ImageViewer window.
    OpenImage(String, Vec<u8>),
}

/// A right-click context-menu action the WM can ask the explorer to
/// perform on its current selection.
#[derive(Clone, Copy)]
pub enum ExplorerMenuAction {
    Open,
    Rename,
    Delete,
    /// Copy the selected entries (paths only — the actual bytes are read
    /// when Paste runs, so copied files may be edited meanwhile).
    Copy,
    /// Paste every copied entry into the current directory.
    Paste,
    /// Restore the selected entry to its original location (trash only).
    Restore,
    /// Physically delete everything in the trash (trash only).
    EmptyTrash,
    Refresh,
    NewFolder,
    NewFile,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Details,
    Grid,
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Size,
    Type,
}

#[derive(Clone, Copy, PartialEq)]
enum ToolbarAction {
    Up,
    New,
    NewFile,
    Del,
    Refresh,
    ToggleView,
}

/// One row of the sidebar: either a section header or a clickable item.
struct SidebarEntry {
    label: String,
    target: String, // empty for headers
}

/// An in-progress inline rename: the selected item's index and the edit
/// buffer with the cursor position in characters. `select_all` is set
/// while the whole original name is "selected": typing then replaces it
/// (like Windows), and any cursor/edit key clears the selection.
struct RenameState {
    item: usize,
    buf: String,
    cursor: usize,
    select_all: bool,
}

#[derive(Clone)]
pub struct DragInfo {
    pub path: String,
    pub is_dir: bool,
}

pub struct FileExplorer {
    buffer: Vec<u32>,
    path: String,
    entries: Vec<DirEntry>,
    selection: usize,
    scroll: usize,
    view: View,
    sort: SortKey,
    sort_asc: bool,
    // Cursor position in window content coordinates; set by the WM
    // every frame so hover highlights can be drawn.
    cursor: (i32, i32),
    // Double-click detection: (item, tick) of the previous press.
    last_click_item: usize,
    last_click_ticks: u64,
    last_click_was_double: bool,
    // Two-step delete confirmation: (selection at press time, tick).
    pending_delete: Option<(usize, u64)>,
    // Inline rename-in-progress (`F2`); `None` when not renaming.
    rename: Option<RenameState>,
    // Extra selected indices beyond `selection` (the primary), and the
    // anchor for Shift+click range selection.
    multi: Vec<usize>,
    anchor: usize,
    // Incremental name search (Ctrl+F): while on, typed characters
    // build `search` and the first entry whose name starts with it is
    // selected.
    search_mode: bool,
    search: String,
    status: String,
    free_bytes: u64,
    sidebar: Vec<SidebarEntry>,
    drag: Option<DragInfo>,
    // The explorer's own clipboard: absolute source paths of the copied
    // entries (with their directory flags), kept across navigation so
    // the user can copy in one folder and paste in another.
    clip: Vec<(String, bool)>,
}

fn is_elf(name: &str) -> bool {
    name.to_lowercase().ends_with(".elf")
}

fn is_text(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".log") || lower.ends_with(".cfg")
}

/// Raw 32-bit pixel dumps (see `tools/gen_wallpaper.py`'s format) and
/// PNG files, both openable in the Image Viewer (the WM tries PNG
/// decoding first and falls back to the raw square-size guess).
fn is_raw(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".raw") || lower.ends_with(".png")
}

fn file_type(name: &str) -> &'static str {
    if is_elf(name) {
        "ELF Program"
    } else if is_text(name) {
        "Text"
    } else if is_raw(name) {
        "Image"
    } else {
        "File"
    }
}

/// Integer-only human-readable size ("12 B", "4.5 KiB", "8.0 MiB").
fn fmt_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        let whole = bytes / (1024 * 1024);
        let frac = (bytes % (1024 * 1024)) * 10 / (1024 * 1024);
        format!("{}.{} MiB", whole, frac)
    } else if bytes >= 1024 {
        let whole = bytes / 1024;
        let frac = (bytes % 1024) * 10 / 1024;
        format!("{}.{} KiB", whole, frac)
    } else {
        format!("{} B", bytes)
    }
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

/// Byte index of the `char_index`-th character in `s`.
fn char_index(s: &str, char_index: usize) -> usize {
    s.char_indices().nth(char_index).map(|(i, _)| i).unwrap_or(s.len())
}

/// A name that collides with nothing in `taken` (lowercase names): the
/// original, or one with a ` (n)` suffix before the extension, like the
/// New-Folder/New-File dedup (`file.txt` -> `file (2).txt`).
fn unique_name(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| *t == name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    // Bounded like the New-Folder/New-File dedup: a directory could in
    // theory hold many "name (n)" variants; beyond the cap the last
    // candidate is used as-is (the copy then fails loudly instead of
    // hanging the desktop).
    let mut i = 2;
    while i < 100 {
        let candidate = format!("{} ({}){}", stem, i, ext);
        if !taken.iter().any(|t| *t == candidate.to_lowercase()) {
            return candidate;
        }
        i += 1;
    }
    format!("{} ({}){}", stem, i, ext)
}

/// Compares two entries: directories first, then by the active sort key.
fn entry_less(a: &DirEntry, b: &DirEntry, key: SortKey, asc: bool) -> bool {
    if a.is_dir != b.is_dir {
        return a.is_dir;
    }
    let ord = match key {
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Type => file_type(&a.name).cmp(file_type(&b.name)),
    };
    if asc {
        ord == Ordering::Less
    } else {
        ord == Ordering::Greater
    }
}

/// Stable insertion sort (dirs first, then key order).
fn sort_entries(entries: &mut [DirEntry], key: SortKey, asc: bool) {
    let mut i = 1;
    while i < entries.len() {
        let mut j = i;
        while j > 0 {
            // Bubble `entries[j]` left while it sorts before the item
            // ahead of it.
            let b_before_a = entry_less(&entries[j], &entries[j - 1], key, asc);
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
            path: users::home(),
            entries: Vec::new(),
            selection: 0,
            scroll: 0,
            view: View::Details,
            sort: SortKey::Name,
            sort_asc: true,
            cursor: (i32::MAX, i32::MAX),
            last_click_item: usize::MAX,
            last_click_ticks: 0,
            last_click_was_double: false,
            pending_delete: None,
            rename: None,
            multi: Vec::new(),
            anchor: 0,
            search_mode: false,
            search: String::new(),
            status: String::new(),
            free_bytes: 0,
            sidebar: Vec::new(),
            drag: None,
            clip: Vec::new(),
        };
        app.navigate(users::home());
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

    /// Re-renders if the cursor moved, so hover states track the mouse.
    /// Called by the window manager on every composite.
    pub fn render_hover(&mut self, x: i32, y: i32) {
        if self.cursor != (x, y) {
            self.cursor = (x, y);
            self.render();
        }
    }

    /// Returns the entry index under a content-local point. Toolbar,
    /// sidebar, headers, empty space, and the scrollbar are not items.
    pub fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        if x < SIDEBAR_W as i32 || x >= (SIDEBAR_W + LIST_W) as i32 || y < LIST_Y as i32 {
            return None;
        }
        let item = match self.view {
            View::Details => {
                if y < (LIST_Y + HEADER_H) as i32 {
                    return None;
                }
                self.scroll + ((y as u32 - LIST_Y - HEADER_H) / ROW_H) as usize
            }
            View::Grid => {
                let col = ((x as u32 - SIDEBAR_W) / GRID_TILE_W) as usize;
                let row = ((y as u32 - LIST_Y) / GRID_TILE_H) as usize;
                self.scroll + row * self.grid_cols() + col
            }
        };
        (item < self.entries.len()).then_some(item)
    }

    /// Absolute source path and directory flag for an entry index.
    pub fn entry_path(&self, item: usize) -> Option<(String, bool)> {
        let entry = self.entries.get(item)?;
        Some((join(&self.path, &entry.name), entry.is_dir))
    }

    /// Returns the item pressed in a drag-capable click. Double-clicks
    /// (which open or enter the item) are explicitly excluded.
    pub fn drag_candidate(&mut self, item: Option<usize>) -> Option<usize> {
        let was_double = self.last_click_was_double;
        self.last_click_was_double = false;
        if was_double { None } else { item }
    }

    /// Updates the drag visual state supplied by the window manager.
    pub fn set_drag(&mut self, drag: Option<DragInfo>) {
        self.drag = drag;
        self.render();
    }

    /// Re-reads the current directory after another explorer moved a file.
    pub fn refresh_now(&mut self) {
        self.refresh();
    }

    /// Handles a drop at a content-local point. Returns `true` when this
    /// explorer was a valid drop surface, even if the move was a no-op or
    /// failed; the WM then refreshes the source explorer as needed.
    pub fn handle_drop(&mut self, x: i32, y: i32, src: &str, is_dir: bool) -> bool {
        let Some(target) = self.drop_target_at(x, y) else {
            return false;
        };
        let source_parent = parent_path(src);
        if target == source_parent || target == src || (is_dir && target.starts_with(&(src.to_string() + "/"))) {
            self.status = format!("{} is already in {}", src.rsplit('/').next().unwrap_or(src), target);
            self.render();
            return true;
        }
        let name = src.rsplit('/').next().unwrap_or(src);
        let destination = join(&target, name);
        match fat::move_file(src, &destination) {
            Ok(()) => {
                self.status = format!("moved {} to {}", name, target);
                self.refresh();
            }
            Err(e) => {
                self.status = format!("move failed: {}", e);
                self.render();
            }
        }
        true
    }

    fn drop_target_at(&self, x: i32, y: i32) -> Option<String> {
        if x < 0 || y < 0 || y >= (HEIGHT - STATUS_H) as i32 {
            return None;
        }
        if x < SIDEBAR_W as i32 {
            return sidebar_item_at(&self.sidebar, y as u32);
        }
        if x >= (SIDEBAR_W + LIST_W) as i32 || y < LIST_Y as i32 {
            return None;
        }
        if let Some(item) = self.item_at(x, y) {
            if let Some(entry) = self.entries.get(item) {
                if entry.is_dir {
                    return Some(join(&self.path, &entry.name));
                }
            }
        }
        // Dropping on a file or empty list space means the current folder.
        Some(self.path.clone())
    }

    fn details_visible_rows(&self) -> usize {
        ((HEIGHT - STATUS_H - LIST_Y - HEADER_H) / ROW_H) as usize
    }

    fn grid_visible_rows(&self) -> usize {
        ((HEIGHT - STATUS_H - LIST_Y) / GRID_TILE_H) as usize
    }

    fn grid_cols(&self) -> usize {
        (LIST_W / GRID_TILE_W).max(1) as usize
    }

    fn visible_items(&self) -> usize {
        match self.view {
            View::Details => self.details_visible_rows(),
            View::Grid => self.grid_visible_rows() * self.grid_cols(),
        }
    }

    fn ensure_selection_visible(&mut self) {
        let visible = self.visible_items();
        let max_scroll = self.entries.len().saturating_sub(visible);
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.selection >= self.scroll + visible {
            self.scroll = self.selection + 1 - visible;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    /// Reloads the entry list for the current path and rebuilds the
    /// sidebar shortcuts (keeps selection and view).
    fn refresh(&mut self) {
        self.entries = match fat::list_dir(&self.path) {
            Ok(mut entries) => {
                sort_entries(&mut entries, self.sort, self.sort_asc);
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
        self.multi.retain(|&i| i < self.entries.len());
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(self.visible_items()));
        self.free_bytes = fat::info()
            .map(|info| info.free_clusters as u64 * info.sectors_per_cluster as u64 * 512)
            .unwrap_or(0);
        self.sidebar = build_sidebar();
        self.render();
    }

    fn navigate(&mut self, path: String) {
        self.path = path;
        self.selection = 0;
        self.scroll = 0;
        self.pending_delete = None;
        self.rename = None;
        self.multi.clear();
        self.anchor = 0;
        self.search_mode = false;
        self.search.clear();
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
        self.multi.clear();
        self.anchor = new;
        self.ensure_selection_visible();
        self.render();
    }

    /// Copies the selected entries' absolute paths (with their directory
    /// flags) into the explorer's clipboard. The actual data stays on
    /// disk until a Paste reads it, so a file edited between Copy and
    /// Paste is copied with its newer contents.
    fn copy_selected(&mut self) {
        let copied: Vec<(String, bool)> = self
            .selected_indices()
            .iter()
            .filter_map(|&i| self.entry_path(i))
            .collect();
        if copied.is_empty() {
            return;
        }
        self.clip = copied;
        let n = self.clip.len();
        self.status = format!("copied {} item{}", n, if n == 1 { "" } else { "s" });
        self.render();
    }

    /// Whether the clipboard holds entries (so the context menu can show
    /// a Paste item only when there is something to paste).
    pub fn has_clipboard(&self) -> bool {
        !self.clip.is_empty()
    }

    /// Pastes every entry in the clipboard into the current directory,
    /// deduplicating names against what is already here (`file.txt`
    /// becomes `file (2).txt`). Files are copied with their current
    /// on-disk contents; directory trees copy recursively.
    fn paste_clipboard(&mut self) {
        if self.clip.is_empty() {
            self.status = "clipboard is empty — copy a file first".to_string();
            self.render();
            return;
        }
        let mut taken: Vec<String> = self.entries.iter().map(|e| e.name.to_lowercase()).collect();
        let mut pasted = 0usize;
        let mut failed: Option<String> = None;
        for (src, _is_dir) in self.clip.clone() {
            let base = src.rsplit('/').next().unwrap_or(&src).to_string();
            let name = unique_name(&base, &taken);
            let dst = join(&self.path, &name);
            match fat::copy_file(&src, &dst) {
                Ok(()) => {
                    pasted += 1;
                    taken.push(name.to_lowercase());
                }
                Err(e) => {
                    if failed.is_none() {
                        failed = Some(format!("{}: {}", base, e));
                    }
                }
            }
        }
        self.status = match failed {
            Some(err) => format!("pasted {} item{}, {} failed ({})", pasted, if pasted == 1 { "" } else { "s" }, self.clip.len().saturating_sub(pasted), err),
            None => format!("pasted {} item{}", pasted, if pasted == 1 { "" } else { "s" }),
        };
        self.refresh();
    }

    /// Whether `index` is part of the current selection (primary or
    /// multi-selected).
    fn is_selected(&self, index: usize) -> bool {
        index == self.selection || self.multi.contains(&index)
    }

    /// All selected indices: the primary plus the multi extras, sorted.
    fn selected_indices(&self) -> Vec<usize> {
        let mut v = vec![self.selection];
        v.extend(self.multi.iter().copied());
        v.sort_unstable();
        v.dedup();
        v
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
        } else if is_raw(&entry.name) {
            match fat::read_file(&path) {
                Ok(content) => {
                    self.status = format!("opened {} ({} bytes)", entry.name, content.len());
                    self.render();
                    Some(FsAction::OpenImage(path, content))
                }
                Err(e) => {
                    self.status = format!("cannot read {}: {}", entry.name, e);
                    self.render();
                    None
                }
            }
        } else if is_text(&entry.name) {
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
            self.status = format!("{}: no viewer ({})", entry.name, fmt_size(entry.size as u64));
            self.render();
            None
        }
    }

    /// Delete armed on the first press, executed on a second press while
    /// still armed. Moves the entry to the trash (a temporary staging
    /// area) rather than physically erasing it; the trash can restore
    /// it later. Inside the trash itself, Delete physically erases.
    fn delete_selected(&mut self) {
        let names: Vec<String> = self
            .selected_indices()
            .iter()
            .filter_map(|&i| self.entries.get(i).map(|e| e.name.clone()))
            .collect();
        if names.is_empty() {
            return;
        }
        let now = interrupts::ticks();
        if let Some((arm_sel, arm_tick)) = self.pending_delete {
            if arm_sel == self.selection && now.saturating_sub(arm_tick) <= DELETE_ARM_TICKS {
                self.pending_delete = None;
                let mut failed = false;
                for name in &names {
                    let path = join(&self.path, name);
                    let result: Result<(), &'static str> = if self.in_trash() {
                        fat::remove(&path).map_err(|_| "remove failed")
                    } else {
                        crate::trash::trash_file(&path).map(|_| ())
                    };
                    if result.is_err() {
                        failed = true;
                    }
                }
                self.status = if failed {
                    "delete failed (some entries)".to_string()
                } else if self.in_trash() {
                    format!("permanently deleted {} items", names.len())
                } else {
                    format!("moved {} items to trash", names.len())
                };
                self.multi.clear();
                self.refresh();
                return;
            }
        }
        self.pending_delete = Some((self.selection, now));
        let prompt = if self.in_trash() {
            format!("permanently delete {} items? press Del again to confirm", names.len())
        } else {
            format!("move {} items to trash? press Del again to confirm", names.len())
        };
        self.status = prompt;
        self.render();
    }

    /// Whether the explorer is currently showing the trash directory
    /// (so Delete becomes permanent and Restore/Empty actions appear).
    pub fn in_trash(&self) -> bool {
        self.path == crate::trash::TRASH_PATH
    }

    /// Restores the selected trash entry to its original location.
    fn restore_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selection).cloned() else {
            return;
        };
        let names = crate::trash::list_names();
        let Some(index) = names.iter().position(|n| *n == entry.name) else {
            self.status = "cannot restore: entry not found in trash".to_string();
            self.render();
            return;
        };
        match crate::trash::restore(index) {
            Ok(path) => {
                self.status = format!("restored to {}", path);
                self.refresh();
            }
            Err(e) => {
                self.status = format!("restore failed: {}", e);
                self.render();
            }
        }
    }

    /// Physically deletes everything in the trash.
    fn empty_trash(&mut self) {
        match crate::trash::empty_trash() {
            Ok(()) => {
                self.status = "trash emptied".to_string();
                self.refresh();
            }
            Err(e) => {
                self.status = format!("empty trash failed: {}", e);
                self.render();
            }
        }
    }

    fn create_folder(&mut self) {
        let mut name = "New Folder".to_string();
        let mut i = 2;
        let taken: Vec<String> = self.entries.iter().map(|e| e.name.to_lowercase()).collect();
        while taken.contains(&name.to_lowercase()) && i < 100 {
            name = format!("New Folder ({})", i);
            i += 1;
        }
        match fat::make_dir(&join(&self.path, &name)) {
            Ok(()) => {
                self.status = format!("created {}", name);
                self.refresh();
                if let Some(pos) = self.entries.iter().position(|e| e.name == name) {
                    self.selection = pos;
                    self.ensure_selection_visible();
                    self.render();
                }
            }
            Err(e) => {
                self.status = format!("create failed: {}", e);
                self.render();
            }
        }
    }

    /// Creates an empty text file ("New File.txt", deduplicated like the
    /// folder naming) and selects it.
    fn create_file(&mut self) {
        let mut name = "New File.txt".to_string();
        let mut i = 2;
        let taken: Vec<String> = self.entries.iter().map(|e| e.name.to_lowercase()).collect();
        while taken.contains(&name.to_lowercase()) && i < 100 {
            name = format!("New File ({}).txt", i);
            i += 1;
        }
        match fat::write_file(&join(&self.path, &name), b"") {
            Ok(()) => {
                self.status = format!("created {}", name);
                self.refresh();
                if let Some(pos) = self.entries.iter().position(|e| e.name == name) {
                    self.selection = pos;
                    self.ensure_selection_visible();
                    self.render();
                }
            }
            Err(e) => {
                self.status = format!("create failed: {}", e);
                self.render();
            }
        }
    }

    /// Enters inline rename mode for the selected item (F2). The whole
    /// name starts "selected": typing replaces it, arrow keys or
    /// Backspace keep it for editing.
    fn start_rename(&mut self) {
        let Some(entry) = self.entries.get(self.selection).cloned() else {
            return;
        };
        let name = entry.name.clone();
        self.rename = Some(RenameState {
            item: self.selection,
            buf: name.clone(),
            cursor: name.chars().count(),
            select_all: true,
        });
        self.pending_delete = None;
        self.status = format!("rename {} — Enter to confirm, Esc to cancel", name);
        self.render();
    }

    fn cancel_rename(&mut self) {
        if self.rename.take().is_some() {
            self.render();
        }
    }

    /// Applies the inline rename: the entry's directory entry is rewritten
    /// under the new name (its contents never move).
    fn commit_rename(&mut self) {
        let Some(state) = self.rename.take() else {
            return;
        };
        let new_name = state.buf.trim().to_string();
        let Some(old_name) = self.entries.get(state.item).map(|e| e.name.clone()) else {
            self.render();
            return;
        };
        if new_name.is_empty() || old_name == new_name {
            self.render();
            return;
        }
        match fat::rename(&join(&self.path, &old_name), &new_name) {
            Ok(()) => {
                self.status = format!("renamed {} to {}", old_name, new_name);
                self.refresh();
                if let Some(pos) = self.entries.iter().position(|e| e.name == new_name) {
                    self.selection = pos;
                    self.ensure_selection_visible();
                    self.render();
                }
            }
            Err(e) => {
                self.status = format!("rename failed: {}", e);
                self.render();
            }
        }
    }

    /// Routes a key event to the rename edit buffer.
    fn rename_key(&mut self, event: keyboard::Event) {
        match event {
            keyboard::Event::Escape => self.cancel_rename(),
            keyboard::Event::Enter => self.commit_rename(),
            keyboard::Event::Backspace => {
                let Some(state) = self.rename.as_mut() else {
                    return;
                };
                if state.select_all {
                    // Backspace with the name "selected" empties it.
                    state.select_all = false;
                    state.buf.clear();
                    state.cursor = 0;
                    self.render();
                } else if state.cursor > 0 {
                    let idx = char_index(&state.buf, state.cursor - 1);
                    state.buf.remove(idx);
                    state.cursor -= 1;
                    self.render();
                }
            }
            keyboard::Event::Left => {
                if let Some(state) = self.rename.as_mut() {
                    state.select_all = false;
                    state.cursor = state.cursor.saturating_sub(1);
                    self.render();
                }
            }
            keyboard::Event::Right => {
                if let Some(state) = self.rename.as_mut() {
                    state.select_all = false;
                    if state.cursor < state.buf.chars().count() {
                        state.cursor += 1;
                        self.render();
                    }
                }
            }
            keyboard::Event::Char(c) => {
                if c == '/' || c == '\\' || c.is_ascii_control() {
                    return;
                }
                let Some(state) = self.rename.as_mut() else {
                    return;
                };
                if state.select_all {
                    // First character typed replaces the whole name.
                    state.select_all = false;
                    state.buf.clear();
                    state.cursor = 0;
                }
                if state.buf.len() >= crate::fat::MAX_NAME {
                    return;
                }
                let idx = char_index(&state.buf, state.cursor);
                state.buf.insert(idx, c);
                state.cursor += 1;
                self.render();
            }
            _ => {}
        }
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            View::Details => View::Grid,
            View::Grid => View::Details,
        };
        self.scroll = self.scroll.min(self.entries.len().saturating_sub(self.visible_items()));
        self.ensure_selection_visible();
        self.render();
    }

    fn toggle_sort(&mut self, key: SortKey) {
        if self.sort == key {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort = key;
            self.sort_asc = true;
        }
        self.refresh();
    }

    fn scroll_click(&mut self, y: u32) {
        let visible = self.visible_items();
        let max_scroll = self.entries.len().saturating_sub(visible);
        if max_scroll == 0 {
            return;
        }
        let track_h = HEIGHT - STATUS_H - LIST_Y;
        let thumb_h = (track_h * visible as u32 / self.entries.len() as u32).max(12);
        let thumb_y = LIST_Y + (track_h - thumb_h) * self.scroll as u32 / max_scroll as u32;
        let page = (visible as u32 / 2).max(1);
        if y < thumb_y {
            self.scroll = self.scroll.saturating_sub(page as usize);
        } else if y >= thumb_y + thumb_h {
            self.scroll = (self.scroll + page as usize).min(max_scroll);
        }
        self.render();
    }

    // ---- click routing -------------------------------------------------

    pub fn handle_click(&mut self, x: i32, y: i32) -> Option<FsAction> {
        // Any click outside the rename box leaves rename mode.
        self.rename = None;
        self.last_click_was_double = false;
        if x < 0 || y < 0 {
            return None;
        }
        let (x, y) = (x as u32, y as u32);
        if y < PATH_H {
            return self.click_breadcrumb(x);
        }
        if y < LIST_Y {
            self.click_toolbar(x);
            return None;
        }
        if y >= HEIGHT - STATUS_H {
            return None;
        }
        if x < SIDEBAR_W {
            self.click_sidebar(y);
            return None;
        }
        if x >= SIDEBAR_W + LIST_W {
            self.scroll_click(y);
            return None;
        }
        match self.view {
            View::Details => self.click_details(x, y),
            View::Grid => self.click_grid(x, y),
        }
    }

    fn click_breadcrumb(&mut self, x: u32) -> Option<FsAction> {
        for crumb in crumb_layout(&self.path, LIST_W - 6) {
            if x >= crumb.x && x < crumb.x + crumb.w {
                self.navigate(crumb.path);
                return None;
            }
        }
        None
    }

    fn click_toolbar(&mut self, x: u32) {
        match toolbar_button_at(x) {
            Some(ToolbarAction::Up) => self.navigate(parent_path(&self.path)),
            Some(ToolbarAction::New) => self.create_folder(),
            Some(ToolbarAction::NewFile) => self.create_file(),
            Some(ToolbarAction::Del) => self.delete_selected(),
            Some(ToolbarAction::Refresh) => {
                self.status = String::new();
                self.refresh();
            }
            Some(ToolbarAction::ToggleView) => self.toggle_view(),
            None => {}
        }
    }

    fn click_sidebar(&mut self, y: u32) {
        if let Some(target) = sidebar_item_at(&self.sidebar, y) {
            self.navigate(target);
        }
    }

    fn select_item(&mut self, item: usize, now: u64) -> Option<FsAction> {
        if item >= self.entries.len() {
            self.pending_delete = None;
            return None;
        }
        let ctrl = keyboard::ctrl_down();
        let shift = keyboard::shift_down();
        if ctrl {
            // Toggle this item in the multi-selection; the primary
            // stays put as the action anchor.
            if item == self.selection {
                // Ctrl+click on the primary keeps it selected.
            } else if let Some(pos) = self.multi.iter().position(|&i| i == item) {
                self.multi.remove(pos);
            } else {
                self.multi.push(item);
            }
        } else if shift {
            // Range from the anchor to this item.
            let (lo, hi) = if self.anchor <= item {
                (self.anchor, item)
            } else {
                (item, self.anchor)
            };
            self.multi.clear();
            self.multi.extend(lo..=hi);
            self.multi.retain(|&i| i != item);
            self.selection = item;
        } else {
            self.multi.clear();
            self.selection = item;
            self.anchor = item;
        }
        self.ensure_selection_visible();
        self.pending_delete = None;
        // Modifier clicks never double-open.
        let is_double = !ctrl
            && !shift
            && self.last_click_item == item
            && now.saturating_sub(self.last_click_ticks) <= DOUBLE_CLICK_TICKS;
        self.last_click_was_double = is_double;
        self.last_click_item = item;
        self.last_click_ticks = now;
        if is_double {
            self.open_selected()
        } else {
            let entry = &self.entries[item];
            if entry.is_dir {
                self.status = format!("{} (folder)", entry.name);
            } else {
                self.status = format!("{} · {}", entry.name, fmt_size(entry.size as u64));
            }
            self.render();
            None
        }
    }

    fn click_details(&mut self, x: u32, y: u32) -> Option<FsAction> {
        if y < LIST_Y + HEADER_H {
            // Column header: sort.
            let (nx, sx, tx) = details_columns();
            if x >= nx.0 && x < nx.1 {
                self.toggle_sort(SortKey::Name);
            } else if x >= sx.0 && x < sx.1 {
                self.toggle_sort(SortKey::Size);
            } else if x >= tx.0 && x < tx.1 {
                self.toggle_sort(SortKey::Type);
            }
            return None;
        }
        let row = ((y - LIST_Y - HEADER_H) / ROW_H) as usize + self.scroll;
        let now = interrupts::ticks();
        self.select_item(row, now)
    }

    fn click_grid(&mut self, x: u32, y: u32) -> Option<FsAction> {
        let cols = self.grid_cols();
        let col = ((x - SIDEBAR_W) / GRID_TILE_W) as usize;
        let row = ((y - LIST_Y) / GRID_TILE_H) as usize;
        let item = self.scroll + row * cols + col;
        let now = interrupts::ticks();
        self.select_item(item, now)
    }

    pub fn handle_key(&mut self, event: keyboard::Event) -> Option<FsAction> {
        // While renaming, every key goes to the edit buffer.
        if self.rename.is_some() {
            self.rename_key(event);
            return None;
        }
        // While searching, every key feeds the search string.
        if self.search_mode {
            match event {
                keyboard::Event::Char(c) => {
                    if self.search.len() < 64 {
                        self.search.push(c);
                    }
                    self.jump_to_match();
                }
                keyboard::Event::Backspace => {
                    self.search.pop();
                    self.jump_to_match();
                }
                keyboard::Event::Enter => self.jump_to_match(),
                keyboard::Event::Escape => {
                    self.search_mode = false;
                    self.status = String::new();
                }
                _ => {}
            }
            self.render();
            return None;
        }
        match event {
            keyboard::Event::Up => self.move_selection(-1),
            keyboard::Event::Down => self.move_selection(1),
            keyboard::Event::Left => match self.view {
                View::Grid => self.move_selection(-(self.grid_cols() as isize)),
                View::Details => {}
            },
            keyboard::Event::Right => match self.view {
                View::Grid => self.move_selection(self.grid_cols() as isize),
                View::Details => {}
            },
            keyboard::Event::Enter => return self.open_selected(),
            keyboard::Event::Backspace => self.navigate(parent_path(&self.path)),
            // The PS/2 driver doesn't map a Delete key; 'd' (pressed
            // twice, armed first) is the explorer's delete gesture.
            keyboard::Event::Char('d') => self.delete_selected(),
            keyboard::Event::Char('r') => {
                self.status = String::new();
                self.refresh();
            }
            keyboard::Event::Char('n') => self.create_folder(),
            keyboard::Event::Char('f') => self.create_file(),
            keyboard::Event::Char('v') => self.toggle_view(),
            keyboard::Event::Ctrl('c') => self.copy_selected(),
            keyboard::Event::Ctrl('v') => self.paste_clipboard(),
            keyboard::Event::Ctrl('f') => {
                self.search_mode = true;
                self.status = "検索モード: 名前の一部を入力 / Esc で終了".to_string();
            }
            keyboard::Event::Escape => {
                self.search_mode = false;
                self.search.clear();
                self.status = String::new();
            }
            keyboard::Event::F2 => self.start_rename(),
            _ => {}
        }
        None
    }

    /// Selects the first entry whose name starts with `search`
    /// (case-insensitive), searching forward from the current selection
    /// with wrap-around.
    fn jump_to_match(&mut self) {
        let needle = self.search.to_lowercase();
        let n = self.entries.len();
        if needle.is_empty() || n == 0 {
            return;
        }
        let start = self.selection.min(n - 1);
        for offset in 0..n {
            let index = (start + offset) % n;
            let matched = self.entries[index].name.to_lowercase().starts_with(&needle);
            if matched {
                let name = self.entries[index].name.clone();
                self.selection = index;
                self.multi.clear();
                self.anchor = index;
                self.ensure_selection_visible();
                self.status = format!("検索: {} — {}", self.search, name);
                return;
            }
        }
        self.status = format!("見つかりません: {}", self.search);
    }

    /// Selects the entry under a content-local point, used by the
    /// right-click context menu so actions target the item under the
    /// cursor. Does nothing on empty space (keeps the current selection).
    pub fn select_at(&mut self, x: i32, y: i32) {
        if let Some(item) = self.item_at(x, y) {
            self.selection = item;
            self.multi.clear();
            self.anchor = item;
            self.ensure_selection_visible();
            self.render();
        }
    }

    /// Runs a right-click context-menu action on the current selection,
    /// returning an `FsAction` for the WM when the action opens
    /// something (mirrors the keyboard shortcuts).
    pub fn context_action(&mut self, action: ExplorerMenuAction) -> Option<FsAction> {
        match action {
            ExplorerMenuAction::Open => self.open_selected(),
            ExplorerMenuAction::Rename => {
                self.start_rename();
                None
            }
            ExplorerMenuAction::Delete => {
                self.delete_selected();
                None
            }
            ExplorerMenuAction::Copy => {
                self.copy_selected();
                None
            }
            ExplorerMenuAction::Paste => {
                self.paste_clipboard();
                None
            }
            ExplorerMenuAction::Restore => {
                self.restore_selected();
                None
            }
            ExplorerMenuAction::EmptyTrash => {
                self.empty_trash();
                None
            }
            ExplorerMenuAction::Refresh => {
                self.status = String::new();
                self.refresh();
                None
            }
            ExplorerMenuAction::NewFolder => {
                self.create_folder();
                None
            }
            ExplorerMenuAction::NewFile => {
                self.create_file();
                None
            }
        }
    }

    // ---- rendering -----------------------------------------------------

    fn render(&mut self) {
        let (cx, cy) = self.cursor;
        gfx::fill_rect(self, 0, 0, WIDTH, HEIGHT, BG);

        self.render_sidebar(cx, cy);
        self.render_toolbar(cx, cy);
        self.render_breadcrumbs(cx, cy);
        match self.view {
            View::Details => self.render_details(cx, cy),
            View::Grid => self.render_grid(cx, cy),
        }
        self.render_rename();
        self.render_scrollbar();
        self.render_status();
    }

    fn render_sidebar(&mut self, cx: i32, cy: i32) {
        gfx::fill_rect(self, 0, LIST_Y, SIDEBAR_W, HEIGHT - STATUS_H - LIST_Y, SIDEBAR_BG);
        gfx::fill_rect(self, SIDEBAR_W - 1, LIST_Y, 1, HEIGHT - STATUS_H - LIST_Y, 0x00_0A1016);
        let drop_target = if self.drag.is_some() { self.drop_target_at(cx, cy) } else { None };
        let sidebar: Vec<(String, String)> = self
            .sidebar
            .iter()
            .map(|e| (e.label.clone(), e.target.clone()))
            .collect();
        let mut y = LIST_Y + 6;
        for (label, target) in sidebar {
            if target.is_empty() {
                gfx::draw_string(self, 10, y, &label, TEXT_FAINT, None);
                y += 14;
                continue;
            }
            let active = self.path == target;
            let hovered = cy >= y as i32 && cy < (y + 16) as i32 && cx >= 2 && cx < SIDEBAR_W as i32 - 2;
            if active {
                gfx::fill_rounded_rect(self, 4, y, SIDEBAR_W - 8, 16, 4, 0x00_22334A);
                gfx::fill_rect(self, 4, y + 3, 2, 10, ACCENT);
            } else if hovered {
                gfx::fill_rounded_rect(self, 4, y, SIDEBAR_W - 8, 16, 4, 0x00_1E2A36);
            }
            if hovered && drop_target.as_deref() == Some(target.as_str()) {
                gfx::fill_rect(self, 4, y + 2, 2, 12, ACCENT);
                gfx::fill_rect(self, SIDEBAR_W - 8, y + 2, 2, 12, ACCENT);
            }
            let color = if active { 0x00_FFFFFF } else { TEXT };
            gfx::draw_string(self, 12, y + 4, &label, color, None);
            y += 18;
        }
    }

    fn render_toolbar(&mut self, cx: i32, cy: i32) {
        gfx::fill_rect(self, 0, PATH_H, WIDTH, TOOLBAR_H, TOOLBAR_BG);
        gfx::fill_rect(self, 0, PATH_H + TOOLBAR_H - 1, WIDTH, 1, 0x00_1A1A1A);
        for action in [ToolbarAction::Up, ToolbarAction::New, ToolbarAction::NewFile, ToolbarAction::Del, ToolbarAction::Refresh] {
            if let Some((bx, bw)) = toolbar_button_rect(action) {
                let hovered = cx >= bx as i32 && cx < (bx + bw) as i32 && cy >= PATH_H as i32 + 3 && cy < (PATH_H + TOOLBAR_H - 3) as i32;
                let (top, bottom) = if hovered {
                    (BTN_HOVER_TOP, BTN_HOVER_BOTTOM)
                } else {
                    (BTN_TOP, BTN_BOTTOM)
                };
                gfx::fill_rounded_rect_gradient_v(self, bx, PATH_H + 3, bw, TOOLBAR_H - 6, 4, top, bottom);
                gfx::fill_rect(self, bx + 1, PATH_H + TOOLBAR_H - 4, bw - 2, 1, BTN_EDGE);
                draw_toolbar_icon(self, action, bx + (bw - 16) / 2, PATH_H + 6);
            }
        }
        // View toggle (text button on the right).
        let (bx, bw) = toolbar_button_rect(ToolbarAction::ToggleView).unwrap();
        let hovered = cx >= bx as i32 && cx < (bx + bw) as i32 && cy >= PATH_H as i32 + 3 && cy < (PATH_H + TOOLBAR_H - 3) as i32;
        let (top, bottom) = if hovered {
            (BTN_HOVER_TOP, BTN_HOVER_BOTTOM)
        } else {
            (BTN_TOP, BTN_BOTTOM)
        };
        gfx::fill_rounded_rect_gradient_v(self, bx, PATH_H + 3, bw, TOOLBAR_H - 6, 4, top, bottom);
        gfx::fill_rect(self, bx + 1, PATH_H + TOOLBAR_H - 4, bw - 2, 1, BTN_EDGE);
        let label = match self.view {
            View::Details => "Icons",
            View::Grid => "List",
        };
        let lx = bx + (bw - gfx::text_width(label)) / 2;
        gfx::draw_string(self, lx, PATH_H + 9, label, TEXT, None);
    }

    fn render_breadcrumbs(&mut self, cx: i32, cy: i32) {
        gfx::fill_rect(self, 0, 0, WIDTH, PATH_H, PATH_BG);
        gfx::fill_rect(self, 0, PATH_H - 1, WIDTH, 1, 0x00_1A1A1A);
        let crumbs = crumb_layout(&self.path, LIST_W - 6);
        let last_index = crumbs.len().saturating_sub(1);
        for (i, crumb) in crumbs.iter().enumerate() {
            let is_last = i == last_index;
            let hovered = !is_last
                && cy >= 4
                && cy < PATH_H as i32 - 4
                && cx >= crumb.x as i32
                && cx < (crumb.x + crumb.w) as i32;
            if is_last {
                gfx::fill_rounded_rect(self, crumb.x, 4, crumb.w, PATH_H - 8, 4, CRUMB_BG);
            }
            let color = if is_last {
                0x00_FFFFFF
            } else if hovered {
                0x00_CFE0EC
            } else {
                TEXT_DIM
            };
            let label = &crumb.label;
            gfx::draw_string(self, crumb.x + 4, 8, label, color, None);
            if !is_last {
                gfx::draw_string(self, crumb.x + crumb.w - 7, 8, ">", TEXT_FAINT, None);
            }
        }
    }

    fn render_details(&mut self, cx: i32, cy: i32) {
        let (nx, sx, tx) = details_columns();
        let drop_target = if self.drag.is_some() { self.drop_target_at(cx, cy) } else { None };
        // Column headers.
        gfx::fill_rect(self, SIDEBAR_W, LIST_Y, LIST_W, HEADER_H, HEADER_BG);
        gfx::fill_rect(self, SIDEBAR_W, LIST_Y + HEADER_H - 1, LIST_W, 1, 0x00_1A1A1A);
        draw_header(self, "Name", nx, cx, cy);
        draw_header(self, "Size", sx, cx, cy);
        draw_header(self, "Type", tx, cx, cy);

        // Rows.
        let rows = self.details_visible_rows();
        for r in 0..rows {
            let index = r + self.scroll;
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            let (name, is_dir, size) = (entry.name.clone(), entry.is_dir, entry.size);
            let ry = LIST_Y + HEADER_H + r as u32 * ROW_H;
            let selected = self.is_selected(index);
            let hovered = cy >= ry as i32 && cy < (ry + ROW_H) as i32 && cx >= SIDEBAR_W as i32;
            if selected {
                gfx::fill_rounded_rect_gradient_v(self, SIDEBAR_W + 2, ry + 1, LIST_W - 4, ROW_H - 2, 4, SELECTED_TOP, SELECTED_BOTTOM);
            } else if hovered {
                gfx::fill_rounded_rect(self, SIDEBAR_W + 2, ry + 1, LIST_W - 4, ROW_H - 2, 4, HOVER_BG);
            }
            if is_dir && drop_target.as_deref() == Some(join(&self.path, &name).as_str()) {
                gfx::fill_rect(self, SIDEBAR_W + 3, ry + 1, LIST_W - 6, 1, ACCENT);
                gfx::fill_rect(self, SIDEBAR_W + 3, ry + ROW_H - 2, LIST_W - 6, 1, ACCENT);
            }
            draw_entry_icon(self, SIDEBAR_W + 8, ry + 4, &name, is_dir);
            let (name_color, size_color) = if selected {
                (0x00_FFFFFF, 0x00_C9DAE8)
            } else if is_dir {
                (DIR_TEXT, TEXT_DIM)
            } else if is_elf(&name) {
                (ELF_TEXT, TEXT_DIM)
            } else {
                (TEXT, TEXT_DIM)
            };
            gfx::draw_string(self, SIDEBAR_W + 26, ry + 5, &name, name_color, None);
            if !is_dir {
                let size = fmt_size(size as u64);
                let sw = gfx::text_width(&size);
                gfx::draw_string(self, sx.1 - sw - 10, ry + 5, &size, size_color, None);
            }
            let kind = if is_dir { "Folder" } else { file_type(&name) };
            let kw = gfx::text_width(kind);
            let kx = tx.0 + 8;
            if kx + kw <= tx.1 {
                gfx::draw_string(self, kx, ry + 5, kind, if selected { 0x00_C9DAE8 } else { TEXT_DIM }, None);
            }
        }
    }

    fn render_grid(&mut self, cx: i32, cy: i32) {
        let cols = self.grid_cols();
        let rows = self.grid_visible_rows();
        let drop_target = if self.drag.is_some() { self.drop_target_at(cx, cy) } else { None };
        gfx::fill_rect(self, SIDEBAR_W, LIST_Y, LIST_W, HEIGHT - STATUS_H - LIST_Y, LIST_BG);
        for r in 0..rows {
            for c in 0..cols {
                let index = self.scroll + r * cols + c;
                let Some(entry) = self.entries.get(index) else {
                    return;
                };
                let (name, is_dir) = (entry.name.clone(), entry.is_dir);
                let tx = SIDEBAR_W + c as u32 * GRID_TILE_W;
                let ty = LIST_Y + r as u32 * GRID_TILE_H;
                let selected = self.is_selected(index);
                let hovered = cx >= tx as i32 && cx < (tx + GRID_TILE_W) as i32 && cy >= ty as i32 && cy < (ty + GRID_TILE_H) as i32;
                if selected {
                    gfx::fill_rounded_rect(self, tx + 4, ty + 3, GRID_TILE_W - 8, GRID_TILE_H - 8, 6, 0x00_24467A);
                } else if hovered {
                    gfx::fill_rounded_rect(self, tx + 4, ty + 3, GRID_TILE_W - 8, GRID_TILE_H - 8, 6, 0x00_1E2A36);
                }
                if is_dir && drop_target.as_deref() == Some(join(&self.path, &name).as_str()) {
                    gfx::fill_rect(self, tx + 5, ty + 3, GRID_TILE_W - 10, 1, ACCENT);
                    gfx::fill_rect(self, tx + 5, ty + GRID_TILE_H - 6, GRID_TILE_W - 10, 1, ACCENT);
                }
                draw_big_icon(self, tx + (GRID_TILE_W - 36) / 2, ty + 6, &name, is_dir);
                // Label, centered and truncated to the tile width.
                // Truncate against the widest possible glyph (Japanese)
                // so tiles never overflow.
                let max_chars = ((GRID_TILE_W - 10) / font::char_width('あ')) as usize;
                let label = if name.chars().count() > max_chars {
                    let mut s: String = name.chars().take(max_chars - 2).collect();
                    s.push_str("..");
                    s
                } else {
                    name
                };
                let lw = gfx::text_width(&label);
                let lx = tx + (GRID_TILE_W - lw) / 2;
                let color = if selected {
                    0x00_FFFFFF
                } else if is_dir {
                    DIR_TEXT
                } else if is_elf(&label) {
                    ELF_TEXT
                } else {
                    TEXT
                };
                gfx::draw_string(self, lx, ty + 44, &label, color, None);
            }
        }
    }

    /// Overlays the inline rename box on the item being renamed (F2):
    /// an accent-bordered dark field with the buffer text and a block
    /// cursor, drawn on top of the list row or grid tile.
    fn render_rename(&mut self) {
        let Some(state) = &self.rename else { return };
        let (item, cursor, buf) = (state.item, state.cursor, state.buf.clone());
        if item >= self.entries.len() {
            return;
        }
        let gh = font::glyph_h() as u32;
        let box_h = (gh + 8).max(16);
        let (bx, by, bw) = match self.view {
            View::Details => {
                if item < self.scroll {
                    return;
                }
                let row = item - self.scroll;
                if row >= self.details_visible_rows() {
                    return;
                }
                let ry = LIST_Y + HEADER_H + row as u32 * ROW_H;
                (SIDEBAR_W + 4, ry + (ROW_H - box_h) / 2, LIST_W - 8)
            }
            View::Grid => {
                let cols = self.grid_cols();
                let row = (item - self.scroll) / cols;
                let col = (item - self.scroll) % cols;
                if row >= self.grid_visible_rows() {
                    return;
                }
                let tx = SIDEBAR_W + col as u32 * GRID_TILE_W;
                let ty = LIST_Y + row as u32 * GRID_TILE_H;
                (tx + 4, ty + 6, GRID_TILE_W - 8)
            }
        };
        gfx::fill_rounded_rect(self, bx, by, bw, box_h, 4, 0x00_101010);
        gfx::fill_rect(self, bx + 2, by, bw - 4, 1, ACCENT);
        gfx::fill_rect(self, bx + 2, by + box_h - 1, bw - 4, 1, ACCENT);
        let text_x = bx + 6;
        let max_chars = ((bw.saturating_sub(12)) / font::glyph_w() as u32) as usize;
        let start = cursor.saturating_sub(max_chars.saturating_sub(1));
        let shown: String = buf.chars().skip(start).take(max_chars).collect();
        gfx::draw_string(self, text_x, by + (box_h - gh) / 2, &shown, TEXT, None);
        let cursor_col = buf.chars().skip(start).take(cursor - start).count() as u32;
        let cx = text_x + cursor_col * font::glyph_w() as u32;
        gfx::fill_rect(self, cx, by + 1, font::glyph_w() as u32, box_h - 2, 0x00_FFCC66);
    }

    fn render_scrollbar(&mut self) {
        let track_h = HEIGHT - STATUS_H - LIST_Y;
        gfx::fill_rect(self, SIDEBAR_W + LIST_W, LIST_Y, SCROLLBAR_W, track_h, SCROLL_TRACK);
        let visible = self.visible_items();
        if visible < self.entries.len() {
            let max_scroll = self.entries.len() - visible;
            let thumb_h = (track_h * visible as u32 / self.entries.len() as u32).max(12);
            let thumb_y = LIST_Y + (track_h - thumb_h) * self.scroll as u32 / max_scroll as u32;
            gfx::fill_rect(self, SIDEBAR_W + LIST_W + 2, thumb_y, SCROLLBAR_W - 4, thumb_h, SCROLL_THUMB);
        }
    }

    fn render_status(&mut self) {
        gfx::fill_rect(self, 0, HEIGHT - STATUS_H, WIDTH, STATUS_H, STATUS_BG);
        gfx::fill_rect(self, 0, HEIGHT - STATUS_H - 1, WIDTH, 1, 0x00_0E141B);
        let status = if self.search_mode {
            format!("検索: {}|", self.search)
        } else if self.status.is_empty() {
            format!("{} items", self.entries.len())
        } else {
            self.status.clone()
        };
        let is_error = status.starts_with("cannot")
            || status.starts_with("error")
            || status.starts_with("delete failed")
            || status.starts_with("create failed")
            || status.starts_with("no viewer");
        let color = if is_error { STATUS_TEXT_BAD } else { STATUS_TEXT };
        gfx::draw_string(self, 6, HEIGHT - STATUS_H + 4, &status, color, None);
        let free = format!("free {}", fmt_size(self.free_bytes));
        let fw = (free.len() * font::glyph_w()) as u32;
        gfx::draw_string(self, WIDTH - fw - 8, HEIGHT - STATUS_H + 4, &free, TEXT_DIM, None);
    }
}

struct Crumb {
    x: u32,
    w: u32,
    label: String,
    path: String,
}

/// Clickable breadcrumb segments for `path`, right-aligned so the deepest
/// segment is always visible; a "..." crumb (clickable, goes to "/") is
/// prepended when the path is too wide.
fn crumb_layout(path: &str, max_w: u32) -> Vec<Crumb> {
    let mut segs: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(seg);
        segs.push((seg.to_string(), acc.clone()));
    }
    let width_of = |label: &str| (label.len() as u32) * font::glyph_w() as u32 + 16;
    let mut start = 0;
    if segs.len() > 2 {
        while start + 2 < segs.len() {
            let w: u32 = segs[start..].iter().map(|(l, _)| width_of(l)).sum();
            if w <= max_w {
                break;
            }
            start += 1;
        }
    }
    let mut out = Vec::new();
    let mut x = 6u32;
    if start > 0 {
        out.push(Crumb { x, w: 24, label: "...".to_string(), path: "/".to_string() });
        x += 30;
    }
    for (i, (label, target)) in segs.iter().enumerate().skip(start) {
        let w = width_of(label);
        out.push(Crumb { x, w, label: label.clone(), path: target.clone() });
        if i + 1 < segs.len() {
            x += w + 2;
        } else {
            x += w;
        }
    }
    out
}

/// Details-view column x-ranges: (Name, Size, Type) as (start, end).
fn details_columns() -> ((u32, u32), (u32, u32), (u32, u32)) {
    let end = SIDEBAR_W + LIST_W;
    ((SIDEBAR_W, 320), (320, 460), (460, end))
}

/// Toolbar button x-rect; the toggle is right-aligned, the rest are a
/// fixed row on the left.
fn toolbar_button_rect(action: ToolbarAction) -> Option<(u32, u32)> {
    match action {
        ToolbarAction::Up => Some((6, 48)),
        ToolbarAction::New => Some((58, 48)),
        ToolbarAction::NewFile => Some((110, 48)),
        ToolbarAction::Del => Some((162, 48)),
        ToolbarAction::Refresh => Some((214, 64)),
        ToolbarAction::ToggleView => Some((WIDTH - 76, 70)),
    }
}

fn toolbar_button_at(x: u32) -> Option<ToolbarAction> {
    [
        ToolbarAction::Up,
        ToolbarAction::New,
        ToolbarAction::NewFile,
        ToolbarAction::Del,
        ToolbarAction::Refresh,
        ToolbarAction::ToggleView,
    ]
    .into_iter()
    .find(|action| {
        toolbar_button_rect(*action).is_some_and(|(bx, bw)| x >= bx && x < bx + bw)
    })
}

/// Sidebar entries model a real user's home first, then expose the
/// system-level folders like a small "This PC" section.
fn build_sidebar() -> Vec<SidebarEntry> {
    let mut out = vec![SidebarEntry { label: "QUICK ACCESS".to_string(), target: String::new() }];
    let home = users::home();
    out.push(SidebarEntry { label: "Home".to_string(), target: home.clone() });
    out.push(SidebarEntry { label: "Desktop".to_string(), target: users::desktop() });
    out.push(SidebarEntry { label: "Documents".to_string(), target: users::documents() });
    out.push(SidebarEntry { label: "Downloads".to_string(), target: format!("{}/Downloads", home) });
    out.push(SidebarEntry { label: "Pictures".to_string(), target: format!("{}/Pictures", home) });
    out.push(SidebarEntry { label: "Music".to_string(), target: format!("{}/Music", home) });
    out.push(SidebarEntry { label: String::new(), target: String::new() });
    out.push(SidebarEntry { label: "SYSTEM".to_string(), target: String::new() });
    out.push(SidebarEntry { label: "Trash".to_string(), target: crate::trash::TRASH_PATH.to_string() });
    out.push(SidebarEntry { label: "Applications".to_string(), target: "/bin".to_string() });
    out.push(SidebarEntry { label: "System".to_string(), target: "/system".to_string() });
    out.push(SidebarEntry { label: "Users".to_string(), target: "/users".to_string() });
    out
}

/// Sidebar item hit test: skips header rows.
fn sidebar_item_at(sidebar: &[SidebarEntry], y: u32) -> Option<String> {
    let mut ry = LIST_Y + 6;
    for entry in sidebar {
        if entry.target.is_empty() {
            ry += 14;
            continue;
        }
        if y >= ry && y < ry + 16 {
            return Some(entry.target.clone());
        }
        ry += 18;
    }
    None
}

fn draw_header(surface: &mut dyn Surface, label: &str, range: (u32, u32), cx: i32, cy: i32) {
    let hovered = cx >= range.0 as i32 && cx < range.1 as i32 && cy >= LIST_Y as i32 && cy < (LIST_Y + HEADER_H) as i32;
    if hovered {
        gfx::fill_rect(surface, range.0, LIST_Y, range.1 - range.0, HEADER_H, 0x00_25313E);
    }
    let color = if hovered { 0x00_C9D6E0 } else { HEADER_FG };
    gfx::draw_string(surface, range.0 + 8, LIST_Y + 8, label, color, None);
}

const HEADER_FG: u32 = 0x00_9AA8B5;

/// Small folder / document / executable glyph (14x10) used in rows.
fn draw_entry_icon(surface: &mut dyn Surface, x: u32, y: u32, name: &str, is_dir: bool) {
    if is_dir {
        gfx::fill_rect(surface, x, y + 1, 5, 2, ICON_FOLDER);
        gfx::fill_rect(surface, x + 5, y, 5, 2, ICON_FOLDER);
        gfx::fill_rect(surface, x + 10, y + 1, 2, 1, ICON_FOLDER);
        gfx::fill_rect(surface, x, y + 3, 14, 7, ICON_FOLDER);
        gfx::fill_rect(surface, x + 1, y + 4, 12, 5, LIST_BG);
    } else {
        let color = if is_elf(name) { ICON_ELF } else { ICON_FILE };
        gfx::fill_rect(surface, x + 2, y, 9, 10, color);
        gfx::fill_rect(surface, x + 3, y + 1, 7, 8, LIST_BG);
        gfx::fill_rect(surface, x + 4, y + 3, 5, 1, color);
        gfx::fill_rect(surface, x + 4, y + 5, 5, 1, color);
        gfx::fill_rect(surface, x + 4, y + 7, 3, 1, color);
    }
}

/// Larger 36x32 glyph for the icon view.
fn draw_big_icon(surface: &mut dyn Surface, x: u32, y: u32, name: &str, is_dir: bool) {
    if is_dir {
        gfx::fill_rect(surface, x + 2, y + 6, 12, 5, ICON_FOLDER);
        gfx::fill_rect(surface, x + 14, y + 2, 12, 5, ICON_FOLDER);
        gfx::fill_rect(surface, x + 26, y + 5, 4, 3, ICON_FOLDER);
        gfx::fill_rect(surface, x, y + 11, 36, 19, ICON_FOLDER);
        gfx::fill_rect(surface, x + 3, y + 14, 30, 13, LIST_BG);
    } else {
        let color = if is_elf(name) { ICON_ELF } else { ICON_FILE };
        gfx::fill_rect(surface, x + 6, y, 22, 28, color);
        gfx::fill_rect(surface, x + 9, y + 3, 16, 22, LIST_BG);
        gfx::fill_rect(surface, x + 11, y + 9, 12, 2, color);
        gfx::fill_rect(surface, x + 11, y + 14, 12, 2, color);
        gfx::fill_rect(surface, x + 11, y + 19, 8, 2, color);
    }
}

/// 16x14 icon for toolbar buttons.
fn draw_toolbar_icon(surface: &mut dyn Surface, action: ToolbarAction, x: u32, y: u32) {
    let color = 0x00_DBE6F0;
    match action {
        ToolbarAction::Up => {
            gfx::fill_rect(surface, x + 7, y + 1, 2, 9, color);
            draw_diagonal(surface, x + 1, y + 7, x + 7, y + 1, color);
            draw_diagonal(surface, x + 15, y + 7, x + 9, y + 1, color);
        }
        ToolbarAction::New => {
            gfx::fill_rect(surface, x + 1, y + 5, 6, 2, color);
            gfx::fill_rect(surface, x + 7, y + 3, 6, 2, color);
            gfx::fill_rect(surface, x, y + 7, 16, 7, color);
            gfx::fill_rect(surface, x + 2, y + 8, 12, 5, BG);
            gfx::fill_rect(surface, x + 11, y + 8, 3, 2, color);
            gfx::fill_rect(surface, x + 12, y + 7, 1, 4, color);
        }
        ToolbarAction::NewFile => {
            // Document with a plus sign in the corner.
            gfx::fill_rect(surface, x + 4, y, 8, 12, color);
            gfx::fill_rect(surface, x + 5, y + 1, 6, 10, BG);
            gfx::fill_rect(surface, x + 6, y + 3, 4, 1, color);
            gfx::fill_rect(surface, x + 6, y + 6, 4, 1, color);
            gfx::fill_rect(surface, x + 6, y + 9, 4, 1, color);
            gfx::fill_rect(surface, x + 11, y + 4, 4, 6, color);
            gfx::fill_rect(surface, x + 12, y + 3, 2, 8, color);
        }
        ToolbarAction::Del => {
            gfx::fill_rect(surface, x + 2, y, 12, 2, color);
            gfx::fill_rect(surface, x + 5, y + 2, 2, 2, color);
            gfx::fill_rect(surface, x + 9, y + 2, 2, 2, color);
            gfx::fill_rect(surface, x + 3, y + 4, 10, 10, color);
            gfx::fill_rect(surface, x + 5, y + 6, 2, 6, BG);
            gfx::fill_rect(surface, x + 9, y + 6, 2, 6, BG);
        }
        ToolbarAction::Refresh => {
            // Circular arrow: ring with the bottom-left corner open.
            gfx::fill_rect(surface, x + 1, y + 1, 14, 3, color);
            gfx::fill_rect(surface, x + 1, y + 10, 14, 3, color);
            gfx::fill_rect(surface, x + 1, y + 1, 3, 7, color);
            gfx::fill_rect(surface, x + 12, y + 6, 3, 7, color);
            draw_diagonal(surface, x + 1, y + 7, x + 5, y + 3, color);
            draw_diagonal(surface, x + 13, y + 11, x + 11, y + 13, color);
            draw_diagonal(surface, x + 12, y + 13, x + 14, y + 13, color);
        }
        ToolbarAction::ToggleView => {}
    }
}

/// 1px-thick diagonal line between two endpoints (any direction).
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

    // Row-wise fast paths over the contiguous buffer (see `fb::State`);
    // hover re-renders the whole window on every cursor move.
    fn blit_span(&mut self, x: u32, y: u32, src: &[u32], offset: usize, len: u32) {
        if y >= HEIGHT || x >= WIDTH || len == 0 {
            return;
        }
        let n = len.min(WIDTH - x) as usize;
        let row_start = (y * WIDTH + x) as usize;
        self.buffer[row_start..row_start + n].copy_from_slice(&src[offset..offset + n]);
    }

    fn fill_span(&mut self, x: u32, y: u32, len: u32, color: u32) {
        if y >= HEIGHT || x >= WIDTH || len == 0 {
            return;
        }
        let n = len.min(WIDTH - x) as usize;
        let row_start = (y * WIDTH + x) as usize;
        self.buffer[row_start..row_start + n].fill(color);
    }
}
