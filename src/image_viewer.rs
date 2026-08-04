//! A simple RAW image viewer for MachaOS.
//! Displays 32-bit raw pixel images (same format as wallpaper.raw).
//! Supports zoom in/out and panning.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::font;
use crate::gfx::{self, Surface};
use crate::keyboard;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

const BG: u32 = 0x00_1A1A1A;
const TOOLBAR_BG: u32 = 0x00_2A2A2A;
const TEXT: u32 = 0x00_FFFFFF;
const TEXT_DIM: u32 = 0x00_999999;

const TOOLBAR_HEIGHT: u32 = 30;

pub struct ImageViewerApp {
    buffer: Vec<u32>,
    image_data: Option<Vec<u32>>,
    image_width: u32,
    image_height: u32,
    zoom: u32, // 1 = 100%, 2 = 200%, etc.
    offset_x: i32,
    offset_y: i32,
    file_path: String,
    status: String,
}

impl ImageViewerApp {
    pub fn new() -> Self {
        let mut app = Self {
            buffer: vec![BG; (WIDTH * HEIGHT) as usize],
            image_data: None,
            image_width: 0,
            image_height: 0,
            zoom: 1,
            offset_x: 0,
            offset_y: 0,
            file_path: String::new(),
            status: "No image loaded".to_string(),
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

    pub fn load_image(&mut self, path: &str, data: &[u8], width: u32, height: u32) {
        let expected_len = (width * height * 4) as usize;
        if data.len() != expected_len {
            self.status = format!(
                "Invalid image size (expected {} bytes, got {})",
                expected_len,
                data.len()
            );
            self.render();
            return;
        }

        // Parse pixels
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for i in 0..(width * height) as usize {
            let offset = i * 4;
            let pixel = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            pixels.push(pixel);
        }

        self.image_data = Some(pixels);
        self.image_width = width;
        self.image_height = height;
        self.file_path = path.to_string();

        // Center image
        let view_width = WIDTH;
        let view_height = HEIGHT - TOOLBAR_HEIGHT;
        self.offset_x = ((view_width as i32) - (width as i32 * self.zoom as i32)) / 2;
        self.offset_y = ((view_height as i32) - (height as i32 * self.zoom as i32)) / 2;

        self.status = format!(
            "{} ({}x{}, zoom {}x)",
            path, width, height, self.zoom
        );
        self.render();
    }

    pub fn handle_click(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);

        // Toolbar area - ignore clicks
        if y < TOOLBAR_HEIGHT {
            return;
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) {
        match event {
            keyboard::Event::Char('+') | keyboard::Event::Char('=') => {
                if self.zoom < 8 {
                    self.zoom += 1;
                    self.status = format!(
                        "{} ({}x{}, zoom {}x)",
                        self.file_path, self.image_width, self.image_height, self.zoom
                    );
                    self.render();
                }
            }
            keyboard::Event::Char('-') => {
                if self.zoom > 1 {
                    self.zoom -= 1;
                    self.status = format!(
                        "{} ({}x{}, zoom {}x)",
                        self.file_path, self.image_width, self.image_height, self.zoom
                    );
                    self.render();
                }
            }
            keyboard::Event::Char('0') => {
                // Reset zoom and center
                self.zoom = 1;
                let view_width = WIDTH;
                let view_height = HEIGHT - TOOLBAR_HEIGHT;
                self.offset_x =
                    ((view_width as i32) - (self.image_width as i32 * self.zoom as i32)) / 2;
                self.offset_y =
                    ((view_height as i32) - (self.image_height as i32 * self.zoom as i32)) / 2;
                self.status = format!(
                    "{} ({}x{}, zoom {}x)",
                    self.file_path, self.image_width, self.image_height, self.zoom
                );
                self.render();
            }
            keyboard::Event::Left => {
                self.offset_x += 20;
                self.render();
            }
            keyboard::Event::Right => {
                self.offset_x -= 20;
                self.render();
            }
            keyboard::Event::Up => {
                self.offset_y += 20;
                self.render();
            }
            keyboard::Event::Down => {
                self.offset_y -= 20;
                self.render();
            }
            _ => {}
        }
    }

    fn render(&mut self) {
        // Clear buffer
        for pixel in &mut self.buffer {
            *pixel = BG;
        }

        // Draw toolbar
        gfx::fill_rect(self, 0, 0, WIDTH, TOOLBAR_HEIGHT, TOOLBAR_BG);
        gfx::draw_string(self, 10, 10, "Image Viewer", TEXT, None);

        // Draw image
        if let Some(image_data) = &self.image_data {
            let view_y_start = TOOLBAR_HEIGHT;
            let view_height = HEIGHT - TOOLBAR_HEIGHT;

            for screen_y in view_y_start..HEIGHT {
                for screen_x in 0..WIDTH {
                    // Calculate image coordinates
                    let img_x = (screen_x as i32 - self.offset_x) / self.zoom as i32;
                    let img_y = (screen_y as i32 - self.offset_y - view_y_start as i32)
                        / self.zoom as i32;

                    if img_x >= 0
                        && img_x < self.image_width as i32
                        && img_y >= 0
                        && img_y < self.image_height as i32
                    {
                        let img_index = (img_y as u32 * self.image_width + img_x as u32) as usize;
                        let pixel = image_data[img_index];
                        let buffer_index = (screen_y * WIDTH + screen_x) as usize;
                        self.buffer[buffer_index] = pixel;
                    }
                }
            }
        } else {
            // No image - show message
            let msg = "No image loaded";
            let msg_width = (msg.len() * font::GLYPH_WIDTH) as u32;
            let msg_x = (WIDTH - msg_width) / 2;
            let msg_y = HEIGHT / 2;
            gfx::draw_string(self, msg_x, msg_y, msg, TEXT_DIM, None);
        }

        // Draw status text
        if !self.status.is_empty() {
            let status = self.status.clone();
            gfx::draw_string(self, 10, HEIGHT - 15, &status, TEXT_DIM, None);
        }
    }
}

impl Surface for ImageViewerApp {
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
