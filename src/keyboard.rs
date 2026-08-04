use core::sync::atomic::{AtomicBool, Ordering};

use crate::port;
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Char(char),
    Backspace,
    Enter,
    Tab,
}

const BUFFER_CAPACITY: usize = 256;

struct EventQueue {
    buffer: [Event; BUFFER_CAPACITY],
    head: usize,
    tail: usize,
}

impl EventQueue {
    const fn new() -> Self {
        Self {
            buffer: [Event::Char('\0'); BUFFER_CAPACITY],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, event: Event) {
        let next = (self.head + 1) % BUFFER_CAPACITY;
        if next != self.tail {
            self.buffer[self.head] = event;
            self.head = next;
        }
    }

    fn pop(&mut self) -> Option<Event> {
        if self.head == self.tail {
            None
        } else {
            let event = self.buffer[self.tail];
            self.tail = (self.tail + 1) % BUFFER_CAPACITY;
            Some(event)
        }
    }
}

static QUEUE: SpinLock<EventQueue> = SpinLock::new(EventQueue::new());
static SHIFT_DOWN: AtomicBool = AtomicBool::new(false);
static CAPS_LOCK: AtomicBool = AtomicBool::new(false);

// PS/2 scancode set 1 keymap (index = scancode)
const KEYMAP: [u8; 128] = [
    0, 0, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=', 0, 0,
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', 0, 0, b'a', b's',
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v',
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', 0, b' ', 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

const KEYMAP_SHIFT: [u8; 128] = [
    0, 0, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+', 0, 0,
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', 0, 0, b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0, 0, 0, b' ', 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn decode(scancode: u8) -> Option<Event> {
    match scancode {
        0x2A | 0x36 => {
            SHIFT_DOWN.store(true, Ordering::Relaxed);
            None
        }
        0xAA | 0xB6 => {
            SHIFT_DOWN.store(false, Ordering::Relaxed);
            None
        }
        0x3A => {
            CAPS_LOCK.fetch_xor(true, Ordering::Relaxed);
            None
        }
        0x0E => Some(Event::Backspace),
        0x1C => Some(Event::Enter),
        0x0F => Some(Event::Tab),
        0x39 => Some(Event::Char(' ')),
        _ if scancode >= 0x80 => None, // key release
        _ => {
            let shifted = SHIFT_DOWN.load(Ordering::Relaxed);
            let caps = CAPS_LOCK.load(Ordering::Relaxed);
            let base = KEYMAP[scancode as usize];
            let shifted_char = KEYMAP_SHIFT[scancode as usize];
            if shifted && shifted_char != 0 {
                Some(Event::Char(shifted_char as char))
            } else if caps && base.is_ascii_alphabetic() {
                Some(Event::Char(base.to_ascii_uppercase() as char))
            } else if base != 0 {
                Some(Event::Char(base as char))
            } else {
                None
            }
        }
    }
}

/// Called from the IRQ1 interrupt handler.
pub fn irq() {
    let scancode = unsafe { port::inb(0x60) };
    if let Some(event) = decode(scancode) {
        QUEUE.lock().push(event);
    }
}

pub fn next_event() -> Option<Event> {
    QUEUE.lock().pop()
}

pub fn drain_buffer() {
    unsafe {
        while port::inb(0x64) & 1 != 0 {
            let _ = port::inb(0x60);
        }
    }
}
