//! prog_linux_wayland_client: a real Linux-ABI process exercising Phase
//! 5's new syscalls end to end — `memfd_create`/`ftruncate`/`mmap`
//! (`MAP_SHARED`), `socket`/`connect`, and `write`/`sendmsg` (the latter
//! for the one message that needs to pass its memfd via `SCM_RIGHTS`) —
//! against `wayland.rs`'s kernel-native compositor task. Draws a solid
//! 8x8 test-color square into its shared memory and walks through the
//! hand-designed (not wire-compatible — see `wayland.rs`'s module docs)
//! handshake that ends with the compositor blitting those exact pixels
//! onto MachaOS's real framebuffer. `shell.rs`'s selftest hook spawns
//! this and then checks the real framebuffer for the color, which is
//! the only way to know the whole chain (real shared memory, real fd
//! passing, real socket IPC between two independently scheduled tasks)
//! actually worked end to end rather than each piece merely returning
//! success codes.
//!
//! No heap here (these freestanding test binaries don't set one up —
//! see the other `prog_linux_*` programs), so every message is built by
//! hand into a fixed-size stack array rather than anything `alloc`-based.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_WRITE: u64 = 1;
const SYS_READ: u64 = 0;
const SYS_CLOSE: u64 = 3;
const SYS_MMAP: u64 = 9;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_SENDMSG: u64 = 46;
const SYS_FTRUNCATE: u64 = 77;
const SYS_EXIT: u64 = 60;
const SYS_MEMFD_CREATE: u64 = 319;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 0x01;

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;

const EAGAIN: u64 = (-11i64) as u64;

const WIDTH: u32 = 8;
const HEIGHT: u32 = 8;
const STRIDE: u32 = WIDTH * 4;
const POOL_SIZE: u32 = STRIDE * HEIGHT;
/// Deliberately not a color anything else on the desktop would paint —
/// `shell.rs`'s selftest hook looks for exactly this in the framebuffer.
const TEST_COLOR: u32 = 0x00_FF10_C0;

const SOCKET_PATH: &[u8] = b"machaos-wayland-0";

fn write_header(buf: &mut [u8], object_id: u32, opcode: u16, size: u16) {
    buf[0..4].copy_from_slice(&object_id.to_ne_bytes());
    let opcode_and_size = (opcode as u32) | ((size as u32) << 16);
    buf[4..8].copy_from_slice(&opcode_and_size.to_ne_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

/// Sends `buf` on `sockfd` via a plain `write()` — every handshake
/// message except `create_pool`, which alone needs `sendmsg` to carry
/// its fd.
fn send_plain(sockfd: u64, buf: &[u8]) -> u64 {
    unsafe { common::syscall(SYS_WRITE, sockfd, buf.as_ptr() as u64, buf.len() as u64, 0) }
}

/// Reads at least `want` bytes off `sockfd` into `buf`, retrying past
/// `-EAGAIN` (see `socket.rs`'s module docs: these syscalls never block
/// kernel-side, so a client that wants to wait has to poll — between
/// syscalls, in ring 3, interrupts are on, so the scheduler still gets
/// to run the compositor task while this spins).
fn recv_at_least(sockfd: u64, buf: &mut [u8], want: usize) -> usize {
    let mut got = 0usize;
    loop {
        let r = unsafe { common::syscall(SYS_READ, sockfd, buf[got..].as_mut_ptr() as u64, (buf.len() - got) as u64, 0) };
        if r == EAGAIN {
            continue;
        }
        got += r as usize;
        if got >= want {
            return got;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    // 1. memfd_create + ftruncate + mmap(MAP_SHARED) — the shared pixel
    //    buffer the compositor will read straight out of.
    let memfd = unsafe { common::syscall(SYS_MEMFD_CREATE, 0, 0, 0, 0) };
    if (memfd as i64) >= 0 {
        ok |= 1 << 0;
    }
    let r = unsafe { common::syscall(SYS_FTRUNCATE, memfd, POOL_SIZE as u64, 0, 0) };
    if r == 0 {
        ok |= 1 << 1;
    }
    let ptr = unsafe {
        common::syscall6(SYS_MMAP, 0, POOL_SIZE as u64, PROT_READ | PROT_WRITE, MAP_SHARED, memfd, 0)
    };
    if (ptr as i64) > 0 {
        ok |= 1 << 2;
    }

    // Paint the whole pool the test color.
    let pixels = ptr as *mut u32;
    for i in 0..(WIDTH * HEIGHT) as usize {
        unsafe { pixels.add(i).write(TEST_COLOR) };
    }

    // 2. socket + connect to the compositor's well-known path.
    let sockfd = unsafe { common::syscall(SYS_SOCKET, AF_UNIX, SOCK_STREAM, 0, 0) };
    if (sockfd as i64) >= 0 {
        ok |= 1 << 3;
    }
    let mut addr = [0u8; 110]; // sa_family: u16 + sun_path: [u8; 108]
    addr[0] = AF_UNIX as u8;
    addr[1] = 0;
    addr[2..2 + SOCKET_PATH.len()].copy_from_slice(SOCKET_PATH);
    let addrlen = 2 + SOCKET_PATH.len() as u64 + 1;
    let r = unsafe { common::syscall(SYS_CONNECT, sockfd, addr.as_ptr() as u64, addrlen, 0) };
    if r == 0 {
        ok |= 1 << 4;
    }

    // 3. wl_display.get_registry(registry_id = 2)
    let mut msg = [0u8; 12];
    write_header(&mut msg, 1, 1, 12);
    write_u32(&mut msg, 8, 2);
    send_plain(sockfd, &msg);

    // 4. Drain the two `global` events the compositor sends back
    //    unconditionally (wl_shm then wl_compositor — see
    //    `wayland.rs::client_session`) — 26 + 33 = 59 bytes total.
    //    Their content isn't parsed: this client and the compositor were
    //    written together and agree on the fixed names (1 = wl_shm,
    //    2 = wl_compositor) out of band, same as the rest of this
    //    hand-scripted handshake — see `wayland.rs`'s module docs.
    let mut scratch = [0u8; 96];
    recv_at_least(sockfd, &mut scratch, 59);

    // 5. wl_registry.bind(name=1 /* wl_shm */, id=3)
    let mut msg = [0u8; 16];
    write_header(&mut msg, 2, 1, 16);
    write_u32(&mut msg, 8, 1);
    write_u32(&mut msg, 12, 3);
    send_plain(sockfd, &msg);

    // 6. wl_shm.create_pool(id=4, size=POOL_SIZE) — via sendmsg, since
    //    this is the one message that has to carry `memfd` in its
    //    ancillary data.
    let mut body = [0u8; 16];
    write_header(&mut body, 3, 0, 16);
    write_u32(&mut body, 8, 4);
    write_u32(&mut body, 12, POOL_SIZE);

    let iov = [body.as_ptr() as u64, body.len() as u64];
    let mut cmsg = [0u8; 20];
    cmsg[0..8].copy_from_slice(&20u64.to_ne_bytes()); // cmsg_len
    cmsg[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    cmsg[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    cmsg[16..20].copy_from_slice(&(memfd as u32).to_ne_bytes());

    let mut msghdr = [0u8; 56];
    msghdr[16..24].copy_from_slice(&(iov.as_ptr() as u64).to_ne_bytes());
    msghdr[24..32].copy_from_slice(&1u64.to_ne_bytes()); // msg_iovlen
    msghdr[32..40].copy_from_slice(&(cmsg.as_ptr() as u64).to_ne_bytes());
    msghdr[40..48].copy_from_slice(&(cmsg.len() as u64).to_ne_bytes());
    let r = unsafe { common::syscall(SYS_SENDMSG, sockfd, msghdr.as_ptr() as u64, 0, 0) };
    if r == body.len() as u64 {
        ok |= 1 << 5;
    }

    // 7. wl_shm_pool.create_buffer(id=5, offset=0, w=8, h=8, stride=32, format=0)
    let mut msg = [0u8; 32];
    write_header(&mut msg, 4, 0, 32);
    write_u32(&mut msg, 8, 5);
    write_u32(&mut msg, 12, 0);
    write_u32(&mut msg, 16, WIDTH);
    write_u32(&mut msg, 20, HEIGHT);
    write_u32(&mut msg, 24, STRIDE);
    write_u32(&mut msg, 28, 0);
    send_plain(sockfd, &msg);

    // 8. wl_registry.bind(name=2 /* wl_compositor */, id=6)
    let mut msg = [0u8; 16];
    write_header(&mut msg, 2, 1, 16);
    write_u32(&mut msg, 8, 2);
    write_u32(&mut msg, 12, 6);
    send_plain(sockfd, &msg);

    // 9. wl_compositor.create_surface(id=7)
    let mut msg = [0u8; 12];
    write_header(&mut msg, 6, 0, 12);
    write_u32(&mut msg, 8, 7);
    send_plain(sockfd, &msg);

    // 10. wl_surface.attach(buffer=5, x=0, y=0)
    let mut msg = [0u8; 20];
    write_header(&mut msg, 7, 1, 20);
    write_u32(&mut msg, 8, 5);
    write_u32(&mut msg, 12, 0);
    write_u32(&mut msg, 16, 0);
    send_plain(sockfd, &msg);

    // 11. wl_surface.commit()
    let mut msg = [0u8; 8];
    write_header(&mut msg, 7, 6, 8);
    let r = send_plain(sockfd, &msg);
    if r == msg.len() as u64 {
        ok |= 1 << 6;
    }

    unsafe {
        common::syscall(SYS_CLOSE, sockfd, 0, 0, 0);
        common::syscall(SYS_CLOSE, memfd, 0, 0, 0);
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT, 0, 0, 0, 0);
    }
}
