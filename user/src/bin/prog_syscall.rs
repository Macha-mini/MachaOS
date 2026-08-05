//! prog_syscall: writes a message to stdout in a single sys_write(fd,
//! ptr, len) call (not the old byte-at-a-time convention), then stores
//! the tick count sys_clock returns in `.result`. A nonzero result proves
//! both syscalls executed and returned through the normal SYSRET path
//! (not just the sys_exit unwind `prog_exit` exercises).

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_WRITE: u64 = 0;
const SYS_CLOCK: u64 = 3;
const FD_STDOUT: u64 = 1;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let message = b"prog_syscall: hello via sys_write\n";
    unsafe {
        common::syscall(SYS_WRITE, FD_STDOUT, message.as_ptr() as u64, message.len() as u64, 0);
    }
    let ticks = unsafe { common::syscall(SYS_CLOCK, 0, 0, 0, 0) };
    common::RESULT.store(ticks, Ordering::Relaxed);
}
