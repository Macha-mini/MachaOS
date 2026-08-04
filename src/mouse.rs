//! PS/2 auxiliary port (mouse) driver. Talks to the 8042 controller the
//! same way `keyboard.rs` does, and decodes standard 3-byte packets into
//! `MouseEvent`s pushed onto a ring buffer (same pattern as
//! `keyboard::EventQueue`).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::pic;
use crate::port;
use crate::sync::SpinLock;

const DATA_PORT: u16 = 0x60;
const STATUS_COMMAND_PORT: u16 = 0x64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub dx: i32,
    pub dy: i32,
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

const BUFFER_CAPACITY: usize = 64;

struct EventQueue {
    buffer: [MouseEvent; BUFFER_CAPACITY],
    head: usize,
    tail: usize,
}

const BLANK_EVENT: MouseEvent = MouseEvent {
    dx: 0,
    dy: 0,
    left: false,
    right: false,
    middle: false,
};

impl EventQueue {
    const fn new() -> Self {
        Self {
            buffer: [BLANK_EVENT; BUFFER_CAPACITY],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, event: MouseEvent) {
        let next = (self.head + 1) % BUFFER_CAPACITY;
        if next != self.tail {
            self.buffer[self.head] = event;
            self.head = next;
        }
    }

    fn pop(&mut self) -> Option<MouseEvent> {
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

// The controller/mouse handshake in `init()` reliably leaves one bogus
// packet's worth of noise in the stream right as reporting turns on;
// discard the first fully-assembled packet rather than surface it as a
// spurious jump.
static SKIP_FIRST_PACKET: AtomicBool = AtomicBool::new(true);

struct PacketState {
    bytes: [u8; 3],
    index: usize,
}

static PACKET: SpinLock<PacketState> = SpinLock::new(PacketState {
    bytes: [0; 3],
    index: 0,
});

fn wait_input_clear() {
    for _ in 0..100_000 {
        if unsafe { port::inb(STATUS_COMMAND_PORT) } & 0x02 == 0 {
            return;
        }
    }
}

fn wait_output_full() {
    for _ in 0..100_000 {
        if unsafe { port::inb(STATUS_COMMAND_PORT) } & 0x01 != 0 {
            return;
        }
    }
}

fn write_command(cmd: u8) {
    wait_input_clear();
    unsafe { port::outb(STATUS_COMMAND_PORT, cmd) }
}

fn write_data(byte: u8) {
    wait_input_clear();
    unsafe { port::outb(DATA_PORT, byte) }
}

fn read_data() -> u8 {
    wait_output_full();
    unsafe { port::inb(DATA_PORT) }
}

fn write_aux(byte: u8) {
    write_command(0xD4); // "next data byte goes to the auxiliary device"
    write_data(byte);
}

pub fn init() {
    crate::keyboard::drain_buffer(); // clear any stale bytes before we start syncing packets

    write_command(0xA8); // enable the auxiliary (mouse) port

    write_command(0x20); // read controller command byte
    let mut status = read_data();
    status |= 0x02; // enable IRQ12 on activity
    status &= !0x20; // enable the auxiliary port clock
    write_command(0x60); // write controller command byte
    write_data(status);

    write_aux(0xF6); // set defaults
    read_data(); // ACK
    write_aux(0xF4); // enable data reporting (streaming)
    read_data(); // ACK

    pic::unmask(2); // slave PIC cascade line
    pic::unmask(12); // IRQ12
}

/// Called from the IRQ12 interrupt handler.
pub fn irq() {
    let data = unsafe { port::inb(DATA_PORT) };

    let mut state = PACKET.lock();
    if state.index == 0 && data & 0x08 == 0 {
        // Not a valid first byte (sync bit unset); drop it and resync.
        return;
    }
    let index = state.index;
    state.bytes[index] = data;
    state.index += 1;
    if state.index < 3 {
        return;
    }
    state.index = 0;
    let b0 = state.bytes[0];
    let mut dx = state.bytes[1] as i32;
    let mut dy = state.bytes[2] as i32;
    if b0 & 0x10 != 0 {
        dx -= 256; // sign-extend the 9-bit two's complement delta
    }
    if b0 & 0x20 != 0 {
        dy -= 256;
    }
    drop(state);

    if SKIP_FIRST_PACKET.swap(false, Ordering::AcqRel) {
        return;
    }

    let event = MouseEvent {
        dx,
        dy: -dy, // PS/2 reports +y as up; screen coordinates grow downward
        left: b0 & 0x01 != 0,
        right: b0 & 0x02 != 0,
        middle: b0 & 0x04 != 0,
    };
    QUEUE.lock().push(event);
}

pub fn next_event() -> Option<MouseEvent> {
    QUEUE.lock().pop()
}
