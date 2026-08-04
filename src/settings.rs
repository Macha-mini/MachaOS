//! System settings: persistent configuration stored on disk and applied
//! to the desktop UI (accent colors, wallpaper, background task display).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::fat;
use crate::font;
use crate::gfx::{self, Surface};

const SETTINGS_PATH: &str = "/system/settings.conf";
const WIDTH: u32 = 420;
const HEIGHT: u32 = 360;

const BG: u32 = 0x00_141B23;
const SECTION_BG: u32 = 0x00_1A2430;
const TEXT: u32 = 0x00_DBE6F0;
const TEXT_DIM: u32 = 0x00_9AA8B5;
const BUTTON_BG: u32 = 0x00_2A3642;
const BUTTON_HOVER: u32 = 0x00_35424F;
const TOGGLE_ON: u32 = 0x00_4CAF50;
const TOGGLE_OFF: u32 = 0x00_4A4A4A;

/// Accent color presets.
const ACCENT_COLORS: [(&str, u32); 5] = [
    ("blue", 0x00_6FB1E8),
    ("purple", 0x00_A87BD4),
    ("green", 0x00_86C77B),
    ("orange", 0x00_E8A87B),
    ("red", 0x00_E87B7B),
];

/// System settings stored in memory and persisted to disk.
pub struct Settings {
    pub accent_color: String,
    pub wallpaper: bool,
    pub show_bg_tasks: bool,
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
        fat::write_file(SETTINGS_PATH, content.as_bytes()).is_ok()
    }

    fn defaults() -> Self {
        Self {
            accent_color: "blue".to_string(),
            wallpaper: true,
            show_bg_tasks: true,
        }
    }

    /// Returns the accent color as a u32 RGB value.
    pub fn accent_color_rgb(&self) -> u32 {
        ACCENT_COLORS
            .iter()
            .find(|(name, _)| *name == self.accent_color)
            .map(|(_, color)| *color)
            .unwrap_or(0x00_6FB1E8)
    }
}

/// Settings app UI: pixel-buffer based like Calculator.
pub struct SettingsApp {
    buffer: Vec<u32>,
    settings: Settings,
    cursor: (i32, i32),
}

impl SettingsApp {
    pub fn new(settings: Settings) -> Self {
        let mut app = Self {
            buffer: vec![BG; (WIDTH * HEIGHT) as usize],
            settings,
            cursor: (i32::MAX, i32::MAX),
        };
        app.render();
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

    pub fn render_hover(&mut self, x: i32, y: i32) {
        if self.cursor != (x, y) {
            self.cursor = (x, y);
            self.render();
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn handle_click(&mut self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        let (x, y) = (x as u32, y as u32);
        let mut changed = false;

        // Accent color swatches (y=60..100)
        if y >= 60 && y < 100 {
            let swatch_w = 50;
            let swatch_h = 30;
            let start_x = 20;
            let gap = 10;
            for (i, (name, _)) in ACCENT_COLORS.iter().enumerate() {
                let sx = start_x + i as u32 * (swatch_w + gap);
                if x >= sx && x < sx + swatch_w {
                    self.settings.accent_color = name.to_string();
                    changed = true;
                    break;
                }
            }
        }

        // Wallpaper toggle (y=140..170)
        if y >= 140 && y < 170 && x >= WIDTH - 80 && x < WIDTH - 20 {
            self.settings.wallpaper = !self.settings.wallpaper;
            changed = true;
        }

        // Background tasks toggle (y=200..230)
        if y >= 200 && y < 230 && x >= WIDTH - 80 && x < WIDTH - 20 {
            self.settings.show_bg_tasks = !self.settings.show_bg_tasks;
            changed = true;
        }

        if changed {
            self.settings.save();
            self.render();
        }
        changed
    }

    fn render(&mut self) {
        let (cx, cy) = self.cursor;
        gfx::fill_rect(self, 0, 0, WIDTH, HEIGHT, BG);

        // Title
        gfx::draw_string(self, 20, 20, "Settings", TEXT, None);

        // Appearance section
        gfx::fill_rect(self, 10, 50, WIDTH - 20, 1, TEXT_DIM);
        gfx::draw_string(self, 20, 60 - 14, "Appearance", TEXT_DIM, None);

        // Accent color label
        gfx::draw_string(self, 20, 60, "Accent color:", TEXT, None);

        // Accent color swatches
        let swatch_w = 50;
        let swatch_h = 30;
        let start_x = 20;
        let gap = 10;
        for (i, (name, color)) in ACCENT_COLORS.iter().enumerate() {
            let sx = start_x + i as u32 * (swatch_w + gap);
            let sy = 80;
            let selected = self.settings.accent_color == *name;
            let hovered = cx >= sx as i32
                && cx < (sx + swatch_w) as i32
                && cy >= sy as i32
                && cy < (sy + swatch_h) as i32;

            // Border for selected
            if selected {
                gfx::fill_rect(self, sx - 2, sy - 2, swatch_w + 4, swatch_h + 4, TEXT);
            }

            // Swatch
            gfx::fill_rect(self, sx, sy, swatch_w, swatch_h, *color);

            // Hover effect
            if hovered && !selected {
                gfx::fill_rect(self, sx, sy, swatch_w, 2, 0x00_FFFFFF);
            }
        }

        // Wallpaper toggle
        gfx::fill_rect(self, 10, 130, WIDTH - 20, 1, TEXT_DIM);
        gfx::draw_string(self, 20, 140 - 14, "Desktop", TEXT_DIM, None);
        gfx::draw_string(self, 20, 150, "Wallpaper", TEXT, None);
        self.draw_toggle(WIDTH - 80, 145, self.settings.wallpaper, cx, cy);

        // Background tasks toggle
        gfx::draw_string(self, 20, 210, "Show background tasks", TEXT, None);
        self.draw_toggle(WIDTH - 80, 205, self.settings.show_bg_tasks, cx, cy);

        // Status
        let status = if self.settings.save() {
            "Settings saved"
        } else {
            "Failed to save"
        };
        gfx::draw_string(self, 20, HEIGHT - 30, status, TEXT_DIM, None);
    }

    fn draw_toggle(&mut self, x: u32, y: u32, on: bool, cx: i32, cy: i32) {
        let w = 50;
        let h = 20;
        let hovered = cx >= x as i32 && cx < (x + w) as i32 && cy >= y as i32 && cy < (y + h) as i32;

        // Track
        let track_color = if on { TOGGLE_ON } else { TOGGLE_OFF };
        gfx::fill_rounded_rect(self, x, y, w, h, 10, track_color);

        // Knob
        let knob_x = if on { x + w - h + 2 } else { x + 2 };
        gfx::fill_rect(self, knob_x, y + 2, h - 4, h - 4, 0x00_FFFFFF);

        // Hover effect
        if hovered {
            gfx::fill_rect(self, x, y, w, 2, 0x00_FFFFFF);
        }
    }
}

impl Surface for SettingsApp {
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
