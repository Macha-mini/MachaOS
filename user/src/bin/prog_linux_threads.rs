//! prog_linux_threads: exercises Phase 7's clone (CLONE_VM thread) and
//! real blocking futex. Spawns one thread via raw clone(2) that bumps a
//! shared counter, then joins it the way pthread_join does: futex_wait on
//! the CLONE_CHILD_CLEARTID word until the thread's exit clears it and
//! wakes the futex. Verifies the shared counter landed at the expected
//! value — proving the address space really is shared (CLONE_VM) and the
//! futex really blocks and wakes.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const SYS_CLONE: u64 = 56;
const SYS_FUTEX: u64 = 202;
const SYS_EXIT: u64 = 60;
const SYS_EXIT_GROUP: u64 = 231;

const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SYSVSEM: u64 = 0x40000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;

const N_INCREMENTS: u64 = 100_000;

/// Shared with the child via CLONE_VM: the counter the thread bumps and
/// the child-tid word pthread_join-style waits on.
static COUNTER: AtomicU64 = AtomicU64::new(0);
static CHILD_TID: AtomicU32 = AtomicU32::new(0);

static mut THREAD_STACK: [u8; 16384] = [0; 16384];

/// Raw clone(2): flags, stack top, parent_tidptr, child_tidptr, tls.
/// Returns the child pid in the parent, 0 in the child (which resumes
/// right here with every register preserved, glibc-style).
unsafe fn raw_clone(flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> i64 {
    let r: i64;
    core::arch::asm!(
        "syscall",
        in("rax") SYS_CLONE,
        in("rdi") flags,
        in("rsi") stack,
        in("rdx") parent_tid,
        in("r10") child_tid,
        in("r8") tls,
        lateout("rax") r,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    r
}

/// Child thread body: bump the shared counter, then exit the thread
/// (sys_exit(60) = thread exit; the kernel's thread_exit_self clears
/// CHILD_TID and wakes the join futex).
fn thread_main() -> ! {
    for _ in 0..N_INCREMENTS {
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    unsafe {
        common::syscall(SYS_EXIT, 0, 0, 0, 0);
    }
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    let stack_top = unsafe { core::ptr::addr_of_mut!(THREAD_STACK) as u64 + 16384 };
    let parent_tid = &CHILD_TID as *const AtomicU32 as u64;
    let child_tid = &CHILD_TID as *const AtomicU32 as u64;
    let flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD
        | CLONE_SYSVSEM | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;

    let r = unsafe { raw_clone(flags, stack_top, parent_tid, child_tid, 0) };
    if r == 0 {
        // Child: run the thread body (never returns).
        thread_main();
    } else if r < 0 {
        // clone failed
        ok |= 1 << 0; // mark failure via absence of other bits
    } else {
        // Parent: CLONE_PARENT_SETTID should have stored the child's pid.
        let tid = CHILD_TID.load(Ordering::Relaxed);
        if tid as i64 == r {
            ok |= 1 << 0;
        }
        // Join the way pthread_join does: futex_wait on the tid word until
        // the thread's exit clears it (the kernel's CLONE_CHILD_CLEARTID
        // handling writes 0 and wakes us).
        loop {
            let t = CHILD_TID.load(Ordering::Relaxed);
            if t == 0 {
                break;
            }
            unsafe {
                common::syscall(
                    SYS_FUTEX,
                    &CHILD_TID as *const AtomicU32 as u64,
                    FUTEX_WAIT,
                    t as u64,
                    0,
                );
            }
        }
        // The shared counter must reflect all the child's increments —
        // proof CLONE_VM really shared the address space.
        if COUNTER.load(Ordering::Relaxed) == N_INCREMENTS {
            ok |= 1 << 1;
        }
        // A FUTEX_WAKE on the (now cleared) tid word is a harmless no-op.
        let woken = unsafe {
            common::syscall(
                SYS_FUTEX,
                &CHILD_TID as *const AtomicU32 as u64,
                FUTEX_WAKE,
                1,
                0,
            )
        };
        if woken == 0 {
            ok |= 1 << 2;
        }
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}
