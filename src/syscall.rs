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
const EFER_NXE: u64 = 1 << 11;

pub(crate) const SYS_WRITE: u64 = 0;
pub(crate) const SYS_EXIT: u64 = 1;
pub(crate) const SYS_READ: u64 = 2;
pub(crate) const SYS_CLOCK: u64 = 3;
pub(crate) const SYS_SEND: u64 = 4;
pub(crate) const SYS_RECV: u64 = 5;
// Returned by write/read/send/recv when an argument (fd, or a pointer
// range the calling process doesn't own) is rejected.
const SYSCALL_ERROR: u64 = u64::MAX;

const FD_STDIN: u64 = 0;
const FD_STDOUT: u64 = 1;
const FD_STDERR: u64 = 2;

pub(crate) unsafe fn rdmsr(msr: u32) -> u64 {
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

pub(crate) unsafe fn wrmsr(msr: u32, value: u64) {
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
/// Sysretq needs the dispatch's return value (rax) and the user's rflags
/// (r11) to survive the `syscall_load_user_rsp` call — park them here
/// while it runs.
#[unsafe(no_mangle)]
static mut SAVED_RETVAL_SCRATCH: u64 = 0;
#[unsafe(no_mangle)]
static mut SAVED_RFLAGS_SCRATCH: u64 = 0;
/// rcx holds the user's rip for `sysretq`; the helper call clobbers it
/// too, so it gets parked here alongside rax/r11.
#[unsafe(no_mangle)]
static mut SAVED_RIP_SCRATCH: u64 = 0;
/// Set by `.Lsyscall_exit` when the exiting syscall was `exit_group`
/// (231) rather than plain `exit` (60) — the thread exit stub uses it to
/// decide whether to take the whole process down too.
#[unsafe(no_mangle)]
static mut EXIT_GROUP_FLAG: u64 = 0;

/// Sentinel `sys_execve` returns on success; the asm's exec-restart path
/// compares against it and iretq's into the new program (whose entry and
/// stack the handler stashed in `EXEC_ENTRY`/`EXEC_USER_RSP`) instead of
/// sysretq'ing to the old, now-freed instruction pointer.
#[unsafe(no_mangle)]
static EXECVE_RESTART_MAGIC: u64 = 0x4E584543_52455354;
/// The value `sys_execve` returns on success (the asm compares the
/// dispatch result against `EXECVE_RESTART_MAGIC`).
pub const EXECVE_RESTART_MAGIC_VALUE: u64 = 0x4E584543_52455354;
#[unsafe(no_mangle)]
static mut EXEC_ENTRY: u64 = 0;
#[unsafe(no_mangle)]
static mut EXEC_USER_RSP: u64 = 0;
/// Set by `sys_rt_sigreturn` (1) / `execve_into_current` (0) to tell the
/// exec-restart asm which consumer returned `EXECVE_RESTART_MAGIC`: a
/// sigreturn means `EXEC_USER_RSP` points at a full 20-qword register-
/// restore block (15 GP regs in pop order, then the iretq frame); an
/// execve means a fresh stack for a plain iretq.
#[unsafe(no_mangle)]
static mut SIGRETURN_RESTORE: u8 = 0;

pub fn set_exec_restart(entry: u64, user_rsp: u64) {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(EXEC_ENTRY), entry);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(EXEC_USER_RSP), user_rsp);
    }
}

/// Signals the exec-restart asm to pop the register-restore block that
/// `sys_rt_sigreturn` built on the user stack before iretq'ing.
pub fn set_sigreturn_restore() {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(SIGRETURN_RESTORE), 1);
    }
}

/// Clears the sigreturn-restore flag (every execve, so a later execve
/// after a sigreturn never takes the register-restore branch).
pub fn clear_sigreturn_restore() {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(SIGRETURN_RESTORE), 0);
    }
}

pub fn exit_group_flag() -> bool {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(EXIT_GROUP_FLAG)) != 0 }
}

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

/// Asm bridge: store the ring-3 rsp of the syscall that just entered —
/// per task, because the asm's global `SAVED_USER_RSP` is clobbered by
/// other tasks' syscalls while this one is suspended in a blocking
/// syscall (the `sysretq` return path must use this copy).
#[unsafe(no_mangle)]
extern "C" fn syscall_save_user_rsp(rsp: u64) {
    crate::task::save_current_user_rsp(rsp);
}

/// Asm bridge: the current task's saved ring-3 rsp (see
/// `syscall_save_user_rsp`).
#[unsafe(no_mangle)]
extern "C" fn syscall_load_user_rsp() -> u64 {
    crate::task::current_user_rsp()
}

/// Copies the asm's global `SAVED_USER_RSP` into the current task's
/// per-task slot. Called at the top of the generic dispatch (the
/// forkish path does the same inline); the `sysretq` return path then
/// reads the per-task copy, which survives other tasks' syscalls.
pub(crate) fn snapshot_saved_user_rsp() {
    let rsp = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SAVED_USER_RSP)) };
    crate::task::save_current_user_rsp(rsp);
}

/// Whether `num` should take the "never returns to ring 3" unwind path
/// (`.Lsyscall_exit` in `syscall_entry`) instead of an ordinary dispatch
/// call. What counts as "exit" depends on the calling process's ABI: the
/// native ABI only has `SYS_EXIT` (1); a Linux-ABI process's `exit` is 60
/// and `exit_group` is 231 (Linux syscall 1 is `write`). Called from the
/// asm before either dispatch table ever sees `num`, so a Linux process's
/// real `write(1, ...)` isn't mistaken for exiting.
#[unsafe(no_mangle)]
extern "C" fn is_exit_syscall(num: u64) -> u64 {
    let exit = if crate::task::current_process_abi() == Some(crate::process::Abi::Linux) {
        num == crate::linux_abi::SYS_EXIT || num == crate::linux_abi::SYS_EXIT_GROUP
    } else {
        num == SYS_EXIT
    };
    exit as u64
}

/// Asm bridge: is `num` fork(57), vfork(58) or clone(56)? Those need the
/// child to resume with the parent's full register state, so the asm
/// routes them to `sys_forkish` instead of the generic dispatch.
#[unsafe(no_mangle)]
extern "C" fn is_forkish_syscall(num: u64) -> u64 {
    match num {
        crate::linux_abi::SYS_CLONE | crate::linux_abi::SYS_FORK | crate::linux_abi::SYS_VFORK => num,
        _ => 0,
    }
}

/// Programs the asm's scratch syscall-stack pointer for the incoming
/// task (see `task::switch_to_next`): syscalls run on the task's own
/// kernel stack so a blocking syscall's saved context is per-task.
pub(crate) fn set_syscall_kernel_rsp(rsp: u64) {
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(SYSCALL_KERNEL_RSP), rsp);
    }
}

/// Handles fork/vfork/clone from the asm special path. `block` points at
/// 15 saved qwords. The asm's `sub rsp, 48` region holds the callee-saved
/// registers: [0..6] = r15 r14 r13 r12 rbp rbx. The 9 qwords pushed at
/// syscall entry sit above it, **in memory order** (the stack grows down,
/// so the last-pushed value is at the lowest address):
/// [6]=r9, [7]=r8, [8]=r10, [9]=rdx, [10]=rsi, [11]=rdi, [12]=num,
/// [13]=r11 (user rflags), [14]=rcx (user rip). The user rsp is read from
/// `SAVED_USER_RSP`. Creates the child task and returns its pid.
#[unsafe(no_mangle)]
extern "C" fn sys_forkish(num: u64, block: *const u64) -> u64 {
    let read = |i: usize| unsafe { core::ptr::read(block.add(i)) };
    // Reorder the stored [r15 r14 r13 r12 rbp rbx] into the frame order
    // context_switch pops: [rbx rbp r12 r13 r14 r15].
    let callee_saved = [read(5), read(4), read(3), read(2), read(1), read(0)];
    let user_rip = read(14);
    let user_rflags = read(13);
    let a1 = read(11);
    let a2 = read(10);
    let a3 = read(9);
    let a4 = read(8);
    let a5 = read(7);
    let a6 = read(6);
    let user_rsp = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SAVED_USER_RSP)) };
    crate::task::save_current_user_rsp(user_rsp);
    crate::process::handle_forkish(
        num, user_rip, user_rflags, user_rsp, a1, a2, a3, a4, a5, a6, callee_saved,
    )
    .unwrap_or(u64::MAX)
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
    # Whether `rax` (the syscall number) means "exit" depends on the
    # calling process's ABI (`is_exit_syscall` — native SYS_EXIT is 1;
    # Linux's is 60/231, since Linux syscall 1 is `write`), so this can't
    # be a bare immediate compare the way it used to be. Preserve every
    # argument register across the call (an ordinary C function, free to
    # clobber all of them) since we still need them after, whichever way
    # it decides.
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    # Phase 7: fork/clone/vfork need the child to resume with the *full*
    # user register state, so take them out of the generic dispatch path
    # before anything clobbers the values on the stack. (is_forkish is a
    # C call — it clobbers the caller-saved regs, but every user value we
    # need is saved on the stack: [rsp+0]=rcx, [rsp+8]=r11, [rsp+16]=rax,
    # [rsp+24..64]=rdi rsi rdx r10 r8 r9. rbx/rbp/r12-r15 survive as
    # callee-saved and still hold the user's originals.)
    mov rdi, rax
    call is_forkish_syscall
    test rax, rax
    jnz .Lsyscall_forkish
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    # Whether `rax` (the syscall number) means "exit" depends on the
    # calling process's ABI (`is_exit_syscall` — native SYS_EXIT is 1;
    # Linux's is 60/231, since Linux syscall 1 is `write`), so this can't
    # be a bare immediate compare the way it used to be. Preserve every
    # argument register across the call (an ordinary C function, free to
    # clobber all of them) since we still need them after, whichever way
    # it decides.
    push rax
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    mov rdi, rax
    call is_exit_syscall
    mov r11, rax
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    test r11, r11
    jnz .Lsyscall_exit

    # Real Linux's syscall ABI promises the caller that rdi/rsi/rdx/r10/
    # r8/r9 (like rbx/rbp/r12-r15, which this path never touches at all)
    # come back exactly as they went in — only rax (the return value)
    # and rcx/r11 (clobbered by the `syscall`/`sysretq` pair itself)
    # change. Real Linux's kernel entry saves the full register set into
    # `pt_regs` and restores it verbatim before `sysretq`, and real
    # glibc code relies on that: rseq registration (ld.so's TLS setup)
    # keeps `__rseq_offset` cached live in r8 *across* the `rseq(2)`
    # syscall and reuses it immediately after on the error path without
    # reloading it — caught by testing against a real dynamically-linked
    # binary, where that fallback write faulted on whatever garbage
    # `syscall_dispatch`'s own internal computation happened to leave in
    # r8, not on the original argument value at all. Save the six here,
    # since the shuffle below and `syscall_dispatch` itself (an ordinary
    # C call, free to clobber any of them) both need to reuse the
    # physical registers, and restore them right after — before this,
    # only the *shuffled* values downstream of the call, never the
    # user's originals, ever made it back past `sysretq`.
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9

    # SysV syscall args arrive in rdi/rsi/rdx/r10/r8/r9 (r10 instead of
    # rcx, which `syscall` clobbers); shuffle num+6 args into what
    # `extern "C" fn syscall_dispatch` expects — arg6 doesn't fit in a
    # register alongside num+arg1..arg5 (7 values, 6 GP registers per the
    # C calling convention), so it goes on the stack, pushed right before
    # `call` like a normal 7th integer argument (the callee's own
    # prologue reads it from there; this is all `call`'s caller has to
    # do for a stack argument — the compiler-generated callee handles
    # the rest). r11 is free to use as scratch here — its user value is
    # already saved on the stack above.
    push r9                 # arg6 (SysV's 7th integer arg goes on the stack)
    mov r11, rdx
    mov rdx, rsi            # arg2 = a2
    mov rsi, rdi            # arg1 = a1
    mov rdi, rax            # num
    mov rcx, r11            # arg3 = a3
    mov r11, r8             # r11 = a5 (r8 is about to be overwritten by arg4)
    mov r8, r10             # arg4 = a4
    mov r9, r11             # arg5 = a5
    call syscall_dispatch
    add rsp, 8              # pop arg6

    # Restore the user's original argument registers (see above) — rax
    # already holds `syscall_dispatch`'s real return value, which must
    # survive these pops untouched, so it's deliberately not among them.
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi

    # execve-restart check (Phase 7b): on success sys_execve replaced
    # the whole address space and parked an exit chain; it returns the
    # EXECVE_RESTART_MAGIC sentinel and expects us to iretq into the new
    # program (entry/rsp stashed by the handler) instead of sysretq'ing
    # to the old rip in the now-freed space.
    cmp rax, [rip + EXECVE_RESTART_MAGIC]
    je .Lsyscall_exec_restart

    pop r11
    pop rcx
    # The global SAVED_USER_RSP is clobbered by other tasks' syscalls
    # while this one was suspended in a blocking syscall (wait4/pipe
    # read/futex wait), so the return rsp comes from the task's own
    # saved copy — and rax/r11/rcx (the result, rflags and rip sysretq
    # needs) survive the helper call via the scratch slots.
    mov [rip + SAVED_RFLAGS_SCRATCH], r11
    mov [rip + SAVED_RETVAL_SCRATCH], rax
    mov [rip + SAVED_RIP_SCRATCH], rcx
    call syscall_load_user_rsp
    mov rsp, rax
    mov rax, [rip + SAVED_RETVAL_SCRATCH]
    mov r11, [rip + SAVED_RFLAGS_SCRATCH]
    mov rcx, [rip + SAVED_RIP_SCRATCH]
    sysretq
.Lsyscall_exec_restart:
    # Two consumers share this magic: sys_execve (fresh program — plain
    # iretq) and sys_rt_sigreturn (restore the full interrupted register
    # set). rt_sigreturn builds a 20-qword block on the user stack (the
    # 15 GP registers, then the iretq frame) and points EXEC_USER_RSP at
    # its bottom, flagging the switch via SIGRETURN_RESTORE.
    cmp byte ptr [rip + SIGRETURN_RESTORE], 1
    je .Lsyscall_sigreturn_restore
    # Iretq into the new program like enter_usermode does — the handler
    # already parked the exit chain and set ring3_kernel_rsp, so the new
    # program's own exit syscall unwinds cleanly to exit_self.
    mov rax, 0x33
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov rax, [rip + EXEC_USER_RSP]
    push 0x33            # ss
    push rax             # user rsp
    mov rax, 0x202       # rflags (IF set)
    push rax
    push 0x3B            # cs
    mov rax, [rip + EXEC_ENTRY]
    push rax             # rip
    iretq
.Lsyscall_sigreturn_restore:
    # The interrupted code resumes with every register exactly as the
    # timer ISR captured it. Deliberately *no* segment-register reloads
    # here (unlike the execve path, which iretq's into a fresh program):
    # `mov fs, ax` would reset the hidden FS base from the GDT, wiping
    # the TLS base the program set via arch_prctl — the next errno/TLS
    # access would fault at fs:0 (busybox `kill -CHLD` crash class 2).
    # The user's ds/es/fs/gs (flat 0x33 + the MSR fs base) survive the
    # syscall untouched, exactly as on the sysretq return paths.
    mov rsp, [rip + EXEC_USER_RSP]
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    iretq
.Lsyscall_exit:
    sti
    # rax still holds the syscall number here; remember whether it was
    # exit_group (231) — the exit stub (thread_exit_self) needs to know
    # whether to take the whole process down with the thread.
    cmp rax, 231
    sete al
    movzx eax, al
    mov [rip + EXIT_GROUP_FLAG], eax
    call ring3_load_return_rsp
    mov rsp, rax
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret

# Phase 7: fork/clone/vfork special path. Entry: rax = syscall number,
# rsp = kernel stack; the 9 user qwords saved at syscall entry sit at
# [rsp+0]=rcx (rip), [rsp+8]=r11 (rflags), [rsp+16]=num, [rsp+24..64]=
# rdi rsi rdx r10 r8 r9. rbx/rbp/r12-r15 still hold the user's originals
# (callee-saved through the C call). sys_forkish creates the child task
# and returns the child's pid; the parent restores its user state and
# sysretq's with the pid in rax. The child never returns here — it
# resumes via `child_resume` on its own fabricated frame.
.Lsyscall_forkish:
    # Make room *below* the 9-qword block (the stack grows down) and
    # store the callee-saved registers there — `push` would overwrite
    # the user block above.
    sub rsp, 48
    mov [rsp + 0], r15
    mov [rsp + 8], r14
    mov [rsp + 16], r13
    mov [rsp + 24], r12
    mov [rsp + 32], rbp
    mov [rsp + 40], rbx
    mov rdi, rax            # syscall number
    mov rsi, rsp            # pointer to the saved state block
    call sys_forkish
    # Parent path: rax = child pid. Restore user rcx/r11/rdi..r9 from the
    # block (rax is deliberately not reloaded — it carries the pid).
    # Block layout: [rsp+0..48]=r15 r14 r13 r12 rbp rbx, then the 9
    # syscall-entry qwords in memory order: [rsp+48]=r9, [rsp+56]=r8,
    # [rsp+64]=r10, [rsp+72]=rdx, [rsp+80]=rsi, [rsp+88]=rdi,
    # [rsp+96]=num, [rsp+104]=r11, [rsp+112]=rcx.
    mov rcx, [rsp + 112]
    mov r11, [rsp + 104]
    mov rdi, [rsp + 88]
    mov rsi, [rsp + 80]
    mov rdx, [rsp + 72]
    mov r10, [rsp + 64]
    mov r8,  [rsp + 56]
    mov r9,  [rsp + 48]
    add rsp, 120
    # Same per-task rsp treatment as the normal return path (this path
    # is synchronous so the global is still valid, but keeping one code
    # shape for both is cheaper than explaining the difference).
    mov [rip + SAVED_RFLAGS_SCRATCH], r11
    mov [rip + SAVED_RETVAL_SCRATCH], rax
    mov [rip + SAVED_RIP_SCRATCH], rcx
    call syscall_load_user_rsp
    mov rsp, rax
    mov rax, [rip + SAVED_RETVAL_SCRATCH]
    mov r11, [rip + SAVED_RFLAGS_SCRATCH]
    mov rcx, [rip + SAVED_RIP_SCRATCH]
    sysretq

# Resume point for a fork/clone child. The child task's fabricated
# kernel-stack frame (see task.rs spawn_child) is laid out so that after
# context_switch pops the 6 callee-saved slots and `ret`s here, the
# remaining qwords are: r9, r8, r10, rdx, rsi, rdi, rax, rcx (user rip),
# r11 (user rflags), then the user rsp. sysretq then drops into ring 3
# exactly where the parent's syscall returned, with rax = 0 (child).
.global child_resume
.type child_resume, @function
child_resume:
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop rax
    pop rcx
    pop r11
    pop rsp
    sysretq

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
    # Re-park the exit chain below the syscall-stack region: the chain
    # at this frame's depth gets overwritten by the deepest syscall
    # frames (fork_copy), which corrupted the exit's ret slot.
    call ring3_repark_chain
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

/// Validates that `[ptr, ptr+len)` belongs to the calling task and
/// returns its physical (kernel-identity) address. For a process this
/// must land entirely inside one of its own mappings (segments, stack, or
/// exit trampoline — see `process::translate`); a pointer into the
/// process's *deep-copied view of kernel memory* is not in `mappings` and
/// so is rejected. Without this check a process could pass a pointer at
/// its own copy of the kernel heap and have the kernel read or overwrite
/// arbitrary kernel memory on its behalf via a syscall. `run_demo` is the
/// only caller that isn't a process; its pointers are already kernel
/// addresses the kernel mapped itself, so those are trusted as-is.
pub(crate) fn resolve_user_buffer(ptr: u64, len: u64) -> Option<u64> {
    if crate::task::current_is_process() {
        crate::process::resolve_syscall_ptr(crate::task::current_pid(), ptr, len)
    } else if paging::is_identity_mapped(ptr, len) {
        Some(ptr)
    } else {
        None
    }
}

fn sys_write(fd: u64, ptr: u64, len: u64) -> u64 {
    if fd != FD_STDOUT && fd != FD_STDERR {
        return SYSCALL_ERROR;
    }
    let Some(phys) = resolve_user_buffer(ptr, len) else {
        return SYSCALL_ERROR;
    };
    let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, len as usize) };
    for &byte in bytes {
        crate::io::print(core::format_args!("{}", byte as char));
    }
    WRITE_COUNT.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    bytes.len() as u64
}

/// Drains whatever's already queued from the keyboard into `buf`, up to
/// `len` bytes. Never blocks: with 0 bytes pending this returns 0
/// immediately rather than waiting, since a real wait would need the
/// syscall handler to be preemptible (it currently isn't — see the module
/// docs on `SYSCALL_KERNEL_RSP`).
fn sys_read(fd: u64, ptr: u64, len: u64) -> u64 {
    if fd != FD_STDIN {
        return SYSCALL_ERROR;
    }
    let Some(phys) = resolve_user_buffer(ptr, len) else {
        return SYSCALL_ERROR;
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, len as usize) };
    let mut n = 0usize;
    while n < buf.len() {
        match crate::keyboard::next_event() {
            Some(crate::keyboard::Event::Char(c)) => {
                buf[n] = c as u8;
                n += 1;
            }
            Some(_) => {} // arrows, Ctrl+letter, etc. — dropped for this minimal syscall
            None => break,
        }
    }
    n as u64
}

/// Delivers `[ptr, ptr+len)` (from the *sender's* address space) to
/// `dest_pid`'s single-message inbox. See `task::deliver_message`: a
/// second `send` before the target reads the first overwrites it.
fn sys_send(dest_pid: u64, ptr: u64, len: u64) -> u64 {
    let Some(phys) = resolve_user_buffer(ptr, len) else {
        return SYSCALL_ERROR;
    };
    let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, len as usize) };
    if crate::task::deliver_message(dest_pid as usize, bytes) {
        0
    } else {
        SYSCALL_ERROR
    }
}

/// Takes the calling process's pending message, if any, copying up to
/// `maxlen` bytes into its buffer. Never blocks: with nothing pending
/// this returns 0 immediately (same reasoning as `sys_read`).
fn sys_recv(ptr: u64, maxlen: u64) -> u64 {
    let Some(phys) = resolve_user_buffer(ptr, maxlen) else {
        return SYSCALL_ERROR;
    };
    let Some(message) = crate::task::take_current_message() else {
        return 0;
    };
    let n = message.len().min(maxlen as usize);
    unsafe {
        core::ptr::copy_nonoverlapping(message.as_ptr(), phys as *mut u8, n);
    }
    n as u64
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64, arg6: u64) -> u64 {
    // Snapshot the ring-3 rsp into this task (see
    // `snapshot_saved_user_rsp`) — the native ABI's sysretq return reads
    // it back per task too, and the global `SAVED_USER_RSP` is
    // clobbered by other tasks' syscalls while this one blocks.
    snapshot_saved_user_rsp();
    // A Linux-ABI process (see `process::Abi`) dispatches through an
    // entirely separate syscall table/numbering/error convention
    // (`linux_abi.rs`) instead of the native one below. `run_demo`'s
    // caller isn't a process at all, so `current_process_abi` is `None`
    // for it and it always falls through to the native table, same as
    // today.
    if crate::task::current_process_abi() == Some(crate::process::Abi::Linux) {
        return crate::linux_abi::syscall_dispatch(num, arg1, arg2, arg3, arg4, arg5, arg6);
    }
    match num {
        SYS_WRITE => sys_write(arg1, arg2, arg3),
        SYS_READ => sys_read(arg1, arg2, arg3),
        SYS_CLOCK => crate::interrupts::ticks(),
        SYS_SEND => sys_send(arg1, arg2, arg3),
        SYS_RECV => sys_recv(arg1, arg2),
        _ => SYSCALL_ERROR,
    }
}

pub fn init() {
    // `enter_usermode` hardcodes 0x33/0x3B (user data/code | RPL 3) rather
    // than reading these constants from asm; keep them honest so a future
    // GDT layout change can't silently desync the two.
    debug_assert_eq!(crate::gdt::USER_DATA | 3, 0x33);
    debug_assert_eq!(crate::gdt::USER_CODE | 3, 0x3B);
    // `is_exit_syscall` hardcodes this value for the native ABI's half of
    // the exit check `syscall_entry` runs before either dispatch table
    // ever sees a syscall number.
    debug_assert_eq!(SYS_EXIT, 1);

    unsafe {
        let top = core::ptr::addr_of_mut!(SYSCALL_STACK) as usize + SYSCALL_STACK_SIZE;
        SYSCALL_KERNEL_RSP = (top & !0xF) as u64;

        // NXE (Phase 6): without it the CPU silently ignores the PTE's NX
        // bit and treats every present page as executable regardless —
        // `paging.rs` sets NX per-segment (R/W but not X) once this is on,
        // but the bit is meaningless dead weight in the page tables until
        // the CPU is told to actually enforce it.
        let efer = rdmsr(MSR_EFER);
        wrmsr(MSR_EFER, efer | EFER_SCE | EFER_NXE);

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

/// Hand-assembles a tiny ring-3 program that writes `message` to stdout in
/// one sys_write call and then exits, runs it, and blocks until it does.
/// Proves the ring 3 <-> ring 0 round trip (GDT selectors, SYSCALL/SYSRET,
/// iretq, multi-argument syscalls) end to end without needing an ELF
/// loader or process table.
pub fn run_demo(message: &[u8]) {
    let code_frame = pmm::frame_alloc().expect("usermode demo: code frame");
    let stack_frame = pmm::frame_alloc().expect("usermode demo: stack frame");

    // Fixed instruction length so the message (appended right after) has a
    // known, computable address: mov eax/edi/esi/edx (5 bytes each) +
    // syscall (2) + mov eax/edi (5 each) + syscall (2) + jmp $ (2).
    const INSTR_LEN: u32 = 5 * 4 + 2 + 5 * 2 + 2 + 2;
    let message_addr = code_frame as u32 + INSTR_LEN;

    let mut program = Vec::with_capacity(INSTR_LEN as usize + message.len());
    let mov_imm32 = |program: &mut Vec<u8>, opcode: u8, value: u32| {
        program.push(opcode);
        program.extend_from_slice(&value.to_le_bytes());
    };
    mov_imm32(&mut program, 0xB8, SYS_WRITE as u32); // mov eax, SYS_WRITE
    mov_imm32(&mut program, 0xBF, FD_STDOUT as u32); // mov edi, FD_STDOUT
    mov_imm32(&mut program, 0xBE, message_addr); // mov esi, <message addr>
    mov_imm32(&mut program, 0xBA, message.len() as u32); // mov edx, <len>
    program.push(0x0F);
    program.push(0x05); // syscall
    mov_imm32(&mut program, 0xB8, SYS_EXIT as u32); // mov eax, SYS_EXIT
    mov_imm32(&mut program, 0xBF, 0); // mov edi, 0
    program.push(0x0F);
    program.push(0x05); // syscall
    program.push(0xEB);
    program.push(0xFE); // jmp $ (safety net; sys_exit never returns)
    debug_assert_eq!(program.len(), INSTR_LEN as usize);
    program.extend_from_slice(message);

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
    // Restore the identity map before freeing — the split+unmap above
    // destroyed the boot map's 2 MiB coverage of these two pages, and the
    // PMM hands freed frames straight back out (see the paging selftest's
    // "normal contract" comment): a later allocation of the same frames
    // then faults the moment anyone writes to them, because their
    // identity mapping is gone. That is exactly the kernel-mode #PF the
    // Phase 10 selftest hit on its first reallocation of this frame
    // (writing to a freshly-allocated phys frame from the kernel map).
    let restored = paging::map_page(
        code_frame as u64,
        code_frame as u64,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE,
    ) && paging::map_page(
        stack_frame as u64,
        stack_frame as u64,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE,
    );
    assert!(restored, "usermode demo: restoring the identity map");
    pmm::frame_free(code_frame);
    pmm::frame_free(stack_frame);
}
