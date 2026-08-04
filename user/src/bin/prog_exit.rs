//! prog_exit: computes sum(1..=1000) = 500500, stores it in `.result`,
//! and returns normally. Returning is what makes the kernel's process
//! trampoline take the "clean exit" path (see `exit_self` in process.rs).

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut sum: u64 = 0;
    let mut i: u64 = 1;
    while i <= 1000 {
        sum += i;
        i += 1;
    }
    common::RESULT.store(sum, Ordering::Relaxed);
}
