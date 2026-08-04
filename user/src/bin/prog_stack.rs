//! prog_stack: recurses deep enough (each frame holding a real,
//! non-optimized-away 256-byte local buffer) to blow well past the
//! 32 KiB the kernel maps for a fresh process's ring-3 stack up front —
//! proving `process::try_grow_stack` (see process.rs) actually grows the
//! stack down into its reserved region instead of the touch being a fatal
//! page fault. Not tail recursive (the addition after the recursive call
//! forces the compiler to keep a real call stack), so this can't collapse
//! into a loop that would stay inside the initial 32 KiB.
//!
//! DEPTH * (~256 bytes buffer + call overhead) comfortably exceeds the
//! 32 KiB mapped at spawn while staying well under the 1 MiB growth
//! ceiling (`USER_STACK_MAX_PAGES` in process.rs).

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const DEPTH: u64 = 2000;

#[inline(never)]
fn recurse(n: u64) -> u64 {
    let buf = core::hint::black_box([n as u8; 256]);
    if n == 0 {
        buf[0] as u64
    } else {
        n + recurse(n - 1)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let sum = recurse(DEPTH);
    common::RESULT.store(sum, Ordering::Relaxed);
}
