//! ELF64 executable parsing. `parse` validates the header and program
//! headers of a 64-bit little-endian ET_EXEC or ET_DYN file (no
//! relocations, no dynamic linking done *here* — an ET_DYN's own
//! PT_DYNAMIC segment, if any, is just loaded and ignored like any other
//! PT_LOAD data; a dynamically-linked binary's actual relocation and
//! symbol resolution is `PT_INTERP`'s job — see `Program::interp` and
//! `process::spawn_linux`'s Phase 4 handling, which loads and runs the
//! real interpreter instead) and returns the entry point plus the
//! PT_LOAD segments, which the process manager (see `process.rs`) maps
//! into a fresh address space. All reads are bounds-checked against the
//! input slice; any malformed field yields an error string rather than a
//! panic.
//!
//! ET_DYN segments carry link-time addresses relative to a base of 0 (the
//! whole point of being position-independent); `process.rs` picks a load
//! address and adds it to every segment's `vaddr` and to `entry` before
//! mapping, the same job a real Linux kernel's ELF loader does before
//! handing off to `ld.so` (see `Program::is_pie`).

use alloc::string::{String, ToString};
use alloc::vec::Vec;

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: u16 = 56;
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 0x3E;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// One PT_LOAD segment, ready to be mapped. `memsz` can exceed
/// `filesz` (BSS): the loader zero-fills the difference.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub file_offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub writable: bool,
    /// From `PF_X`. Not yet enforced (mapping the segment doesn't set the
    /// NX bit on non-executable pages — `paging.rs` has no NX support
    /// yet), but recorded so that piece can be added later without
    /// touching the ELF parser again.
    pub executable: bool,
}

#[derive(Debug)]
pub struct Program {
    pub entry: u64,
    pub segments: Vec<Segment>,
    /// Whether this is an ET_DYN (PIE) image, whose segment addresses are
    /// relative to a base of 0 and need a load bias applied.
    pub is_pie: bool,
    /// Location of the program header table in the file. None of this
    /// binary's PT_LOAD segments necessarily covers file offset `phoff`
    /// (MachaOS's own `user/linker.ld` places `.result` first, well past
    /// it), so a caller that needs the table mapped somewhere for AT_PHDR
    /// (see `process::setup_linux_stack`) reads these bytes straight out
    /// of the original file and places its own copy, rather than
    /// computing an address within a loaded segment the way a real
    /// toolchain's output (which always covers offset 0) would allow.
    pub phoff: u64,
    pub phentsize: u16,
    pub phnum: u16,
    /// From a `PT_INTERP` segment, if this binary has one: the path (as
    /// a NUL-terminated string in the file, decoded here) of the dynamic
    /// linker that should actually run it — see `process::spawn_linux`'s
    /// Phase 4 handling.
    pub interp: Option<String>,
}

fn read_u16(data: &[u8], off: usize) -> Result<u16, &'static str> {
    data.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or("truncated ELF header")
}

fn read_u32(data: &[u8], off: usize) -> Result<u32, &'static str> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or("truncated ELF header")
}

fn read_u64(data: &[u8], off: usize) -> Result<u64, &'static str> {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(data.get(off..off + 8).ok_or("truncated ELF header")?);
    Ok(u64::from_le_bytes(bytes))
}

pub fn parse(data: &[u8]) -> Result<Program, &'static str> {
    if data.len() < ELF_HEADER_SIZE {
        return Err("truncated ELF header");
    }
    if data[0..4] != [0x7F, b'E', b'L', b'F'] {
        return Err("not an ELF file");
    }
    if data[EI_CLASS] != ELFCLASS64 {
        return Err("not a 64-bit ELF");
    }
    if data[EI_DATA] != ELFDATA2LSB {
        return Err("not a little-endian ELF");
    }
    let e_type = read_u16(data, 16)?;
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err("not an executable (ET_EXEC/ET_DYN)");
    }
    let is_pie = e_type == ET_DYN;
    if read_u16(data, 18)? != EM_X86_64 {
        return Err("not an x86_64 ELF");
    }

    let entry = read_u64(data, 24)?;
    let phoff = read_u64(data, 32)?;
    let phentsize = read_u16(data, 54)?;
    let phnum = read_u16(data, 56)?;
    if phentsize != PROGRAM_HEADER_SIZE {
        return Err("unexpected program header size");
    }
    let table_end = phoff
        .checked_add((phnum as u64).checked_mul(phentsize as u64).ok_or("overflow")?)
        .ok_or("program header table overflow")?;
    if table_end > data.len() as u64 {
        return Err("program header table outside file");
    }

    let mut segments = Vec::new();
    let mut interp = None;
    for i in 0..phnum {
        let off = (phoff + i as u64 * phentsize as u64) as usize;
        let p_type = read_u32(data, off)?;
        if p_type == PT_INTERP {
            let p_offset = read_u64(data, off + 8)?;
            let p_filesz = read_u64(data, off + 32)?;
            let end = p_offset.checked_add(p_filesz).ok_or("interp range overflow")?;
            let bytes = data.get(p_offset as usize..end as usize).ok_or("interp data outside file")?;
            let s = core::str::from_utf8(bytes).map_err(|_| "invalid interp string")?;
            interp = Some(s.trim_end_matches('\0').to_string());
            continue;
        }
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = read_u32(data, off + 4)?;
        let p_offset = read_u64(data, off + 8)?;
        let p_vaddr = read_u64(data, off + 16)?;
        let p_filesz = read_u64(data, off + 32)?;
        let p_memsz = read_u64(data, off + 40)?;
        if p_memsz == 0 {
            continue; // zero-length placeholder segments (e.g. rust-lld's)
        }
        let file_end = p_offset
            .checked_add(p_filesz)
            .ok_or("segment file range overflow")?;
        if file_end > data.len() as u64 {
            return Err("segment data outside file");
        }
        if p_filesz > p_memsz {
            return Err("segment filesz exceeds memsz");
        }
        segments.push(Segment {
            vaddr: p_vaddr,
            file_offset: p_offset,
            filesz: p_filesz,
            memsz: p_memsz,
            writable: p_flags & PF_W != 0,
            executable: p_flags & PF_X != 0,
        });
    }
    if segments.is_empty() {
        return Err("no loadable segments");
    }
    Ok(Program {
        entry,
        segments,
        is_pie,
        phoff,
        phentsize,
        phnum,
        interp,
    })
}
