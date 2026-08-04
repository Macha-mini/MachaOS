use core::fmt;

use crate::port;

const COM1: u16 = 0x3F8;

pub fn init() {
    unsafe {
        port::outb(COM1 + 1, 0x00); // disable interrupts
        port::outb(COM1 + 3, 0x80); // DLAB on
        port::outb(COM1 + 0, 0x03); // divisor 3 -> 38400 baud
        port::outb(COM1 + 1, 0x00);
        port::outb(COM1 + 3, 0x03); // 8 data bits, no parity, 1 stop bit
        port::outb(COM1 + 2, 0xC7); // enable FIFO, clear, 14-byte threshold
        port::outb(COM1 + 4, 0x0B); // IRQs enabled, RTS/DSR set
    }
}

fn tx_ready() -> bool {
    unsafe { port::inb(COM1 + 5) & 0x20 != 0 }
}

pub fn write_byte(byte: u8) {
    while !tx_ready() {
        core::hint::spin_loop();
    }
    unsafe { port::outb(COM1, byte) }
}

pub fn write_str(s: &str) {
    for byte in s.bytes() {
        write_byte(byte);
    }
}

pub struct SerialWriter;

impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::serial::write_str(s);
        Ok(())
    }
}
