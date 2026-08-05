//! A simple pixel-based paint application for MachaOS.
//! Supports drawing with a pen or eraser, choosing colors from a palette,
//! and saving/loading images in raw pixel format.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::fat;
use crate::gfx::{self, Surface};
use crate::keyboard;

const CANVAS_WIDTH: u32 = 320;
const CANVAS_HEIGHT: u32 = 240;
const TOOLBAR_HEIGHT: u32 = 40;
const WIDTH: u32 = CANVAS_WIDTH;
const HEIGHT: u32 = CANVAS_HEIGHT + TOOLBAR_HEIGHT;

const BG: u32 = 0x00_202020;
const TOOLBAR_BG: u32 = 0x00_2A2A2A;
const CANVAS_BG: u32 = 0x00_FFFFFF;
const TEXT: u32 = 0x00_FFFFFF;
const TEXT_DIM: u32 = 0x00_9A9A9A;

// Color palette (8 colors)
const COLORS: [u32; 8] = [
    0x00_000000, // Black
    0x00_FFFFFF, // White
    0x00_FF0000, // Red
    0x00_00FF00, // Green
    0x00_0000FF, // Blue
    0x00_FFFF00, // Yellow
    0x00_00FFFF, // Cyan
    0x00_FF00FF, // Magenta
];

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Pen,
    Eraser,
}

pub struct PaintApp {
    buffer: Vec<u32>,
    canvas: Vec<u32>,
    current_color: u32,
    current_tool: Tool,
    drawing: bool,
    last_x: i32,
    last_y: i32,
    status: String,
    file_path: String,
}

impl PaintApp {
    pub fn new() -> Self {
        let mut app = Self {
            buffer: vec![BG; (WIDTH * HEIGHT) as usize],
            canvas: vec![CANVAS_BG; (CANVAS_WIDTH * CANVAS_HEIGHT) as usize],
            current_color: COLORS[0], // Black
            current_tool: Tool::Pen,
            drawing: false,
            last_x: -1,
            last_y: -1,
            status: String::new(),
            file_path: String::new(),
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

    pub fn handle_click(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);

        // Toolbar area
        if y < TOOLBAR_HEIGHT {
            self.handle_toolbar_click(x, y);
            return;
        }

        // Canvas area
        let canvas_y = y - TOOLBAR_HEIGHT;
        if x < CANVAS_WIDTH && canvas_y < CANVAS_HEIGHT {
            self.drawing = true;
            self.last_x = x as i32;
            self.last_y = canvas_y as i32;
            self.draw_pixel(x, canvas_y);
        }
    }

    pub fn handle_release(&mut self) {
        self.drawing = false;
        self.last_x = -1;
        self.last_y = -1;
    }

    pub fn handle_mouse_move(&mut self, x: i32, y: i32, pressed: bool) {
        if !pressed || x < 0 || y < 0 {
            return;
        }
        let (x, y) = (x as u32, y as u32);

        // Only draw in canvas area
        if y >= TOOLBAR_HEIGHT && x < CANVAS_WIDTH {
            let canvas_y = y - TOOLBAR_HEIGHT;
            if canvas_y < CANVAS_HEIGHT {
                if self.drawing {
                    // Draw line from last position to current
                    self.draw_line(self.last_x, self.last_y, x as i32, canvas_y as i32);
                }
                self.last_x = x as i32;
                self.last_y = canvas_y as i32;
            }
        }
    }

    pub fn handle_key(&mut self, event: keyboard::Event) -> Option<PaintAction> {
        match event {
            keyboard::Event::Ctrl('s') => {
                // Save to file
                let path = if self.file_path.is_empty() {
                    "/users/macha/Pictures/painting.raw".to_string()
                } else {
                    self.file_path.clone()
                };
                match self.save(&path) {
                    Ok(()) => {
                        self.status = format!("Saved to {}", path);
                        self.file_path = path;
                    }
                    Err(e) => {
                        self.status = format!("Save failed: {}", e);
                    }
                }
                self.render();
                None
            }
            keyboard::Event::Ctrl('o') => {
                // Return action to load a file
                Some(PaintAction::Load)
            }
            keyboard::Event::Char('1') => {
                self.current_tool = Tool::Pen;
                self.render();
                None
            }
            keyboard::Event::Char('2') => {
                self.current_tool = Tool::Eraser;
                self.render();
                None
            }
            _ => None,
        }
    }

    pub fn load_image(&mut self, path: &str, data: &[u8]) {
        let expected_len = (CANVAS_WIDTH * CANVAS_HEIGHT * 4) as usize;
        if data.len() != expected_len {
            self.status = format!("Invalid image size (expected {} bytes)", expected_len);
            self.render();
            return;
        }

        // Parse pixels
        for i in 0..(CANVAS_WIDTH * CANVAS_HEIGHT) as usize {
            let offset = i * 4;
            let pixel = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]);
            self.canvas[i] = pixel;
        }

        self.file_path = path.to_string();
        self.status = format!("Loaded {}", path);
        self.render();
    }

    fn save(&self, path: &str) -> Result<(), &'static str> {
        let mut data = Vec::with_capacity((CANVAS_WIDTH * CANVAS_HEIGHT * 4) as usize);
        for &pixel in &self.canvas {
            data.extend_from_slice(&pixel.to_le_bytes());
        }
        fat::write_file(path, &data).map_err(|_| "write failed")
    }

    fn handle_toolbar_click(&mut self, x: u32, y: u32) {
        // Tool buttons (left side)
        let tool_size = 30;
        let tool_y = 5;
        let pen_x = 5;
        let eraser_x = 40;

        if y >= tool_y && y < tool_y + tool_size {
            if x >= pen_x && x < pen_x + tool_size {
                self.current_tool = Tool::Pen;
                self.status = "Tool: Pen".to_string();
                self.render();
                return;
            }
            if x >= eraser_x && x < eraser_x + tool_size {
                self.current_tool = Tool::Eraser;
                self.status = "Tool: Eraser".to_string();
                self.render();
                return;
            }
        }

        // Color palette (right side)
        let color_size = 20;
        let color_y = 10;
        let color_start_x = 100;

        if y >= color_y && y < color_y + color_size {
            for (i, &color) in COLORS.iter().enumerate() {
                let color_x = color_start_x + (i as u32 * (color_size + 5));
                if x >= color_x && x < color_x + color_size {
                    self.current_color = color;
                    self.status = format!("Color: {}", i + 1);
                    self.render();
                    return;
                }
            }
        }
    }

    fn draw_pixel(&mut self, x: u32, y: u32) {
        if x >= CANVAS_WIDTH || y >= CANVAS_HEIGHT {
            return;
        }
        let index = (y * CANVAS_WIDTH + x) as usize;
        self.canvas[index] = match self.current_tool {
            Tool::Pen => self.current_color,
            Tool::Eraser => CANVAS_BG,
        };
        self.render();
    }

    fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        // Bresenham's line algorithm
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        let mut x = x0;
        let mut y = y0;

        loop {
            if x >= 0 && y >= 0 && (x as u32) < CANVAS_WIDTH && (y as u32) < CANVAS_HEIGHT {
                self.draw_pixel(x as u32, y as u32);
            }

            if x == x1 && y == y1 {
                break;
            }

            let e2 = 2 * err;
            if e2 >= dy {
                if x == x1 {
                    break;
                }
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                if y == y1 {
                    break;
                }
                err += dx;
                y += sy;
            }
        }
    }

    fn render(&mut self) {
        // Clear buffer
        for pixel in &mut self.buffer {
            *pixel = BG;
        }

        // Draw toolbar
        gfx::fill_rect(self, 0, 0, WIDTH, TOOLBAR_HEIGHT, TOOLBAR_BG);

        // Draw tool buttons
        let tool_size = 30;
        let tool_y = 5;
        let pen_x = 5;
        let eraser_x = 40;

        // Pen button
        let pen_selected = self.current_tool == Tool::Pen;
        gfx::fill_rect(
            self,
            pen_x,
            tool_y,
            tool_size,
            tool_size,
            if pen_selected { 0x00_444444 } else { 0x00_333333 },
        );
        // Draw pen icon
        gfx::fill_rect(self, pen_x + 10, tool_y + 5, 2, 15, TEXT);
        gfx::fill_rect(self, pen_x + 8, tool_y + 18, 6, 2, TEXT);

        // Eraser button
        let eraser_selected = self.current_tool == Tool::Eraser;
        gfx::fill_rect(
            self,
            eraser_x,
            tool_y,
            tool_size,
            tool_size,
            if eraser_selected { 0x00_444444 } else { 0x00_333333 },
        );
        // Draw eraser icon
        gfx::fill_rect(self, eraser_x + 8, tool_y + 10, 14, 10, TEXT);

        // Draw color palette
        let color_size = 20;
        let color_y = 10;
        let color_start_x = 100;

        for (i, &color) in COLORS.iter().enumerate() {
            let color_x = color_start_x + (i as u32 * (color_size + 5));
            gfx::fill_rect(self, color_x, color_y, color_size, color_size, color);

            // Highlight selected color
            if color == self.current_color {
                gfx::fill_rect(self, color_x - 1, color_y - 1, color_size + 2, 1, TEXT);
                gfx::fill_rect(self, color_x - 1, color_y + color_size, color_size + 2, 1, TEXT);
                gfx::fill_rect(self, color_x - 1, color_y, 1, color_size, TEXT);
                gfx::fill_rect(self, color_x + color_size, color_y, 1, color_size, TEXT);
            }
        }

        // Draw canvas
        for y in 0..CANVAS_HEIGHT {
            for x in 0..CANVAS_WIDTH {
                let canvas_index = (y * CANVAS_WIDTH + x) as usize;
                let buffer_index = ((y + TOOLBAR_HEIGHT) * WIDTH + x) as usize;
                self.buffer[buffer_index] = self.canvas[canvas_index];
            }
        }

        // Draw status text (if any)
        if !self.status.is_empty() {
            let status = self.status.clone();
            gfx::draw_string(self, 5, HEIGHT - 12, &status, TEXT_DIM, None);
        }
    }
}

impl Surface for PaintApp {
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

pub enum PaintAction {
    Load,
}
