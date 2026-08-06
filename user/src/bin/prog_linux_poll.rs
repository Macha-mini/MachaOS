//! prog_linux_poll: exercises Phase 9a's event-loop substrate — poll,
//! epoll, eventfd and timerfd (the primitives GUI clients and window
//! managers block on). Result = 0x3F when all six checks pass:
//!   bit0  poll sees POLLIN on a pipe with data
//!   bit1  poll on an empty pipe with timeout 0 returns 0
//!   bit2  poll timeout elapses (100 ms) with nothing ready
//!   bit3  eventfd write 5 / read 5 round trip
//!   bit4  epoll_wait reports the registered pipe fd (with its data)
//!   bit5  timerfd: not ready before expiry, ready after

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_PIPE2: u64 = 293;
const SYS_WRITE: u64 = 1;
const SYS_READ: u64 = 0;
const SYS_POLL: u64 = 7;
const SYS_EVENTFD2: u64 = 290;
const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_EPOLL_WAIT: u64 = 232;
const SYS_TIMERFD_CREATE: u64 = 283;
const SYS_TIMERFD_SETTIME: u64 = 286;
const SYS_EXIT_GROUP: u64 = 231;

const POLLIN: i16 = 0x001;
const EPOLLIN: u32 = 0x001;
const EPOLL_CTL_ADD: u64 = 1;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// The kernel's packed `struct epoll_event` (u32 events, u64 data — 12
/// bytes, data at offset 4).
#[repr(C, packed)]
struct EpollEvent {
    events: u32,
    data: u64,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;
    let msg = b"x";

    // 1. poll sees POLLIN on a pipe that has data.
    let mut fds = [0i32; 2];
    unsafe {
        common::syscall(SYS_PIPE2, &mut fds as *mut _ as u64, 0, 0, 0);
        common::syscall(SYS_WRITE, fds[1] as u64, msg.as_ptr() as u64, 1, 0);
    }
    let mut pfd = PollFd { fd: fds[0], events: POLLIN, revents: 0 };
    let r = unsafe { common::syscall(SYS_POLL, &mut pfd as *mut _ as u64, 1, 0, 0) };
    if r == 1 && pfd.revents & POLLIN != 0 {
        ok |= 1;
    }

    // 2. poll on an empty pipe with timeout 0 is not ready.
    let mut fds2 = [0i32; 2];
    unsafe {
        common::syscall(SYS_PIPE2, &mut fds2 as *mut _ as u64, 0, 0, 0);
    }
    let mut pfd2 = PollFd { fd: fds2[0], events: POLLIN, revents: 0 };
    let r = unsafe { common::syscall(SYS_POLL, &mut pfd2 as *mut _ as u64, 1, 0, 0) };
    if r == 0 {
        ok |= 2;
    }

    // 3. a 100 ms poll on the still-empty pipe times out with 0.
    let r = unsafe { common::syscall(SYS_POLL, &mut pfd2 as *mut _ as u64, 1, 100, 0) };
    if r == 0 {
        ok |= 4;
    }

    // 4. eventfd counter: write 5, read 5 back.
    let efd = unsafe { common::syscall(SYS_EVENTFD2, 0, 0, 0, 0) };
    let five = 5u64;
    let mut got = 0u64;
    let r = unsafe { common::syscall(SYS_WRITE, efd, &five as *const u64 as u64, 8, 0) };
    let r2 = unsafe { common::syscall(SYS_READ, efd, &mut got as *mut u64 as u64, 8, 0) };
    if r == 8 && r2 == 8 && got == 5 {
        ok |= 8;
    }

    // 5. epoll: register the first pipe's read end, wait for it.
    let ep = unsafe { common::syscall(SYS_EPOLL_CREATE1, 0, 0, 0, 0) };
    let ev = EpollEvent { events: EPOLLIN, data: 0x1234 };
    let r = unsafe {
        common::syscall(
            SYS_EPOLL_CTL,
            ep,
            EPOLL_CTL_ADD,
            fds[0] as u64,
            &ev as *const _ as u64,
        )
    };
    let mut out = [EpollEvent { events: 0, data: 0 }];
    let r2 = unsafe { common::syscall(SYS_EPOLL_WAIT, ep, &mut out as *mut _ as u64, 1, 0) };
    if r == 0 && r2 == 1 && out[0].events & EPOLLIN != 0 && out[0].data == 0x1234 {
        ok |= 16;
    }

    // 6. timerfd: not ready before expiry, ready after.
    let tf = unsafe { common::syscall(SYS_TIMERFD_CREATE, 0, 0, 0, 0) };
    let spec = [0i64, 0, 0, 300_000_000]; // interval (0,0) + value (0, 300 ms)
    unsafe {
        common::syscall(SYS_TIMERFD_SETTIME, tf, 0, &spec as *const _ as u64, 0);
    }
    let mut tpfd = PollFd { fd: tf as i32, events: POLLIN, revents: 0 };
    let r = unsafe { common::syscall(SYS_POLL, &mut tpfd as *mut _ as u64, 1, 0, 0) };
    if r == 0 {
        let r = unsafe { common::syscall(SYS_POLL, &mut tpfd as *mut _ as u64, 1, 600, 0) };
        if r == 1 && tpfd.revents & POLLIN != 0 {
            ok |= 32;
        }
    }

    common::RESULT.store(if ok == 63 { 0x3F } else { ok }, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT_GROUP, 0, 0, 0, 0);
    }
}
