//! prog_linux_fs: exercises Phase 9b's pseudo-filesystem and fs
//! syscalls — /dev/null, /dev/zero, /dev/urandom, /proc/self/stat,
//! mkdir, statx, rename and unlink. Result = 0xFF when all eight
//! checks pass.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_WRITE: u64 = 1;
const SYS_READ: u64 = 0;
const SYS_GETPID: u64 = 39;
const SYS_MKDIR: u64 = 83;
const SYS_RMDIR: u64 = 84;
const SYS_UNLINK: u64 = 87;
const SYS_RENAME: u64 = 82;
const SYS_STATX: u64 = 332;
const SYS_EXIT_GROUP: u64 = 231;

const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;

const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;

const AT_FDCWD: u64 = 0xFFFFFFFFFFFF_FF9C; // -100

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    // 1. /dev/null: writes succeed, reads hit EOF.
    let null_path = *b"/dev/null\0";
    let fd = unsafe { common::syscall(SYS_OPEN, &null_path as *const u8 as u64, O_WRONLY, 0, 0) } as i64;
    if fd >= 0 {
        let msg = b"discard me";
        let n = unsafe { common::syscall(SYS_WRITE, fd as u64, msg.as_ptr() as u64, msg.len() as u64, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if n == msg.len() as u64 {
            ok |= 1;
        }
    }
    let fd = unsafe { common::syscall(SYS_OPEN, &null_path as *const u8 as u64, O_RDONLY, 0, 0) } as i64;
    if fd >= 0 {
        let mut buf = [0u8; 16];
        let n = unsafe { common::syscall(SYS_READ, fd as u64, &mut buf as *mut u8 as u64, 16, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if n == 0 {
            ok |= 2;
        }
    }

    // 2. /dev/zero: reads return zeros.
    let zero_path = *b"/dev/zero\0";
    let fd = unsafe { common::syscall(SYS_OPEN, &zero_path as *const u8 as u64, O_RDONLY, 0, 0) } as i64;
    if fd >= 0 {
        let mut buf = [0xFFu8; 8];
        let n = unsafe { common::syscall(SYS_READ, fd as u64, &mut buf as *mut u8 as u64, 8, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if n == 8 && buf == [0u8; 8] {
            ok |= 4;
        }
    }

    // 3. /dev/urandom: reads return the requested length.
    let urand_path = *b"/dev/urandom\0";
    let fd = unsafe { common::syscall(SYS_OPEN, &urand_path as *const u8 as u64, O_RDONLY, 0, 0) } as i64;
    if fd >= 0 {
        let mut buf = [0u8; 16];
        let n = unsafe { common::syscall(SYS_READ, fd as u64, &mut buf as *mut u8 as u64, 16, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if n == 16 {
            ok |= 8;
        }
    }

    // 4. mkdir + statx sees a directory.
    let dir = *b"/users/macha/Documents/ltest\0";
    let r = unsafe { common::syscall(SYS_MKDIR, &dir as *const u8 as u64, 0o755, 0, 0) };
    let mut stx = [0u8; 256];
    let rs = unsafe {
        common::syscall6(
            SYS_STATX,
            AT_FDCWD,
            &dir as *const u8 as u64,
            0,
            0,
            &mut stx as *mut u8 as u64,
            0,
        )
    };
    if r == 0 && rs == 0 {
        let mode = u16::from_ne_bytes([stx[0x1c], stx[0x1d]]);
        if mode & S_IFDIR != 0 {
            ok |= 16;
        }
    }

    // 5. create a file in it, write, statx shows the size.
    let file = *b"/users/macha/Documents/ltest/data.txt\0";
    let fd = unsafe {
        common::syscall(
            SYS_OPEN,
            &file as *const u8 as u64,
            O_WRONLY | O_CREAT | O_TRUNC,
            0o644,
            0,
        )
    } as i64;
    if fd >= 0 {
        let msg = b"hello";
        let n = unsafe { common::syscall(SYS_WRITE, fd as u64, msg.as_ptr() as u64, 5, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if n == 5 {
            ok |= 32;
        }
    }
    let mut stx = [0u8; 256];
    let rs = unsafe {
        common::syscall6(
            SYS_STATX,
            AT_FDCWD,
            &file as *const u8 as u64,
            0,
            0,
            &mut stx as *mut u8 as u64,
            0,
        )
    };
    if rs == 0 {
        let size = u64::from_ne_bytes(stx[0x28..0x30].try_into().unwrap());
        let mode = u16::from_ne_bytes([stx[0x1c], stx[0x1d]]);
        if size == 5 && mode & S_IFREG != 0 {
            ok |= 64;
        }
    }

    // 6. rename, then read the content back under the new name.
    let newfile = *b"/users/macha/Documents/ltest/renamed.txt\0";
    let r = unsafe { common::syscall(SYS_RENAME, &file as *const u8 as u64, &newfile as *const u8 as u64, 0, 0) };
    if r == 0 {
        let fd = unsafe { common::syscall(SYS_OPEN, &newfile as *const u8 as u64, O_RDONLY, 0, 0) } as i64;
        if fd >= 0 {
            let mut buf = [0u8; 16];
            let n = unsafe { common::syscall(SYS_READ, fd as u64, &mut buf as *mut u8 as u64, 16, 0) };
            unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
            if n == 5 && &buf[..5] == b"hello" {
                ok |= 128;
            }
        }
    }

    // 7. unlink, then statx reports ENOENT.
    let r = unsafe { common::syscall(SYS_UNLINK, &newfile as *const u8 as u64, 0, 0, 0) };
    let mut stx = [0u8; 256];
    let rs = unsafe {
        common::syscall6(
            SYS_STATX,
            AT_FDCWD,
            &newfile as *const u8 as u64,
            0,
            0,
            &mut stx as *mut u8 as u64,
            0,
        )
    };
    if r == 0 && (rs as i64) == -2 {
        ok |= 256;
    }

    // 8. /proc/self/stat starts with our pid.
    let proc = *b"/proc/self/stat\0";
    let me = unsafe { common::syscall(SYS_GETPID, 0, 0, 0, 0) };
    let fd = unsafe { common::syscall(SYS_OPEN, &proc as *const u8 as u64, O_RDONLY, 0, 0) } as i64;
    if fd >= 0 {
        let mut buf = [0u8; 64];
        let n = unsafe { common::syscall(SYS_READ, fd as u64, &mut buf as *mut u8 as u64, 64, 0) };
        unsafe { common::syscall(SYS_CLOSE, fd as u64, 0, 0, 0) };
        if pid_matches(me, &buf[..n as usize]) {
            ok |= 512;
        }
    }

    // Clean up the test directory we created. Without this, a re-run of
    // the selftest (QEMU rebooting after a previous failure, or a later
    // `make test` reusing a dirty disk image) fails check 4 with EEXIST.
    unsafe {
        common::syscall(SYS_RMDIR, &dir as *const u8 as u64, 0, 0, 0);
    }

    common::RESULT.store(if ok == 1023 { 0xFF } else { ok }, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}

/// True if `buf` starts with the decimal digits of `pid` followed by a
/// space (the `pid (comm) ...` head of /proc/self/stat).
fn pid_matches(pid: u64, buf: &[u8]) -> bool {
    let mut digits = [0u8; 8];
    let mut n = 0usize;
    let mut v = pid;
    if v == 0 {
        digits[0] = 0;
        n = 1;
    }
    while v > 0 {
        digits[n] = (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    if buf.len() <= n || buf[n] != b' ' {
        return false;
    }
    for i in 0..n {
        if buf[n - 1 - i] != b'0' + digits[i] {
            return false;
        }
    }
    true
}
