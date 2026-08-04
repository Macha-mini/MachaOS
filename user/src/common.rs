//! Shared pieces for the MachaOS user-mode test programs. Each binary
//! embeds its own copy of this file.
//!
//! `RESULT` is placed in the `.result` section, which the user linker
//! script pins to `PROC_RESULT_VIRT` (0x2FF0000). The kernel maps that
//! page into every process's address space and reads the value back
//! after the process exits — it is the only way these freestanding
//! programs can communicate a result to the kernel.

use core::sync::atomic::AtomicU64;

#[unsafe(no_mangle)]
#[unsafe(link_section = ".result")]
pub static RESULT: AtomicU64 = AtomicU64::new(0);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Raw `syscall` instruction wrapper matching the kernel's calling
/// convention (src/syscall.rs): rax = number, args in rdi/rsi/rdx/r10,
/// return value in rax. `syscall` itself clobbers rcx/r11 (used to save
/// the return RIP/RFLAGS), so both must be marked clobbered here even
/// though this wrapper doesn't use them directly.
#[inline(always)]
pub unsafe fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") num => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}
