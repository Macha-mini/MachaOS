//! prog_ipc: receives one message via sys_recv and stores its first byte
//! in `.result`. The kernel delivers the message (see the selftest)
//! before this process is ever scheduled, so a single non-blocking
//! sys_recv call is enough to observe it — sys_recv never blocks (see
//! src/syscall.rs), so a real client would poll it.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_RECV: u64 = 5;
/// Stored when sys_recv found nothing pending, distinguishing "no
/// message" from any real (0..=255) message byte.
const NO_MESSAGE: u64 = 0xDEAD;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut buf = [0u8; 8];
    let n = unsafe { common::syscall(SYS_RECV, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0) };
    // sys_recv returns 0 for "nothing pending" and u64::MAX for "rejected
    // pointer" — neither means a byte actually landed in `buf`, so only a
    // count within (0, buf.len()] is real.
    let result = if n > 0 && n <= buf.len() as u64 { buf[0] as u64 } else { NO_MESSAGE };
    common::RESULT.store(result, Ordering::Relaxed);
}
