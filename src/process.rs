//! Process management: ELF programs loaded into their own address space
//! and scheduled like kernel tasks.
//!
//! A process gets a *private* four-level page table: a fresh PML4, PDPT,
//! and page directories that deep-copy the kernel's identity map (the
//! kernel image, heap, MMIO and stacks stay reachable, so ring-0 code —
//! all code, until the ring-3 work lands — can still run kernel
//! functions), plus private mappings for the ELF's segments at virtual
//! addresses the kernel never touches. The scheduler switches CR3 to the
//! process's PML4 when it is scheduled.
//!
//! Because every process's map is its own, a wild pointer in process A
//! can only hit pages A owns; a page fault in process context kills the
//! process (`kill_current`, invoked from the #PF handler) instead of
//! panicking the kernel. Clean exit works by returning from the ELF's
//! `_start`, which falls through to `exit_self`.
//!
//! Test programs report a result through the `.result` page pinned at
//! `PROC_RESULT_VIRT` by the user linker script (user/linker.ld); after
//! the process exits, `read_result` translates that virtual address
//! through the process's own mappings and reads the physical frame
//! (which stays identity-mapped in the kernel's map).

use alloc::vec::Vec;

use crate::allocator;
use crate::elf;
use crate::gdt;
use crate::interrupts;
use crate::io;
use crate::paging;
use crate::pmm;
use crate::syscall;
use crate::task;

/// Virtual address of the `.result` page every test program writes its
/// result to (see user/linker.ld — keep the two in sync).
pub const PROC_RESULT_VIRT: u64 = 0x2FF0000;

/// No process segment may start below 2 MiB (kernel image, PMM, tables).
const MIN_SEGMENT_VADDR: u64 = 2 * 1024 * 1024;

/// Every process gets the same fixed ring-3 stack address (256 MiB — well
/// clear of the 48 MiB test-program load address and the kernel's own
/// territory) since each process has its own private page table, so
/// there's no collision between processes reusing it.
const USER_STACK_TOP: u64 = 0x1000_0000;
const USER_STACK_PAGES: u64 = 8; // 32 KiB
const USER_STACK_BASE: u64 = USER_STACK_TOP - USER_STACK_PAGES * paging::PAGE_SIZE;
/// One page right above the stack: where the ELF's `_start` lands when it
/// `ret`s normally, since ring-3 code can't `ret` straight into the
/// kernel's `exit_self` (no privilege change on a bare `ret`, and that
/// page isn't PAGE_USER anyway). Holds `mov eax, SYS_EXIT; syscall`.
const USER_EXIT_STUB_VIRT: u64 = USER_STACK_TOP;

/// One mapped chunk of a process's address space: virtual range
/// `[vaddr, vaddr+len)` backed by the physical range starting at `phys`.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub vaddr: u64,
    pub phys: u64,
    pub len: u64,
}

/// Why a process stopped running.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExitInfo {
    /// `_start` returned; the process left through `exit_self`.
    Normal,
    /// The process faulted; CR2 at the time.
    PageFault { cr2: u64 },
    /// The process raised some other CPU exception (divide-by-zero,
    /// invalid opcode, a privileged instruction causing #GP, ...).
    Exception { vector: u8 },
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    Running,
    Exited,
}

pub struct Process {
    pub entry: usize,
    /// Initial ring-3 stack pointer (see `setup_user_stack`).
    pub user_rsp: u64,
    /// Every frame the process owns: its page tables (PML4, PDPT, PDs,
    /// PTs) and the pages backing its segments. Freed on reap.
    frames: Vec<usize>,
    /// The mapped segment ranges, for virtual-to-physical translation.
    pub mappings: Vec<Mapping>,
    /// Exit state. Volatile-read by `task::process_exit_status` so a
    /// `wait` loop can observe the mark without an optimizer barrier.
    pub(crate) state: ProcessState,
    pub(crate) exit_info: Option<ExitInfo>,
}

impl Process {
    pub fn is_exited(&self) -> bool {
        self.state == ProcessState::Exited
    }

    pub fn state_label(&self) -> &'static str {
        match self.state {
            ProcessState::Running => "running",
            ProcessState::Exited => "exited",
        }
    }

    pub fn mark_exited(&mut self, info: ExitInfo) {
        if self.state == ProcessState::Running {
            self.state = ProcessState::Exited;
            self.exit_info = Some(info);
        }
    }
}

/// Parses and loads `elf_bytes` as a new process, returning its pid.
pub fn spawn(elf_bytes: &[u8], name: &'static str) -> Result<usize, &'static str> {
    let program = elf::parse(elf_bytes)?;
    validate_segments(&program)?;

    let mut process = Process {
        entry: program.entry as usize,
        user_rsp: 0,
        frames: Vec::new(),
        mappings: Vec::new(),
        state: ProcessState::Running,
        exit_info: None,
    };

    let pml4 = match build_address_space(&mut process) {
        Ok(pml4) => pml4,
        Err(e) => {
            free_process(process);
            return Err(e);
        }
    };
    for segment in &program.segments {
        if let Err(e) = load_segment(&mut process, pml4, segment, elf_bytes) {
            free_process(process);
            return Err(e);
        }
    }
    if let Err(e) = setup_user_stack(&mut process, pml4) {
        free_process(process);
        return Err(e);
    }

    Ok(task::spawn_process(process_entry_trampoline, name, pml4, process))
}

/// The process's segments must live outside the kernel's own territory:
/// everything below 2 MiB (kernel image, PMM bitmap, page tables) and the
/// heap range (with a 1 MiB margin). The user linker script places test
/// programs at 48 MiB, well clear of both.
fn validate_segments(program: &elf::Program) -> Result<(), &'static str> {
    let (heap_start, heap_end) = allocator::heap_range();
    let heap_start = heap_start as u64;
    let heap_end = heap_end as u64;
    for segment in &program.segments {
        let start = segment.vaddr;
        let end = start
            .checked_add(segment.memsz)
            .ok_or("segment range overflow")?;
        if end <= start {
            return Err("empty segment");
        }
        if start < MIN_SEGMENT_VADDR {
            return Err("segment below 2 MiB");
        }
        if start < heap_end + 1024 * 1024 && end > heap_start.saturating_sub(1024 * 1024) {
            return Err("segment overlaps kernel heap");
        }
        if end > paging::IDENTITY_MAP_END {
            return Err("segment beyond 4 GiB");
        }
        // The kernel reserves [USER_STACK_BASE, USER_EXIT_STUB_VIRT + one
        // page) in every process for its ring-3 stack and exit trampoline
        // (see `setup_user_stack`); a segment landing there would get
        // silently overwritten (or overwrite them) once mapped.
        if start < USER_EXIT_STUB_VIRT + paging::PAGE_SIZE && end > USER_STACK_BASE {
            return Err("segment overlaps the reserved process stack region");
        }
    }
    Ok(())
}

/// Allocates a fresh, zeroed frame and records it in the process.
fn alloc_frame(process: &mut Process) -> Result<usize, &'static str> {
    let frame = pmm::frame_alloc().ok_or("out of memory")?;
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, pmm::FRAME_SIZE);
    }
    process.frames.push(frame);
    Ok(frame)
}

/// Builds the process's private address space: a fresh PML4 + PDPT +
/// four page directories, each a *copy* of the corresponding kernel
/// structure (2 MiB entries copied verbatim, split page tables deep
/// copied so the process never shares a PT with the kernel — a process
/// page mapped into a shared PT would leak into the kernel's map).
/// Returns the PML4's physical address.
fn build_address_space(process: &mut Process) -> Result<u64, &'static str> {
    let pml4 = alloc_frame(process)?;
    let pdpt = alloc_frame(process)?;
    let kernel = paging::kernel_pml4() as *const u64;
    unsafe {
        let pml4_ptr = pml4 as *mut u64;
        // Copy all 512 entries (only entry 0 — the 4 GiB identity map —
        // is populated), then point entry 0 at the fresh PDPT.
        core::ptr::copy_nonoverlapping(kernel, pml4_ptr, 512);
        *pml4_ptr.add(0) = pdpt as u64 | paging::PAGE_PRESENT | paging::PAGE_WRITABLE;

        let kernel_pdpt = (*kernel.add(0) & !0xFFF) as *const u64;
        let pdpt_ptr = pdpt as *mut u64;
        for i in 0..512 {
            let entry = *kernel_pdpt.add(i);
            if entry & paging::PAGE_PRESENT == 0 {
                continue;
            }
            if entry & (1 << 7) != 0 {
                return Err("unexpected 1 GiB page in boot map"); // the boot map never uses these
            }
            let pd = alloc_frame(process)?;
            let kernel_pd = (entry & !0xFFF) as *const u64;
            let pd_ptr = pd as *mut u64;
            for j in 0..512 {
                let pde = *kernel_pd.add(j);
                if pde & paging::PAGE_PRESENT == 0 {
                    continue;
                }
                if pde & (1 << 7) != 0 {
                    *pd_ptr.add(j) = pde; // 2 MiB page: copy verbatim
                } else {
                    // 4 KiB page table: deep copy it.
                    let pt = alloc_frame(process)?;
                    let kernel_pt = (pde & !0xFFF) as *const u64;
                    core::ptr::copy_nonoverlapping(kernel_pt, pt as *mut u64, 512);
                    *pd_ptr.add(j) = pt as u64 | (pde & 0xFFF);
                }
            }
            *pdpt_ptr.add(i) = pd as u64 | (entry & 0xFFF);
        }
    }
    Ok(pml4 as u64)
}

/// Allocates physical pages for one segment, copies its file bytes
/// (zero-filling BSS), and maps them at the segment's virtual addresses
/// in the process's address space.
fn load_segment(
    process: &mut Process,
    pml4: u64,
    segment: &elf::Segment,
    data: &[u8],
) -> Result<(), &'static str> {
    let start = segment.vaddr & !(paging::PAGE_SIZE - 1);
    let end = align_up(segment.vaddr + segment.memsz, paging::PAGE_SIZE);
    let len = end - start;
    let pages = (len / paging::PAGE_SIZE) as usize;

    let phys = pmm::alloc_contiguous(pages).ok_or("out of memory")?;
    for i in 0..pages {
        process.frames.push(phys + i * pmm::FRAME_SIZE);
    }

    let mut flags = paging::PAGE_PRESENT | paging::PAGE_USER;
    if segment.writable {
        flags |= paging::PAGE_WRITABLE;
    }
    if !paging::map_range_in(pml4, start, phys as u64, len, flags, &mut process.frames) {
        return Err("failed to map segment");
    }

    unsafe {
        // Zero the whole segment first, then copy the file bytes in.
        core::ptr::write_bytes(phys as *mut u8, 0, len as usize);
        let file_offset = segment.file_offset as usize;
        let file_len = segment.filesz as usize;
        if file_len > 0 {
            let dst = phys as usize;
            let src = data.as_ptr().add(file_offset);
            core::ptr::copy_nonoverlapping(src, dst as *mut u8, file_len);
        }
    }
    process.mappings.push(Mapping {
        vaddr: start,
        phys: phys as u64,
        len,
    });
    Ok(())
}

const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

/// Maps a ring-3 stack and an "exit trampoline" page into the process's
/// address space, and records the initial stack pointer for
/// `process_entry_trampoline`. The stack's top slot holds a return address
/// pointing at the trampoline, so when the ELF's `_start` (an ordinary
/// `extern "C" fn`, compiled with a normal prologue/epilogue) executes its
/// closing `ret`, it lands on `mov eax, SYS_EXIT; syscall` instead of
/// falling into kernel code it has no ring-3 access to.
fn setup_user_stack(process: &mut Process, pml4: u64) -> Result<(), &'static str> {
    let stack_len = USER_STACK_PAGES * paging::PAGE_SIZE;
    let stack_phys = pmm::alloc_contiguous(USER_STACK_PAGES as usize).ok_or("out of memory")?;
    for i in 0..USER_STACK_PAGES as usize {
        process.frames.push(stack_phys + i * pmm::FRAME_SIZE);
    }
    unsafe {
        core::ptr::write_bytes(stack_phys as *mut u8, 0, stack_len as usize);
    }
    if !paging::map_range_in(
        pml4,
        USER_STACK_BASE,
        stack_phys as u64,
        stack_len,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER,
        &mut process.frames,
    ) {
        return Err("failed to map user stack");
    }

    let stub_phys = alloc_frame(process)?;
    unsafe {
        let stub: [u8; 9] = [
            0xB8, syscall::SYS_EXIT as u8, 0x00, 0x00, 0x00, // mov eax, SYS_EXIT
            0x0F, 0x05, // syscall
            0xEB, 0xFE, // jmp $ (never reached; sys_exit never returns)
        ];
        core::ptr::copy_nonoverlapping(stub.as_ptr(), stub_phys as *mut u8, stub.len());
    }
    if !paging::map_range_in(
        pml4,
        USER_EXIT_STUB_VIRT,
        stub_phys as u64,
        paging::PAGE_SIZE,
        paging::PAGE_PRESENT | paging::PAGE_USER,
        &mut process.frames,
    ) {
        return Err("failed to map exit trampoline");
    }

    // One return-address slot at the top of the stack. Written through
    // the frame's kernel-identity address: the process's own PML4 (where
    // USER_STACK_BASE is actually mapped) isn't active yet — CR3 only
    // switches to it when the scheduler first runs this process.
    let initial_rsp = USER_STACK_TOP - 8;
    let offset = (initial_rsp - USER_STACK_BASE) as usize;
    unsafe {
        core::ptr::write_unaligned((stack_phys + offset) as *mut u64, USER_EXIT_STUB_VIRT);
    }
    process.user_rsp = initial_rsp;
    Ok(())
}

/// Entry point every process task starts at: run the ELF's `_start` in
/// ring 3, and once it exits (either by returning, through the exit
/// trampoline, or via a page fault redirect — see `kill_current`), exit
/// normally. Runs with the process's own PML4 active throughout.
extern "C" fn process_entry_trampoline() -> ! {
    let entry = task::current_process_entry();
    let user_rsp = task::current_process_user_rsp();
    unsafe {
        syscall::run_ring3(entry as u64, user_rsp);
    }
    exit_self()
}

/// Marks the current process exited and idles forever. The scheduler
/// skips exited processes, so control passes back to the kernel tasks at
/// the next timer tick, and `reap` can then free the process's frames.
extern "C" fn exit_self() -> ! {
    task::mark_current_exited(ExitInfo::Normal);
    loop {
        interrupts::halt();
    }
}

/// Called from the #PF handler when the faulting task is a process:
/// records the exit reason and redirects the ISR's return address so the
/// CPU never returns to the faulting instruction (which would fault
/// again immediately) — it lands in `exit_self` instead.
/// Redirects the ISR's return address to `exit_self` so the CPU never
/// resumes the faulting instruction (which would just fault again). The
/// fault may have happened in ring 3 (frame.cs/ss still hold the ring-3
/// selectors the CPU pushed); `exit_self` calls kernel functions and its
/// page isn't PAGE_USER, so force ring 0 regardless of where the fault
/// came from — CR3 doesn't change, and every address space deep-copies
/// the kernel's map, so `exit_self` and the process's own (still mapped,
/// still valid) stack are both reachable from ring 0 here.
fn redirect_to_exit(frame: &mut interrupts::InterruptFrame) {
    frame.rip = exit_self as extern "C" fn() -> ! as usize as u64;
    frame.cs = gdt::KERNEL_CODE as u64;
    frame.ss = gdt::KERNEL_DATA as u64;
}

/// Called from the #PF handler when the faulting task is a process.
pub fn kill_current(cr2: u64, frame: &mut interrupts::InterruptFrame) {
    task::mark_current_exited(ExitInfo::PageFault { cr2 });
    redirect_to_exit(frame);

    let mut buf = [0u8; 160];
    let message = io::sprint(
        &mut buf,
        format_args!(
            "[PROC] killed by page fault at {:#x} (error {:#x})\n",
            cr2, frame.error_code
        ),
    );
    io::exception_print(message);
}

/// Called from #DE/#UD/#GP (and similar) when the faulting task is a
/// process: a ring-3 program hitting a divide-by-zero, invalid opcode, or
/// privileged instruction kills just that process, the same way a page
/// fault does, rather than taking down the kernel.
pub fn kill_current_exception(vector: u8, frame: &mut interrupts::InterruptFrame) {
    task::mark_current_exited(ExitInfo::Exception { vector });
    redirect_to_exit(frame);

    let mut buf = [0u8; 128];
    let message = io::sprint(
        &mut buf,
        format_args!(
            "[PROC] killed by exception vector {} (error {:#x})\n",
            vector, frame.error_code
        ),
    );
    io::exception_print(message);
}

/// Waits (halting) until the process at `pid` exits or `timeout_ticks`
/// (1/100 s each) elapse, returning its exit info.
pub fn wait(pid: usize, timeout_ticks: u64) -> Option<ExitInfo> {
    let deadline = interrupts::ticks() + timeout_ticks;
    loop {
        if let Some(info) = task::process_exit_status(pid) {
            return Some(info);
        }
        if interrupts::ticks() >= deadline {
            return None;
        }
        interrupts::halt();
    }
}

/// Removes the process from the scheduler and returns every frame it
/// owned to the physical memory manager.
pub fn reap(pid: usize) {
    if let Some(process) = task::remove_process(pid) {
        free_process(process);
    }
}

fn free_process(process: Process) {
    for frame in &process.frames {
        pmm::frame_free(*frame);
    }
    // The Process itself (with its heap-allocated vecs) drops here.
}

/// Reads the u64 the process stored in its `.result` page (only valid
/// after the process exited; call before `reap`).
pub fn read_result(pid: usize) -> Option<u64> {
    let mappings = task::process_mappings(pid)?;
    let mapping = mappings
        .iter()
        .find(|m| m.vaddr <= PROC_RESULT_VIRT && PROC_RESULT_VIRT < m.vaddr + m.len)?;
    let offset = (PROC_RESULT_VIRT - mapping.vaddr) as usize;
    Some(unsafe { core::ptr::read_volatile((mapping.phys as usize + offset) as *const u64) })
}

/// Human-readable description of an exit reason.
pub fn describe_exit(info: &ExitInfo) -> alloc::string::String {
    match info {
        ExitInfo::Normal => alloc::string::String::from("exited normally"),
        ExitInfo::PageFault { cr2 } => {
            use alloc::format;
            format!("killed by page fault at {:#x}", cr2)
        }
        ExitInfo::Exception { vector } => {
            use alloc::format;
            format!("killed by exception vector {}", vector)
        }
    }
}
