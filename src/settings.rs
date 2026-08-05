//! System settings: persistent configuration stored on disk and applied
//! to the desktop UI (accent colors, wallpaper, background task display,
//! display resolution, and font size). The Settings app itself is a
//! Win11-style panel with a left navigation sidebar and a content pane.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::cpuid;
use crate::fat;
use crate::fb;
use crate::font;
use crate::gfx::{self, Surface};
use crate::multiboot;

const SETTINGS_PATH: &str = "/system/settings.conf";

const WIDTH: u32 = 480;
const SIDEBAR_W: u32 = 140;

const BG: u32 = 0x00_202020;
const SIDEBAR_BG: u32 = 0x00_1A1A1A;
const TEXT: u32 = 0x00_FFFFFF;
const TEXT_DIM: u32 = 0x00_9A9A9A;
const ROW_BG: u32 = 0x00_2C2C2C;
const ROW_HOVER: u32 = 0x00_343434;
const NAV_HOVER: u32 = 0x00_262626;
const NAV_SELECTED: u32 = 0x00_2C2C2C;
const TOGGLE_OFF: u32 = 0x00_4A4A4A;
const DIVIDER: u32 = 0x00_2A2A2A;

/// Accent color presets.
const ACCENT_COLORS: [(&str, u32); 5] = [
    ("blue", 0x00_4CC2FF),
    ("purple", 0x00_A87BD4),
    ("green", 0x00_86C77B),
    ("orange", 0x00_E8A87B),
    ("red", 0x00_E87B7B),
];

/// The height of the window at the current font scale; grows with the
/// font so the System page's option list always fits.
fn window_height() -> u32 {
    380 + (font::scale() as u32 - 1) * 180
}

/// One row of a selectable option list.
fn row_height() -> u32 {
    font::glyph_h() as u32 + 12
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    System,
    Personalization,
    About,
}

/// System settings stored in memory and persisted to disk.
pub struct Settings {
    pub accent_color: String,
    pub wallpaper: bool,
    pub show_bg_tasks: bool,
    /// `None` = native (physical framebuffer) resolution.
    pub resolution: Option<(u32, u32)>,
    pub font_scale_percent: u16,
}

impl Settings {
    /// Loads settings from disk, falling back to defaults if the file
    /// doesn't exist or is malformed.
    pub fn load() -> Self {
        let mut settings = Self::defaults();
        if let Ok(data) = fat::read_file(SETTINGS_PATH) {
            if let Ok(text) = core::str::from_utf8(&data) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((key, value)) = line.split_once('=') {
                        let key = key.trim();
                        let value = value.trim();
                        match key {
                            "accent_color" => settings.accent_color = value.to_string(),
                            "wallpaper" => settings.wallpaper = value == "true",
                            "show_bg_tasks" => settings.show_bg_tasks = value == "true",
                            "resolution" => {
                                settings.resolution = parse_resolution(value);
                            }
                            "font_scale" => {
                                settings.font_scale_percent = value.parse().unwrap_or(100);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        settings
    }

    /// Saves current settings to disk.
    pub fn save(&self) -> bool {
        let mut content = String::new();
        let _ = writeln!(content, "# MachaOS settings");
        let _ = writeln!(content, "accent_color={}", self.accent_color);
        let _ = writeln!(content, "wallpaper={}", self.wallpaper);
        let _ = writeln!(content, "show_bg_tasks={}", self.show_bg_tasks);
        let _ = writeln!(
            content,
            "resolution={}",
            match self.resolution {
                Some((w, h)) => format!("{}x{}", w, h),
                None => "native".to_string(),
            }
        );
        let _ = writeln!(content, "font_scale={}", self.font_scale_percent);
        fat::write_file(SETTINGS_PATH, content.as_bytes()).is_ok()
    }

    fn defaults() -> Self {
        Self {
            accent_color: "blue".to_string(),
            wallpaper: true,
            show_bg_tasks: true,
            resolution: None,
            font_scale_percent: 100,
        }
    }

    /// Returns the accent color as a u32 RGB value.
    pub fn accent_color_rgb(&self) -> u32 {
        ACCENT_COLORS
            .iter()
            .find(|(name, _)| *name == self.accent_color)
            .map(|(_, color)| *color)
            .unwrap_or(0x00_4CC2FF)
    }
}

fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    if value == "native" {
        return None;
    }
    let (w, h) = value.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    let (pw, ph) = fb::dimensions();
    if w > pw || h > ph || w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// What the Settings app asks the window manager to do after a click.
pub enum SettingsChange {
    /// Nothing visible changed; no action needed.
    None,
    /// Cosmetic settings (accent, wallpaper, bg-tasks) changed: reload
    /// the in-memory copy.
    Reload,
    /// The display resolution changed; the WM must rescale the desktop.
    Resolution(u32, u32),
    /// The font scale changed; the WM must rebuild the desktop at the
    /// new scale.
    FontScale(u8),
}

/// Settings app UI: pixel-buffer based like Calculator, Win11-flavored.
pub struct SettingsApp {
    buffer: Vec<u32>,
    settings: Settings,
    page: Page,
    cursor: (i32, i32),
    about_lines: Vec<String>,
}

impl SettingsApp {
    pub fn new(settings: Settings) -> Self {
        let height = window_height();
        let mut app = Self {
            buffer: vec![BG; (WIDTH * height) as usize],
            settings,
            page: Page::System,
            cursor: (i32::MAX, i32::MAX),
            about_lines: build_about_lines(),
        };
        app.render();
        app
    }

    pub fn width(&self) -> u32 {
        WIDTH
    }

    pub fn height(&self) -> u32 {
        window_height()
    }

    pub fn pixels(&self) -> &[u32] {
        &self.buffer
    }

    pub fn render_hover(&mut self, x: i32, y: i32) {
        if self.cursor != (x, y) {
            self.cursor = (x, y);
            self.render();
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Vertical extent of the page content under the page title.
    fn content_top(&self) -> u32 {
        12 + font::glyph_h() as u32 + 8
    }

    /// Hit-test and apply a click inside the app's content area.
    pub fn handle_click(&mut self, x: i32, y: i32) -> SettingsChange {
        if x < 0 || y < 0 {
            return SettingsChange::None;
        }
        let (x, y) = (x as u32, y as u32);

        // Left navigation sidebar.
        if x < SIDEBAR_W {
            let nav_h = 36u32;
            for (i, page) in [Page::System, Page::Personalization, Page::About].iter().enumerate() {
                let ny = 12 + i as u32 * (nav_h + 8);
                if y >= ny && y < ny + nav_h {
                    self.page = *page;
                    self.render();
                    return SettingsChange::None;
                }
            }
            return SettingsChange::None;
        }

        let cx = x - SIDEBAR_W;
        match self.page {
            Page::System => self.handle_system_click(cx, y),
            Page::Personalization => self.handle_personalization_click(cx, y),
            Page::About => SettingsChange::None,
        }
    }

    fn handle_system_click(&mut self, x: u32, y: u32) -> SettingsChange {
        let rh = row_height();
        let mut yy = self.content_top();

        // Resolution section.
        let label_h = font::glyph_h() as u32 + 6;
        yy += label_h;
        let resolutions = supported_resolutions();
        let native = fb::dimensions();
        for (i, &(w, h)) in resolutions.iter().enumerate() {
            let ry = yy + i as u32 * rh;
            if y >= ry && y < ry + rh && x >= 16 && x < WIDTH - SIDEBAR_W - 16 {
                let next = if (w, h) == native { None } else { Some((w, h)) };
                if self.settings.resolution != next {
                    self.settings.resolution = next;
                    self.settings.save();
                    self.render();
                    return SettingsChange::Resolution(w, h);
                }
                return SettingsChange::None;
            }
        }

        // Font size section.
        yy += resolutions.len() as u32 * rh + 10;
        yy += label_h;
        let font_options = [(100u16, "100%"), (200, "200%")];
        for (i, &(percent, _)) in font_options.iter().enumerate() {
            let ry = yy + i as u32 * rh;
            if y >= ry && y < ry + rh && x >= 16 && x < WIDTH - SIDEBAR_W - 16 {
                let scale = (percent / 100) as u8;
                if self.settings.font_scale_percent != percent {
                    self.settings.font_scale_percent = percent;
                    self.settings.save();
                    self.render();
                    return SettingsChange::FontScale(scale);
                }
                return SettingsChange::None;
            }
        }
        SettingsChange::None
    }

    fn handle_personalization_click(&mut self, x: u32, y: u32) -> SettingsChange {
        let mut yy = self.content_top();
        let label_h = font::glyph_h() as u32 + 6;
        yy += label_h;

        // Accent color swatches.
        let swatch = 28u32;
        let gap = 12u32;
        for (i, (name, _)) in ACCENT_COLORS.iter().enumerate() {
            let sx = SIDEBAR_W + 16 + i as u32 * (swatch + gap);
            if x >= sx && x < sx + swatch && y >= yy && y < yy + swatch {
                if self.settings.accent_color != *name {
                    self.settings.accent_color = name.to_string();
                    self.settings.save();
                    self.render();
                    return SettingsChange::Reload;
                }
                return SettingsChange::None;
            }
        }

        yy += swatch + 18;

        // Wallpaper toggle.
        if self.toggle_hit(x, y, yy) {
            self.settings.wallpaper = !self.settings.wallpaper;
            self.settings.save();
            self.render();
            return SettingsChange::Reload;
        }

        yy += 24 + 14;

        // Background tasks toggle.
        if self.toggle_hit(x, y, yy) {
            self.settings.show_bg_tasks = !self.settings.show_bg_tasks;
            self.settings.save();
            self.render();
            return SettingsChange::Reload;
        }
        SettingsChange::None
    }

    /// Hit-test a Win11 switch at the given row's y position.
    fn toggle_hit(&self, x: u32, y: u32, row_y: u32) -> bool {
        let toggle_x = WIDTH - SIDEBAR_W - 16 - 44;
        let toggle_y = row_y + 2;
        y >= toggle_y && y < toggle_y + 22 && x >= toggle_x && x < toggle_x + 44
    }

    fn render(&mut self) {
        let (cx, cy) = self.cursor;
        let height = window_height();
        let accent = self.settings.accent_color_rgb();
        gfx::fill_rect(self, 0, 0, WIDTH, height, BG);
        self.render_sidebar(accent, cx, cy);
        self.render_page(accent, cx, cy);
        // Bottom status line.
        let status = if self.settings.save() {
            "Settings saved"
        } else {
            "Failed to save"
        };
        gfx::draw_string(self, SIDEBAR_W + 16, height - font::glyph_h() as u32 - 8, status, TEXT_DIM, None);
    }

    fn render_sidebar(&mut self, accent: u32, cx: i32, cy: i32) {
        let height = window_height();
        gfx::fill_rect(self, 0, 0, SIDEBAR_W, height, SIDEBAR_BG);
        gfx::fill_rect(self, SIDEBAR_W - 1, 0, 1, height, DIVIDER);

        let nav_h = 36u32;
        for (i, (page, label)) in [Page::System, Page::Personalization, Page::About]
            .iter()
            .zip(["System", "Personalization", "About"])
            .enumerate()
        {
            let ny = 12 + i as u32 * (nav_h + 8);
            let hovered = cx >= 8
                && cx < SIDEBAR_W as i32 - 8
                && cy >= ny as i32
                && cy < (ny + nav_h) as i32;
            let selected = self.page == *page;
            let bg = if selected {
                NAV_SELECTED
            } else if hovered {
                NAV_HOVER
            } else {
                SIDEBAR_BG
            };
            gfx::fill_rounded_rect(self, 8, ny, SIDEBAR_W - 16, nav_h, 4, bg);
            if selected {
                gfx::fill_rounded_rect(self, 8, ny + 8, 3, nav_h - 16, 2, accent);
            }
            let text_color = if selected { TEXT } else { TEXT_DIM };
            let ty = ny + (nav_h - font::glyph_h() as u32) / 2;
            gfx::draw_string(self, 18, ty, label, text_color, None);
        }
    }

    fn render_page(&mut self, accent: u32, cx: i32, cy: i32) {
        match self.page {
            Page::System => self.render_system(accent, cx, cy),
            Page::Personalization => self.render_personalization(accent, cx, cy),
            Page::About => self.render_about(),
        }
    }

    fn page_title(&mut self, title: &str) {
        gfx::draw_string(self, SIDEBAR_W + 16, 12, title, TEXT, None);
    }

    fn section_label(&mut self, y: u32, label: &str) {
        gfx::draw_string(self, SIDEBAR_W + 16, y, label, TEXT_DIM, None);
    }

    fn render_system(&mut self, accent: u32, cx: i32, cy: i32) {
        let rh = row_height();
        self.page_title("Display");
        let mut yy = self.content_top();

        // Resolution option rows.
        self.section_label(yy, "Resolution");
        yy += font::glyph_h() as u32 + 6;
        let resolutions = supported_resolutions();
        let native = fb::dimensions();
        let selected: Option<(u32, u32)> = self.settings.resolution.or(Some(native));
        for (i, &(w, h)) in resolutions.iter().enumerate() {
            let ry = yy + i as u32 * rh;
            self.render_option_row(ry, accent, cx, cy, Some((w, h)) == selected, w, h);
        }

        // Font size option rows.
        yy += resolutions.len() as u32 * rh + 10;
        self.section_label(yy, "Font size");
        yy += font::glyph_h() as u32 + 6;
        let options = [
            (100u16, "100%  (Default)"),
            (200, "200%  (Large)"),
        ];
        for (i, &(percent, label)) in options.iter().enumerate() {
            let ry = yy + i as u32 * rh;
            let is_selected = self.settings.font_scale_percent == percent;
            let label_text = label.to_string();
            self.render_option_row_text(ry, accent, cx, cy, is_selected, &label_text);
        }

        // Current settings summary.
        yy += 2 * rh + 12;
        let (pw, ph) = native;
        let vw = format!("Screen: {} x {}", pw, ph);
        gfx::draw_string(self, SIDEBAR_W + 16, yy, &vw, TEXT_DIM, None);
    }

    /// A resolution row: radio circle + "W x H" label.
    fn render_option_row(&mut self, y: u32, accent: u32, cx: i32, cy: i32, selected: bool, w: u32, h: u32) {
        let label = format!("{} x {}", w, h);
        self.render_option_row_text(y, accent, cx, cy, selected, &label);
    }

    fn render_option_row_text(&mut self, y: u32, accent: u32, cx: i32, cy: i32, selected: bool, label: &str) {
        let rh = row_height();
        let x = SIDEBAR_W + 16;
        let hovered = cx >= x as i32
            && cx < (WIDTH - 16) as i32
            && cy >= y as i32
            && cy < (y + rh) as i32;
        let bg = if selected {
            gfx::blend(accent, ROW_BG, 45)
        } else if hovered {
            ROW_HOVER
        } else {
            ROW_BG
        };
        gfx::fill_rounded_rect(self, x, y, WIDTH - SIDEBAR_W - 32, rh, 6, bg);
        if selected {
            gfx::fill_rounded_rect(self, x, y, WIDTH - SIDEBAR_W - 32, 1, 1, accent);
        }

        // Radio indicator.
        let ry = y + (rh - 12) / 2;
        let ring = if selected { accent } else { 0x00_5A5A5A };
        gfx::fill_rounded_rect(self, x + 12, ry, 12, 12, 6, ring);
        if selected {
            gfx::fill_rounded_rect(self, x + 16, ry + 4, 4, 4, 2, TEXT);
        }
        let ty = y + (rh - font::glyph_h() as u32) / 2;
        gfx::draw_string(self, x + 34, ty, label, TEXT, None);
    }

    fn render_personalization(&mut self, accent: u32, cx: i32, cy: i32) {
        self.page_title("Personalization");
        let mut yy = self.content_top();

        self.section_label(yy, "Accent color");
        yy += font::glyph_h() as u32 + 6;
        let swatch = 28u32;
        let gap = 12u32;
        for (i, (name, color)) in ACCENT_COLORS.iter().enumerate() {
            let sx = SIDEBAR_W + 16 + i as u32 * (swatch + gap);
            let selected = self.settings.accent_color == *name;
            let hovered = cx >= sx as i32
                && cx < (sx + swatch) as i32
                && cy >= yy as i32
                && cy < (yy + swatch) as i32;
            if selected || hovered {
                let ring = if selected { TEXT } else { 0x00_4A4A4A };
                gfx::fill_rounded_rect(self, sx - 3, yy - 3, swatch + 6, swatch + 6, 8, ring);
            }
            gfx::fill_rounded_rect(self, sx, yy, swatch, swatch, 6, *color);
        }

        yy += swatch + 18;

        // Wallpaper toggle row.
        gfx::draw_string(self, SIDEBAR_W + 16, yy, "Wallpaper", TEXT, None);
        self.draw_toggle(SIDEBAR_W + 16, yy + 2, self.settings.wallpaper, accent, cx, cy);

        yy += 24 + 14;

        // Background tasks toggle row.
        gfx::draw_string(self, SIDEBAR_W + 16, yy, "Show background tasks", TEXT, None);
        self.draw_toggle(SIDEBAR_W + 16, yy + 2, self.settings.show_bg_tasks, accent, cx, cy);
    }

    fn render_about(&mut self) {
        self.page_title("About");
        let lines = self.about_lines.clone();
        let mut y = self.content_top() + 4;
        for line in &lines {
            gfx::draw_string(self, SIDEBAR_W + 16, y, line, TEXT_DIM, None);
            y += font::glyph_h() as u32 + 6;
        }
    }

    fn draw_toggle(&mut self, x: u32, y: u32, on: bool, accent: u32, cx: i32, cy: i32) {
        let w = 44;
        let h = 22;
        let _ = x;
        let track_x = WIDTH - SIDEBAR_W - 16 - w;
        let hovered = cx >= track_x as i32
            && cx < (track_x + w) as i32
            && cy >= y as i32
            && cy < (y + h) as i32;
        let track_color = if on { accent } else { TOGGLE_OFF };
        gfx::fill_rounded_rect(self, track_x, y, w, h, 11, track_color);
        let knob_x = if on { track_x + w - h + 2 } else { track_x + 2 };
        gfx::fill_rounded_rect(self, knob_x, y + 2, h - 4, h - 4, 9, 0x00_FFFFFF);
        if hovered {
            gfx::fill_rounded_rect(self, track_x - 2, y - 2, w + 4, h + 4, 13, 0x00_4A4A4A);
        }
    }
}

/// The resolutions offered by the Settings app, filtered to sizes that
/// fit within the physical framebuffer.
fn supported_resolutions() -> Vec<(u32, u32)> {
    let (pw, ph) = fb::dimensions();
    fb::SUPPORTED_RESOLUTIONS
        .iter()
        .copied()
        .filter(|&(w, h)| w <= pw && h <= ph)
        .collect()
}

fn build_about_lines() -> Vec<String> {
    let mut lines = vec![];
    lines.push("MachaOS v0.1.0".to_string());

    let vendor = cpuid::vendor_id();
    lines.push(format!("CPU: {}", core::str::from_utf8(&vendor).unwrap_or("unknown")));
    if let Some(brand) = cpuid::brand_string() {
        let brand = core::str::from_utf8(&brand).unwrap_or("").trim_end().to_string();
        if !brand.is_empty() {
            lines.push(brand);
        }
    }

    let info = multiboot::info();
    if let (Some(lower), Some(upper)) = (info.memory_lower_kb(), info.memory_upper_kb()) {
        lines.push(format!("Memory: {} KiB", lower + upper));
    }
    let (pw, ph) = fb::dimensions();
    lines.push(format!("Display: {}x{}x32", pw, ph));
    lines.push(format!(
        "Heap: {} / {} KiB",
        crate::allocator::allocated_bytes() / 1024,
        crate::allocator::total_bytes() / 1024
    ));
    lines
}

impl Surface for SettingsApp {
    fn width(&self) -> u32 {
        WIDTH
    }

    fn height(&self) -> u32 {
        window_height()
    }

    fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        if x < WIDTH && y < self.height() {
            let idx = (y * WIDTH + x) as usize;
            self.buffer[idx] = color;
        }
    }

    fn get_pixel(&self, x: u32, y: u32) -> Option<u32> {
        if x < WIDTH && y < self.height() {
            Some(self.buffer[(y * WIDTH + x) as usize])
        } else {
            None
        }
    }

    // Row-wise fast paths over the contiguous buffer (see `fb::State`);
    // hover re-renders the whole window on every cursor move.
    fn blit_span(&mut self, x: u32, y: u32, src: &[u32], offset: usize, len: u32) {
        let h = self.height();
        if y >= h || x >= WIDTH || len == 0 {
            return;
        }
        let n = len.min(WIDTH - x) as usize;
        let row_start = (y * WIDTH + x) as usize;
        self.buffer[row_start..row_start + n].copy_from_slice(&src[offset..offset + n]);
    }

    fn fill_span(&mut self, x: u32, y: u32, len: u32, color: u32) {
        let h = self.height();
        if y >= h || x >= WIDTH || len == 0 {
            return;
        }
        let n = len.min(WIDTH - x) as usize;
        let row_start = (y * WIDTH + x) as usize;
        self.buffer[row_start..row_start + n].fill(color);
    }
}
