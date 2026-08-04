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
pub(crate) const SYS_READ: u64 = 2;
pub(crate) const SYS_CLOCK: u64 = 3;
pub(crate) const SYS_SEND: u64 = 4;
pub(crate) const SYS_RECV: u64 = 5;
pub(crate) const SYS_WIN_CREATE: u64 = 6;
pub(crate) const SYS_WIN_UPDATE: u64 = 7;
pub(crate) const SYS_FILE_READ: u64 = 8;
pub(crate) const SYS_FILE_WRITE: u64 = 9;
pub(crate) const SYS_WIN_LIST: u64 = 10;
pub(crate) const SYS_WIN_READ: u64 = 11;
pub(crate) const SYS_FB_INFO: u64 = 12;
pub(crate) const SYS_FB_PRESENT: u64 = 13;
pub(crate) const SYS_GETPID: u64 = 14;
pub(crate) const SYS_WIN_FOCUS: u64 = 15;
pub(crate) const SYS_SHELL_EXEC: u64 = 16;
pub(crate) const SYS_FB_PRESENT_RECT: u64 = 17;
// Returned by write/read/send/recv when an argument (fd, or a pointer
// range the calling process doesn't own) is rejected.
const SYSCALL_ERROR: u64 = u64::MAX;

const FD_STDIN: u64 = 0;
const FD_STDOUT: u64 = 1;
const FD_STDERR: u64 = 2;

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
    # SYSCALL only guarantees RCX/R11 survive (hardware uses them to save
    # RIP/RFLAGS); everything else the user might be relying on across
    # this call — the SysV arg registers, since a caller can legitimately
    # keep a value alive in e.g. R8 across a call the way any other
    # register is fair game — has to be saved here and restored below, or
    # `syscall_dispatch`'s own machinery (an ordinary call, free to
    # clobber caller-saved registers) silently corrupts user state that
    # has nothing to do with this syscall's own arguments.
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    # SYS_EXIT (1) is checked here, before rax (the syscall number) gets
    # shuffled into an argument register below, and handled without ever
    # calling syscall_dispatch: unlike every other syscall it never
    # returns to user mode, so its "return value" can't just be a normal
    # sentinel returned from dispatch (that would collide with a genuine
    # error return, e.g. -1 from a bad sys_write fd).
    cmp rax, 1
    je .Lsyscall_exit

    # SysV syscall args arrive in rdi/rsi/rdx/r10/r8/r9 (r10 instead of
    # rcx, which `syscall` clobbers); shuffle num+4 args into the rdi..r8
    # slots `extern "C" fn syscall_dispatch` expects, working off the
    # register values (unchanged by the pushes above) rather than the
    # stack copies, which exist purely to restore the user's originals.
    mov r11, rdx
    mov rdx, rsi
    mov rsi, rdi
    mov rdi, rax
    mov rcx, r11
    mov r8, r10
    call syscall_dispatch

    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
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
fn resolve_user_buffer(ptr: u64, len: u64) -> Option<u64> {
    if crate::task::current_is_process() {
        crate::process::translate(crate::task::current_pid(), ptr, len)
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
/// `dest_pid`'s bounded inbox queue. See `task::deliver_message`: when the
/// queue is full, its oldest message is dropped.
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

/// Largest window dimension/pixel count sys_win_create/sys_win_update
/// will accept — just a sanity bound against a hostile or buggy huge
/// allocation request, well above this kernel's 1920x1080 display.
const MAX_WINDOW_DIM: u64 = 2048;

/// Creates (or, called again, replaces) the calling process's window.
/// The kernel window server allocates the surface immediately; the user
/// compositor discovers it on its next metadata poll.
fn sys_win_create(width: u64, height: u64, title_ptr: u64, title_len: u64) -> u64 {
    if width == 0 || height == 0 || width > MAX_WINDOW_DIM || height > MAX_WINDOW_DIM {
        return SYSCALL_ERROR;
    }
    let title = if title_len == 0 {
        alloc::string::String::new()
    } else {
        let Some(phys) = resolve_user_buffer(title_ptr, title_len) else {
            return SYSCALL_ERROR;
        };
        let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, title_len as usize) };
        alloc::string::String::from_utf8_lossy(bytes).into_owned()
    };
    if crate::window_server::create(crate::task::current_pid(), width as u32, height as u32, title) { 0 } else { SYSCALL_ERROR }
}

/// Replaces the calling process's window pixels wholesale. `pixel_count`
/// is in `u32` pixels (not bytes) and must exactly match the window's
/// `width * height` — a mismatch is dropped silently by the compositor
/// side (see `wm::update_process_window`), not reported here.
fn sys_win_update(pixels_ptr: u64, pixel_count: u64) -> u64 {
    if pixel_count == 0 || pixel_count > MAX_WINDOW_DIM * MAX_WINDOW_DIM || pixels_ptr % 4 != 0 {
        return SYSCALL_ERROR;
    }
    let Some(phys) = resolve_user_buffer(pixels_ptr, pixel_count * 4) else { return SYSCALL_ERROR };
    let pixels = unsafe { core::slice::from_raw_parts(phys as *const u32, pixel_count as usize) }.to_vec();
    if crate::window_server::update(crate::task::current_pid(), pixels) { 0 } else { SYSCALL_ERROR }
}

const WINDOW_RECORD_SIZE: usize = 64;

/// Writes fixed-size window metadata records for the user compositor.
fn sys_win_list(ptr: u64, max_records: u64) -> u64 {
    if max_records > crate::window_server::MAX_WINDOWS as u64 { return SYSCALL_ERROR; }
    let bytes = max_records as usize * WINDOW_RECORD_SIZE;
    let Some(phys) = resolve_user_buffer(ptr, bytes as u64) else { return SYSCALL_ERROR };
    let focused = crate::window_server::focused();
    let mut count = 0usize;
    crate::window_server::with_windows(|windows| {
        for window in windows.iter().take(max_records as usize) {
            let mut record = [0u8; WINDOW_RECORD_SIZE];
            let out = &mut record;
            out.fill(0);
            out[0..8].copy_from_slice(&(window.pid as u64).to_le_bytes());
            out[8..12].copy_from_slice(&window.x.to_le_bytes());
            out[12..16].copy_from_slice(&window.y.to_le_bytes());
            out[16..20].copy_from_slice(&window.width.to_le_bytes());
            out[20..24].copy_from_slice(&window.height.to_le_bytes());
            out[24..28].copy_from_slice(&(if focused == Some(window.pid) { 1u32 } else { 0 }).to_le_bytes());
            let title = window.title.as_bytes();
            let title_len = title.len().min(crate::window_server::TITLE_CAPACITY);
            out[28..32].copy_from_slice(&(title_len as u32).to_le_bytes());
            out[32..32 + title_len].copy_from_slice(&title[..title_len]);
            let destination = (phys + (count * WINDOW_RECORD_SIZE) as u64) as *mut u8;
            unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), destination, WINDOW_RECORD_SIZE); }
            count += 1;
        }
    });
    count as u64
}

fn sys_win_read(pid: u64, ptr: u64, max_pixels: u64) -> u64 {
    if max_pixels > MAX_WINDOW_DIM * MAX_WINDOW_DIM { return SYSCALL_ERROR; }
    let Some(phys) = resolve_user_buffer(ptr, max_pixels * 4) else { return SYSCALL_ERROR };
    let mut result = SYSCALL_ERROR;
    crate::window_server::with_window(pid as usize, |window| {
        let Some(window) = window else { return };
        if window.pixels.len() > max_pixels as usize { return; }
        unsafe { core::ptr::copy_nonoverlapping(window.pixels.as_ptr(), phys as *mut u32, window.pixels.len()) };
        result = window.pixels.len() as u64;
    });
    result
}

fn sys_fb_info() -> u64 {
    let (width, height) = crate::fb::dimensions();
    (width as u64) << 32 | height as u64
}

fn sys_fb_present(ptr: u64, pixel_count: u64) -> u64 {
    let (width, height) = crate::fb::dimensions();
    if pixel_count != width as u64 * height as u64 { return SYSCALL_ERROR; }
    let Some(phys) = resolve_user_buffer(ptr, pixel_count * 4) else { return SYSCALL_ERROR };
    let pixels = unsafe { core::slice::from_raw_parts(phys as *const u32, pixel_count as usize) };
    if crate::fb::present_pixels(&pixels) { 0 } else { SYSCALL_ERROR }
}

fn sys_fb_present_rect(ptr: u64, x: u64, y: u64, packed_size: u64) -> u64 {
    let width = (packed_size >> 32) as u32;
    let height = packed_size as u32;
    if width == 0 || height == 0 || width > 1920 || height > 128 { return SYSCALL_ERROR; }
    let Some(phys) = resolve_user_buffer(ptr, width as u64 * height as u64 * 4) else { return SYSCALL_ERROR };
    let pixels = unsafe { core::slice::from_raw_parts(phys as *const u32, (width * height) as usize) };
    if crate::fb::present_rect_pixels(x as u32, y as u32, width, height, pixels) { 0 } else { SYSCALL_ERROR }
}

fn sys_win_focus(pid: u64) -> u64 {
    crate::window_server::focus(pid as usize);
    0
}

fn sys_shell_exec(line_ptr: u64, line_len: u64, out_ptr: u64, out_len: u64) -> u64 {
    if line_len == 0 || line_len > 512 || out_len > 8192 { return SYSCALL_ERROR; }
    let Some(line_phys) = resolve_user_buffer(line_ptr, line_len) else { return SYSCALL_ERROR };
    let Some(out_phys) = resolve_user_buffer(out_ptr, out_len) else { return SYSCALL_ERROR };
    let bytes = unsafe { core::slice::from_raw_parts(line_phys as *const u8, line_len as usize) };
    let Ok(line) = core::str::from_utf8(bytes) else { return SYSCALL_ERROR };
    crate::io::start_capture();
    crate::shell::execute(line);
    let output = crate::io::take_capture();
    if output.len() > out_len as usize { return SYSCALL_ERROR; }
    unsafe { core::ptr::copy_nonoverlapping(output.as_ptr(), out_phys as *mut u8, output.len()) };
    output.len() as u64
}

const MAX_PATH_LEN: u64 = 256;
const MAX_FILE_TRANSFER: u64 = 64 * 1024;

/// Reads a whole file into a user buffer. This deliberately stays a small
/// path-based API until the user-space filesystem layer has file descriptors.
fn sys_file_read(path_ptr: u64, path_len: u64, buf_ptr: u64, buf_len: u64) -> u64 {
    if path_len == 0 || path_len > MAX_PATH_LEN || buf_len > MAX_FILE_TRANSFER {
        return SYSCALL_ERROR;
    }
    let Some(path_phys) = resolve_user_buffer(path_ptr, path_len) else { return SYSCALL_ERROR };
    let Some(buf_phys) = resolve_user_buffer(buf_ptr, buf_len) else { return SYSCALL_ERROR };
    let path_bytes = unsafe { core::slice::from_raw_parts(path_phys as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(path_bytes) else { return SYSCALL_ERROR };
    let Ok(data) = crate::fat::read_file(path) else { return SYSCALL_ERROR };
    if data.len() > buf_len as usize { return SYSCALL_ERROR; }
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf_phys as *mut u8, data.len()) };
    data.len() as u64
}

/// Replaces a whole file from a user buffer. The kernel validates both the
/// path and payload before touching the filesystem.
fn sys_file_write(path_ptr: u64, path_len: u64, buf_ptr: u64, len: u64) -> u64 {
    if path_len == 0 || path_len > MAX_PATH_LEN || len > MAX_FILE_TRANSFER {
        return SYSCALL_ERROR;
    }
    let Some(path_phys) = resolve_user_buffer(path_ptr, path_len) else { return SYSCALL_ERROR };
    let Some(buf_phys) = resolve_user_buffer(buf_ptr, len) else { return SYSCALL_ERROR };
    let path_bytes = unsafe { core::slice::from_raw_parts(path_phys as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(path_bytes) else { return SYSCALL_ERROR };
    let bytes = unsafe { core::slice::from_raw_parts(buf_phys as *const u8, len as usize) };
    if crate::fat::write_file(path, bytes).is_err() { return SYSCALL_ERROR; }
    len
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    match num {
        SYS_WRITE => sys_write(arg1, arg2, arg3),
        SYS_READ => sys_read(arg1, arg2, arg3),
        SYS_CLOCK => crate::interrupts::ticks(),
        SYS_SEND => sys_send(arg1, arg2, arg3),
        SYS_RECV => sys_recv(arg1, arg2),
        SYS_WIN_CREATE => sys_win_create(arg1, arg2, arg3, arg4),
        SYS_WIN_UPDATE => sys_win_update(arg1, arg2),
        SYS_FILE_READ => sys_file_read(arg1, arg2, arg3, arg4),
        SYS_FILE_WRITE => sys_file_write(arg1, arg2, arg3, arg4),
        SYS_WIN_LIST => sys_win_list(arg1, arg2),
        SYS_WIN_READ => sys_win_read(arg1, arg2, arg3),
        SYS_FB_INFO => sys_fb_info(),
        SYS_FB_PRESENT => sys_fb_present(arg1, arg2),
        SYS_GETPID => crate::task::current_pid() as u64,
        SYS_WIN_FOCUS => sys_win_focus(arg1),
        SYS_SHELL_EXEC => sys_shell_exec(arg1, arg2, arg3, arg4),
        SYS_FB_PRESENT_RECT => sys_fb_present_rect(arg1, arg2, arg3, arg4),
        _ => SYSCALL_ERROR,
    }
}

pub fn init() {
    // `enter_usermode` hardcodes 0x33/0x3B (user data/code | RPL 3) rather
    // than reading these constants from asm; keep them honest so a future
    // GDT layout change can't silently desync the two.
    debug_assert_eq!(crate::gdt::USER_DATA | 3, 0x33);
    debug_assert_eq!(crate::gdt::USER_CODE | 3, 0x3B);
    // `syscall_entry` hardcodes `cmp rax, 1` to special-case SYS_EXIT
    // before it's even dispatched (see the asm comment above).
    debug_assert_eq!(SYS_EXIT, 1);

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
    pmm::frame_free(code_frame);
    pmm::frame_free(stack_frame);
}
