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
}

impl Task {
    fn is_exited(&self) -> bool {
        self.process
            .as_ref()
            .is_some_and(|process| process.is_exited())
    }
}

const STACK_SIZE: usize = 16 * 1024;
const QUANTUM_TICKS: u64 = 5; // 50ms at the 100Hz PIT rate

/// Bytes of headroom kept below `kernel_stack_top` when programming
/// TSS.RSP0 for a task (see `scheduler_tick`'s `set_kernel_stack` call).
///
/// A process's `enter_usermode` (syscall.rs) "parks" a return chain —
/// `process_entry_trampoline` -> `run_ring3` -> `enter_usermode`'s own
/// 6 pushed registers, plus each frame's own return address — on this
/// same 16 KiB buffer, at whatever depth that short call chain reaches,
/// then does its `iretq` into ring 3. That parked chain isn't touched
/// again until the process's `exit` syscall unwinds it (`.Lsyscall_exit`
/// in syscall.rs). But every *other* trip through ring 0 while the
/// process runs — any interrupt or exception, not just ones that kill
/// the process — gets its stack frame from TSS.RSP0, which if pointed
/// at the literal top would land *above* the parked chain and grow
/// straight through it. A shallow handler (the timer tick) or one that
/// never returns to ring 3 (`kill_current`, which redirects rip to
/// `exit_self` instead of resuming) never surfaced this. A #PF that
/// resolves and resumes ring 3 — `process::handle_fault`'s stack-growth
/// path — calls deep enough (`pmm::alloc_contiguous`, `paging::map_range_in`)
/// to overwrite it, corrupting the very state `exit` later needs,
/// which showed up as `exit`'s `ret` landing on garbage. This reserve
/// keeps every such trip through ring 0 confined below the parked chain
/// instead. Comfortably covers that chain's actual depth (a handful of
/// stack frames, well under 100 bytes) with room to spare.
const RING3_PARK_RESERVE: u64 = 1024;

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
fn tasks_mut() -> &'static mut Vec<Task> {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

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
    });
    spawn(counter_task_0, "bg-0");
    spawn(counter_task_1, "bg-1");
    spawn(counter_task_2, "bg-2");
}

fn spawn(entry: fn() -> !, name: &'static str) {
    push_task(entry as *const () as usize, name, paging::kernel_pml4(), None);
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
    let tasks = tasks_mut();
    let count = tasks.len();
    if count < 2 {
        return;
    }
    let current = CURRENT.load(Ordering::Relaxed);
    // Round-robin over the runnable tasks, skipping exited processes.
    let mut next = None;
    for step in 1..count {
        let candidate = (current + step) % count;
        if !tasks[candidate].is_exited() {
            next = Some(candidate);
            break;
        }
    }
    let Some(next) = next else { return }; // kernel tasks are always runnable
    CURRENT.store(next, Ordering::Relaxed);

    if paging::read_cr3() != tasks[next].cr3 {
        paging::write_cr3(tasks[next].cr3);
    }
    crate::gdt::set_kernel_stack(tasks[next].kernel_stack_top - RING3_PARK_RESERVE);
    let new_rsp = tasks[next].context.rsp;
    let old_rsp_ptr = &mut tasks[current].context.rsp as *mut usize;
    unsafe {
        context_switch(old_rsp_ptr, new_rsp);
    }
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
/// processes).
pub fn current_process_user_rsp() -> u64 {
    tasks()[CURRENT.load(Ordering::Relaxed)]
        .process
        .as_ref()
        .expect("current task is not a process")
        .user_rsp
}

/// Which syscall table the current task's process should dispatch
/// through (`None` for a kernel task, which never issues a syscall in
/// the first place). Used by `syscall::syscall_dispatch` to route
/// between the native and Linux (`linux_abi.rs`) tables.
pub fn current_process_abi() -> Option<crate::process::Abi> {
    tasks()[CURRENT.load(Ordering::Relaxed)].process.as_ref().map(|p| p.abi)
}

/// The current task's CR3 (its own PML4), if it is a process — `None` for
/// a kernel task. Used by `process::handle_fault` to map a new page into
/// the *faulting* process's address space regardless of which task's
/// stack the #PF handler happens to be running on.
pub fn current_process_cr3() -> Option<u64> {
    let task = &tasks()[CURRENT.load(Ordering::Relaxed)];
    task.process.as_ref().map(|_| task.cr3)
}

/// Runs `f` on the current task's process, if it is one. Used by
/// `process::handle_fault` to grow the ring-3 stack from inside the #PF
/// handler, where the fault could interrupt any point in the process's
/// execution — the same non-reentrancy argument `deliver_message`'s doc
/// comment makes applies here (interrupts are already disabled for the
/// whole handler).
pub fn with_current_process_mut<R>(f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    tasks_mut()[CURRENT.load(Ordering::Relaxed)]
        .process
        .as_mut()
        .map(f)
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
        Some(core::ptr::read_volatile(core::ptr::addr_of!(process.exit_info)).unwrap())
    }
}

/// The process's segment mappings, used by `process::read_result` to
/// translate a virtual address to a physical frame.
pub fn process_mappings(pid: usize) -> Option<&'static [crate::process::Mapping]> {
    tasks()
        .get(pid)
        .and_then(|task| task.process.as_ref().map(|p| p.mappings.as_slice()))
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

/// Marks the current task's process as exited. Keeps the first exit info
/// recorded (a page-fault kill must not be overwritten by the exit stub
/// that follows it).
pub fn mark_current_exited(info: ExitInfo) {
    if let Some(process) = tasks_mut()[CURRENT.load(Ordering::Relaxed)]
        .process
        .as_mut()
    {
        process.mark_exited(info);
    }
}

/// Detaches a process from the scheduler, returning it so the caller can
/// free its frames. Refuses to remove the running task. Call with
/// interrupts disabled (the caller owns the freed frames afterwards).
pub fn remove_process(pid: usize) -> Option<Process> {
    let tasks = tasks_mut();
    if pid >= tasks.len() {
        return None;
    }
    let current = CURRENT.load(Ordering::Relaxed);
    if current == pid {
        return None;
    }
    let task = tasks.swap_remove(pid);
    if current == tasks.len() {
        // The current task was the last entry and got swapped into `pid`.
        CURRENT.store(pid, Ordering::Relaxed);
    } else if pid < current {
        CURRENT.store(current - 1, Ordering::Relaxed);
    }
    task.process
}

fn counter_task_0() -> ! {
    loop {
        COUNTERS[0].fetch_add(1, Ordering::Relaxed);
    }
}

fn counter_task_1() -> ! {
    loop {
        COUNTERS[1].fetch_add(1, Ordering::Relaxed);
    }
}

fn counter_task_2() -> ! {
    loop {
        COUNTERS[2].fetch_add(1, Ordering::Relaxed);
    }
}
