//! Block-device abstraction: AHCI (SATA) when the driver is up,
//! otherwise the first ATA PIO device. The FAT layer only talks to
//! `Disk`, so swapping the storage backend is a boot-time decision.

use alloc::format;
use alloc::string::String;

pub enum Disk {
    Ata(crate::ata::AtaDevice),
    Ahci,
}

impl Disk {
    pub fn read_sectors(&self, lba: u64, count: usize, buf: &mut [u8]) -> Result<(), &'static str> {
        match self {
            Disk::Ata(d) => d.read_sectors(lba, count, buf),
            Disk::Ahci => crate::ahci::read_sectors(lba, count, buf),
        }
    }

    pub fn write_sectors(&self, lba: u64, count: usize, buf: &[u8]) -> Result<(), &'static str> {
        match self {
            Disk::Ata(d) => d.write_sectors(lba, count, buf),
            Disk::Ahci => crate::ahci::write_sectors(lba, count, buf),
        }
    }

    pub fn sectors(&self) -> u64 {
        match self {
            Disk::Ata(d) => d.sectors,
            Disk::Ahci => crate::ahci::sectors(),
        }
    }

    pub fn description(&self) -> String {
        match self {
            Disk::Ata(d) => {
                let model = d.model_string();
                format!(
                    "ATA {} ({} MiB)",
                    if model.is_empty() { "(unnamed)" } else { &model },
                    d.sectors / 2048
                )
            }
            Disk::Ahci => crate::ahci::describe(),
        }
    }
}

/// The first usable block device: AHCI port 0 if the SATA driver is
/// up, otherwise ATA PIO device 0 (non-ATAPI). `None` means no disk.
pub fn first_disk() -> Option<Disk> {
    if crate::ahci::active() {
        Some(Disk::Ahci)
    } else {
        crate::ata::device(0)
            .filter(|d| d.present && !d.atapi)
            .map(Disk::Ata)
    }
}
