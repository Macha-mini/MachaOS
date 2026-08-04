//! The GUI main loop: captures kernel input and forwards it to the user-space
//! compositor. The compositor owns focus, hit-testing, and final composition;
//! this kernel loop only handles interrupt queues and scheduling.

use crate::{interrupts, io, keyboard, mouse, process, user_prog};

fn encode_key(event: keyboard::Event) -> Option<[u8; 6]> {
    match event {
        keyboard::Event::Char(c) if c.is_ascii() => Some([0, c as u8, 0, 0, 0, 0]),
        keyboard::Event::Backspace => Some([1, 0, 0, 0, 0, 0]),
        keyboard::Event::Enter => Some([2, 0, 0, 0, 0, 0]),
        keyboard::Event::Ctrl(c) if c.is_ascii() => Some([4, c as u8, 0, 0, 0, 0]),
        keyboard::Event::Left => Some([7, 0, 0, 0, 0, 0]),
        keyboard::Event::Right => Some([8, 0, 0, 0, 0, 0]),
        keyboard::Event::Up => Some([9, 0, 0, 0, 0, 0]),
        keyboard::Event::Down => Some([10, 0, 0, 0, 0, 0]),
        keyboard::Event::Tab => Some([11, 0, 0, 0, 0, 0]),
        _ => None,
    }
}

fn encode_mouse(event: mouse::MouseEvent) -> [u8; 6] {
    let [dx0, dx1] = (event.dx as i16).to_le_bytes();
    let [dy0, dy1] = (event.dy as i16).to_le_bytes();
    [5, dx0, dx1, dy0, dy1, event.left as u8]
}

pub fn run() -> ! {
    let compositor = match process::spawn(user_prog::PROG_COMPOSITOR, "compositor") {
        Ok(pid) => pid,
        Err(e) => { io::print(format_args!("failed to launch compositor: {}\n", e)); loop { interrupts::halt(); } }
    };

    // Calculator runs as a real ring-3 process now (user/src/bin/prog_calculator.rs),
    // not kernel-resident `AppKind` state — its window shows up a frame or
    // two after boot, once it calls sys_win_create and the loop below
    // drains that request. See wm.rs's module docs on `WinCommand`.
    if let Err(e) = process::spawn(user_prog::PROG_CALCULATOR, "calculator") {
        io::print(format_args!("failed to launch Calculator: {}\n", e));
    }
    if let Err(e) = process::spawn(user_prog::PROG_TERMINAL, "terminal") {
        io::print(format_args!("failed to launch Terminal: {}\n", e));
    }
    if let Err(e) = process::spawn(user_prog::PROG_NOTEPAD, "notepad") {
        io::print(format_args!("failed to launch Notepad: {}\n", e));
    }

    loop {
        while let Some(event) = mouse::next_event() {
            let encoded = encode_mouse(event);
            process::send_from_kernel(compositor, &encoded);
        }
        while let Some(event) = keyboard::next_event() {
            if let Some(encoded) = encode_key(event) {
                process::send_from_kernel(compositor, &encoded);
            }
        }
        interrupts::halt();
    }
}
