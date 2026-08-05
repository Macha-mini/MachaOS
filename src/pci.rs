//! Minimal PCI configuration-space access: enough to find and enable
//! the e1000 NIC (and, later, AHCI/USB devices). Two 32-bit ports,
//! `CONFIG_ADDRESS` (0xCF8) and `CONFIG_DATA` (0xCFC), give access to
//! each device's 256-byte config space. No bridge scanning: the NIC is
//! expected on bus 0 (QEMU puts it there).

use crate::port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Reads a 32-bit word from a device's config space.
pub fn config_read(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | (u32::from(offset) & 0xFC);
    unsafe {
        port::outl(CONFIG_ADDRESS, addr);
        port::inl(CONFIG_DATA)
    }
}

/// Writes a 32-bit word to a device's config space.
pub fn config_write(bus: u8, dev: u8, func: u8, offset: u8, value: u32) {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | (u32::from(offset) & 0xFC);
    unsafe {
        port::outl(CONFIG_ADDRESS, addr);
        port::outl(CONFIG_DATA, value);
    }
}

/// Scans bus 0 for the first function matching `vendor`/`device`.
/// Returns `(bus, dev, func)`.
pub fn find_device(vendor: u16, device: u16) -> Option<(u8, u8, u8)> {
    for dev in 0..32u8 {
        for func in 0..8u8 {
            let id = config_read(0, dev, func, 0);
            if id & 0xFFFF == u32::from(vendor) && (id >> 16) as u16 == device {
                return Some((0, dev, func));
            }
            // Non-multifunction devices only respond on func 0.
            if func == 0 && id != 0xFFFF_FFFF && config_read(0, dev, 0, 0x0E) & 0x80 == 0 {
                break;
            }
        }
    }
    None
}
