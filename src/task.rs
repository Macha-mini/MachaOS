//! Preemptive kernel-thread multitasking with per-task address spaces.
//!
//! Two kinds of schedulable entities exist:
//!
//! - **Kernel tasks**: ring-0-only functions sharing the kernel's own
//!   address space (the boot PML4). The three background counter tasks
//!   and the bootstrap "main" task are of this kind.
//! - **Processes**: ELF programs loaded into a *private* address space
//!   (see `process.rs`). Until the ring-3 work lands they still execute
//!   in ring 0, but they have their own PML4 and their own page
//!   mappings, and a page fault in one of them kills just that process
//!   instead of the whole system.
//!
//! Context switching relies on every interrupt on this kernel running on
//! whichever task's stack happened to be current when it fired (there is
//! no ring change, so the CPU never switches stacks itself). Calling
//! `context_switch` from inside `timer()` therefore only needs to save
//! the callee-saved registers and swap `rsp`; the rest of the interrupted
//! task's state (general registers, vector, RIP/CS/RFLAGS/RSP/SS) is
//! already sitting safely on that task's own stack as the ISR frame that
//! `isr_common` (see isr_stubs.asm) pushed, untouched by any of this.
//! Switching to a process additionally writes its PML4 to CR3 before the
//! stack swap; kernel stacks and kernel code are identity-mapped in every
//! address space (processes deep-copy the boot map), so both the old and
//! new `rsp` stay valid across the switch.
//!
//! Locking story: `init()` builds the table before interrupts are
//! enabled; `scheduler_tick()` runs from the (non-reentrant, because
//! interrupt gates disable further interrupts) timer handler and is the
//! only place that ever *switches* tasks; `push_task`/`remove_process`
//! mutate the table from normal context with interrupts disabled (they
//! are the only non-timer mutators); the various read-only accessors are
//! safe because on this single CPU a task's `state` is written only by
//! the task itself (or the ISR handling its fault) before it stops
//! running. The static-mut access through `addr_of_mut!` is the standard
//! Rust 2024 workaround for `static_mut_refs` and is sound under those
//! rules.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::interrupts;
use crate::paging;
use crate::process::{ExitInfo, Process};

core::arch::global_asm!(
    r#"
.section .text
.code64

.global context_switch
.type context_switch, @function
context_switch:
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret

.global task_trampoline
.type task_trampoline, @function
task_trampoline:
    sti
    pop rdi
    call rdi
.L_task_trampoline_hang:
    hlt
    jmp .L_task_trampoline_hang
"#
);

unsafe extern "C" {
    fn context_switch(old_rsp: *mut usize, new_rsp: usize);
    fn task_trampoline();
}

struct Context {
    rsp: usize,
}

struct Task {
    context: Context,
    // Keeps a spawned task's stack allocation alive; `None` for the
    // bootstrap task, which runs on the original kernel stack.
    _stack: Option<Vec<u8>>,
    name: &'static str,
    // PML4 the task runs with: the kernel's own map, or a process's
    // private address space.
    cr3: u64,
    // Top of this task's own kernel stack. Written into TSS.rsp0 whenever
    // this task becomes current: a ring 3 -> ring 0 transition (any
    // interrupt/exception firing while a process is in ring 3) always
    // switches SS:RSP to TSS.rsp0, so it must point at *this* task's
    // stack, not a shared one, or an interrupt hitting a different
    // process at the same fixed address would clobber it.
    kernel_stack_top: u64,
    // Resume point `syscall.rs`'s `.Lsyscall_exit` unwinds to when this
    // task's process calls `exit`: the kernel-side rsp `run_ring3` saved
    // just before its `iretq` into ring 3. Per-task (not a global) because
    // a second process entering ring 3 before the first one exits would
    // otherwise overwrite a shared slot.
    ring3_kernel_rsp: u64,
    // `Some` exactly for processes; carries the ELF entry, the frames
    // and mappings the process owns, and its exit state.
    process: Option<Process>,
    // `Some(owner pid)` for a CLONE_VM thread: the task shares the
    // owner's Process (address space + fd table) instead of owning one.
    // Per-thread ring-3 state (user_rsp/fs_base/exited) lives in the
    // fields below; everything else resolves through the owner.
    thread_of: Option<usize>,
    // Per-thread ring-3 stack pointer (the `stack` argument of clone(2)).
    thread_user_rsp: u64,
    // Per-thread TLS base (CLONE_SETTLS). See `scheduler_tick`'s
    // save/restore of FS_BASE below.
    fs_base: u64,
    // CLONE_CHILD_CLEARTID target: on thread exit the kernel writes 0
    // here and wakes the futex (what pthread_join waits on).
    child_tid: Option<u64>,
    // Set once a CLONE_VM thread has exited (its `process: None`, so
    // `Task::is_exited` can't rely on the Process's state alone).
    thread_exited: bool,
    // `Some(futex address)` while the task is blocked in FUTEX_WAIT;
    // the scheduler skips blocked tasks and `futex_wake` clears it.
    block_key: Option<u64>,
    /// The ring-3 rsp this task had when it entered its most recent
    /// syscall — the asm's global `SAVED_USER_RSP` is overwritten by
    /// every other task's syscalls while this one is suspended in a
    /// blocking syscall (wait4/pipe read/futex wait), so the return
    /// path must read this per-task copy instead.
    saved_user_rsp: u64,
}

impl Task {
    fn is_exited(&self) -> bool {
        self.thread_exited
            || self
                .process
                .as_ref()
                .is_some_and(|process| process.is_exited())
    }

    fn is_blocked(&self) -> bool {
        self.block_key.is_some()
    }
}

const STACK_SIZE: usize = 16 * 1024;
const QUANTUM_TICKS: u64 = 5; // 50ms at the 100Hz PIT rate

/// Bytes of headroom kept below `kernel_stack_top` when programming
/// TSS.RSP0 for a task (see `scheduler_tick`'s `set_kernel_stack` call).
/// Interrupt and syscall frames live in `[top - RESERVE, top)`. The
/// deepest syscall (the fork/clone path's `fork_copy`) reaches ~1 KiB
/// below the syscall entry, so 4 KiB gives it plenty of room, and the
/// exit chains re-parked *below* the reserve (see `repark_exit_chain`)
/// are never touched by any ring-0 trip.
const RING3_PARK_RESERVE: u64 = 4096;

/// IA32_FS_BASE — see `scheduler_tick`'s save/restore of
/// `process::Process::fs_base` around a task switch.
const MSR_FS_BASE: u32 = 0xC000_0100;

// Built once in `init()` before interrupts are enabled, then mutated from
// `scheduler_tick()` (timer-interrupt context) or from `push_task` /
// `remove_process` with interrupts disabled. See the module docs.
static mut TASKS: Vec<Task> = Vec::new();
static CURRENT: AtomicUsize = AtomicUsize::new(0);

const COUNTER_COUNT: usize = 3;
pub static COUNTERS: [AtomicU64; COUNTER_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

// Rust 2024 denies implicit `&`/`&mut` formation on `static mut` items
// (`static_mut_refs`); going through `addr_of_mut!`/`addr_of!` and
// dereferencing the raw pointer locally is the standard workaround, and
// is sound here precisely because `TASKS` is only ever touched from the
// contexts listed in the module docs.
fn tasks() -> &'static Vec<Task> {
    unsafe { &*core::ptr::addr_of!(TASKS) }
}

pub fn init() {
    tasks_mut().push(Task {
        context: Context { rsp: 0 }, // overwritten by the first switch away from this task
        _stack: None,
        name: "main",
        cr3: paging::kernel_pml4(),
        kernel_stack_top: crate::gdt::boot_stack_top(),
        ring3_kernel_rsp: 0,
        process: None,
        thread_of: None,
        thread_user_rsp: 0,
        fs_base: 0,
        child_tid: None,
        thread_exited: false,
        block_key: None,
        saved_user_rsp: 0,
    });
    spawn(counter_task_0, "bg-0");
    spawn(counter_task_1, "bg-1");
    spawn(counter_task_2, "bg-2");
}

fn spawn(entry: fn() -> !, name: &'static str) {
    push_task(entry as *const () as usize, name, paging::kernel_pml4(), None);
}

/// Registers a kernel-native background task (own stack, kernel address
/// space, no `Process`) — `wayland.rs`'s compositor loop uses this
/// instead of running as a Linux-ABI process, so it can freely `hlt`-wait
/// for a connection or more data the way `process::wait` does; a
/// syscall-driven process can't (see `syscall.rs`'s module docs on
/// `SFMASK` clearing IF for a syscall's whole duration).
pub fn spawn_kernel_task(entry: fn() -> !, name: &'static str) {
    spawn(entry, name);
}

/// Registers a new task running `entry` (a kernel function address) on a
/// fresh 16 KiB stack, returning its index ("pid"). Runs with interrupts
/// disabled around the table mutation. `cr3` selects the address space
/// the task runs in; `process` is `Some` exactly when `entry` is a
/// process trampoline.
pub fn spawn_process(entry: extern "C" fn() -> !, name: &'static str, cr3: u64, process: Process) -> usize {
    push_task(entry as usize, name, cr3, Some(process))
}

fn push_task(entry: usize, name: &'static str, cr3: u64, process: Option<Process>) -> usize {
    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let stack_top = stack.as_mut_ptr() as usize + STACK_SIZE;

    // `A` is 16-byte aligned; `X = A - 64` lands the fabricated frame such
    // that, after context_switch's 6 pops and its `ret` (into
    // task_trampoline), and after the trampoline's own `pop rdi` and
    // `call rdi` (into `entry`), rsp is 16-aligned minus 8 at each landing
    // point — exactly what the SysV ABI expects on function entry.
    let a = stack_top & !0xF;
    let x = a - 64;
    unsafe {
        let base = x as *mut usize;
        for i in 0..6 {
            *base.add(i) = 0; // r15, r14, r13, r12, rbp, rbx
        }
        let trampoline: unsafe extern "C" fn() = task_trampoline;
        *base.add(6) = trampoline as usize; // context_switch's `ret` target
        *base.add(7) = entry as usize; // trampoline's `pop rdi` target
    }

    interrupts::disable_interrupts();
    tasks_mut().push(Task {
        context: Context { rsp: x },
        _stack: Some(stack),
        name,
        cr3,
        kernel_stack_top: stack_top as u64,
        ring3_kernel_rsp: 0,
        process,
        thread_of: None,
        thread_user_rsp: 0,
        fs_base: 0,
        child_tid: None,
        thread_exited: false,
        block_key: None,
        saved_user_rsp: 0,
    });
    let pid = tasks().len() - 1;
    interrupts::enable_interrupts();
    pid
}

/// Called from `interrupts::timer()`, strictly *after* `pic::eoi(0)` — if
/// the EOI hasn't been sent yet and this switches away, IRQ0's
/// in-service bit stays set on the PIC and every interrupt on the system
/// stops dead until this exact task is scheduled again to send it, which
/// itself only happens via another timer interrupt. Sending EOI first
/// avoids that.
pub fn scheduler_tick() {
    if interrupts::ticks() % QUANTUM_TICKS != 0 {
        return;
    }
    switch_to_next()
}

/// Finds the next runnable task after `current` and switches to it,
/// saving the outgoing task's context. Shared by `scheduler_tick`
/// (quantum expiry, from the timer ISR) and `yield_blocked` (a blocking
/// syscall — FUTEX_WAIT — switching away immediately with IF already
/// cleared by SFMASK). Skips exited *and* blocked tasks.
fn switch_to_next() {
    let tasks = tasks_mut();
    let count = tasks.len();
    if count < 2 {
        return;
    }
    let current = CURRENT.load(Ordering::Relaxed);
    // Round-robin over the runnable tasks, skipping exited/blocked ones.
    let mut next = None;
    for step in 1..count {
        let candidate = (current + step) % count;
        if !tasks[candidate].is_exited() && !tasks[candidate].is_blocked() {
            next = Some(candidate);
            break;
        }
    }
    let Some(next) = next else { return }; // kernel tasks are always runnable

    // FS_BASE (the Linux ABI's TLS base — set via arch_prctl /
    // CLONE_SETTLS) is a single CPU-global MSR, not part of what
    // `context_switch` itself saves/restores in callee-saved registers.
    // Save the outgoing task's and restore the incoming one's before
    // switching, or a second task's TLS would silently clobber the
    // first's the moment they're interleaved.
    tasks[current].fs_base = unsafe { crate::syscall::rdmsr(MSR_FS_BASE) };
    CURRENT.store(next, Ordering::Relaxed);
    unsafe { crate::syscall::wrmsr(MSR_FS_BASE, tasks[next].fs_base) };

    if paging::read_cr3() != tasks[next].cr3 {
        paging::write_cr3(tasks[next].cr3);
    }
    crate::gdt::set_kernel_stack(tasks[next].kernel_stack_top - RING3_PARK_RESERVE);
    // Syscalls run on the *task's own* kernel stack (below the parked
    // ring-3 return chain — see RING3_PARK_RESERVE) instead of a shared
    // global stack, so a blocking syscall's saved context points at this
    // task's stack, not a scratch area another task's syscall would
    // clobber. Program the asm's scratch global for the incoming task.
    unsafe {
        crate::syscall::set_syscall_kernel_rsp(tasks[next].kernel_stack_top - RING3_PARK_RESERVE);
    }
    let new_rsp = tasks[next].context.rsp;
    let old_rsp_ptr = &mut tasks[current].context.rsp as *mut usize;
    unsafe {
        context_switch(old_rsp_ptr, new_rsp);
    }
}

/// Blocks the current task on `key` and switches away immediately; when
/// `wake_all(key)` later marks it runnable, this returns and the caller
/// (a FUTEX_WAIT handler) completes its syscall normally. Must be called
/// with interrupts disabled (the whole syscall runs with IF cleared by
/// SFMASK), from a syscall running on the task's own kernel stack.
pub fn yield_blocked(key: u64) {
    let tasks = tasks_mut();
    let current = CURRENT.load(Ordering::Relaxed);
    tasks[current].block_key = Some(key);
    drop(tasks);
    // Switch away without advancing the round-robin position beyond the
    // next runnable task; when woken, resume here and return.
    switch_to_next();
    // Woken: clear the block marker and let the futex handler re-check.
    tasks_mut()[CURRENT.load(Ordering::Relaxed)].block_key = None;
}

/// Cooperative round-robin yield for a polling syscall (wait4's wait
/// loop): switches to the next runnable task without blocking this one,
/// so it gets rescheduled on the next quantum and can re-check its
/// condition. Same stack requirements as `yield_blocked`.
pub fn yield_rr() {
    switch_to_next();
}

/// Marks every task blocked on `key` runnable again (FUTEX_WAKE). Wakes
/// up to `max` of them; returns how many were woken.
pub fn wake_all(key: u64, max: usize) -> usize {
    let tasks = tasks_mut();
    let mut woken = 0;
    for task in tasks.iter_mut() {
        if woken >= max {
            break;
        }
        if task.block_key == Some(key) {
            task.block_key = None;
            woken += 1;
        }
    }
    woken
}

unsafe extern "C" {
    fn child_resume();
}

/// Creates a child task that resumes in ring 3 at the parent's
/// fork/clone syscall return point: `user_rip`/`user_rflags`/`user_rsp`
/// plus the full register set captured at syscall entry, with `rax = 0`
/// (the "I am the child" signal). The fabricated kernel-stack frame
/// matches what `child_resume` (syscall.rs) pops: 6 callee-saved slots
/// consumed by `context_switch` itself, then r9..rdi, rax, rcx, r11, rsp
/// — see the frame layout below. A second fabricated chain below the
/// frame is what the child's own `exit` syscall unwinds to (the same
/// shape `enter_usermode` parks for a normally-spawned process):
/// `exit_stub` is `process::exit_self` for a fork child or
/// `process::thread_exit_self` for a CLONE_VM thread.
pub fn spawn_child(
    name: &'static str,
    cr3: u64,
    thread_of: Option<usize>,
    process: Option<Process>,
    user_rip: u64,
    user_rflags: u64,
    user_rsp: u64,
    user_args: [u64; 6], // rdi rsi rdx r10 r8 r9
    callee_saved: [u64; 6], // rbx rbp r12 r13 r14 r15
    fs_base: u64,
    child_tid: Option<u64>,
    exit_stub: usize,
) -> usize {
    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let stack_top = stack.as_mut_ptr() as usize + STACK_SIZE;
    // Exit chain (below the resume frame, higher addresses): 6 slots
    // `.Lsyscall_exit` pops into r15..rbx, then the stub it `ret`s to.
    let chain = stack_top - 56 - 8;
    unsafe {
        let chain_p = chain as *mut usize;
        for i in 0..6 {
            *chain_p.add(i) = 0;
        }
        *chain_p.add(6) = exit_stub;
    }
    // Resume frame: [rbx rbp r12 r13 r14 r15] [child_resume] [r9 r8 r10
    // rdx rsi rdi] [0] [rcx=rip] [r11=rflags] [rsp]. `context_switch`
    // pops r15 r14 r13 r12 rbp rbx (in that order, from the frame's
    // lowest address), so the callee-saved slots must be written
    // *reversed*: [0]=r15 .. [5]=rbx.
    let frame = chain - 136;
    unsafe {
        let f = frame as *mut usize;
        for (i, &v) in callee_saved.iter().rev().enumerate() {
            *f.add(i) = v as usize;
        }
        *f.add(6) = child_resume as usize;
        *f.add(7) = user_args[5] as usize; // r9
        *f.add(8) = user_args[4] as usize; // r8
        *f.add(9) = user_args[3] as usize; // r10
        *f.add(10) = user_args[2] as usize; // rdx
        *f.add(11) = user_args[1] as usize; // rsi
        *f.add(12) = user_args[0] as usize; // rdi
        *f.add(13) = 0; // rax = 0 (child)
        *f.add(14) = user_rip as usize; // rcx
        *f.add(15) = user_rflags as usize; // r11
        *f.add(16) = user_rsp as usize; // rsp
    }
    let kernel_stack_top = stack_top as u64;
    interrupts::disable_interrupts();
    tasks_mut().push(Task {
        context: Context { rsp: frame },
        _stack: Some(stack),
        name,
        cr3,
        kernel_stack_top,
        ring3_kernel_rsp: chain as u64,
        process,
        thread_of,
        thread_user_rsp: user_rsp,
        fs_base,
        child_tid,
        thread_exited: false,
        block_key: None,
        saved_user_rsp: 0,
    });
    let pid = tasks().len() - 1;
    interrupts::enable_interrupts();
    pid
}

pub fn task_count() -> usize {
    tasks().len()
}

pub fn task_name(index: usize) -> &'static str {
    tasks()[index].name
}

/// "running"/"exited" for a process task, `None` for kernel tasks.
pub fn process_state_label(index: usize) -> Option<&'static str> {
    tasks()[index]
        .process
        .as_ref()
        .map(Process::state_label)
}

pub fn current_is_process() -> bool {
    tasks()
        .get(CURRENT.load(Ordering::Relaxed))
        .is_some_and(|task| task.process.is_some())
}

/// Index of the currently running task ("pid" for a process).
pub fn current_pid() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

/// ELF entry address of the current task (only valid for processes).
pub fn current_process_entry() -> usize {
    tasks()[CURRENT.load(Ordering::Relaxed)]
        .process
        .as_ref()
        .expect("current task is not a process")
        .entry
}

/// Initial ring-3 stack pointer of the current task (only valid for
/// processes / CLONE_VM threads).
pub fn current_process_user_rsp() -> u64 {
    let task = &tasks()[CURRENT.load(Ordering::Relaxed)];
    if let Some(process) = &task.process {
        process.user_rsp
    } else {
        task.thread_user_rsp
    }
}

/// The current task's FS_BASE (TLS base) as stored at the last task
/// switch. `arch_prctl(ARCH_SET_FS)` and `clone(CLONE_SETTLS)` write it
/// via `set_current_fs_base`; `scheduler_tick` keeps it current.
pub fn current_fs_base() -> u64 {
    tasks()[CURRENT.load(Ordering::Relaxed)].fs_base
}

pub fn set_current_fs_base(base: u64) {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)].fs_base = base;
}

/// The current task's `CLONE_CHILD_CLEARTID` target, if it set one.
pub fn current_child_tid() -> Option<u64> {
    tasks()[CURRENT.load(Ordering::Relaxed)].child_tid
}

pub fn set_current_child_tid(addr: Option<u64>) {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)].child_tid = addr;
}

/// Which syscall table the current task's process should dispatch
/// through (`None` for a kernel task, which never issues a syscall in
/// the first place). Used by `syscall::syscall_dispatch` to route
/// between the native and Linux (`linux_abi.rs`) tables. For a CLONE_VM
/// thread this resolves through its owner process — the thread shares
/// the owner's ABI, and its `exit(60)` must take the same exit path.
pub fn current_process_abi() -> Option<crate::process::Abi> {
    let tasks = tasks();
    let current = CURRENT.load(Ordering::Relaxed);
    let owner = if tasks[current].process.is_some() {
        current
    } else {
        tasks[current].thread_of?
    };
    tasks.get(owner)?.process.as_ref().map(|p| p.abi)
}

/// The current task's CR3 (its own PML4), if it is a process or a
/// CLONE_VM thread — `None` for a kernel task. Used by
/// `process::handle_fault` to map a new page into the *faulting*
/// process's address space regardless of which task's stack the #PF
/// handler happens to be running on.
pub fn current_process_cr3() -> Option<u64> {
    let task = &tasks()[CURRENT.load(Ordering::Relaxed)];
    if task.process.is_some() || task.thread_of.is_some() {
        Some(task.cr3)
    } else {
        None
    }
}

/// The current task's name (diagnostics).
pub fn current_name() -> &'static str {
    tasks()[CURRENT.load(Ordering::Relaxed)].name
}

/// Re-parks the current task's exit chain at a fixed, safe location
/// *below* the syscall-stack region (`[top - RESERVE, top)`): the
/// original chain `enter_usermode` leaves at its own frame depth gets
/// overwritten by the deepest syscall frames (the fork path's
/// `fork_copy` reaches ~1 KiB below the syscall entry), which corrupted
/// the chain's ret slot and made exits jump into user-stack garbage.
/// Writing a fresh self-contained chain — 6 zero slots + the exit stub —
/// and pointing `ring3_kernel_rsp` at it keeps exits deterministic no
/// matter how deep the syscalls got.
pub fn repark_exit_chain(exit_stub: usize) {
    let current = CURRENT.load(Ordering::Relaxed);
    let top = tasks()[current].kernel_stack_top;
    // Two pages of headroom below the syscall-stack region: the deepest
    // syscall frames (the fork path's fork_copy builds a fresh Process,
    // and an execve loads a whole new image) reach well past 4 KiB, and
    // a clobbered chain's ret slot made exits jump into garbage (a
    // ring-0 #UD at the corrupted target).
    let chain = (top - RING3_PARK_RESERVE - 8192) as usize;
    unsafe {
        let chain_p = chain as *mut usize;
        for i in 0..6 {
            *chain_p.add(i) = 0;
        }
        *chain_p.add(6) = exit_stub;
    }
    tasks_mut()[current].ring3_kernel_rsp = chain as u64;
}

/// The current task's kernel stack top (diagnostics).
pub fn current_kernel_stack_top() -> u64 {
    tasks()[CURRENT.load(Ordering::Relaxed)].kernel_stack_top
}

/// Saves the current task's ring-3 rsp at syscall entry; the asm's
/// `sysretq`/exec-restart paths read it back via `current_user_rsp`
/// (the global `SAVED_USER_RSP` can't be trusted after a blocking
/// syscall — other tasks overwrite it).
pub fn save_current_user_rsp(rsp: u64) {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)].saved_user_rsp = rsp;
}

/// The current task's saved ring-3 rsp (see `save_current_user_rsp`).
pub fn current_user_rsp() -> u64 {
    tasks()[CURRENT.load(Ordering::Relaxed)].saved_user_rsp
}

/// Runs `f` on the current task's process — its own, or the CLONE_VM
/// owner's for a thread task. Used by `process::handle_fault` to grow the
/// ring-3 stack from inside the #PF handler, where the fault could
/// interrupt any point in the process's execution — the same
/// non-reentrancy argument `deliver_message`'s doc comment makes applies
/// here (interrupts are already disabled for the whole handler).
pub fn with_current_process_mut<R>(f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let tasks = tasks_mut();
    let current = CURRENT.load(Ordering::Relaxed);
    let owner = if tasks[current].process.is_some() {
        current
    } else {
        tasks[current].thread_of?
    };
    tasks.get_mut(owner)?.process.as_mut().map(f)
}

/// Runs `f` on the process at task index `pid` (threads resolve through
/// `thread_of` like `with_current_process_mut`). `None` if there is no
/// such task or it has no process. Used by signal delivery, which
/// targets an arbitrary pid from the killer's own syscall context.
pub fn with_process_mut<R>(pid: usize, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let tasks = tasks_mut();
    let task = tasks.get(pid)?;
    let owner = if task.process.is_some() { pid } else { task.thread_of? };
    tasks.get_mut(owner)?.process.as_mut().map(f)
}

/// The raw task table (pub for `execve`, which swaps the current task's
/// process out from under the running syscall; private otherwise).
pub fn tasks_mut() -> &'static mut Vec<Task> {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

pub fn replace_current_process(pid: usize, process: Process) -> Option<Process> {
    tasks_mut().get_mut(pid)?.process.replace(process)
}

/// Updates a task's CR3 (execve installs a fresh address space).
pub fn set_current_cr3(pid: usize, cr3: u64) {
    if let Some(task) = tasks_mut().get_mut(pid) {
        task.cr3 = cr3;
    }
}

/// Points a task's ring-3 return chain at a freshly parked chain
/// (execve parks one below the running syscall's frames).
pub fn set_current_ring3_kernel_rsp(pid: usize, rsp: u64) {
    if let Some(task) = tasks_mut().get_mut(pid) {
        task.ring3_kernel_rsp = rsp;
    }
}

/// Saves the current task's kernel-side resume point for when its process
/// exits (see `Task::ring3_kernel_rsp`). Called from `syscall::run_ring3`
/// right before it drops into ring 3.
pub fn set_current_ring3_return_rsp(rsp: u64) {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)].ring3_kernel_rsp = rsp;
}

/// Reads back the value `set_current_ring3_return_rsp` stored for the
/// current task. Called from `syscall.rs`'s `.Lsyscall_exit` path.
pub fn current_ring3_return_rsp() -> u64 {
    tasks()[CURRENT.load(Ordering::Relaxed)].ring3_kernel_rsp
}

/// The process's exit status once it has exited, `None` while it runs
/// (or when `pid` is not a process).
///
/// The fields are read with volatile loads: a `wait` loop spins between
/// `hlt`s waiting for the process to mark itself, and `halt()` is
/// `asm!("hlt", options(nomem))` — not a compiler memory barrier — so a
/// plain load could be hoisted out of the loop and observe a stale
/// "still running" forever.
pub fn process_exit_status(pid: usize) -> Option<ExitInfo> {
    let tasks = tasks();
    let Some(task) = tasks.get(pid) else {
        return None;
    };
    let Some(process) = task.process.as_ref() else {
        return None;
    };
    unsafe {
        if core::ptr::read_volatile(core::ptr::addr_of!(process.state)) != crate::process::ProcessState::Exited {
            return None;
        }
        // None once `wait4` reaped the zombie (see `reap_exit_status`).
        core::ptr::read_volatile(core::ptr::addr_of!(process.exit_info))
    }
}

/// The forking parent of the process at `pid` (see `Process::parent`).
pub fn process_parent(pid: usize) -> Option<usize> {
    tasks().get(pid)?.process.as_ref()?.parent()
}

/// True once the process at `pid` has exited (whether or not `wait4`
/// already reaped the zombie).
pub fn process_is_exited(pid: usize) -> bool {
    let tasks = tasks();
    let Some(task) = tasks.get(pid) else {
        return false;
    };
    let Some(process) = task.process.as_ref() else {
        return false;
    };
    unsafe {
        core::ptr::read_volatile(core::ptr::addr_of!(process.state)) == crate::process::ProcessState::Exited
    }
}

/// The process's segment mappings, used by `process::read_result` to
/// translate a virtual address to a physical frame.
pub fn process_mappings(pid: usize) -> Option<&'static [crate::process::Mapping]> {
    let tasks = tasks();
    let task = tasks.get(pid)?;
    // A CLONE_VM thread maps through its owner's address space.
    let owner = if task.process.is_some() {
        pid
    } else {
        task.thread_of?
    };
    tasks
        .get(owner)?
        .process
        .as_ref()
        .map(|p| p.mappings.as_slice())
}

/// Delivers `bytes` to `pid`'s single-message inbox (a mailbox, not a
/// queue: a second delivery before the first is read overwrites it).
/// Returns false if `pid` isn't a running process. Safe to call from
/// syscall context: interrupts are already disabled there for the whole
/// non-blocking, non-reentrant duration (see syscall.rs's module docs),
/// which is the same precondition `push_task`/`remove_process` rely on.
pub fn deliver_message(pid: usize, bytes: &[u8]) -> bool {
    let Some(task) = tasks_mut().get_mut(pid) else {
        return false;
    };
    let Some(process) = task.process.as_mut() else {
        return false;
    };
    if process.is_exited() {
        return false;
    }
    process.inbox = Some(bytes.to_vec());
    true
}

/// Takes (clearing) the current task's pending message, if any.
pub fn take_current_message() -> Option<alloc::vec::Vec<u8>> {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)]
        .process
        .as_mut()
        .and_then(|process| process.inbox.take())
}

/// Marks the current task as exited. For a CLONE_VM thread this only
/// flags the thread itself; for a process task it marks the whole
/// process *and* every thread sharing it (a dead owner's address space
/// is reaped soon; sibling threads must not keep referencing it). Keeps
/// the first exit info recorded (a page-fault kill must not be
/// overwritten by the exit stub that follows it).
pub fn mark_current_exited(info: ExitInfo) {
    let tasks = tasks_mut();
    let current = CURRENT.load(Ordering::Relaxed);
    if tasks[current].process.is_some() {
        for task in tasks.iter_mut() {
            if task.thread_of == Some(current) {
                task.thread_exited = true;
            }
        }
        if let Some(process) = tasks[current].process.as_mut() {
            process.mark_exited(info);
        }
    } else {
        tasks[current].thread_exited = true;
    }
}

/// `exit_group`: marks the whole process — the owner task, every
/// CLONE_VM thread sharing it, and the Process itself — exited, no
/// matter which task of the group called it.
pub fn mark_current_exited_group(info: ExitInfo) {
    let tasks = tasks_mut();
    let current = CURRENT.load(Ordering::Relaxed);
    let owner = if tasks[current].process.is_some() {
        current
    } else {
        tasks[current].thread_of.unwrap_or(current)
    };
    for task in tasks.iter_mut() {
        if task.thread_of == Some(owner) {
            task.thread_exited = true;
        }
    }
    if let Some(process) = tasks.get_mut(owner).and_then(|t| t.process.as_mut()) {
        process.mark_exited(info);
    }
}

/// True if the current task is a CLONE_VM thread (shares another task's
/// Process).
pub fn current_is_thread() -> bool {
    tasks()[CURRENT.load(Ordering::Relaxed)].process.is_none()
}

/// Detaches a task from the scheduler, returning its Process (if it owns
/// one) so the caller can free its frames. Refuses to remove the running
/// task. Uses ordered removal (`Vec::remove`), not `swap_remove`, so task
/// indices ("pids") stay stable — waitpid/child tracking and CLONE_VM
/// `thread_of` references rely on that. Call with interrupts disabled
/// (the caller owns the freed frames afterwards).
///
/// Removing a mid-table task shifts every later index down one; this
/// keeps the rest of the table consistent:
/// - the removed process's dead CLONE_VM threads are dropped first (from
///   the highest index down, so nothing shifts underneath) — an exited
///   owner's threads are flagged exited too (`mark_current_exited` /
///   `mark_current_exited_group`), and their `thread_of` link would
///   otherwise dangle onto the wrong task after the shift;
/// - every remaining task's stored index reference (`Process::parent`,
///   `Task::thread_of`) above the removed slot is decremented;
/// - processes whose `parent` was exactly the removed process are
///   orphaned (set to `None`) instead of left pointing at themselves or
///   an unrelated task.
pub fn remove_process(pid: usize) -> Option<Process> {
    let tasks = tasks_mut();
    if pid >= tasks.len() {
        return None;
    }
    let current = CURRENT.load(Ordering::Relaxed);
    if current == pid {
        return None;
    }
    // Drop the owner's dead threads first, highest index first, so each
    // `remove` only shifts indices we've already passed. A live thread of
    // an exited owner can't exist (exiting marks the whole group), so
    // this is pure garbage collection.
    let mut i = tasks.len();
    let mut removed_below_current = 0usize;
    while i > 0 {
        i -= 1;
        if i == pid {
            continue;
        }
        if tasks[i].thread_of == Some(pid) && tasks[i].thread_exited {
            tasks.remove(i);
            if i < current {
                removed_below_current += 1;
            }
        }
    }
    let task = tasks.remove(pid);
    if pid < current {
        removed_below_current += 1;
    }
    if removed_below_current > 0 {
        CURRENT.store(current - removed_below_current, Ordering::Relaxed);
    }
    // Re-point every remaining reference that crossed the removed slot.
    for t in tasks.iter_mut() {
        if let Some(process) = &mut t.process {
            match process.parent() {
                Some(par) if par == pid => process.set_parent(None),
                Some(par) if par > pid => process.set_parent(Some(par - 1)),
                _ => {}
            }
        }
        match t.thread_of {
            Some(own) if own == pid => t.thread_of = None, // defensive; threads of the owner were removed above
            Some(own) if own > pid => t.thread_of = Some(own - 1),
            _ => {}
        }
    }
    task.process
}

// Background demo tasks: bump a counter to prove preemptive
// multitasking works. They deliberately pace themselves with `hlt`
// (waking on the next timer tick) instead of spinning: an unconditional
// `loop { fetch_add }` in three tasks would burn 100% of one core and
// starve the desktop/main task for CPU, which makes the whole OS feel
// sluggish in QEMU. Pacing keeps the counters visibly advancing while
// leaving the CPU to the UI.
fn counter_task(index: usize) -> ! {
    loop {
        for _ in 0..100 {
            COUNTERS[index].fetch_add(1, Ordering::Relaxed);
        }
        interrupts::halt();
    }
}

fn counter_task_0() -> ! {
    counter_task(0)
}

fn counter_task_1() -> ! {
    counter_task(1)
}

fn counter_task_2() -> ! {
    counter_task(2)
}
