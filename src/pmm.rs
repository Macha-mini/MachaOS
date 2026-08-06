//! Physical memory manager: a bitmap over 4 KiB frames, fed from the
//! Multiboot memory map. Everything outside the "usable" regions (and
//! below the 1 MiB mark, where the BIOS, VGA memory, and the page tables
//! live) starts out marked in use, so a missing region can never be
//! handed out by accident.
//!
//! Memory beyond `MAX_PHYS_MEM` is simply ignored: the boot-time identity
//! map covers the whole 16 GiB below (see `paging::IDENTITY_MAP_END`, and
//! the static PDs `paging::extend_identity_map` fills in `kmain`), so all
//! of it is reachable; QEMU is typically run with `-m 4G` to `-m 16G`.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::multiboot::MultibootInfo;
use crate::sync::SpinLock;

pub const FRAME_SIZE: usize = 4096;
pub const MAX_PHYS_MEM: usize = 16 * 1024 * 1024 * 1024; // 16 GiB
const FRAME_COUNT: usize = MAX_PHYS_MEM / FRAME_SIZE; // 4194304
const BITMAP_BYTES: usize = FRAME_COUNT / 8; // 524288

/// Reserved (never handed out by `frame_alloc`/`alloc_contiguous`) below
/// this physical address, on top of the kernel image itself. The kernel
/// heap (`allocator::init`) is identity-mapped — its physical address
/// *is* its virtual one, and every process's page table deep-copies the
/// kernel's own identity map — so wherever the heap's frames land is
/// unusable to every process as well, not just the kernel. Reserving
/// this low range keeps the heap's first-fit allocation from landing on
/// it, freeing it up for two conventions that both need to be down here:
/// MachaOS's own native-ABI test programs (`user/linker.ld`, 48 MiB) and
/// the address most real (non-PIE) Linux binaries are linked to load at
/// (traditionally 0x400000 = 4 MiB) — see `process::validate_segments`,
/// which rejects anything overlapping wherever the heap (now above this
/// line) actually ends up, with a 1 MiB margin on both sides.
pub const LOW_RESERVED_END: usize = 64 * 1024 * 1024;

// All frames start marked "in use"; `init` clears exactly the usable
// ones. Access is single-threaded in practice (boot init, and later
// syscall/kernel paths that take the spin lock), but the lock documents
// the intent and keeps future callers honest.
static LOCK: SpinLock<()> = SpinLock::new(());

static mut BITMAP: [u8; BITMAP_BYTES] = [0xFF; BITMAP_BYTES];

// Highest frame index (inclusive) that came from a usable region, set by
// `init`. Frames beyond it are never allocated, so statistics reflect
// actual RAM rather than MAX_PHYS_MEM.
static LAST_USABLE_FRAME: AtomicUsize = AtomicUsize::new(0);

const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

unsafe fn set_frame(frame: usize) {
    BITMAP[frame / 8] |= 1 << (frame % 8);
}

unsafe fn clear_frame(frame: usize) {
    BITMAP[frame / 8] &= !(1 << (frame % 8));
}

unsafe fn frame_is_free(frame: usize) -> bool {
    BITMAP[frame / 8] & (1 << (frame % 8)) == 0
}

/// Marks the half-open range `[base, base + len)` as in use. Used by the
/// boot path to protect the kernel image and, after fb::init, the linear
/// framebuffer (QEMU reports it as reserved, but belt and braces).
pub fn mark_reserved(base: usize, len: usize) {
    if len == 0 {
        return;
    }
    let start = align_down(base, FRAME_SIZE);
    let end = align_up(base.saturating_add(len), FRAME_SIZE).min(MAX_PHYS_MEM);
    let _guard = LOCK.lock();
    unsafe {
        let mut frame = start / FRAME_SIZE;
        while frame * FRAME_SIZE < end {
            set_frame(frame);
            frame += 1;
        }
    }
}

/// Scans the Multiboot memory map and marks every usable frame (at or
/// above 1 MiB) as free, then re-protects the kernel image. Must be
/// called before `allocator::init()`, which takes its backing memory
/// from here.
pub fn init(info: &MultibootInfo) {
    let _guard = LOCK.lock();
    let mut last_frame = 0usize;
    unsafe {
        for region in info.memory_map() {
            if region.region_type != 1 {
                continue;
            }
            if region.base < 1024 * 1024 as u64 {
                continue; // below 1 MiB: BIOS data, VGA, and page tables
            }
            let start = region.base as usize;
            let end = (region.base + region.length) as usize;
            let start_frame = align_up(start, FRAME_SIZE) / FRAME_SIZE;
            let end_frame = align_down(end, FRAME_SIZE).min(MAX_PHYS_MEM) / FRAME_SIZE;
            if end_frame > last_frame {
                last_frame = end_frame - 1;
            }
            let mut frame = start_frame;
            while frame < end_frame {
                clear_frame(frame);
                frame += 1;
            }
        }
    }
    LAST_USABLE_FRAME.store(last_frame, Ordering::Relaxed);
    drop(_guard); // mark_reserved takes the lock itself

    unsafe extern "C" {
        static _kernel_start: u8;
        static _kernel_end: u8;
    }
    let kernel_start = core::ptr::addr_of!(_kernel_start) as usize;
    let kernel_end = core::ptr::addr_of!(_kernel_end) as usize;
    mark_reserved(kernel_start, kernel_end - kernel_start);
    mark_reserved(0, LOW_RESERVED_END);
}

/// Allocates a single 4 KiB frame, returning its physical address.
pub fn frame_alloc() -> Option<usize> {
    let _guard = LOCK.lock();
    unsafe {
        let last = LAST_USABLE_FRAME.load(Ordering::Relaxed);
        for frame in 0..=last {
            if frame_is_free(frame) {
                set_frame(frame);
                return Some(frame * FRAME_SIZE);
            }
        }
    }
    None
}

/// Allocates `count` contiguous 4 KiB frames, returning the physical
/// address of the first one.
pub fn alloc_contiguous(count: usize) -> Option<usize> {
    let _guard = LOCK.lock();
    unsafe {
        let last = LAST_USABLE_FRAME.load(Ordering::Relaxed);
        if count == 0 || count - 1 > last {
            return None;
        }
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        for frame in 0..=last {
            if frame_is_free(frame) {
                if run_len == 0 {
                    run_start = frame;
                }
                run_len += 1;
                if run_len == count {
                    for f in run_start..run_start + count {
                        set_frame(f);
                    }
                    return Some(run_start * FRAME_SIZE);
                }
            } else {
                run_len = 0;
            }
        }
    }
    None
}

/// Releases a frame previously returned by `frame_alloc`/`alloc_contiguous`.
pub fn frame_free(phys: usize) {
    if phys % FRAME_SIZE != 0 || phys >= MAX_PHYS_MEM {
        return;
    }
    let _guard = LOCK.lock();
    unsafe {
        clear_frame(phys / FRAME_SIZE);
    }
}

pub fn free_frames() -> usize {
    let _guard = LOCK.lock();
    count_free()
}

fn count_free() -> usize {
    let mut free = 0usize;
    unsafe {
        for frame in 0..=LAST_USABLE_FRAME.load(Ordering::Relaxed) {
            if frame_is_free(frame) {
                free += 1;
            }
        }
    }
    free
}

pub fn used_frames() -> usize {
    let _guard = LOCK.lock();
    let total = LAST_USABLE_FRAME.load(Ordering::Relaxed) + 1;
    total - count_free()
}

pub fn total_frames() -> usize {
    LAST_USABLE_FRAME.load(Ordering::Relaxed) + 1
}
