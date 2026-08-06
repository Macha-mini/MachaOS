use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::io;
use crate::pic;
use crate::task;

global_asm!(include_str!("isr_stubs.asm"), options(att_syntax));

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    pub rbx: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

pub type Handler = fn(&mut InterruptFrame);

static mut HANDLERS: [Option<Handler>; 256] = [None; 256];

pub fn set_handler(vector: u8, handler: Option<Handler>) {
    unsafe {
        HANDLERS[vector as usize] = handler;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn isr_dispatch(frame: *mut InterruptFrame) {
    let frame = unsafe { &mut *frame };
    let vector = frame.vector as usize;
    if let Some(handler) = unsafe { HANDLERS[vector] } {
        handler(frame);
    } else {
        panic!("unhandled interrupt vector {}", vector);
    }
}

pub fn register_default_handlers() {
    for vector in 0..256 {
        set_handler(vector as u8, Some(unhandled));
    }
    set_handler(0, Some(divide_error));
    set_handler(1, Some(debug_exception));
    set_handler(3, Some(breakpoint));
    set_handler(4, Some(overflow));
    set_handler(6, Some(invalid_opcode));
    set_handler(8, Some(double_fault));
    set_handler(13, Some(general_protection_fault));
    set_handler(14, Some(page_fault));
    set_handler(32, Some(timer));
    set_handler(33, Some(keyboard));
    set_handler(44, Some(mouse));
}

fn exception(name: &str, frame: &mut InterruptFrame) -> ! {
    let mut head_buf = [0u8; 96];
    io::exception_print(io::sprint(&mut head_buf, format_args!("\n===== EXCEPTION: {} =====\n", name)));
    let mut reg_buf = [0u8; 512];
    let regs = format_registers(&mut reg_buf, frame);
    io::exception_print(regs);
    persist_exception(name, regs);
    halt_forever()
}

/// Formats the register dump into a caller-provided stack buffer (no
/// allocation — this runs from exception context).
pub(crate) fn format_registers<'a>(buf: &'a mut [u8; 512], frame: &InterruptFrame) -> &'a str {
    io::sprint(
        buf,
        format_args!(
            "vector: {}\nRIP: {:#x}  CS: {:#x}  RFLAGS: {:#x}\nRSP: {:#x}  SS: {:#x}\n\
             RAX: {:#x}  RBX: {:#x}  RCX: {:#x}  RDX: {:#x}\n\
             RSI: {:#x}  RDI: {:#x}  RBP: {:#x}\n\
             R8:  {:#x}  R9:  {:#x}  R10: {:#x}  R11: {:#x}\n\
             R12: {:#x}  R13: {:#x}  R14: {:#x}  R15: {:#x}\n",
            frame.vector,
            frame.rip,
            frame.cs,
            frame.rflags,
            frame.rsp,
            frame.ss,
            frame.rax,
            frame.rbx,
            frame.rcx,
            frame.rdx,
            frame.rsi,
            frame.rdi,
            frame.rbp,
            frame.r8,
            frame.r9,
            frame.r10,
            frame.r11,
            frame.r12,
            frame.r13,
            frame.r14,
            frame.r15,
        ),
    )
}

/// Best-effort: persist a fatal exception to disk for post-mortem
/// diagnosis without a serial connection. Concatenates a one-line entry
/// heading (with the tick count) and the register dump into a stack
/// buffer, then appends via `crashlog` (which swallows every error).
fn persist_exception(name: &str, registers: &str) {
    let mut log_buf = [0u8; 768];
    let entry = io::sprint(
        &mut log_buf,
        format_args!("===== EXCEPTION: {} @ tick {} =====\n{}", name, ticks(), registers),
    );
    crate::crashlog::append(entry);
}

/// Shared by the exceptions a ring-3 process can plausibly trigger on its
/// own (divide-by-zero, invalid opcode, a privileged instruction causing
/// #GP): if the current task is a process, kill just it, mirroring how
/// `page_fault` already handles #PF; otherwise it's a real kernel bug and
/// stays fatal.
fn process_or_exception(name: &str, frame: &mut InterruptFrame) {
    if task::current_is_process() {
        crate::process::kill_current_exception(frame.vector as u8, frame);
        return;
    }
    exception(name, frame)
}

fn divide_error(frame: &mut InterruptFrame) {
    process_or_exception("#DE Divide-by-zero", frame)
}

fn debug_exception(frame: &mut InterruptFrame) {
    exception("#DB Debug", frame)
}

fn breakpoint(frame: &mut InterruptFrame) {
    let mut buf = [0u8; 96];
    io::exception_print(io::sprint(
        &mut buf,
        format_args!("[BREAKPOINT] hit at RIP={:#x}\n", frame.rip),
    ));
}

fn overflow(frame: &mut InterruptFrame) {
    exception("#OF Overflow", frame)
}

fn invalid_opcode(frame: &mut InterruptFrame) {
    process_or_exception("#UD Invalid opcode", frame)
}

fn double_fault(frame: &mut InterruptFrame) {
    exception("#DF Double fault (IST used)", frame)
}

fn general_protection_fault(frame: &mut InterruptFrame) {
    process_or_exception("#GP General protection fault", frame)
}

fn page_fault(frame: &mut InterruptFrame) {
    let cr2 = read_cr2();
    // A fault inside a process (that is, with the process's address space
    // active) kills just that process: the page it touched is simply not
    // mapped, which is the one kind of fault a process can cause on its
    // own. Anything else — a fault while a kernel task runs, or in the
    // kernel's own map — is still fatal. (Until ring 3 lands, kernel code
    // executing *in* process context is also attributed to the process.)
    if task::current_is_process() {
        // A legitimate stack-growth touch (see `process::handle_fault`)
        // gets mapped in and the faulting instruction just re-executes —
        // returning from an interrupt handler without redirecting `rip`
        // resumes exactly where the fault happened. Anything else is a
        // real fault: not-present outside the growth region, or a
        // protection violation.
        if crate::process::handle_fault(cr2, frame.error_code) {
            return;
        }
        crate::process::kill_current(cr2, frame);
        return;
    }
    let err = frame.error_code;
    let mut head_buf = [0u8; 192];
    let head = io::sprint(
        &mut head_buf,
        format_args!(
            "\n===== EXCEPTION: #PF Page fault =====\nfaulting address (CR2): {:#x}\nerror code: {:#x} (present={}, write={}, user={})\n",
            cr2,
            err,
            err & 1 != 0,
            err & 2 != 0,
            err & 4 != 0,
        ),
    );
    io::exception_print(head);
    let mut reg_buf = [0u8; 512];
    let regs = format_registers(&mut reg_buf, frame);
    io::exception_print(regs);
    // Post-mortem log entry: heading (with CR2 + error code) followed by
    // the register dump, in one buffer so the whole thing can be
    // appended to the disk log.
    let mut log_buf = [0u8; 768];
    let entry = io::sprint(
        &mut log_buf,
        format_args!(
            "===== EXCEPTION: #PF @ tick {} =====\nCR2: {:#x}  error code: {:#x} (present={}, write={}, user={})\n{}",
            ticks(),
            cr2,
            err,
            err & 1 != 0,
            err & 2 != 0,
            err & 4 != 0,
            regs
        ),
    );
    crate::crashlog::append(entry);
    halt_forever()
}

fn unhandled(frame: &mut InterruptFrame) {
    exception("unknown interrupt", frame)
}

pub fn read_cr2() -> u64 {
    let mut value: u64;
    unsafe {
        asm!("mov {0}, cr2", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

fn timer(frame: &mut InterruptFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    // EOI must go out before a possible task switch: if scheduler_tick()
    // switches away from this task before the PIC hears about it, IRQ0's
    // in-service bit stays latched and every interrupt on the system
    // stalls until this task runs again to send it — which would require
    // another timer interrupt that can now never arrive.
    pic::eoi(0);
    // Phase 8b: deliver a pending signal to the current task if it's a
    // ring-3 process (redirects this frame into a handler / terminates).
    crate::process::deliver_pending_signals(frame);
    task::scheduler_tick();
}

fn keyboard(_frame: &mut InterruptFrame) {
    crate::keyboard::irq();
    pic::eoi(1);
}

fn mouse(_frame: &mut InterruptFrame) {
    crate::mouse::irq();
    pic::eoi(12);
}

pub fn halt() {
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

pub fn halt_forever() -> ! {
    loop {
        halt();
    }
}

pub fn enable_interrupts() {
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

pub fn disable_interrupts() {
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}
