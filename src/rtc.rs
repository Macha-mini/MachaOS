//! CMOS RTC (real-time clock) driver on the 0x70/0x71 ports. Gives the
//! wall-clock date and time; the PIT only measures time since boot, so
//! this is the one source of "what time is it really".

use crate::port;

#[derive(Clone, Copy, Debug)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

// Register indices.
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
const REG_CENTURY: u8 = 0x32;

const STATUS_A_UIP: u8 = 0x80; // update in progress
const STATUS_B_BINARY: u8 = 0x04; // 0 = BCD
const STATUS_B_24H: u8 = 0x02; // 0 = 12-hour

fn read_cmos(reg: u8) -> u8 {
    unsafe {
        // Bit 7 masks NMIs while the address latch holds the index.
        port::outb(CMOS_ADDR, reg | 0x80);
        port::inb(CMOS_DATA)
    }
}

fn bcd_to_bin(value: u8) -> u8 {
    (value & 0x0F) + (value >> 4) * 10
}

/// Reads the current date and time. CMOS keeps the time ticking even when
/// the machine is off, so this reflects the real wall clock.
pub fn now() -> DateTime {
    // Wait out an in-progress update so the registers can't change
    // underneath us.
    while read_cmos(REG_STATUS_A) & STATUS_A_UIP != 0 {}

    let status_b = read_cmos(REG_STATUS_B);
    let binary = status_b & STATUS_B_BINARY != 0;
    let hour24 = status_b & STATUS_B_24H != 0;

    let mut second = read_cmos(REG_SECONDS);
    let mut minute = read_cmos(REG_MINUTES);
    let mut hour = read_cmos(REG_HOURS);
    let mut day = read_cmos(REG_DAY);
    let mut month = read_cmos(REG_MONTH);
    let mut year = read_cmos(REG_YEAR);
    let mut century = read_cmos(REG_CENTURY);

    if !binary {
        second = bcd_to_bin(second);
        minute = bcd_to_bin(minute);
        hour = bcd_to_bin(hour & 0x7F);
        day = bcd_to_bin(day);
        month = bcd_to_bin(month);
        year = bcd_to_bin(year);
        century = bcd_to_bin(century);
    }
    if !hour24 {
        let pm = hour & 0x80 != 0;
        hour &= 0x7F;
        if pm && hour < 12 {
            hour += 12;
        } else if !pm && hour == 12 {
            hour = 0;
        }
    }
    if !(19..=20).contains(&century) {
        century = 20; // century register absent (0) or bogus
    }
    let year = century as u16 * 100 + year as u16;

    DateTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}
