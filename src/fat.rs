//! FAT32 filesystem on top of the ATA PIO driver.
//!
//! The whole FAT is cached in RAM at mount time (a 64 MiB volume with
//! 4 KiB clusters needs only 64 KiB of cache), which makes cluster-chain
//! walks cheap. Directory scans read sectors on demand. Mutating
//! operations keep a per-sector dirty map for the FAT and flush it back
//! to disk before returning.

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::ata::AtaDevice;
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatError {
    NoDisk,
    Io(&'static str),
    InvalidVolume,
    NotFound,
    NotDir,
    AlreadyExists,
    NotEmpty,
    InvalidName,
    OutOfSpace,
    Corrupt,
}

impl core::fmt::Display for FatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            FatError::NoDisk => "no disk mounted",
            FatError::Io(e) => e,
            FatError::InvalidVolume => "not a FAT32 volume",
            FatError::NotFound => "not found",
            FatError::NotDir => "not a directory",
            FatError::AlreadyExists => "already exists",
            FatError::NotEmpty => "directory not empty",
            FatError::InvalidName => "invalid name",
            FatError::OutOfSpace => "out of space",
            FatError::Corrupt => "corrupt filesystem",
        };
        f.write_str(message)
    }
}

const EOF_MARK: u32 = 0x0FFF_FFFF;
const CLUSTER_MASK: u32 = 0x0FFF_FFFF;
const CHAIN_GUARD: u32 = 1_000_000;
pub const MAX_NAME: usize = 128;

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u32,
    pub first_cluster: u32,
}

pub struct VolumeInfo {
    pub total_sectors: u32,
    pub sectors_per_cluster: u8,
    pub fat_sectors: u32,
    pub free_clusters: u32,
    pub root_cluster: u32,
}

pub struct Fat32 {
    device: AtaDevice,
    sectors_per_cluster: u8,
    reserved_sectors: u32,
    fat_count: u8,
    fat_size_sectors: u32,
    root_cluster: u32,
    first_data_sector: u32,
    total_sectors: u32,
    fat: Vec<u32>,
    fat_dirty: Vec<bool>,
    free_clusters: u32,
}

impl Fat32 {
    fn open(device: AtaDevice) -> Result<Fat32, FatError> {
        let mut sector = [0u8; 512];
        device.read_sectors(0, 1, &mut sector).map_err(FatError::Io)?;

        let bytes_per_sector = u16::from_le_bytes([sector[0x0B], sector[0x0C]]);
        let sectors_per_cluster = sector[0x0D];
        let reserved_sectors = u16::from_le_bytes([sector[0x0E], sector[0x0F]]) as u32;
        let fat_count = sector[0x10];
        let total_sectors = u32::from_le_bytes([sector[0x20], sector[0x21], sector[0x22], sector[0x23]]);
        let fat_size_sectors = u32::from_le_bytes([sector[0x24], sector[0x25], sector[0x26], sector[0x27]]);
        let root_cluster = u32::from_le_bytes([sector[0x2C], sector[0x2D], sector[0x2E], sector[0x2F]]);

        if bytes_per_sector != 512
            || sectors_per_cluster == 0
            || !sectors_per_cluster.is_power_of_two()
            || sector[0x1FE] != 0x55
            || sector[0x1FF] != 0xAA
            || fat_size_sectors == 0
            || root_cluster < 2
            || total_sectors == 0
        {
            return Err(FatError::InvalidVolume);
        }

        let first_data_sector = reserved_sectors + fat_count as u32 * fat_size_sectors;

        // Cache the whole FAT in RAM.
        let entries = fat_size_sectors as usize * 512 / 4;
        let mut fat = vec![0u32; entries];
        let mut buf = [0u8; 512];
        for i in 0..fat_size_sectors {
            device
                .read_sectors((reserved_sectors + i) as u64, 1, &mut buf)
                .map_err(FatError::Io)?;
            for j in 0..128 {
                fat[i as usize * 128 + j] = u32::from_le_bytes(buf[j * 4..j * 4 + 4].try_into().unwrap());
            }
        }

        Ok(Fat32 {
            device,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            fat_size_sectors,
            root_cluster,
            first_data_sector,
            total_sectors,
            fat,
            fat_dirty: vec![false; fat_size_sectors as usize],
            free_clusters: 0,
        })
    }

    fn cluster_to_sector(&self, cluster: u32) -> u64 {
        (self.first_data_sector + (cluster - 2) * self.sectors_per_cluster as u32) as u64
    }

    fn next_cluster(&self, cluster: u32) -> u32 {
        self.fat[cluster as usize] & CLUSTER_MASK
    }

    fn is_eof(cluster: u32) -> bool {
        cluster >= EOF_MARK
    }

    fn read_dir(&self, cluster: u32, out: &mut Vec<DirEntry>) -> Result<(), FatError> {
        let mut sector = [0u8; 512];
        let mut current = cluster;
        let mut lfn_parts: Vec<Vec<u16>> = Vec::new();
        let mut guard = 0u32;
        loop {
            for s in 0..self.sectors_per_cluster as u32 {
                self.device
                    .read_sectors(self.cluster_to_sector(current) + s as u64, 1, &mut sector)
                    .map_err(FatError::Io)?;
                let mut offset = 0usize;
                while offset < 512 {
                    let first = sector[offset];
                    if first == 0x00 {
                        return Ok(()); // end of directory
                    }
                    if first == 0xE5 {
                        lfn_parts.clear();
                        offset += 32;
                        continue;
                    }
                    let attr = sector[offset + 0x0B];
                    if attr == 0x0F {
                        let mut part = Vec::with_capacity(13);
                        for i in 0..5 {
                            part.push(u16::from_le_bytes([sector[offset + 1 + i * 2], sector[offset + 2 + i * 2]]));
                        }
                        for i in 0..6 {
                            part.push(u16::from_le_bytes([sector[offset + 14 + i * 2], sector[offset + 15 + i * 2]]));
                        }
                        for i in 0..2 {
                            part.push(u16::from_le_bytes([sector[offset + 28 + i * 2], sector[offset + 29 + i * 2]]));
                        }
                        // LFN entries are stored in reverse order, so each
                        // chunk is prepended to rebuild the name in order.
                        lfn_parts.insert(0, part);
                        offset += 32;
                        continue;
                    }
                    let name = if lfn_parts.is_empty() {
                        sfn_name(&sector[offset..offset + 11])
                    } else {
                        assemble_lfn(&lfn_parts)
                    };
                    let high = u16::from_le_bytes([sector[offset + 0x14], sector[offset + 0x15]]) as u32;
                    let low = u16::from_le_bytes([sector[offset + 0x1A], sector[offset + 0x1B]]) as u32;
                    let first_cluster = (high << 16) | low;
                    let size = u32::from_le_bytes(sector[offset + 0x1C..offset + 0x20].try_into().unwrap());
                    let is_dir = attr & 0x10 != 0;
                    lfn_parts.clear();
                    if !name.is_empty() && name != "." && name != ".." {
                        out.push(DirEntry {
                            name,
                            is_dir,
                            size,
                            first_cluster,
                        });
                    }
                    offset += 32;
                }
            }
            current = self.next_cluster(current);
            guard += 1;
            if Fat32::is_eof(current) {
                return Ok(());
            }
            if current < 2 || guard > CHAIN_GUARD {
                return Err(FatError::Corrupt);
            }
        }
    }

    fn read_file_data(&self, first_cluster: u32, size: usize) -> Result<Vec<u8>, FatError> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut data = Vec::with_capacity(size);
        let mut sector = [0u8; 512];
        let mut cluster = first_cluster;
        let mut remaining = size;
        let mut guard = 0u32;
        while remaining > 0 {
            if cluster < 2 || guard > CHAIN_GUARD {
                return Err(FatError::Corrupt);
            }
            guard += 1;
            for s in 0..self.sectors_per_cluster as u32 {
                self.device
                    .read_sectors(self.cluster_to_sector(cluster) + s as u64, 1, &mut sector)
                    .map_err(FatError::Io)?;
                let take = remaining.min(512);
                data.extend_from_slice(&sector[..take]);
                remaining -= take;
                if remaining == 0 {
                    break;
                }
            }
            cluster = self.next_cluster(cluster);
            if remaining > 0 && Fat32::is_eof(cluster) {
                return Err(FatError::Corrupt); // chain shorter than size
            }
        }
        Ok(data)
    }

    fn resolve_parent(&self, path: &str) -> Result<(u32, String), FatError> {
        let mut parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let name = parts.pop().ok_or(FatError::InvalidName)?.to_string();
        if name.len() > MAX_NAME {
            return Err(FatError::InvalidName);
        }
        let mut cluster = self.root_cluster;
        for comp in parts {
            let mut entries = Vec::new();
            self.read_dir(cluster, &mut entries)?;
            // FAT names are case-insensitive.
            let dir = entries
                .iter()
                .find(|e| e.is_dir && name_matches(&e.name, comp))
                .ok_or(FatError::NotFound)?;
            cluster = dir.first_cluster;
        }
        Ok((cluster, name))
    }

    fn scan_free_clusters(&mut self) {
        let mut free = 0u32;
        for &entry in &self.fat[2..] {
            if entry & CLUSTER_MASK == 0 {
                free += 1;
            }
        }
        self.free_clusters = free;
    }
}

fn name_matches(entry_name: &str, needle: &str) -> bool {
    entry_name.eq_ignore_ascii_case(needle)
}

fn sfn_name(sfn: &[u8]) -> String {
    let stem_end = sfn[..8].iter().position(|&b| b == b' ').unwrap_or(8);
    let ext_end = sfn[8..].iter().position(|&b| b == b' ').unwrap_or(3);
    let mut name = String::new();
    if stem_end > 0 {
        name.push_str(&String::from_utf8_lossy(&sfn[..stem_end]));
    }
    if ext_end > 0 {
        name.push('.');
        name.push_str(&String::from_utf8_lossy(&sfn[8..8 + ext_end]));
    }
    name
}

fn assemble_lfn(parts: &[Vec<u16>]) -> String {
    let mut name = String::new();
    'outer: for part in parts {
        for &c in part {
            if c == 0x0000 || c == 0xFFFF {
                break 'outer;
            }
            name.push(if c < 0x80 { c as u8 as char } else { '?' });
        }
    }
    name
}

static MOUNTED: SpinLock<Option<Fat32>> = SpinLock::new(None);

pub fn mounted() -> bool {
    MOUNTED.lock().as_ref().is_some()
}

/// Mounts the first ATA block device as FAT32. Safe to call again after
/// a failed attempt.
pub fn mount() -> Result<(), FatError> {
    let device = crate::ata::device(0)
        .filter(|d| d.present && !d.atapi)
        .ok_or(FatError::NoDisk)?;
    let mut fat = Fat32::open(device)?;
    fat.scan_free_clusters();
    *MOUNTED.lock() = Some(fat);
    Ok(())
}

pub fn info() -> Option<VolumeInfo> {
    let guard = MOUNTED.lock();
    guard.as_ref().map(|fat| VolumeInfo {
        total_sectors: fat.total_sectors,
        sectors_per_cluster: fat.sectors_per_cluster,
        fat_sectors: fat.fat_size_sectors,
        free_clusters: fat.free_clusters,
        root_cluster: fat.root_cluster,
    })
}

pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, FatError> {
    let guard = MOUNTED.lock();
    let fat = guard.as_ref().ok_or(FatError::NoDisk)?;
    let mut parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut cluster = fat.root_cluster;
    for comp in parts.drain(..) {
        let mut entries = Vec::new();
        fat.read_dir(cluster, &mut entries)?;
        let dir = entries
            .iter()
            .find(|e| e.is_dir && name_matches(&e.name, comp))
            .ok_or(FatError::NotFound)?;
        cluster = dir.first_cluster;
    }
    let mut entries = Vec::new();
    fat.read_dir(cluster, &mut entries)?;
    Ok(entries)
}

pub fn read_file(path: &str) -> Result<Vec<u8>, FatError> {
    let guard = MOUNTED.lock();
    let fat = guard.as_ref().ok_or(FatError::NoDisk)?;
    let (parent, name) = fat.resolve_parent(path)?;
    let mut entries = Vec::new();
    fat.read_dir(parent, &mut entries)?;
    let entry = entries
        .iter()
        .find(|e| !e.is_dir && name_matches(&e.name, &name))
        .ok_or(FatError::NotFound)?;
    fat.read_file_data(entry.first_cluster, entry.size as usize)
}
