//! Ring 3 entry/exit: the SYSCALL/SYSRET fast path (see gdt.rs for the GDT
//! layout SYSRET's STAR encoding forces) plus `run_ring3`, which jumps
//! into a ring-3 program via `iretq` and comes back out through a manual
//! stack-switch-and-`ret` the moment that program's `exit` syscall lands —
//! the same push-registers/save-rsp/.../pop-registers/ret shape `task.rs`
//! uses for `context_switch`, just entered via `iretq` instead of `call`.
//! `process.rs` calls it from every process task's trampoline to actually
//! run the process's code at CPL 3 instead of CPL 0.
//!
//! The kernel-side resume point (`ring3_kernel_rsp`) lives per-task in
//! `task.rs`, not in a global here: a process can sit in ring 3 for many
//! scheduler quanta before its exit syscall lands, and a second process
//! entering ring 3 in the meantime would otherwise stomp a shared slot.
//! `SYSCALL_KERNEL_RSP`/`SAVED_USER_RSP` stay global scratch because their
//! lifetime is just one syscall's handling, which never overlaps another
//! (SFMASK clears IF for its duration and this kernel is single-CPU).
//!
//! `run_demo` is a standalone regression test for just this layer (GDT
//! selectors, SYSCALL/SYSRET, iretq) that hand-assembles a tiny program
//! instead of going through the ELF loader — useful for isolating a bug
//! in this file from one in `process.rs`/`elf.rs`.

use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::paging;
use crate::pmm;

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1 << 0;

pub(crate) const SYS_WRITE: u64 = 0;
pub(crate) const SYS_EXIT: u64 = 1;
// Returned by `syscall_dispatch` to tell the asm stub to unwind back into
// `enter_usermode`'s caller instead of `sysretq`-ing to user mode.
const EXIT_SENTINEL: u64 = u64::MAX;

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") lo,
        out("edx") hi,
        options(nostack, preserves_flags)
    );
    ((hi as u64) << 32) | lo as u64
}

unsafe fn wrmsr(msr: u32, value: u64) {
    core::arch::asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") value as u32,
        in("edx") (value >> 32) as u32,
        options(nostack, preserves_flags)
    );
}

// Scratch kernel stack SYSCALL switches onto (SYSCALL, unlike an interrupt
// gate, does not switch stacks itself). Single-CPU only: SFMASK clears IF
// on entry so nothing can reenter this before the real per-task/per-CPU
// stack story lands with SMP.
const SYSCALL_STACK_SIZE: usize = 8192;
static mut SYSCALL_STACK: [u8; SYSCALL_STACK_SIZE] = [0; SYSCALL_STACK_SIZE];

#[unsafe(no_mangle)]
static mut SYSCALL_KERNEL_RSP: u64 = 0;
#[unsafe(no_mangle)]
static mut SAVED_USER_RSP: u64 = 0;

unsafe extern "C" {
    fn syscall_entry();
    fn enter_usermode(entry: u64, user_rsp: u64);
}

// Bridge from asm to the per-task storage in task.rs (see the module docs
// for why this can't just be a global here).
#[unsafe(no_mangle)]
extern "C" fn ring3_save_return_rsp(rsp: u64) {
    crate::task::set_current_ring3_return_rsp(rsp);
}

#[unsafe(no_mangle)]
extern "C" fn ring3_load_return_rsp() -> u64 {
    crate::task::current_ring3_return_rsp()
}

global_asm!(
    r#"
.section .text
.code64

.global syscall_entry
.type syscall_entry, @function
syscall_entry:
    mov [rip + SAVED_USER_RSP], rsp
    mov rsp, [rip + SYSCALL_KERNEL_RSP]
    push rcx
    push r11
    mov rsi, rdi
    mov rdi, rax
    call syscall_dispatch
    cmp rax, -1
    je .Lsyscall_exit
    pop r11
    pop rcx
    mov rsp, [rip + SAVED_USER_RSP]
    sysretq
.Lsyscall_exit:
    sti
    call ring3_load_return_rsp
    mov rsp, rax
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret

.global enter_usermode
.type enter_usermode, @function
enter_usermode:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov rbx, rdi
    mov r12, rsi
    mov rdi, rsp
    sub rsp, 8
    call ring3_save_return_rsp
    add rsp, 8
    mov rdi, rbx
    mov rsi, r12

    mov ax, 0x33
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax

    push 0x33
    push rsi
    mov rax, 0x202  # user RFLAGS: IF only, no VM/RF/ID pollution from the kernel
    push rax
    push 0x3B
    push rdi
    iretq
"#
);

/// Drops into ring 3 at `entry` with `user_rsp` as the initial stack
/// pointer, on the current task's own private address space. Behaves like
/// a normal (if unusually expensive) function call: it returns once the
/// ring-3 code's `exit` syscall lands, via the same manual stack-restore
/// `context_switch` uses, not via `sysretq`.
pub unsafe fn run_ring3(entry: u64, user_rsp: u64) {
    unsafe {
        enter_usermode(entry, user_rsp);
    }
}

// Observable proof (for selftest) that sys_write syscalls actually made it
// through ring 3 -> ring 0, independent of scraping printed output.
static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn write_count() -> u64 {
    WRITE_COUNT.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(num: u64, arg0: u64) -> u64 {
    match num {
        SYS_WRITE => {
            crate::io::print(core::format_args!("{}", arg0 as u8 as char));
            WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
            0
        }
        SYS_EXIT => EXIT_SENTINEL,
        _ => 0,
    }
}

pub fn init() {
    // `enter_usermode` hardcodes 0x33/0x3B (user data/code | RPL 3) rather
    // than reading these constants from asm; keep them honest so a future
    // GDT layout change can't silently desync the two.
    debug_assert_eq!(crate::gdt::USER_DATA | 3, 0x33);
    debug_assert_eq!(crate::gdt::USER_CODE | 3, 0x3B);

    unsafe {
        let top = core::ptr::addr_of_mut!(SYSCALL_STACK) as usize + SYSCALL_STACK_SIZE;
        SYSCALL_KERNEL_RSP = (top & !0xF) as u64;

        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | EFER_SCE);

        // STAR[47:32] = SYSCALL's CS (SS = CS+8); STAR[63:48] = SYSRET's
        // base (see gdt.rs::SYSRET_BASE for why the GDT is laid out the
        // way it is).
        let star = ((crate::gdt::SYSRET_BASE as u64) << 48) | ((crate::gdt::KERNEL_CODE as u64) << 32);
        wrmsr(MSR_STAR, star);
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);
        // Clear IF/TF/DF on entry; the handler runs with interrupts off
        // for its (short, non-blocking) duration and SYSRET restores the
        // caller's original RFLAGS from R11.
        wrmsr(MSR_SFMASK, 0x700);
    }
}

/// Hand-assembles a tiny ring-3 program that prints `message` one syscall
/// at a time and then exits, runs it, and blocks until it does. Proves the
/// ring 3 <-> ring 0 round trip (GDT selectors, SYSCALL/SYSRET, iretq) end
/// to end without needing an ELF loader or process table.
pub fn run_demo(message: &[u8]) {
    let code_frame = pmm::frame_alloc().expect("usermode demo: code frame");
    let stack_frame = pmm::frame_alloc().expect("usermode demo: stack frame");

    let mut program = Vec::new();
    for &byte in message {
        program.push(0xB8); // mov eax, imm32
        program.extend_from_slice(&(SYS_WRITE as u32).to_le_bytes());
        program.push(0xBF); // mov edi, imm32
        program.extend_from_slice(&(byte as u32).to_le_bytes());
        program.push(0x0F); // syscall
        program.push(0x05);
    }
    program.push(0xB8); // mov eax, SYS_EXIT
    program.extend_from_slice(&(SYS_EXIT as u32).to_le_bytes());
    program.push(0xBF); // mov edi, 0
    program.extend_from_slice(&0u32.to_le_bytes());
    program.push(0x0F); // syscall
    program.push(0x05);
    program.push(0xEB); // jmp $ (safety net; sys_exit never returns)
    program.push(0xFE);

    assert!(program.len() <= paging::PAGE_SIZE as usize, "usermode demo program too big");

    // Map before writing: a frame handed back by `pmm::frame_alloc` may be
    // one `paging` previously split and unmapped (e.g. by the paging
    // selftest just before this one), in which case its identity mapping
    // is gone even though the boot map covers the 2 MiB region around it.
    let code_mapped = paging::map_page(
        code_frame as u64,
        code_frame as u64,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER,
    );
    let stack_mapped = paging::map_page(
        stack_frame as u64,
        stack_frame as u64,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER,
    );
    assert!(code_mapped && stack_mapped, "usermode demo: mapping failed");

    unsafe {
        core::ptr::write_bytes(code_frame as *mut u8, 0x90, paging::PAGE_SIZE as usize); // pad with NOPs
        core::ptr::copy_nonoverlapping(program.as_ptr(), code_frame as *mut u8, program.len());
    }

    unsafe {
        enter_usermode(code_frame as u64, stack_frame as u64 + paging::PAGE_SIZE);
    }

    paging::unmap_page(code_frame as u64);
    paging::unmap_page(stack_frame as u64);
    pmm::frame_free(code_frame);
    pmm::frame_free(stack_frame);
}
