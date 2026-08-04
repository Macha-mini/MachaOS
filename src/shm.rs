//! Anonymous shared-memory objects (`memfd_create`) — the backing for
//! Linux's `MAP_SHARED`.
//!
//! `linux_abi.rs`'s regular file-backed `mmap` copies a file's bytes into
//! private, per-process frames (real enough for `ld.so` loading a
//! read-only shared library — see that module's docs). A `wl_shm`-style
//! use case needs the opposite: the *same* physical frames visible from
//! two different processes' page tables, so a client's writes are the
//! compositor's reads without either side copying anything. That's what
//! a `SharedMem` object here is — refcounted across every fd (in any
//! process) that refers to it, freed only once the last one closes.
//!
//! Kept as one contiguous physical allocation per object rather than
//! per-page frames: every real caller (`memfd_create` then exactly one
//! `ftruncate` before the first `mmap`, matching how `wl_shm` clients
//! actually use it) only ever grows once before mapping, and staying
//! contiguous lets a mapping still be described by `process::Mapping`'s
//! single `{vaddr, phys, len}` shape instead of needing a new one.

use alloc::vec::Vec;

use crate::pmm;
use crate::sync::SpinLock;

struct SharedMem {
    /// Physical base of the contiguous block backing this object; 0
    /// (with `pages == 0`) until the first `truncate` call.
    phys: usize,
    pages: usize,
    /// Logical size in bytes, `<= pages * FRAME_SIZE`.
    size: usize,
    /// One reference per open fd across every process (see
    /// `process::FdEntry`'s `Drop` impl, which calls `close`) — not per
    /// mapping. A mapping just borrows these frames for as long as some
    /// fd somewhere keeps the object alive; `munmap` never frees them.
    refcount: usize,
}

static TABLE: SpinLock<Vec<Option<SharedMem>>> = SpinLock::new(Vec::new());

/// Creates a new, empty (zero-size, unallocated) object with one
/// reference. Returns its id.
pub fn create() -> usize {
    let mut table = TABLE.lock();
    let obj = SharedMem { phys: 0, pages: 0, size: 0, refcount: 1 };
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(obj);
            return i;
        }
    }
    table.push(Some(obj));
    table.len() - 1
}

/// `ftruncate`: sets the object's logical size, growing (never
/// shrinking) its physical backing to fit — reallocating one fresh
/// contiguous block and copying forward, since frames already handed out
/// to an existing mapping can't be relocated once a process has them
/// page-mapped. Real callers only ever call this once, right after
/// `create`, before anything is mapped, so the copy-forward path is
/// dead in practice but keeps a second call from corrupting a live
/// mapping instead of just wasting work.
pub fn truncate(id: usize, new_size: usize) -> bool {
    let mut table = TABLE.lock();
    let Some(Some(obj)) = table.get_mut(id) else {
        return false;
    };
    if new_size <= obj.size {
        obj.size = new_size;
        return true;
    }
    let needed_pages = new_size.div_ceil(pmm::FRAME_SIZE).max(1);
    if needed_pages <= obj.pages {
        obj.size = new_size;
        return true;
    }
    let Some(new_phys) = pmm::alloc_contiguous(needed_pages) else {
        return false;
    };
    unsafe {
        core::ptr::write_bytes(new_phys as *mut u8, 0, needed_pages * pmm::FRAME_SIZE);
        if obj.pages > 0 {
            core::ptr::copy_nonoverlapping(obj.phys as *const u8, new_phys as *mut u8, obj.pages * pmm::FRAME_SIZE);
            for p in 0..obj.pages {
                pmm::frame_free(obj.phys + p * pmm::FRAME_SIZE);
            }
        }
    }
    obj.phys = new_phys;
    obj.pages = needed_pages;
    obj.size = new_size;
    true
}

pub fn size_of(id: usize) -> Option<usize> {
    TABLE.lock().get(id).and_then(|s| s.as_ref()).map(|o| o.size)
}

/// Physical base address of `id`'s backing, for mapping it into a
/// process's page tables (or, for the kernel-native compositor, reading
/// it directly — the kernel's own low memory is identity-mapped).
pub fn phys_of(id: usize) -> Option<usize> {
    TABLE.lock().get(id).and_then(|s| s.as_ref()).map(|o| o.phys)
}

pub fn pages_of(id: usize) -> Option<usize> {
    TABLE.lock().get(id).and_then(|s| s.as_ref()).map(|o| o.pages)
}

/// Bumps the refcount — called when an fd referring to this object is
/// duplicated into another process's fd table (SCM_RIGHTS).
pub fn dup(id: usize) {
    if let Some(Some(obj)) = TABLE.lock().get_mut(id) {
        obj.refcount += 1;
    }
}

/// Drops one reference; frees the backing frames once the last one goes.
pub fn close(id: usize) {
    let mut table = TABLE.lock();
    let Some(slot) = table.get_mut(id) else {
        return;
    };
    let Some(obj) = slot else {
        return;
    };
    obj.refcount -= 1;
    if obj.refcount == 0 {
        for p in 0..obj.pages {
            pmm::frame_free(obj.phys + p * pmm::FRAME_SIZE);
        }
        *slot = None;
    }
}
