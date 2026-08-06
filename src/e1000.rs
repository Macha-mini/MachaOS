//! Intel 82540EM "e1000" NIC driver — the device QEMU provides by
//! default on x86. Polled, no interrupts: the RX ring is drained by
//! whoever calls `poll_rx` (the ARP resolver and, later, the network
//! stack task). Rings and buffers are plain 4 KiB PMM frames — the
//! kernel is identity-mapped, so their physical addresses are directly
//! usable as both CPU pointers and DMA addresses (everything lives
//! below 4 GiB, which is all the 82540EM needs).
//!
//! The QEMU user-mode network ("slirp") gives the guest 10.0.2.15/24
//! with gateway 10.0.2.2 and DNS 10.0.2.3.

use alloc::vec;
use alloc::vec::Vec;
use core::ptr;

use crate::pci;
use crate::pmm;
use crate::sync::SpinLock;

const VENDOR_INTEL: u16 = 0x8086;
const DEV_E1000_82540EM: u16 = 0x100E;

// Register offsets (8254x family).
const REG_CTRL: u32 = 0x0000;
const REG_STATUS: u32 = 0x0008;
const REG_RCTL: u32 = 0x0100;
const REG_TCTL: u32 = 0x0400;
const REG_TIPG: u32 = 0x0410;
const REG_RDBAL: u32 = 0x2800;
const REG_RDBAH: u32 = 0x2804;
const REG_RDLEN: u32 = 0x2808;
const REG_RDH: u32 = 0x2810;
const REG_RDT: u32 = 0x2818;
const REG_TDBAL: u32 = 0x3800;
const REG_TDBAH: u32 = 0x3804;
const REG_TDLEN: u32 = 0x3808;
const REG_TDH: u32 = 0x3810;
const REG_TDT: u32 = 0x3818;
const REG_RAL0: u32 = 0x5400;
const REG_RAH0: u32 = 0x5404;

const CTRL_RST: u32 = 1 << 26;
const CTRL_SLU: u32 = 1 << 6;
const STATUS_LU: u32 = 1 << 1;
const RCTL_EN: u32 = 1 << 1;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;

const NUM_DESC: usize = 32;
const RX_BUF_SIZE: usize = 2048;

// Descriptor flags.
const DESC_EOP: u8 = 1; // end of packet
const DESC_IFCS: u8 = 2; // insert FCS/CRC
const DESC_RS: u8 = 8; // report status
const DESC_DD: u8 = 1; // descriptor done

#[repr(C, align(16))]
struct RxDesc {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C, align(16))]
struct TxDesc {
    addr: u64,
    length: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

pub struct E1000 {
    mmio: *mut u32,
    mac: [u8; 6],
    rx_ring: *mut RxDesc,
    rx_buffers: [usize; NUM_DESC],
    rx_cur: usize,
    tx_ring: *mut TxDesc,
    tx_buf: usize,
    tx_cur: usize,
    rx_count: u64,
    tx_count: u64,
}

// The NIC lives behind a SpinLock and is only touched from kernel
// context on the single CPU; raw pointers are fine under that guard.
unsafe impl Send for E1000 {}

pub static NIC: SpinLock<Option<E1000>> = SpinLock::new(None);

impl E1000 {
    #[inline]
    fn reg(&self, off: u32) -> u32 {
        unsafe { ptr::read_volatile(self.mmio.add((off / 4) as usize)) }
    }

    #[inline]
    fn set_reg(&self, off: u32, value: u32) {
        unsafe { ptr::write_volatile(self.mmio.add((off / 4) as usize), value) };
    }

    /// Read the MAC the NIC was assigned (QEMU loads it into RA[0] at
    /// reset).
    fn read_mac(&self) -> [u8; 6] {
        let ral = self.reg(REG_RAL0);
        let rah = self.reg(REG_RAH0);
        [
            ral as u8,
            (ral >> 8) as u8,
            (ral >> 16) as u8,
            (ral >> 24) as u8,
            rah as u8,
            (rah >> 8) as u8,
        ]
    }

    fn init_rings(&mut self) -> Result<(), &'static str> {
        self.rx_ring = pmm::alloc_contiguous(1).ok_or("no memory for RX ring")? as *mut RxDesc;
        self.tx_ring = pmm::alloc_contiguous(1).ok_or("no memory for TX ring")? as *mut TxDesc;
        self.tx_buf = pmm::alloc_contiguous(1).ok_or("no memory for TX buffer")?;
        // The 82540EM only addresses DMA below 4 GiB. First-fit
        // allocation from low memory normally guarantees this, but with
        // the PMM now covering 16 GiB, reject explicitly so a future
        // allocation-policy change can never silently break the NIC.
        const DMA_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
        if (self.rx_ring as u64) >= DMA_LIMIT
            || (self.tx_ring as u64) >= DMA_LIMIT
            || (self.tx_buf as u64) >= DMA_LIMIT
        {
            return Err("e1000: DMA ring above 4 GiB (82540EM is 32-bit)");
        }
        unsafe {
            ptr::write_bytes(self.rx_ring as *mut u8, 0, 4096);
            ptr::write_bytes(self.tx_ring as *mut u8, 0, 4096);
            ptr::write_bytes(self.tx_buf as *mut u8, 0, 4096);
        }
        for i in 0..NUM_DESC {
            self.rx_buffers[i] = pmm::alloc_contiguous(1).ok_or("no memory for RX buffers")?;
            if (self.rx_buffers[i] as u64) >= DMA_LIMIT {
                return Err("e1000: DMA buffer above 4 GiB (82540EM is 32-bit)");
            }
            unsafe { ptr::write_bytes(self.rx_buffers[i] as *mut u8, 0, 4096) };
            let d = unsafe { &mut *self.rx_ring.add(i) };
            d.addr = self.rx_buffers[i] as u64;
            d.length = 0;
            d.status = 0;
        }
        self.rx_cur = 0;
        self.tx_cur = 0;
        Ok(())
    }
}

/// Finds and initializes the NIC. Safe to call more than once (no-op
/// after the first success).
pub fn init() -> Result<(), &'static str> {
    if NIC.lock().is_some() {
        return Ok(());
    }
    let Some((bus, dev, func)) = pci::find_device(VENDOR_INTEL, DEV_E1000_82540EM) else {
        return Err("e1000 (82540EM) not found on PCI bus 0");
    };
    // Enable the device: memory + I/O decoding and bus mastering (DMA).
    let cmd = pci::config_read(bus, dev, func, 0x04);
    pci::config_write(bus, dev, func, 0x04, cmd | 0x7);
    let bar0 = pci::config_read(bus, dev, func, 0x10) & 0xFFFF_FFF0;
    if bar0 == 0 {
        return Err("e1000 BAR0 not assigned");
    }

    let mut nic = E1000 {
        mmio: bar0 as *mut u32,
        mac: [0; 6],
        rx_ring: core::ptr::null_mut(),
        rx_buffers: [0; NUM_DESC],
        rx_cur: 0,
        tx_ring: core::ptr::null_mut(),
        tx_buf: 0,
        tx_cur: 0,
        rx_count: 0,
        tx_count: 0,
    };

    // Reset the controller and wait for it to come back.
    nic.set_reg(REG_CTRL, nic.reg(REG_CTRL) | CTRL_RST);
    let mut spins = 0;
    while nic.reg(REG_CTRL) & CTRL_RST != 0 {
        spins += 1;
        if spins > 2_000_000 {
            return Err("e1000 reset timeout");
        }
        core::hint::spin_loop();
    }
    nic.mac = nic.read_mac();
    if nic.mac == [0; 6] {
        return Err("e1000 MAC is all zeroes");
    }

    nic.init_rings()?;

    // RX ring: base/length/tail, then enable reception (broadcast +
    // multicast accepted, CRC stripped).
    nic.set_reg(REG_RDBAL, nic.rx_ring as u32);
    nic.set_reg(REG_RDBAH, 0);
    nic.set_reg(REG_RDLEN, (NUM_DESC * 16) as u32);
    nic.set_reg(REG_RDH, 0);
    nic.set_reg(REG_RDT, (NUM_DESC - 1) as u32);
    nic.set_reg(REG_RCTL, RCTL_EN | RCTL_BAM | RCTL_MPE | RCTL_SECRC);

    // TX ring: base/length, enable with short-packet padding.
    nic.set_reg(REG_TDBAL, nic.tx_ring as u32);
    nic.set_reg(REG_TDBAH, 0);
    nic.set_reg(REG_TDLEN, (NUM_DESC * 16) as u32);
    nic.set_reg(REG_TDH, 0);
    nic.set_reg(REG_TDT, 0);
    nic.set_reg(REG_TCTL, TCTL_EN | TCTL_PSP);
    nic.set_reg(REG_TIPG, 10);

    // Force the link up (no PHY autoneg to wait for in QEMU).
    nic.set_reg(REG_CTRL, nic.reg(REG_CTRL) | CTRL_SLU);

    *NIC.lock() = Some(nic);
    Ok(())
}

/// The NIC's MAC address, if the driver is up.
pub fn mac() -> Option<[u8; 6]> {
    NIC.lock().as_ref().map(|n| n.mac)
}

/// Link status (QEMU's slirp link is always up once the driver is).
pub fn link_up() -> bool {
    let guard = NIC.lock();
    guard.as_ref().map(|n| n.reg(REG_STATUS) & STATUS_LU != 0).unwrap_or(false)
}

/// (rx_packets, tx_packets) counters.
pub fn stats() -> (u64, u64) {
    let guard = NIC.lock();
    guard.as_ref().map(|n| (n.rx_count, n.tx_count)).unwrap_or((0, 0))
}

/// Transmits one Ethernet frame (synchronous: waits for the descriptor
/// to complete).
pub fn send(packet: &[u8]) -> Result<(), &'static str> {
    let mut guard = NIC.lock();
    let Some(nic) = guard.as_mut() else {
        return Err("NIC not initialized");
    };
    if packet.is_empty() || packet.len() > RX_BUF_SIZE {
        return Err("bad packet size");
    }
    unsafe {
        ptr::copy_nonoverlapping(packet.as_ptr(), nic.tx_buf as *mut u8, packet.len());
    }
    let idx = nic.tx_cur;
    let d = unsafe { &mut *nic.tx_ring.add(idx) };
    d.addr = nic.tx_buf as u64;
    d.length = packet.len() as u16;
    d.cmd = DESC_EOP | DESC_IFCS | DESC_RS;
    d.status = 0;
    // Poke the tail; the NIC transmits from the old tail to the new.
    nic.set_reg(REG_TDT, ((idx + 1) % NUM_DESC) as u32);
    nic.tx_cur = (idx + 1) % NUM_DESC;
    let mut spins = 0;
    while unsafe { ptr::read_volatile(&d.status) } & DESC_DD == 0 {
        spins += 1;
        if spins > 100_000 {
            return Err("TX timeout");
        }
        core::hint::spin_loop();
    }
    nic.tx_count += 1;
    Ok(())
}

/// Returns the next received Ethernet frame, or `None` if the RX ring
/// is empty. The frame is copied out so the ring buffer can be
/// recycled immediately.
pub fn poll_rx() -> Option<Vec<u8>> {
    let mut guard = NIC.lock();
    let nic = guard.as_mut()?;
    let idx = nic.rx_cur;
    let d = unsafe { &mut *nic.rx_ring.add(idx) };
    if unsafe { ptr::read_volatile(&d.status) } & DESC_DD == 0 {
        return None;
    }
    let len = (d.length as usize).min(RX_BUF_SIZE);
    let mut out = vec![0u8; len];
    unsafe {
        ptr::copy_nonoverlapping(nic.rx_buffers[idx] as *const u8, out.as_mut_ptr(), len);
    }
    // Hand the descriptor back to the NIC.
    d.status = 0;
    d.length = 0;
    nic.set_reg(REG_RDT, idx as u32);
    nic.rx_cur = (idx + 1) % NUM_DESC;
    nic.rx_count += 1;
    Some(out)
}
