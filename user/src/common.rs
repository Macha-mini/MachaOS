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
