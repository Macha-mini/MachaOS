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
use crate::rtc;
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
    // Where the entry lives on disk ((directory cluster, byte offset in
    // its first sector)); used by the mutating operations.
    pub(crate) location: (u32, usize),
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
                            location: (current, s as usize * 512 + offset),
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

    // ---- mutating operations -------------------------------------------------

    fn set_cluster(&mut self, cluster: u32, value: u32) {
        self.fat[cluster as usize] = value;
        self.fat_dirty[cluster as usize / 128] = true;
    }

    fn alloc_cluster(&mut self) -> Result<u32, FatError> {
        for cluster in 2..self.fat.len() {
            if self.fat[cluster] & CLUSTER_MASK == 0 {
                self.set_cluster(cluster as u32, EOF_MARK);
                self.free_clusters = self.free_clusters.saturating_sub(1);
                return Ok(cluster as u32);
            }
        }
        Err(FatError::OutOfSpace)
    }

    fn free_chain(&mut self, mut cluster: u32) -> Result<(), FatError> {
        let mut guard = 0u32;
        while !Fat32::is_eof(cluster) {
            if cluster < 2 || guard > CHAIN_GUARD {
                return Err(FatError::Corrupt);
            }
            let next = self.next_cluster(cluster);
            self.set_cluster(cluster, 0);
            self.free_clusters += 1;
            cluster = next;
            guard += 1;
        }
        Ok(())
    }

    fn flush_fat(&mut self) -> Result<(), FatError> {
        let mut sector = [0u8; 512];
        for i in 0..self.fat_dirty.len() {
            if !self.fat_dirty[i] {
                continue;
            }
            for j in 0..128 {
                sector[j * 4..j * 4 + 4].copy_from_slice(&self.fat[i * 128 + j].to_le_bytes());
            }
            self.device
                .write_sectors((self.reserved_sectors + i as u32) as u64, 1, &sector)
                .map_err(FatError::Io)?;
            self.fat_dirty[i] = false;
        }
        Ok(())
    }

    fn cluster_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * 512
    }

    fn find_dir_entry(&self, parent: u32, name: &str) -> Option<DirEntry> {
        let mut entries = Vec::new();
        self.read_dir(parent, &mut entries).ok()?;
        entries.into_iter().find(|e| name_matches(&e.name, name))
    }

    fn delete_entry_at(&mut self, cluster: u32, offset: usize) -> Result<(), FatError> {
        let sector_idx = (offset / 512) as u64;
        let slot_offset = offset % 512;
        let mut sector = [0u8; 512];
        self.device
            .read_sectors(self.cluster_to_sector(cluster) + sector_idx, 1, &mut sector)
            .map_err(FatError::Io)?;
        sector[slot_offset] = 0xE5;
        // Free this entry's LFN chain: entries immediately before the SFN
        // in the same sector. (Chains spanning a sector boundary are rare
        // and harmless to leave: orphaned LFN entries are skipped by
        // readers because their checksum no longer matches anything.)
        let mut back = slot_offset;
        while back >= 32 && sector[back - 32 + 0x0B] == 0x0F {
            sector[back - 32] = 0xE5;
            back -= 32;
        }
        self.device
            .write_sectors(self.cluster_to_sector(cluster) + sector_idx, 1, &sector)
            .map_err(FatError::Io)
    }

    fn write_file_impl(&mut self, path: &str, content: &[u8]) -> Result<(), FatError> {
        let (parent, name) = self.resolve_parent(path)?;
        validate_name(&name)?;

        if let Some(existing) = self.find_dir_entry(parent, &name) {
            if existing.is_dir {
                return Err(FatError::NotDir);
            }
            self.delete_entry_at(existing.location.0, existing.location.1)?;
            self.free_chain(existing.first_cluster)?;
        }

        let cluster_bytes = self.cluster_bytes();
        let needed = content.len().div_ceil(cluster_bytes);
        let mut first_cluster = 0u32;
        if needed > 0 {
            let mut clusters = Vec::with_capacity(needed);
            for _ in 0..needed {
                clusters.push(self.alloc_cluster()?);
            }
            for i in 0..clusters.len() - 1 {
                self.set_cluster(clusters[i], clusters[i + 1]);
            }
            self.set_cluster(*clusters.last().unwrap(), EOF_MARK);
            first_cluster = clusters[0];

            let mut sector = [0u8; 512];
            let mut written = 0usize;
            'clusters: for &cluster in &clusters {
                for s in 0..self.sectors_per_cluster as u32 {
                    let take = (content.len() - written).min(512);
                    sector[..take].copy_from_slice(&content[written..written + take]);
                    sector[take..].fill(0);
                    self.device
                        .write_sectors(self.cluster_to_sector(cluster) + s as u64, 1, &sector)
                        .map_err(FatError::Io)?;
                    written += take;
                    if written == content.len() {
                        break 'clusters;
                    }
                }
            }
        }

        self.write_dir_entries(parent, &name, first_cluster, content.len() as u32, false)?;
        self.flush_fat()
    }

    fn make_dir_impl(&mut self, path: &str) -> Result<(), FatError> {
        let (parent, name) = self.resolve_parent(path)?;
        validate_name(&name)?;
        if self.find_dir_entry(parent, &name).is_some() {
            return Err(FatError::AlreadyExists);
        }

        let cluster = self.alloc_cluster()?;
        self.set_cluster(cluster, EOF_MARK);

        // Fresh cluster with "." and ".." entries in its first sector.
        let mut sector = [0u8; 512];
        let mut dot = [0u8; 32];
        dot[..11].copy_from_slice(b".          "); // "." + space padding (11 bytes)
        dot[0x0B] = 0x10; // directory attribute
        dot[0x14..0x16].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        dot[0x1A..0x1C].copy_from_slice(&((cluster & 0xFFFF) as u16).to_le_bytes());
        let mut dotdot = [0u8; 32];
        dotdot[..11].copy_from_slice(b"..         "); // ".." + space padding
        dotdot[0x0B] = 0x10;
        dotdot[0x14..0x16].copy_from_slice(&((parent >> 16) as u16).to_le_bytes());
        dotdot[0x1A..0x1C].copy_from_slice(&((parent & 0xFFFF) as u16).to_le_bytes());
        sector[..32].copy_from_slice(&dot);
        sector[32..64].copy_from_slice(&dotdot);
        self.device
            .write_sectors(self.cluster_to_sector(cluster), 1, &sector)
            .map_err(FatError::Io)?;

        self.write_dir_entries(parent, &name, cluster, 0, true)?;
        self.flush_fat()
    }

    fn remove_impl(&mut self, path: &str) -> Result<(), FatError> {
        let (parent, name) = self.resolve_parent(path)?;
        let entry = self
            .find_dir_entry(parent, &name)
            .ok_or(FatError::NotFound)?;
        if entry.is_dir {
            let mut contents = Vec::new();
            self.read_dir(entry.first_cluster, &mut contents)?;
            if !contents.is_empty() {
                return Err(FatError::NotEmpty);
            }
        }
        self.delete_entry_at(entry.location.0, entry.location.1)?;
        self.free_chain(entry.first_cluster)?;
        self.flush_fat()
    }

    /// Locates a run of `slots` free (0x00/0xE5) directory slots inside a
    /// single sector of `parent`'s chain, returning (cluster, byte offset).
    fn find_free_run(&self, parent: u32, slots: usize) -> Result<Option<(u32, usize)>, FatError> {
        let mut sector = [0u8; 512];
        let mut current = parent;
        let mut guard = 0u32;
        loop {
            for s in 0..self.sectors_per_cluster as u32 {
                self.device
                    .read_sectors(self.cluster_to_sector(current) + s as u64, 1, &mut sector)
                    .map_err(FatError::Io)?;
                let mut run = 0usize;
                for offset in (0..512).step_by(32) {
                    run = if sector[offset] == 0x00 || sector[offset] == 0xE5 {
                        run + 1
                    } else {
                        0
                    };
                    if run == slots {
                        return Ok(Some((current, offset + 32 - slots * 32)));
                    }
                }
            }
            let next = self.next_cluster(current);
            if Fat32::is_eof(next) {
                break;
            }
            current = next;
            guard += 1;
            if guard > CHAIN_GUARD {
                return Err(FatError::Corrupt);
            }
        }
        Ok(None)
    }

    /// Appends one zeroed cluster to a directory chain, returning it.
    fn extend_dir(&mut self, parent: u32) -> Result<u32, FatError> {
        let new_cluster = self.alloc_cluster()?;
        let mut current = parent;
        let mut guard = 0u32;
        while !Fat32::is_eof(self.next_cluster(current)) {
            current = self.next_cluster(current);
            guard += 1;
            if guard > CHAIN_GUARD {
                return Err(FatError::Corrupt);
            }
        }
        self.set_cluster(current, new_cluster);
        let zero = [0u8; 512];
        for s in 0..self.sectors_per_cluster as u32 {
            self.device
                .write_sectors(self.cluster_to_sector(new_cluster) + s as u64, 1, &zero)
                .map_err(FatError::Io)?;
        }
        Ok(new_cluster)
    }

    /// Writes an LFN + SFN entry pair for `name` into `parent`'s
    /// directory, extending the directory if it is full.
    fn write_dir_entries(
        &mut self,
        parent: u32,
        name: &str,
        first_cluster: u32,
        size: u32,
        is_dir: bool,
    ) -> Result<(), FatError> {
        let lfn_chars: Vec<u16> = name.chars().map(|c| c as u16).collect();
        let lfn_count = lfn_chars.len().div_ceil(13);
        let sfn = make_sfn(name);
        let checksum = sfn_checksum(&sfn);

        let mut entries: Vec<[u8; 32]> = Vec::with_capacity(lfn_count + 1);
        for i in (0..lfn_count).rev() {
            let mut part = vec![0xFFFFu16; 13];
            let mut n = 0usize;
            for (k, &c) in lfn_chars[i * 13..].iter().enumerate() {
                if n < 13 {
                    part[n] = c;
                    n += 1;
                }
                let _ = k;
            }
            if i == 0 && n < 13 {
                part[n] = 0x0000; // terminator in the last part
            }
            entries.push(lfn_entry_bytes((i + 1) as u8, &part, i == 0, checksum));
        }

        let mut sfn_entry = [0u8; 32];
        sfn_entry[..11].copy_from_slice(&sfn);
        sfn_entry[0x0B] = if is_dir { 0x10 } else { 0x20 };
        let now = rtc::now();
        let (date, time) = fat_datetime(now);
        sfn_entry[0x0E] = time.to_le_bytes()[0]; // creation time
        sfn_entry[0x0F] = time.to_le_bytes()[1];
        sfn_entry[0x10] = date.to_le_bytes()[0]; // creation date
        sfn_entry[0x11] = date.to_le_bytes()[1];
        sfn_entry[0x12] = date.to_le_bytes()[0]; // last access date
        sfn_entry[0x13] = date.to_le_bytes()[1];
        sfn_entry[0x16] = time.to_le_bytes()[0]; // write time
        sfn_entry[0x17] = time.to_le_bytes()[1];
        sfn_entry[0x18] = date.to_le_bytes()[0]; // write date
        sfn_entry[0x19] = date.to_le_bytes()[1];
        sfn_entry[0x14..0x16].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        sfn_entry[0x1A..0x1C].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());        sfn_entry[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
        entries.push(sfn_entry);

        let place = match self.find_free_run(parent, entries.len())? {
            Some(place) => place,
            None => (self.extend_dir(parent)?, 0),
        };
        let (cluster, offset) = place;
        let sector_idx = (offset / 512) as u64;
        let slot_offset = offset % 512;
        let mut sector = [0u8; 512];
        self.device
            .read_sectors(self.cluster_to_sector(cluster) + sector_idx, 1, &mut sector)
            .map_err(FatError::Io)?;
        for (i, entry) in entries.iter().enumerate() {
            sector[slot_offset + i * 32..slot_offset + (i + 1) * 32].copy_from_slice(entry);
        }
        self.device
            .write_sectors(self.cluster_to_sector(cluster) + sector_idx, 1, &sector)
            .map_err(FatError::Io)
    }
}

fn validate_name(name: &str) -> Result<(), FatError> {
    if name.is_empty()
        || name.len() > MAX_NAME
        || name.starts_with('.')
        || name.chars().any(|c| c.is_ascii_control() || c == '/')
    {
        return Err(FatError::InvalidName);
    }
    Ok(())
}

/// Builds an 8.3 short name from an (ASCII) long name. Collisions with
/// existing entries (and ~1 numbering) are not resolved yet.
fn make_sfn(name: &str) -> [u8; 11] {
    let mut stem_end = name.len();
    if let Some(dot) = name.rfind('.') {
        stem_end = dot;
    }
    let stem = &name[..stem_end];
    let ext = if stem_end < name.len() { &name[stem_end + 1..] } else { "" };

    let mut sfn = [b' '; 11];
    let mut i = 0usize;
    for &b in stem.as_bytes() {
        if i < 8 && is_sfn_char(b) {
            sfn[i] = b.to_ascii_uppercase();
            i += 1;
        }
    }
    let mut j = 0usize;
    for &b in ext.as_bytes() {
        if j < 3 && is_sfn_char(b) {
            sfn[8 + j] = b.to_ascii_uppercase();
            j += 1;
        }
    }
    sfn
}

fn is_sfn_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'$' | b'%'
                | b'\''
                | b'-'
                | b'_'
                | b'@'
                | b'~'
                | b'`'
                | b'!'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'^'
                | b'#'
                | b'&'
        )
}

fn sfn_checksum(sfn: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &b in sfn {
        sum = ((sum & 1) << 7) | (sum >> 1);
        sum = sum.wrapping_add(b);
    }
    sum
}

fn lfn_entry_bytes(seq: u8, part: &[u16], is_last: bool, checksum: u8) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0] = seq | if is_last { 0x40 } else { 0 };
    for (i, &c) in part.iter().enumerate() {
        let idx = match i {
            0..=4 => 1 + i * 2,
            5..=10 => 14 + (i - 5) * 2,
            _ => 28 + (i - 11) * 2,
        };
        entry[idx] = (c & 0xFF) as u8;
        entry[idx + 1] = (c >> 8) as u8;
    }
    entry[0x0B] = 0x0F; // LFN attribute
    entry[0x0D] = checksum;
    entry
}

/// FAT date/time words from an RTC reading.
fn fat_datetime(dt: rtc::DateTime) -> (u16, u16) {
    let year = (dt.year as u16).saturating_sub(1980).clamp(0, 127);
    let date = (year << 9) | ((dt.month as u16 & 0x0F) << 5) | (dt.day as u16 & 0x1F);
    let time =
        ((dt.hour as u16 & 0x1F) << 11) | ((dt.minute as u16 & 0x3F) << 5) | ((dt.second as u16 / 2) & 0x1F);
    (date, time)
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

/// Writes (or overwrites) a file, creating an LFN + SFN entry.
pub fn write_file(path: &str, content: &[u8]) -> Result<(), FatError> {
    let mut guard = MOUNTED.lock();
    let fat = guard.as_mut().ok_or(FatError::NoDisk)?;
    fat.write_file_impl(path, content)
}

/// Creates a directory with `.` and `..` entries.
pub fn make_dir(path: &str) -> Result<(), FatError> {
    let mut guard = MOUNTED.lock();
    let fat = guard.as_mut().ok_or(FatError::NoDisk)?;
    fat.make_dir_impl(path)
}

/// Removes a file, or an empty directory.
pub fn remove(path: &str) -> Result<(), FatError> {
    let mut guard = MOUNTED.lock();
    let fat = guard.as_mut().ok_or(FatError::NoDisk)?;
    fat.remove_impl(path)
}

/// Returns whether `path` exists and is a directory. `/` is a directory.
pub fn is_dir(path: &str) -> Result<bool, FatError> {
    let guard = MOUNTED.lock();
    let fat = guard.as_ref().ok_or(FatError::NoDisk)?;
    if path == "/" {
        return Ok(true);
    }
    let (parent, name) = fat.resolve_parent(path)?;
    match fat.find_dir_entry(parent, &name) {
        Some(entry) => Ok(entry.is_dir),
        None => Err(FatError::NotFound),
    }
}

/// Moves a file — or an empty directory — from `src` to `dst`.
///
/// Implemented as copy + delete (the FAT layer has no rename); an
/// existing file at `dst` is overwritten, an existing directory makes
/// `make_dir` fail with `AlreadyExists`, and moving a non-empty
/// directory fails with `NotEmpty` (no recursive copy).
pub fn move_file(src: &str, dst: &str) -> Result<(), FatError> {
    if src == dst {
        return Ok(());
    }
    if is_dir(src)? {
        if !list_dir(src)?.is_empty() {
            return Err(FatError::NotEmpty);
        }
        make_dir(dst)?;
        remove(src)?;
        Ok(())
    } else {
        let data = read_file(src)?;
        write_file(dst, &data)?;
        remove(src)
    }
}
