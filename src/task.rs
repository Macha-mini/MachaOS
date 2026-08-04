//! Preemptive kernel-thread multitasking. Tasks are ring-0-only and share
//! one address space (no per-task page tables); switching happens purely
//! from the timer interrupt (`interrupts::timer` calls `scheduler_tick`
//! after sending EOI). There is no cooperative `yield_now` — the task
//! table is built once in `init()` before interrupts are enabled and is
//! only ever mutated afterward from inside the (non-reentrant, because
//! interrupt gates disable further interrupts) timer handler, so no lock
//! is needed around it, mirroring `interrupts::HANDLERS`.
//!
//! Context switching relies on every interrupt on this kernel running on
//! whichever task's stack happened to be current when it fired (there is
//! no ring change, so the CPU never switches stacks itself). Calling
//! `context_switch` from inside `timer()` therefore only needs to save
//! the callee-saved registers and swap `rsp`; the rest of the interrupted
//! task's state (general registers, vector, RIP/CS/RFLAGS/RSP/SS) is
//! already sitting safely on that task's own stack as the ISR frame that
//! `isr_common` (see isr_stubs.asm) pushed, untouched by any of this.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::interrupts;

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
}

const STACK_SIZE: usize = 16 * 1024;
const QUANTUM_TICKS: u64 = 5; // 50ms at the 100Hz PIT rate

// Built once in `init()` before interrupts are enabled, then only ever
// touched from `scheduler_tick()` (timer-interrupt context, which cannot
// be reentered), so no lock is needed. See module docs.
static mut TASKS: Vec<Task> = Vec::new();
static CURRENT: AtomicUsize = AtomicUsize::new(0);

const COUNTER_COUNT: usize = 3;
pub static COUNTERS: [AtomicU64; COUNTER_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

// Rust 2024 denies implicit `&`/`&mut` formation on `static mut` items
// (`static_mut_refs`); going through `addr_of_mut!`/`addr_of!` and
// dereferencing the raw pointer locally is the standard workaround, and
// is sound here precisely because `TASKS` is only ever touched from
// `scheduler_tick()` (non-reentrant interrupt context) after `init()`
// finishes, as documented above.
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
    });
    spawn(counter_task_0, "bg-0");
    spawn(counter_task_1, "bg-1");
    spawn(counter_task_2, "bg-2");
}

fn spawn(entry: fn() -> !, name: &'static str) {
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

    tasks_mut().push(Task {
        context: Context { rsp: x },
        _stack: Some(stack),
        name,
    });
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
    let next = (current + 1) % count;
    CURRENT.store(next, Ordering::Relaxed);

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
