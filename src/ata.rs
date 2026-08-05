//! Polled ATA PIO (IDE) driver. QEMU's default `pc` machine exposes IDE
//! controllers, so no AHCI is needed yet. Only LBA28 is used — plenty for
//! the small disk images this OS targets.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::port;
use crate::sync::SpinLock;

pub const PRIMARY_BASE: u16 = 0x1F0;
pub const SECONDARY_BASE: u16 = 0x170;
#[allow(dead_code)]
pub const PRIMARY_CTRL: u16 = 0x3F6;
#[allow(dead_code)]
pub const SECONDARY_CTRL: u16 = 0x376;

const REG_DATA: u16 = 0;
const REG_SECTOR_COUNT: u16 = 2;
const REG_LBA_LO: u16 = 3;
const REG_LBA_MID: u16 = 4;
const REG_LBA_HI: u16 = 5;
const REG_DRIVE: u16 = 6;
const REG_STATUS: u16 = 7;

const STATUS_BSY: u8 = 0x80;
const STATUS_DRDY: u8 = 0x40;
const STATUS_DRQ: u8 = 0x08;
const STATUS_ERR: u8 = 0x01;

const CMD_IDENTIFY: u8 = 0xEC;
const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;

// 32-bit "timeout" guards. QEMU devices answer in microseconds; these are
// just there so a wedged controller can't hang the kernel forever.
const BUSY_WAIT_ITERATIONS: u32 = 100_000_000;

#[derive(Clone, Copy)]
pub struct AtaDevice {
    pub base: u16,
    pub master: bool,
    pub present: bool,
    pub atapi: bool,
    pub lba48: bool,
    pub sectors: u64,
    pub model: [u8; 40],
}

impl AtaDevice {
    fn drive_select(&self, lba: u64) -> u8 {
        0xE0 | if self.master { 0 } else { 0x10 } | ((lba >> 24) as u8 & 0x0F)
    }

    pub fn model_string(&self) -> String {
        let bytes = &self.model;
        let end = bytes.iter().rposition(|&b| b != b' ' && b != 0).map(|i| i + 1).unwrap_or(0);
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    pub fn read_sectors(&self, lba: u64, count: usize, buf: &mut [u8]) -> Result<(), &'static str> {
        if !self.present || self.atapi {
            return Err("no ATA block device");
        }
        if count == 0 || count > 256 || buf.len() < count * 512 {
            return Err("invalid read request");
        }
        if lba >= (1 << 28) {
            return Err("LBA28 range exceeded");
        }
        unsafe {
            port::outb(self.base + REG_DRIVE, self.drive_select(lba));
            port::outb(self.base + REG_SECTOR_COUNT, count as u8);
            port::outb(self.base + REG_LBA_LO, lba as u8);
            port::outb(self.base + REG_LBA_MID, (lba >> 8) as u8);
            port::outb(self.base + REG_LBA_HI, (lba >> 16) as u8);
            port::outb(self.base + REG_STATUS, CMD_READ);
            for i in 0..count {
                wait_drq(self.base)?;
                let words = buf.as_mut_ptr().add(i * 512) as *mut u16;
                for j in 0..256 {
                    words.add(j).write(port::inw(self.base + REG_DATA));
                }
            }
        }
        Ok(())
    }

    pub fn write_sectors(&self, lba: u64, count: usize, buf: &[u8]) -> Result<(), &'static str> {
        if !self.present || self.atapi {
            return Err("no ATA block device");
        }
        if count == 0 || count > 256 || buf.len() < count * 512 {
            return Err("invalid write request");
        }
        if lba >= (1 << 28) {
            return Err("LBA28 range exceeded");
        }
        unsafe {
            port::outb(self.base + REG_DRIVE, self.drive_select(lba));
            port::outb(self.base + REG_SECTOR_COUNT, count as u8);
            port::outb(self.base + REG_LBA_LO, lba as u8);
            port::outb(self.base + REG_LBA_MID, (lba >> 8) as u8);
            port::outb(self.base + REG_LBA_HI, (lba >> 16) as u8);
            port::outb(self.base + REG_STATUS, CMD_WRITE);
            for i in 0..count {
                wait_drq(self.base)?;
                let words = buf.as_ptr().add(i * 512) as *const u16;
                for j in 0..256 {
                    port::outw(self.base + REG_DATA, words.add(j).read());
                }
                wait_idle(self.base)?; // wait for this sector to hit the platter
            }
        }
        Ok(())
    }
}

fn wait_bsy_clear(base: u16) -> Result<(), &'static str> {
    for _ in 0..BUSY_WAIT_ITERATIONS {
        let status = unsafe { port::inb(base + REG_STATUS) };
        if status & STATUS_BSY == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("ATA: BSY never cleared")
}

fn wait_drq(base: u16) -> Result<(), &'static str> {
    for _ in 0..BUSY_WAIT_ITERATIONS {
        let status = unsafe { port::inb(base + REG_STATUS) };
        if status & STATUS_BSY == 0 {
            if status & STATUS_ERR != 0 {
                return Err("ATA: drive error");
            }
            if status & STATUS_DRQ != 0 {
                return Ok(());
            }
        }
        core::hint::spin_loop();
    }
    Err("ATA: DRQ never set")
}

fn wait_idle(base: u16) -> Result<(), &'static str> {
    for _ in 0..BUSY_WAIT_ITERATIONS {
        let status = unsafe { port::inb(base + REG_STATUS) };
        if status & STATUS_BSY == 0 {
            if status & STATUS_ERR != 0 {
                return Err("ATA: write error");
            }
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err("ATA: write never completed")
}

/// Probes one drive (channel x master/slave) with IDENTIFY. Returns None
/// for empty slots; ATAPI devices (e.g. the boot CD) come back with
/// `atapi = true` so the filesystem layer can skip them.
fn identify(base: u16, master: bool) -> Option<AtaDevice> {
    let mut device = AtaDevice {
        base,
        master,
        present: false,
        atapi: false,
        lba48: false,
        sectors: 0,
        model: [0; 40],
    };
    unsafe {
        port::outb(base + REG_DRIVE, if master { 0xA0 } else { 0xB0 });
        port::outb(base + REG_SECTOR_COUNT, 0);
        port::outb(base + REG_LBA_LO, 0);
        port::outb(base + REG_LBA_MID, 0);
        port::outb(base + REG_LBA_HI, 0);
        port::outb(base + REG_STATUS, CMD_IDENTIFY);
        if port::inb(base + REG_STATUS) == 0 {
            return None; // nothing on this slot
        }
        if wait_bsy_clear(base).is_err() {
            return None;
        }
        let status = port::inb(base + REG_STATUS);
        if status & STATUS_ERR != 0 {
            // Could be an ATAPI device: signature is in the LBA regs.
            let mid = port::inb(base + REG_LBA_MID);
            let hi = port::inb(base + REG_LBA_HI);
            if mid == 0x14 && hi == 0xEB {
                device.present = true;
                device.atapi = true;
                return Some(device);
            }
            return None;
        }
        if wait_drq(base).is_err() {
            return None;
        }
        let mut words = [0u16; 256];
        for word in words.iter_mut() {
            *word = port::inw(base + REG_DATA);
        }
        // Word 0: 0x0000 on real ATA drives, 0x0040/0x0400 on QEMU's
        // emulated disks; 0xEB14 is the ATAPI signature (already handled
        // above via the error path, but never rely on that alone).
        if words[0] == 0x0000 || words[0] == 0x0040 || words[0] == 0x0400 {
            device.present = true;
            device.lba48 = words[83] & (1 << 10) != 0;
            device.sectors = (words[61] as u64) << 16 | words[60] as u64;
            for (i, word) in words[27..47].iter().enumerate() {
                device.model[i * 2] = (word >> 8) as u8;
                device.model[i * 2 + 1] = (word & 0xFF) as u8;
            }
            return Some(device);
        }
    }
    None
}

static DEVICES: SpinLock<Vec<AtaDevice>> = SpinLock::new(Vec::new());

pub fn init() {
    let mut devices = Vec::new();
    for base in [PRIMARY_BASE, SECONDARY_BASE] {
        for master in [true, false] {
            if let Some(device) = identify(base, master) {
                devices.push(device);
            }
        }
    }
    *DEVICES.lock() = devices;
}

pub fn count() -> usize {
    DEVICES.lock().len()
}

pub fn device(index: usize) -> Option<AtaDevice> {
    DEVICES.lock().get(index).copied()
}

pub fn describe(index: usize) -> Option<String> {
    DEVICES.lock().get(index).map(|device| {
        let kind = if device.atapi { "ATAPI" } else { "ATA" };
        let model = device.model_string();
        let capacity = device.sectors / 2048; // MiB
        format!(
            "[{}] {} {} ({} MiB, LBA{})",
            index,
            kind,
            if model.is_empty() { "(unnamed)" } else { &model },
            capacity,
            if device.lba48 { "48" } else { "28" }
        )
    })
}
