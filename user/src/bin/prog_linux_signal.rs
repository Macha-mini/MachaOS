//! prog_linux_signal: exercises Phase 8b's signal handler delivery. The
//! program installs a handler for SIGUSR1 and SIGUSR2 (via rt_sigaction),
//! raises each on itself (kill), and verifies that the kernel delivered
//! them asynchronously — the handler ran and `rt_sigreturn` restored the
//! interrupted context (proven by the *second* signal still being
//! deliverable: if the mask weren't restored, it would stay blocked
//! forever). Result = 0xB when both handlers ran once, in order.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::{AtomicU64, Ordering};

const SYS_RT_SIGACTION: u64 = 13;
const SYS_KILL: u64 = 62;
const SYS_GETPID: u64 = 39;
const SYS_EXIT_GROUP: u64 = 231;

const SIGUSR1: u64 = 10;
const SIGUSR2: u64 = 12;

static HITS: AtomicU64 = AtomicU64::new(0);
static LAST_SIG: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
extern "C" fn handle_sig(sig: u64) {
    LAST_SIG.store(sig, Ordering::Relaxed);
    HITS.fetch_add(1, Ordering::Relaxed);
}

/// Waits for `HITS` to reach `n`, spinning with a bounded trip count.
fn wait_hits(n: u64) -> bool {
    let mut spins = 0u64;
    while HITS.load(Ordering::Relaxed) < n {
        spins += 1;
        if spins > 200_000_000 {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;
    let me = unsafe { common::syscall(SYS_GETPID, 0, 0, 0, 0) };

    // struct sigaction (kernel layout): handler, flags, restorer, mask.
    let sa = [handle_sig as u64, 0u64, 0u64, 0u64];
    let r = unsafe { common::syscall(SYS_RT_SIGACTION, SIGUSR1, &sa as *const u64 as u64, 0, 8) };
    if r != 0 {
        common::RESULT.store(0, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 1, 0, 0, 0);
        }
    }
    let r = unsafe { common::syscall(SYS_RT_SIGACTION, SIGUSR2, &sa as *const u64 as u64, 0, 8) };
    if r != 0 {
        common::RESULT.store(0, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 2, 0, 0, 0);
        }
    }

    // Raise both signals on ourselves; the kernel delivers each at the
    // next timer tick (the handler runs in ring 3 between our syscalls).
    unsafe {
        common::syscall(SYS_KILL, me, SIGUSR1, 0, 0);
    }
    if wait_hits(1) && LAST_SIG.load(Ordering::Relaxed) == SIGUSR1 {
        ok |= 1;
    }
    unsafe {
        common::syscall(SYS_KILL, me, SIGUSR2, 0, 0);
    }
    if wait_hits(2) && LAST_SIG.load(Ordering::Relaxed) == SIGUSR2 {
        ok |= 2;
    }

    // A signal with no handler (SIGTERM, SIG_DFL) must terminate us — the
    // parent's wait reports the signal death. Never reached if it works.
    common::RESULT.store(if ok == 3 { 0xB } else { 0 }, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_KILL, me, 15, 0, 0); // SIGTERM — default action: kill
    }
    // If the kernel didn't terminate us (bug), the exit below runs.
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}
