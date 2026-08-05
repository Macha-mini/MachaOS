//! prog_linux_fork: exercises Phase 7b's fork + pipe2 + wait4. The
//! parent creates a pipe, forks; the child writes a message and exits;
//! the parent reads the pipe to EOF (the child's exit closes its write
//! end), verifies the bytes, and wait4's the child. Proves the
//! fork-copied address space and fd table (pipe ends shared by
//! refcount), real pipe blocking/EOF, and wait4 status reporting.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_PIPE2: u64 = 293;
const SYS_FORK: u64 = 57;
const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_WAIT4: u64 = 61;
const SYS_EXIT: u64 = 60;
const SYS_EXIT_GROUP: u64 = 231;

const MSG: &[u8] = b"hello from child\n";

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    let mut fds = [0i32; 2];
    let r = unsafe { common::syscall(SYS_PIPE2, fds.as_mut_ptr() as u64, 0, 0, 0) };
    if r != 0 {
        common::RESULT.store(0, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 1, 0, 0, 0);
        }
    }

    let pid = unsafe { common::syscall(SYS_FORK, 0, 0, 0, 0) } as i64;
    if pid == 0 {
        // Child: write the message, close the write end, exit.
        unsafe {
            common::syscall(SYS_CLOSE, fds[0] as u64, 0, 0, 0);
            common::syscall(SYS_WRITE, fds[1] as u64, MSG.as_ptr() as u64, MSG.len() as u64, 0);
            common::syscall(SYS_CLOSE, fds[1] as u64, 0, 0, 0);
            common::syscall(SYS_EXIT, 0, 0, 0, 0);
        }
    } else if pid < 0 {
        common::RESULT.store(0, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 2, 0, 0, 0);
        }
    }

    // Parent: close the write end, read until EOF.
    unsafe {
        common::syscall(SYS_CLOSE, fds[1] as u64, 0, 0, 0);
    }
    let mut buf = [0u8; 64];
    let mut total = 0usize;
    loop {
        let n = unsafe {
            common::syscall(
                SYS_READ,
                fds[0] as u64,
                buf.as_mut_ptr().add(total) as u64,
                (buf.len() - total) as u64,
                0,
            )
        };
        if n == 0 {
            break;
        }
        total += n as usize;
    }
    if &buf[..total] == MSG {
        ok |= 1;
    }

    // wait4: the child exited normally → status 0, WIFEXITED.
    let mut status = 0i32;
    let r = unsafe { common::syscall(SYS_WAIT4, pid as u64, &mut status as *mut i32 as u64, 0, 0) };
    if r == pid as u64 && status == 0 {
        ok |= 2;
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}
