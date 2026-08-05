//! AHCI (SATA) driver for QEMU's `-device ich9-ahci` (Intel ICH9,
//! PCI 8086:2922). Polled, no interrupts: commands are issued through
//! the port's command list and completion is detected by watching
//! PxCI/PxTFD. The disk lives on port 0; rings and buffers are PMM
//! frames (identity-mapped, so physical addresses are CPU pointers
//! too). Everything is below 4 GiB, so the 32-bit DBA fields suffice.
//!
//! When this driver is up, `crate::disk::first_disk()` hands the FAT
//! layer `Disk::Ahci` instead of the ATA PIO device.

use crate::pci;
use crate::pmm;
use crate::sync::SpinLock;

use alloc::format;

const VENDOR_INTEL: u16 = 0x8086;
const DEV_ICH9_AHCI: u16 = 0x2922;

// HBA registers (offsets from ABAR).
const HBA_CAP: u32 = 0x00;
const HBA_GHC: u32 = 0x04;
const HBA_PI: u32 = 0x0C;

// GHC bits.
const GHC_AE: u32 = 1 << 31;
const GHC_HR: u32 = 1;

// Port registers (port base = 0x100 + port * 0x80).
const PX_CLB: u32 = 0x00;
const PX_FB: u32 = 0x08;
const PX_IS: u32 = 0x10;
const PX_IE: u32 = 0x14;
const PX_CMD: u32 = 0x18;
const PX_TFD: u32 = 0x20;
const PX_SSTS: u32 = 0x28;
const PX_CI: u32 = 0x38;

// PxCMD bits.
const PXCMD_ST: u32 = 1 << 0;
const PXCMD_SUD: u32 = 1 << 1;
const PXCMD_POD: u32 = 1 << 2;
const PXCMD_FRE: u32 = 1 << 4;
const PXCMD_FR: u32 = 1 << 14;
const PXCMD_CR: u32 = 1 << 15;

// PxTFD bits.
const PXTFD_ERR: u32 = 1 << 0;
const PXTFD_DRQ: u32 = 1 << 3;
const PXTFD_BSY: u32 = 1 << 7;

// PxIS: task-file error status.
const PXIS_TFES: u32 = 1 << 30;

// Command header / table layout.
const CMD_FLAG_CFL: u32 = 5; // command FIS length in dwords
const CMD_FLAG_WRITE: u32 = 1 << 6;

const CMD_IDENTIFY: u8 = 0xEC;
const CMD_READ_DMA_EXT: u8 = 0x25;
const CMD_WRITE_DMA_EXT: u8 = 0x35;

/// Max sectors per command (128 * 512 B = 64 KiB, comfortably under
/// the 4 MiB PRD limit).
const MAX_CHUNK_SECTORS: usize = 128;

const SPIN_LIMIT: u32 = 20_000_000;

pub struct AhciPort {
    present: bool,
    clb: usize, // command list (1 KiB, page-aligned)
    ct: usize, // command table (128 B + PRDs, page-aligned)
    fb: usize, // FIS receive area (256 B, page-aligned)
    buf: usize, // identify buffer (512 B)
}

pub struct Ahci {
    mmio: *mut u32,
    port: AhciPort,
    sectors: u64,
    model: [u8; 40],
}

/// Flushes CPU cache lines covering `[addr, addr+len)` so device-DMA
/// writes become visible (and our writes reach the device). Needed
/// because the kernel maps everything write-back.
#[inline]
fn flush_cache(addr: usize, len: usize) {
    unsafe {
        let mut a = addr;
        let end = addr + len;
        while a < end {
            core::arch::asm!("clflush [{}]", in(reg) a, options(nostack));
            a += 64;
        }
    }
}

// The HBA lives behind a SpinLock and is only touched from kernel
// context on the single CPU; raw pointers are fine under that guard.
unsafe impl Send for Ahci {}

pub static AHCI: SpinLock<Option<Ahci>> = SpinLock::new(None);

impl Ahci {
    #[inline]
    fn reg(&self, off: u32) -> u32 {
        unsafe { core::ptr::read_volatile(self.mmio.add((off / 4) as usize)) }
    }

    #[inline]
    fn set_reg(&self, off: u32, value: u32) {
        unsafe { core::ptr::write_volatile(self.mmio.add((off / 4) as usize), value) };
    }

    fn model_string(&self) -> alloc::string::String {
        let bytes = &self.model;
        let end = bytes
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map(|i| i + 1)
            .unwrap_or(0);
        alloc::string::String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    /// Runs one command on port 0's slot 0: `cmd` with a 48-bit LBA and
    /// sector count, DMA to/from `buf` (identity-mapped kernel memory).
    fn command(
        &mut self,
        cmd: u8,
        lba: u64,
        count: u16,
        buf: *const u8,
        write: bool,
    ) -> Result<(), &'static str> {
        let pbase = 0x100 + 0x80 * 0;
        let port = &self.port;
        if !port.present {
            return Err("AHCI port 0 has no device");
        }
        let clb = port.clb as *mut u8;
        let ct = port.ct as *mut u8;
        // IDENTIFY moves 512 bytes regardless of the (zero) sector count.
        let bytes = if cmd == CMD_IDENTIFY { 512 } else { count as usize * 512 };

        // Command header, slot 0. QEMU's AHCICmdHdr is
        // { u16 opts; u16 prdtl; ... } — PRDTL lives in bits 16-31.
        unsafe {
            let hdr = clb as *mut u32;
            core::ptr::write_volatile(
                hdr.add(0),
                CMD_FLAG_CFL | CMD_FLAG_WRITE * write as u32 | (1 << 16), // PRDTL = 1
            );
            core::ptr::write_volatile(hdr.add(1), 0); // PRDBC
            core::ptr::write_volatile(hdr.add(2), ct as u32); // CTBA
            core::ptr::write_volatile(hdr.add(3), 0); // CTBAU
        }
        // Command table: zero, then the H2D FIS at offset 0.
        unsafe { core::ptr::write_bytes(ct, 0, 512) };
        let fis = unsafe { &mut *(ct as *mut [u8; 20]) };
        fis[0] = 0x27; // H2D register FIS
        fis[1] = 0x80; // C (command)
        fis[2] = cmd;
        // 48-bit LBA: bytes 4,5,6 (low) + 8,9,10 (high); byte 7 is the
        // device/select register, whose LBA flag must be set or QEMU
        // falls back to CHS addressing (sector_num = -1 -> abort).
        let lb = lba.to_le_bytes();
        fis[4] = lb[0];
        fis[5] = lb[1];
        fis[6] = lb[2];
        fis[7] = 0x40; // device: LBA flag
        fis[8] = lb[3];
        fis[9] = lb[4];
        fis[10] = lb[5];
        fis[12] = count as u8;
        fis[13] = (count >> 8) as u8;
        // PRDT at command-table offset 0x80 (QEMU reads it from
        // cfis_addr + 0x80). Entry layout: addr @0 (u64), reserved @8,
        // size-1 @12 (QEMU's AHCI_SG flags_size, 22 bits, zero-based).
        let prd = unsafe { &mut *(ct.add(0x80) as *mut [u8; 16]) };
        prd[0..8].copy_from_slice(&((buf as usize) as u64).to_le_bytes());
        prd[8..12].copy_from_slice(&0u32.to_le_bytes());
        prd[12..16].copy_from_slice(&((bytes as u32 - 1) & 0x003F_FFFF).to_le_bytes());

        // Issue and wait for completion.
        self.set_reg(pbase + PX_CI, 1);
        let mut spins = 0;
        loop {
            if self.reg(pbase + PX_CI) & 1 == 0 {
                break;
            }
            spins += 1;
            if spins > SPIN_LIMIT {
                return Err("AHCI command timeout");
            }
            core::hint::spin_loop();
        }
        // DMA complete: task file should be idle and error-free.
        if self.reg(pbase + PX_TFD) & (PXTFD_BSY | PXTFD_DRQ | PXTFD_ERR) != 0 {
            return Err("AHCI task file error");
        }
        if self.reg(pbase + PX_IS) & PXIS_TFES != 0 {
            self.set_reg(pbase + PX_IS, PXIS_TFES); // clear
            return Err("AHCI task file error status");
        }
        Ok(())
    }

    /// Sends IDENTIFY DEVICE and fills in sectors/model.
    fn identify(&mut self) -> Result<(), &'static str> {
        let buf = self.port.buf as *mut u8;
        unsafe { core::ptr::write_bytes(buf, 0, 512) };
        self.command(CMD_IDENTIFY, 0, 0, buf, false)?;
        // The device DMA-wrote this buffer; flush any stale CPU cache
        // lines before reading it back (WB mapping + DMA incoherence).
        flush_cache(self.port.buf, 512);
        let words = unsafe { core::slice::from_raw_parts(self.port.buf as *const u16, 256) };
        // LBA48 capacity at words 100-103, LBA28 at 60-61.
        let lba48 = (words[103] as u64) << 48
            | (words[102] as u64) << 32
            | (words[101] as u64) << 16
            | words[100] as u64;
        self.sectors = if lba48 != 0 {
            lba48
        } else {
            ((words[61] as u64) << 16) | words[60] as u64
        };
        for (i, word) in words[27..47].iter().enumerate() {
            self.model[i * 2] = (word >> 8) as u8;
            self.model[i * 2 + 1] = (word & 0xFF) as u8;
        }
        Ok(())
    }

    fn read_sectors(&mut self, lba: u64, count: usize, buf: &mut [u8]) -> Result<(), &'static str> {
        let mut done = 0;
        while done < count {
            let n = (count - done).min(MAX_CHUNK_SECTORS);
            let chunk = &mut buf[done * 512..];
            self.command(CMD_READ_DMA_EXT, lba + done as u64, n as u16, chunk.as_ptr(), false)?;
            // The device DMA-wrote the chunk; drop stale cache lines.
            flush_cache(chunk.as_ptr() as usize, n * 512);
            done += n;
        }
        Ok(())
    }

    fn write_sectors(&mut self, lba: u64, count: usize, buf: &[u8]) -> Result<(), &'static str> {
        let mut done = 0;
        while done < count {
            let n = (count - done).min(MAX_CHUNK_SECTORS);
            let chunk = &buf[done * 512..];
            // Push any cached writes to RAM so the device DMA sees them.
            flush_cache(chunk.as_ptr() as usize, n * 512);
            self.command(CMD_WRITE_DMA_EXT, lba + done as u64, n as u16, chunk.as_ptr(), true)?;
            done += n;
        }
        Ok(())
    }
}

/// Finds and initializes the ICH9 AHCI controller. No-op once up.
pub fn init() -> Result<(), &'static str> {
    if AHCI.lock().is_some() {
        return Ok(());
    }
    let Some((bus, dev, func)) = pci::find_device(VENDOR_INTEL, DEV_ICH9_AHCI) else {
        return Err("ICH9 AHCI (8086:2922) not found on PCI bus 0");
    };
    let cmd = pci::config_read(bus, dev, func, 0x04);
    pci::config_write(bus, dev, func, 0x04, cmd | 0x7);
    let abar = pci::config_read(bus, dev, func, 0x24) & 0xFFFF_FFF0;
    if abar == 0 {
        return Err("AHCI ABAR not assigned");
    }

    let mut ahci = Ahci {
        mmio: abar as *mut u32,
        port: AhciPort {
            present: false,
            clb: 0,
            ct: 0,
            fb: 0,
            buf: 0,
        },
        sectors: 0,
        model: [0; 40],
    };

    // HBA reset, then AHCI-enable.
    ahci.set_reg(HBA_GHC, GHC_HR);
    let mut spins = 0;
    while ahci.reg(HBA_GHC) & GHC_HR != 0 {
        spins += 1;
        if spins > SPIN_LIMIT {
            return Err("AHCI reset timeout");
        }
        core::hint::spin_loop();
    }
    ahci.set_reg(HBA_GHC, GHC_AE);

    // Port 0 must be implemented and have a device.
    if ahci.reg(HBA_PI) & 1 == 0 {
        return Err("AHCI port 0 not implemented");
    }
    let pbase = 0x100 + 0x80 * 0;

    // Stop the port if it is somehow running.
    let pcmd = ahci.reg(pbase + PX_CMD);
    ahci.set_reg(pbase + PX_CMD, pcmd & !(PXCMD_ST | PXCMD_FRE));
    spins = 0;
    while ahci.reg(pbase + PX_CMD) & (PXCMD_FR | PXCMD_CR) != 0 {
        spins += 1;
        if spins > SPIN_LIMIT {
            return Err("AHCI port stop timeout");
        }
        core::hint::spin_loop();
    }

    // DMA structures (all page-aligned PMM frames).
    let clb = pmm::alloc_contiguous(1).ok_or("no memory for AHCI command list")?;
    let ct = pmm::alloc_contiguous(1).ok_or("no memory for AHCI command table")?;
    let fb = pmm::alloc_contiguous(1).ok_or("no memory for AHCI FIS area")?;
    let buf = pmm::alloc_contiguous(1).ok_or("no memory for AHCI identify buffer")?;
    unsafe {
        core::ptr::write_bytes(clb as *mut u8, 0, 4096);
        core::ptr::write_bytes(ct as *mut u8, 0, 4096);
        core::ptr::write_bytes(fb as *mut u8, 0, 4096);
        core::ptr::write_bytes(buf as *mut u8, 0, 4096);
    }
    ahci.port.clb = clb;
    ahci.port.ct = ct;
    ahci.port.fb = fb;
    ahci.port.buf = buf;

    // Point the port at the structures and enable the FIS receive.
    ahci.set_reg(pbase + PX_CLB, clb as u32);
    ahci.set_reg(pbase + PX_FB, fb as u32);
    ahci.set_reg(pbase + PX_IE, 0); // polled
    ahci.set_reg(pbase + PX_CMD, PXCMD_FRE | PXCMD_POD | PXCMD_SUD);
    spins = 0;
    while ahci.reg(pbase + PX_CMD) & PXCMD_FR == 0 {
        spins += 1;
        if spins > SPIN_LIMIT {
            return Err("AHCI FIS receive timeout");
        }
        core::hint::spin_loop();
    }

    // Device present (DET=3) and active (IPM=1).
    spins = 0;
    let ssts = loop {
        let ssts = ahci.reg(pbase + PX_SSTS);
        if ssts & 0x0F == 0x03 && (ssts >> 8) & 0x0F == 0x01 {
            break ssts;
        }
        spins += 1;
        if spins > SPIN_LIMIT {
            return Err("AHCI: no device on port 0");
        }
        core::hint::spin_loop();
    };
    let _ = ssts;

    // Start the command engine.
    ahci.set_reg(pbase + PX_CMD, ahci.reg(pbase + PX_CMD) | PXCMD_ST);
    ahci.port.present = true;

    ahci.identify()?;
    if ahci.sectors == 0 {
        return Err("AHCI identify returned zero sectors");
    }

    *AHCI.lock() = Some(ahci);
    Ok(())
}

/// True once the driver is up with a usable disk.
pub fn active() -> bool {
    let guard = AHCI.lock();
    guard.as_ref().map(|a| a.port.present).unwrap_or(false)
}

/// Sector count of the AHCI disk.
pub fn sectors() -> u64 {
    AHCI.lock().as_ref().map(|a| a.sectors).unwrap_or(0)
}

/// Human-readable description for the boot log / fatinfo.
pub fn describe() -> alloc::string::String {
    let guard = AHCI.lock();
    guard
        .as_ref()
        .map(|a| {
            let model = a.model_string();
            format!(
                "[0] SATA {} ({} MiB, LBA48)",
                if model.is_empty() { "(unnamed)" } else { &model },
                a.sectors / 2048
            )
        })
        .unwrap_or_else(|| "AHCI not initialized".into())
}

/// Reads `count` sectors starting at `lba` into `buf`.
pub fn read_sectors(lba: u64, count: usize, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut guard = AHCI.lock();
    guard.as_mut().ok_or("AHCI not initialized")?.read_sectors(lba, count, buf)
}

/// Writes `count` sectors starting at `lba` from `buf`.
pub fn write_sectors(lba: u64, count: usize, buf: &[u8]) -> Result<(), &'static str> {
    let mut guard = AHCI.lock();
    guard.as_mut().ok_or("AHCI not initialized")?.write_sectors(lba, count, buf)
}
