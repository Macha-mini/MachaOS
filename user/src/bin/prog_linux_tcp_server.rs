//! prog_linux_tcp_server: exercises Phase 10's TCP *server* side of the
//! Linux ABI — `socket(AF_INET)`/`bind`/`listen`/`accept` — which the
//! existing Phase 10 selftest (busybox wget / nslookup, both clients)
//! never touched. The program binds port 8000, listens, prints a serial
//! marker, then blocks in `accept` until the host test driver (which
//! reaches it through QEMU user-net's `hostfwd=tcp::18000-:8000`)
//! connects. It echoes whatever the host sends and exits 0 — the
//! kernel's `wait4`-style wait in `shell.rs`'s selftest hook checks the
//! exit code, and the whole chain (real SYN → SYN-ACK → ACK handshake,
//! accept queue, per-connection data path) is verified end to end.
//!
//! Result = 0x55 when all five syscalls round-trip and the echo matched.
//!
//! No heap here (these freestanding test binaries don't set one up),
//! so the sockaddr is a fixed stack array.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_SOCKET: u64 = 41;
const SYS_BIND: u64 = 49;
const SYS_LISTEN: u64 = 50;
const SYS_ACCEPT: u64 = 43;
const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_EXIT_GROUP: u64 = 231;

const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const PORT: u16 = 8000;

/// `struct sockaddr_in` — family(u16) + port(u16, BE) + addr(u32, BE)
/// + 8 bytes of zero padding. The kernel only reads the first 8 bytes.
#[repr(C)]
struct SockAddrIn {
    family: u16,
    port_be: u16,
    addr_be: u32,
    zero: [u8; 8],
}

fn write_str(s: &[u8]) {
    unsafe {
        common::syscall(SYS_WRITE, 1, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    // 1. socket(AF_INET, SOCK_STREAM).
    let fd = unsafe { common::syscall(SYS_SOCKET, AF_INET, SOCK_STREAM, 0, 0) };
    if (fd as i64) < 0 {
        write_str(b"[TCP-SRV-FAIL] socket\n");
        common::RESULT.store(1, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 1, 0, 0, 0);
        }
    }

    // 2. bind(fd, {AF_INET, 8000, INADDR_ANY}).
    let addr = SockAddrIn {
        family: AF_INET as u16,
        port_be: PORT.to_be(),
        addr_be: 0, // INADDR_ANY
        zero: [0; 8],
    };
    let r = unsafe { common::syscall(SYS_BIND, fd, &addr as *const _ as u64, 16, 0) };
    if (r as i64) < 0 {
        write_str(b"[TCP-SRV-FAIL] bind\n");
        common::RESULT.store(2, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 2, 0, 0, 0);
        }
    }

    // 3. listen(fd, 4).
    let r = unsafe { common::syscall(SYS_LISTEN, fd, 4, 0, 0) };
    if (r as i64) < 0 {
        write_str(b"[TCP-SRV-FAIL] listen\n");
        common::RESULT.store(3, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 3, 0, 0, 0);
        }
    }

    // 4. Ready marker — the host driver watches the serial log for this
    //    before opening its connection.
    write_str(b"[TCP-SRV-READY] listening on port 8000\n");

    // 5. accept(fd, &addr, &len) — blocks until the host connects.
    let mut peer = SockAddrIn {
        family: 0,
        port_be: 0,
        addr_be: 0,
        zero: [0; 8],
    };
    let mut addrlen: u32 = 16;
    let conn = unsafe { common::syscall(SYS_ACCEPT, fd, &mut peer as *mut _ as u64, &mut addrlen as *mut _ as u64, 0) };
    if (conn as i64) < 0 {
        write_str(b"[TCP-SRV-FAIL] accept\n");
        common::RESULT.store(4, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 4, 0, 0, 0);
        }
    }

    // 6. Read what the host sent, echo it back.
    let mut buf = [0u8; 128];
    let n = unsafe { common::syscall(SYS_READ, conn, buf.as_mut_ptr() as u64, buf.len() as u64, 0) };
    if (n as i64) <= 0 {
        write_str(b"[TCP-SRV-FAIL] read\n");
        common::RESULT.store(5, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 5, 0, 0, 0);
        }
    }
    let got = &buf[..n as usize];
    // The driver sends "ping-from-host\n"; accept the exact match.
    let expected = b"ping-from-host\n";
    let echoed = got == expected;
    unsafe {
        common::syscall(SYS_WRITE, conn, got.as_ptr() as u64, got.len() as u64, 0);
    }
    unsafe {
        common::syscall(SYS_CLOSE, conn, 0, 0, 0);
    }

    if echoed {
        write_str(b"[TCP-SRV-DONE] echoed the host's ping\n");
        common::RESULT.store(0x55, Ordering::Relaxed);
        unsafe {
            common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
        }
    }
    common::RESULT.store(6, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 6, 0, 0, 0);
    }
}
