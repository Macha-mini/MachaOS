//! The GUI main loop: drains mouse/keyboard events into the window
//! manager, recomposites when something changed, and otherwise halts
//! until the next interrupt (timer, keyboard, or mouse).

use crate::{fb, font, interrupts, keyboard, mouse, settings, wm};

pub fn run() -> ! {
    // Apply the persisted display settings (resolution + font size)
    // before the window manager lays anything out.
    let sys_settings = settings::Settings::load();
    if let Some((w, h)) = sys_settings.resolution {
        fb::set_virtual_resolution(w, h);
    }
    let scale = if sys_settings.font_scale_percent >= 200 { 2 } else { 1 };
    font::set_scale(scale);

    let (screen_w, screen_h) = fb::virtual_dimensions();
    let mut manager = wm::WindowManager::new(screen_w, screen_h);
    manager.composite();

    loop {
        let mut dirty = false;
        while let Some(event) = mouse::next_event() {
            manager.handle_mouse(event);
            dirty = true;
        }
        while let Some(event) = keyboard::next_event() {
            manager.handle_key(event);
            dirty = true;
        }
        if dirty || manager.clock_tick_due() {
            manager.composite();
        }
        interrupts::halt();
    }
}
