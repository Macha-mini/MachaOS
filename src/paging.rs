//! The boot assembly (see `boot` in main.rs) identity-maps the first 4 GiB
//! of physical memory using 2 MiB pages. Any physical address the kernel
//! wants to access directly (RAM, or MMIO such as the VBE linear
//! framebuffer) must fall below this bound.
//!
//! On top of that map, `map_page`/`unmap_page` manipulate individual 4 KiB
//! pages. Because the whole 4 GiB is already covered by 2 MiB entries,
//! touching a 4 KiB page inside one of them first *splits* that 2 MiB
//! entry into a fresh page table (each 4 KiB entry keeps the original
//! page's physical address and flags). The split page table stays in
//! place afterwards; that's a one-time cost per 2 MiB region.

use crate::pmm;

pub const IDENTITY_MAP_END: u64 = 4 * 1024 * 1024 * 1024;
pub const PAGE_SIZE: u64 = 4096;

pub const PAGE_PRESENT: u64 = 1;
pub const PAGE_WRITABLE: u64 = 2;
pub const PAGE_USER: u64 = 4;

// Defined by the boot assembly in main.rs (.bss).
unsafe extern "C" {
    static page_table_pml4: u8;
    static page_table_pdp: u8;
    static page_table_pd: u8;
}

const PRESENT_MASK: u64 = 0xFFF; // low 12 attribute bits
const PS_BIT: u64 = 1 << 7;
const ADDR_MASK: u64 = !(0xFFF);

pub fn is_identity_mapped(addr: u64, len: u64) -> bool {
    match addr.checked_add(len) {
        Some(end) => end <= IDENTITY_MAP_END,
        None => false,
    }
}

fn table_base(entry: u64) -> *mut u64 {
    (entry & ADDR_MASK) as *mut u64
}

/// Walks the identity map to the page table that would contain `virt`,
/// splitting a 2 MiB entry if one is in the way. Returns the address of
/// the (guaranteed present) page-table entry for `virt`, or `None` when
/// the entry itself is not present and no frame could be allocated.
unsafe fn pt_entry_ptr(virt: u64, entry_flags: u64) -> Option<*mut u64> {
    let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
    let pd_idx = ((virt >> 21) & 0x1FF) as usize;
    let pt_idx = ((virt >> 12) & 0x1FF) as usize;

    let pml4e = *core::ptr::addr_of!(page_table_pml4).cast::<u64>().add(pml4_idx);
    if pml4e & PAGE_PRESENT == 0 {
        return None; // the boot map covers the whole 4 GiB; anything else is a bug
    }
    let pdpte = *table_base(pml4e).add(pdpt_idx);
    if pdpte & PAGE_PRESENT == 0 || pdpte & PS_BIT != 0 {
        return None; // no 1 GiB pages in the identity map
    }
    let pd_base = table_base(pdpte);
    let pde = *pd_base.add(pd_idx);
    if pde & PAGE_PRESENT == 0 {
        // Nothing mapped here: install a fresh (zeroed) page table.
        let frame = pmm::frame_alloc()?;
        core::ptr::write_bytes(frame as *mut u8, 0, pmm::FRAME_SIZE);
        *pd_base.add(pd_idx) = frame as u64 | entry_flags | PAGE_PRESENT;
        return Some((frame as *mut u64).add(pt_idx));
    }
    if pde & PS_BIT != 0 {
        // Split the 2 MiB entry into a page table. Every 4 KiB entry
        // keeps the physical address and attribute bits of the region it
        // covers; only PS itself is dropped.
        let frame = pmm::frame_alloc()?;
        let pt = frame as *mut u64;
        let phys_base = pde & !((1 << 21) - 1);
        let page_flags = pde & PRESENT_MASK & !PS_BIT;
        for i in 0..512 {
            *pt.add(i) = phys_base + (i as u64) * PAGE_SIZE | page_flags;
        }
        *pd_base.add(pd_idx) = frame as u64 | (pde & PRESENT_MASK & !PS_BIT);
        return Some(pt.add(pt_idx));
    }
    Some(table_base(pde).add(pt_idx))
}

/// Identity-maps `virt` to `phys` as a single 4 KiB page. Both must be
/// 4 KiB aligned and below `IDENTITY_MAP_END`. `flags` is a subset of
/// PAGE_* attributes (PAGE_PRESENT is forced on).
pub fn map_page(virt: u64, phys: u64, flags: u64) -> bool {
    if virt >= IDENTITY_MAP_END
        || phys >= IDENTITY_MAP_END
        || virt % PAGE_SIZE != 0
        || phys % PAGE_SIZE != 0
    {
        return false;
    }
    let Some(entry) = (unsafe { pt_entry_ptr(virt, flags & PRESENT_MASK) }) else {
        return false;
    };
    unsafe {
        *entry = phys | flags & PRESENT_MASK | PAGE_PRESENT;
    }
    invlpg(virt);
    true
}

/// Removes the 4 KiB mapping at `virt` (splitting a 2 MiB entry first if
/// needed). The physical frame is *not* released; callers own it.
pub fn unmap_page(virt: u64) -> bool {
    if virt >= IDENTITY_MAP_END || virt % PAGE_SIZE != 0 {
        return false;
    }
    let Some(entry) = (unsafe { pt_entry_ptr(virt, PAGE_PRESENT | PAGE_WRITABLE) }) else {
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
