use crate::port;

const CHANNEL0: u16 = 0x40;
const COMMAND: u16 = 0x43;
const PIT_FREQUENCY_HZ: u32 = 1193182;
const TICK_HZ: u32 = 100;

pub fn init() {
    let divisor: u16 = (PIT_FREQUENCY_HZ / TICK_HZ) as u16;
    unsafe {
        port::outb(COMMAND, 0x36); // channel 0, lobyte/hibyte, mode 3 (square wave)
        port::outb(CHANNEL0, (divisor & 0xFF) as u8);
        port::outb(CHANNEL0, (divisor >> 8) as u8);
    }
}
