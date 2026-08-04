//! prog_fault: stores a magic value in `.result`, then deliberately
//! writes to virtual address 4 GiB (0x1_0000_0000), which no process's
//! page table maps. That store must trigger a page fault; the kernel's
//! page-fault handler is expected to kill the process instead of
//! panicking the whole system. The write is volatile so the optimizer
//! cannot delete it.
//!
//! 0x1_0000_0000 is outside the identity map (which covers 0..4 GiB),
//! so PML4 index 1 is not present — a guaranteed #PF.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const FAULT_ADDR: usize = 0x1_0000_0000;
const MAGIC: u64 = 0xFACE_FEED;

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    common::RESULT.store(MAGIC, Ordering::Relaxed);
    unsafe {
        core::ptr::write_volatile(FAULT_ADDR as *mut u64, MAGIC);
    }
}
