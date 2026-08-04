//! The GUI main loop: drains mouse/keyboard events into the window
//! manager, recomposites when something changed, and otherwise halts
//! until the next interrupt (timer, keyboard, or mouse).

use crate::{fb, interrupts, keyboard, mouse, wm};

pub fn run() -> ! {
    let (screen_w, screen_h) = fb::dimensions();
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
