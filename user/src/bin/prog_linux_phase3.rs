//! prog_linux_phase3: exercises the Phase 3 syscalls (futex, rseq,
//! prlimit64, sched_getaffinity, sysinfo) linux_abi.rs added for static
//! glibc binaries' startup. No real glibc toolchain was available to
//! build an actual glibc test binary against, so this calls each syscall
//! directly and checks its documented (deliberately simplified — see
//! linux_abi.rs's module docs) behavior, the same way prog_linux_syscall.rs
//! checks the Phase 2 set.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::{AtomicU32, Ordering};

const SYS_FUTEX: u64 = 202;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_SYSINFO: u64 = 99;
const SYS_EXIT: u64 = 60;
const SYS_PRLIMIT64: u64 = 302;
const SYS_RSEQ: u64 = 334;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const EAGAIN: i64 = 11;
const EINVAL: i64 = 22;
const RLIM_INFINITY: u64 = u64::MAX;

static LOCK_WORD: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    // FUTEX_WAIT with the *current* value: uncontended-acquisition case,
    // must return 0 (see linux_abi.rs's sys_futex doc comment) rather
    // than actually blocking (nothing could ever wake it here).
    let addr = LOCK_WORD.load(Ordering::Relaxed) as u64;
    let _ = addr;
    // FUTEX_WAIT with a *stale* expected value (LOCK_WORD is 0, expecting
    // 1): the word doesn't match, so it must fail EAGAIN immediately
    // without blocking. (FUTEX_WAIT with a *matching* value now really
    // blocks — Phase 7 — so that case is exercised by the clone/thread
    // test, not here where nothing would ever wake it.)
    let r = unsafe { common::syscall(SYS_FUTEX, &LOCK_WORD as *const _ as u64, FUTEX_WAIT, 1, 0) };
    if r == (-EAGAIN) as u64 {
        ok |= 1 << 0;
    }

    // FUTEX_WAIT with a *stale* value: must fail EAGAIN immediately.
    let r = unsafe { common::syscall(SYS_FUTEX, &LOCK_WORD as *const _ as u64, FUTEX_WAIT, 0xFFFF_FFFF, 0) };
    if r == (-EAGAIN) as u64 {
        ok |= 1 << 1;
    }

    // FUTEX_WAKE: always "0 waiters woken".
    let r = unsafe { common::syscall(SYS_FUTEX, &LOCK_WORD as *const _ as u64, FUTEX_WAKE, 1, 0) };
    if r == 0 {
        ok |= 1 << 2;
    }

    // rseq: always refused.
    let r = unsafe { common::syscall(SYS_RSEQ, 0, 0, 0, 0) };
    if r == (-EINVAL) as u64 {
        ok |= 1 << 3;
    }

    // prlimit64: query (old_limit != 0) reports RLIM_INFINITY for both fields.
    let mut rlim = [0u64; 2];
    let r = unsafe { common::syscall(SYS_PRLIMIT64, 0, 0, 0, rlim.as_mut_ptr() as u64) };
    if r == 0 && rlim[0] == RLIM_INFINITY && rlim[1] == RLIM_INFINITY {
        ok |= 1 << 4;
    }

    // sched_getaffinity: CPU 0 present in the returned mask.
    let mut mask = [0u8; 8];
    let r = unsafe { common::syscall(SYS_SCHED_GETAFFINITY, 0, mask.len() as u64, mask.as_mut_ptr() as u64, 0) };
    if r == 8 && mask[0] == 1 {
        ok |= 1 << 5;
    }

    // sysinfo: totalram/mem_unit are plausible (nonzero, mem_unit == 1).
    let mut info = [0u8; 112];
    let r = unsafe { common::syscall(SYS_SYSINFO, info.as_mut_ptr() as u64, 0, 0, 0) };
    let totalram = u64::from_le_bytes(info[32..40].try_into().unwrap());
    let mem_unit = u32::from_le_bytes(info[104..108].try_into().unwrap());
    if r == 0 && totalram > 0 && mem_unit == 1 {
        ok |= 1 << 6;
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT, 0, 0, 0, 0);
    }
}
