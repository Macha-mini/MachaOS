//! prog_linux_signal: exercises Phase 8b's signal handler delivery. The
//! program installs a handler for SIGUSR1 and SIGUSR2 (via rt_sigaction),
//! raises each on itself (kill), and verifies that the kernel delivered
//! them asynchronously — the handler ran and `rt_sigreturn` restored the
//! interrupted context (proven by the *second* signal still being
//! deliverable: if the mask weren't restored, it would stay blocked
//! forever). A third phase holds a canary pointer in rdi across a
//! delivery and keeps writing through it: if rt_sigreturn doesn't
//! restore the full GP register set, rdi comes back as the signal
//! number and the write-through #PFs — the busybox `kill -CHLD` flaky.
//! Result = 0xF when all three phases passed.

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
/// Written through the pointer held in rdi during phase 3 — a leaked rdi
/// turns the write into a #PF at the signal number.
static CANARY: AtomicU64 = AtomicU64::new(0);

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

    // Phase 3: full GP register restore. Raise a third SIGUSR1, then spin
    // writing through a canary pointer held in rdi until it is delivered
    // (HITS 2 -> 3). A sigreturn that restores only rip/rsp leaves rdi =
    // sig (10) in the interrupted loop, and the very next `mov [rdi],
    // rax` page-faults at address 0xA — killing us before the SIGTERM.
    unsafe {
        common::syscall(SYS_KILL, me, SIGUSR1, 0, 0);
    }
    let ptr = &CANARY as *const AtomicU64 as u64;
    let mut rdi = ptr;
    unsafe {
        core::arch::asm!(
            "xor rcx, rcx",
            "2:",
            "mov rax, [rdi]",
            "add rax, 1",
            "mov [rdi], rax", // write through the pointer — #PF if rdi leaked
            "mov rax, [rip + {hits}]",
            "cmp rax, 3",
            "jge 3f",
            "add rcx, 1",
            "cmp rcx, 50000000", // safety bound; delivery is at the next tick
            "jb 2b",
            "3:",
            hits = sym HITS,
            inout("rdi") rdi => rdi,
            out("rcx") _,
            out("rax") _,
            options(nostack),
        );
    }
    if ok == 3
        && rdi == ptr
        && CANARY.load(Ordering::Relaxed) > 0
        && HITS.load(Ordering::Relaxed) >= 3
    {
        ok |= 4;
    }

    // A signal with no handler (SIGTERM, SIG_DFL) must terminate us — the
    // parent's wait reports the signal death. Never reached if it works.
    common::RESULT.store(if ok == 7 { 0xF } else { 0 }, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_KILL, me, 15, 0, 0); // SIGTERM — default action: kill
    }
    // If the kernel didn't terminate us (bug), the exit below runs.
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}
