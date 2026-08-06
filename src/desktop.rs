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
    // Serial sync point for the headless GUI test (`make test-gui`): the
    // host polls for this line, then screendumps the physical framebuffer
    // to verify the desktop actually rendered. (Plain `io::print`, not the
    // `println!` macro: this module is declared before `#[macro_use] io`.)
    crate::io::print(format_args!(
        "[OK] desktop: first frame composited ({}x{})\n",
        screen_w, screen_h
    ));

    loop {
        let mut dirty = false;
        let mut cursor_only = false;
        while let Some(event) = mouse::next_event() {
            // `handle_mouse` reports whether the event actually changed
            // something on screen; a pure cursor move returns false and
            // lets us skip the full recomposite.
            if manager.handle_mouse(event) {
                dirty = true;
            } else {
                cursor_only = true;
            }
        }
        while let Some(event) = keyboard::next_event() {
            manager.handle_key(event);
            dirty = true;
        }
        if dirty || manager.clock_tick_due() {
            if manager.dragging_window() {
                // While a window is being dragged only its region
                // changes, so composite just that instead of the whole
                // desktop (the drag frame rate is the bottleneck).
                manager.composite_drag();
            } else {
                manager.composite();
            }
        } else if cursor_only {
            manager.composite_cursor_only();
        }
        interrupts::halt();
    }
}
