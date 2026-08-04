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
        // Requests queued by sys_win_create/sys_win_update (and processes
        // that exited since the last pass) — this loop is the only place
        // a `WindowManager` is reachable from, so it's the only place
        // either can be applied. See wm.rs's module docs on `WinCommand`.
        dirty |= manager.drain_commands();
        dirty |= manager.reap_exited_process_windows();
        if dirty || manager.clock_tick_due() {
            manager.composite();
        }
        interrupts::halt();
    }
}
