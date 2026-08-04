//! prog_linux_syscall: verifies `syscall::syscall_dispatch` actually
//! routes a Linux-ABI process (`process::Abi::Linux`, set by
//! `process::spawn_linux_test`) to `linux_abi::syscall_dispatch` instead
//! of the native table — calls Linux syscall number 39 (`getpid`, wired
//! up for real in the skeleton) and expects a positive pid back, then an
//! unrecognized number (999) and expects `-ENOSYS` (-38) back, matching
//! the real Linux "negative errno" convention rather than MachaOS's
//! native `u64::MAX` error sentinel.
//!
//! Exits via a syscall numbered 1, same as MachaOS's native SYS_EXIT —
//! not a coincidence but the one documented gap in the skeleton
//! (`linux_abi.rs`'s module docs): `syscall_entry`'s asm hijacks syscall
//! number 1 as the native exit *before* either dispatch table ever sees
//! it, regardless of a process's `Abi`. Under real Linux numbering 1 is
//! `write`, not `exit` (`exit` is 60) — Phase 2 has to fix that once a
//! Linux process needs to actually call `write`. This program relies on
//! the same hijack purely as the only way to cleanly terminate right
//! now.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_GETPID_LINUX: u64 = 39;
const SYS_UNKNOWN_LINUX: u64 = 999;
const SYS_EXIT_HIJACKED: u64 = 1;
const ENOSYS: i64 = 38;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    let pid = unsafe { common::syscall(SYS_GETPID_LINUX, 0, 0, 0, 0) };
    if pid > 0 && pid < 1_000_000 {
        ok |= 1 << 0;
    }

    let unknown = unsafe { common::syscall(SYS_UNKNOWN_LINUX, 0, 0, 0, 0) };
    if unknown == (-ENOSYS) as u64 {
        ok |= 1 << 1;
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_HIJACKED, 0, 0, 0, 0);
    }
}
