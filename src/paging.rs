//! The boot assembly (see `boot` in main.rs) identity-maps the first 4 GiB
//! of physical memory using 2 MiB pages. Any physical address the kernel
//! wants to access directly (RAM, or MMIO such as the VBE linear
//! framebuffer) must fall below this bound.
//!
//! On top of that map, `map_page`/`unmap_page` manipulate individual 4 KiB
//! pages of the kernel's own address space. Because the whole 4 GiB is
//! already covered by 2 MiB entries, touching a 4 KiB page inside one of
//! them first *splits* that 2 MiB entry into a fresh page table (each 4 KiB
//! entry keeps the original page's physical address and flags). The split
//! page table stays in place afterwards; that's a one-time cost per 2 MiB
//! region.
//!
//! `map_range_in` is the same machinery driven at an arbitrary PML4 (for
//! example a process's private address space, see `process.rs`); it reports
//! every frame it allocates so the caller can free them when the address
//! space dies.

use alloc::vec::Vec;

use crate::pmm;

pub const IDENTITY_MAP_END: u64 = 4 * 1024 * 1024 * 1024;
pub const PAGE_SIZE: u64 = 4096;

pub const PAGE_PRESENT: u64 = 1;
pub const PAGE_WRITABLE: u64 = 2;
pub const PAGE_USER: u64 = 4;

// Defined by the boot assembly in main.rs (.bss).
unsafe extern "C" {
    static page_table_pml4: u8;
}

const PRESENT_MASK: u64 = 0xFFF; // low 12 attribute bits
const PS_BIT: u64 = 1 << 7;
const ADDR_MASK: u64 = !(0xFFF);

/// Physical address of the kernel's own PML4 (the boot map). Its virtual
/// address equals its physical address, since the boot map is an identity
/// map, so this doubles as the CR3 value kernel tasks run with.
pub fn kernel_pml4() -> u64 {
    core::ptr::addr_of!(page_table_pml4) as u64
}

pub fn is_identity_mapped(addr: u64, len: u64) -> bool {
    match addr.checked_add(len) {
        Some(end) => end <= IDENTITY_MAP_END,
        None => false,
    }
}

fn table_base(entry: u64) -> *mut u64 {
    (entry & ADDR_MASK) as *mut u64
}

/// Walks the four-level page table rooted at `pml4_base` down to the entry
/// that would contain `virt`, splitting a 2 MiB entry if one is in the way
/// (installing a fresh 4 KiB page table when a directory entry is missing).
/// Returns the address of the (guaranteed present) page-table entry for
/// `virt`, or `None` when an entry is missing and no frame could be
/// allocated. Every frame allocated here is appended to
/// `allocated_frames` so callers can free them later; `None` for the
/// kernel's own map (its table frames live forever).
unsafe fn pt_entry_ptr_in(
    pml4_base: u64,
    virt: u64,
    entry_flags: u64,
    allocated_frames: Option<&mut Vec<usize>>,
) -> Option<*mut u64> {
    let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
    let pd_idx = ((virt >> 21) & 0x1FF) as usize;
    let pt_idx = ((virt >> 12) & 0x1FF) as usize;

    let pml4e = *(pml4_base as *const u64).add(pml4_idx);
    if pml4e & PAGE_PRESENT == 0 {
        return None; // the boot map covers the whole 4 GiB; anything else is a bug
    }
    // A USER mapping must be reachable at *every* level the page walk
    // passes through: the CPU checks U/S on the PML4, PDPT, PD and PT
    // entries alike, so promote the upper levels when requested. This
    // cannot widen the kernel's protection: a supervisor PDE/PTE in the
    // same chain still blocks ring 3.
    if entry_flags & PAGE_USER != 0 && pml4e & PAGE_USER == 0 {
        *(pml4_base as *mut u64).add(pml4_idx) |= PAGE_USER;
    }
    let pdpte = *table_base(pml4e).add(pdpt_idx);
    if pdpte & PAGE_PRESENT == 0 || pdpte & PS_BIT != 0 {
        return None; // no 1 GiB pages in the identity map
    }
    if entry_flags & PAGE_USER != 0 && pdpte & PAGE_USER == 0 {
        *table_base(pml4e).add(pdpt_idx) |= PAGE_USER;
    }
    let pd_base = table_base(pdpte);
    let pde = *pd_base.add(pd_idx);
    if pde & PAGE_PRESENT == 0 {
        // Nothing mapped here: install a fresh (zeroed) page table.
        let frame = pmm::frame_alloc()?;
        core::ptr::write_bytes(frame as *mut u8, 0, pmm::FRAME_SIZE);
        if let Some(frames) = allocated_frames {
            frames.push(frame);
        }
        *pd_base.add(pd_idx) = frame as u64 | entry_flags | PAGE_PRESENT;
        return Some((frame as *mut u64).add(pt_idx));
    }
    if pde & PS_BIT != 0 {
        // Split the 2 MiB entry into a page table. Every 4 KiB entry
        // keeps the physical address and attribute bits of the region it
        // covers; only PS itself is dropped. A USER mapping in this
        // region also needs USER set on the page directory itself: the
        // page walk requires U/S at *every* level it passes through.
        let frame = pmm::frame_alloc()?;
        if let Some(frames) = allocated_frames {
            frames.push(frame);
        }
        let pt = frame as *mut u64;
        let phys_base = pde & !((1 << 21) - 1);
        let page_flags = pde & PRESENT_MASK & !PS_BIT;
        for i in 0..512 {
            *pt.add(i) = phys_base + (i as u64) * PAGE_SIZE | page_flags;
        }
        *pd_base.add(pd_idx) = frame as u64 | (pde & PRESENT_MASK & !PS_BIT) | (entry_flags & PAGE_USER);
        return Some(pt.add(pt_idx));
    }
    // Existing page table. If the caller is mapping a USER page here,
    // promote the directory entry as well (see above); the entries
    // themselves stay exactly as mapped, so a supervisor PTE in this
    // table is still unreachable from ring 3.
    let pd_entry = pd_base.add(pd_idx);
    if entry_flags & PAGE_USER != 0 && *pd_entry & PAGE_USER == 0 {
        *pd_entry |= PAGE_USER;
    }
    Some(table_base(pde).add(pt_idx))
}

/// Maps the contiguous virtual range `[virt, virt+len)` to the contiguous
/// physical range starting at `phys` (all 4 KiB aligned) in the address
/// space rooted at `pml4_base`. `flags` is a subset of PAGE_* attributes
/// (PAGE_PRESENT is forced on). Table frames allocated on the way are
/// appended to `allocated_frames`. Returns false when the range is
/// invalid or a frame could not be allocated.
pub fn map_range_in(
    pml4_base: u64,
    virt: u64,
    phys: u64,
    len: u64,
    flags: u64,
    allocated_frames: &mut Vec<usize>,
) -> bool {
    if virt % PAGE_SIZE != 0
        || phys % PAGE_SIZE != 0
        || len % PAGE_SIZE != 0
        || len == 0
        || virt.checked_add(len).is_none()
        || virt + len > IDENTITY_MAP_END
    {
        return false;
    }
    let mut v = virt;
    let mut p = phys;
    let end = virt + len;
    while v < end {
        let Some(entry) = (unsafe {
            pt_entry_ptr_in(pml4_base, v, flags & PRESENT_MASK, Some(allocated_frames))
        }) else {
            return false;
        };
        unsafe {
            *entry = p | flags & PRESENT_MASK | PAGE_PRESENT;
        }
        invlpg(v);
        v += PAGE_SIZE;
        p += PAGE_SIZE;
    }
    true
}

/// Identity-maps `virt` to `phys` as a single 4 KiB page in the kernel's
/// own address space. Both must be 4 KiB aligned and below
/// `IDENTITY_MAP_END`. `flags` is a subset of PAGE_* attributes
/// (PAGE_PRESENT is forced on).
pub fn map_page(virt: u64, phys: u64, flags: u64) -> bool {
    if virt >= IDENTITY_MAP_END
        || phys >= IDENTITY_MAP_END
        || virt % PAGE_SIZE != 0
        || phys % PAGE_SIZE != 0
    {
        return false;
    }
    let Some(entry) = (unsafe { pt_entry_ptr_in(kernel_pml4(), virt, flags & PRESENT_MASK, None) })
    else {
        return false;
    };
    unsafe {
        *entry = phys | flags & PRESENT_MASK | PAGE_PRESENT;
    }
    invlpg(virt);
    true
}

/// Removes the 4 KiB mapping at `virt` (splitting a 2 MiB entry first if
/// needed) from the kernel's own address space. The physical frame is
/// *not* released; callers own it.
pub fn unmap_page(virt: u64) -> bool {
    if virt >= IDENTITY_MAP_END || virt % PAGE_SIZE != 0 {
        return false;
    }
    let Some(entry) =
        (unsafe { pt_entry_ptr_in(kernel_pml4(), virt, PAGE_PRESENT | PAGE_WRITABLE, None) })
    else {
        return false;
    };
    unsafe {
        *entry = 0;
    }
    invlpg(virt);
    true
}

pub fn invlpg(addr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{0}]", in(reg) addr, options(nostack, preserves_flags));
    }
}

pub fn read_cr3() -> u64 {
    let mut value: u64;
    unsafe {
        core::arch::asm!("mov {0}, cr3", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

pub fn write_cr3(cr3: u64) {
    unsafe {
        core::arch::asm!("mov cr3, {0}", in(reg) cr3, options(nostack, preserves_flags));
    }
}
