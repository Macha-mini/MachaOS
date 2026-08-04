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
/// return value in rax.
///
/// Only `rax`'s post-syscall value is defined (the result) — `syscall`
/// itself clobbers rcx/r11 (used to save the return RIP/RFLAGS), and the
/// kernel's own dispatch path (`syscall_dispatch`, an ordinary function
/// call) is free to clobber rdi/rsi/rdx/r10/r8/r9 too on its way there;
/// nothing on the kernel side restores them before `sysretq` (this
/// matches real Linux syscall semantics: only rax's result is
/// guaranteed). So every one of them must be marked clobbered here, not
/// just rcx/r11 — otherwise the compiler may keep some other, unrelated
/// live value in one of the argument registers across this call, on the
/// assumption `in(reg)` registers survive asm blocks unless told
/// otherwise, and have it silently overwritten. This didn't surface
/// until a caller made two `syscall` calls with a local variable live
/// across both (see prog_linux_syscall.rs) — a single call, or calls
/// whose results are used immediately, happened not to keep anything
/// live in exactly these registers.
#[inline(always)]
pub unsafe fn syscall(num: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") num => ret,
            inout("rdi") a1 => _,
            inout("rsi") a2 => _,
            inout("rdx") a3 => _,
            inout("r10") a4 => _,
            out("rcx") _,
            out("r11") _,
            out("r8") _,
            out("r9") _,
            options(nostack),
        );
    }
    ret
}
