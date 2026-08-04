//! prog_linux_syscall: verifies `syscall::syscall_dispatch` actually
//! routes a Linux-ABI process (`process::Abi::Linux`, set by
//! `process::spawn_linux_test`) to `linux_abi::syscall_dispatch` instead
//! of the native table — calls Linux syscall number 39 (`getpid`, wired
//! up for real in the skeleton) and expects a positive pid back, then an
//! unrecognized number (999) and expects `-ENOSYS` (-38) back, matching
//! the real Linux "negative errno" convention rather than MachaOS's
//! native `u64::MAX` error sentinel.
//!
//! Exits via the real Linux `exit` (syscall 60) — `syscall_entry`'s asm
//! checks the current process's `Abi` before deciding what "exit" means
//! (see syscall.rs's module docs), so unlike when this program was first
//! written, syscall number 1 now correctly reaches `linux_abi.rs` as
//! `write` instead of being hijacked as the native ABI's exit.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_WRITE_LINUX: u64 = 1;
const SYS_GETPID_LINUX: u64 = 39;
const SYS_UNKNOWN_LINUX: u64 = 999;
const SYS_EXIT_LINUX: u64 = 60;
const ENOSYS: i64 = 38;
const FD_STDOUT: u64 = 1;

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

    // Linux syscall 1 is `write`, not exit — this only prints correctly
    // (rather than being hijacked into an early exit) because
    // `syscall_entry` now checks the process's Abi. See linux_abi.rs.
    let message = b"prog_linux_syscall: hello via Linux write(1, ...)\n";
    let n = unsafe { common::syscall(SYS_WRITE_LINUX, FD_STDOUT, message.as_ptr() as u64, message.len() as u64, 0) };
    if n == message.len() as u64 {
        ok |= 1 << 2;
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_LINUX, 0, 0, 0, 0);
    }
}
