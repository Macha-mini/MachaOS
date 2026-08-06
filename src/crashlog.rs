//! Best-effort persistence of kernel panics and CPU exceptions to the
//! mounted FAT32 volume, so a crash can be diagnosed later without a
//! serial connection (the previous behavior — serial/VGA output only —
//! made a crashed machine a black box unless a terminal was attached).
//!
//! Every entry is appended to a fixed file (`/system/panic.log`, the
//! directory is created by `make disk`), capped to the newest
//! `MAX_LOG_SIZE` bytes so the log can never grow without bound. All
//! errors are swallowed: this runs at the edge of death (from the panic
//! handler or an exception handler, right before the machine halts) and
//! nothing here is allowed to make the situation worse. A nested panic
//! inside the disk write is broken by the `WRITING` guard — panics never
//! return, so the flag never needs clearing.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::fat;

/// Fixed log path on the mounted volume (`/system` is created by
/// `make disk`; a boot without the disk simply skips the write).
const PANIC_LOG_PATH: &str = "/system/panic.log";
/// Keep only the most recent bytes of the log.
const MAX_LOG_SIZE: usize = 64 * 1024;

/// Guards against recursion: `append` itself uses the heap and the disk
/// driver, either of which can panic mid-write. Set once on entry and
/// never cleared — a second fatal event while writing is dropped rather
/// than recursed into.
static WRITING: AtomicBool = AtomicBool::new(false);

/// Appends `entry` to the panic log. Never panics; never returns an
/// error — the caller is about to halt the machine regardless. The FAT
/// side uses a `try_lock` under a single acquisition, so a panic that
/// fires *inside* a FAT operation (while the volume lock is held) drops
/// the write instead of deadlocking the panic handler on its own lock.
pub fn append(entry: &str) {
    if WRITING.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = fat::try_append_file(PANIC_LOG_PATH, entry.as_bytes(), MAX_LOG_SIZE);
}
