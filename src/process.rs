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
use crate::fat;
use crate::gdt;
use crate::interrupts;
use crate::io;
use crate::paging;
use crate::pmm;
use crate::shm;
use crate::socket;
use crate::syscall;
use crate::task;
use crate::vfs;

/// fd 0/1/2 are reserved for stdin/stdout/stderr, which `syscall.rs`
/// already serves directly (see `sys_write`/`sys_read`) rather than
/// through a `FileHandle`; real files start at fd 3.
pub(crate) const FIRST_FILE_FD: usize = 3;
/// Virtual address of the `.result` page every test program writes its
/// result to (see user/linker.ld — keep the two in sync).
pub const PROC_RESULT_VIRT: u64 = 0x2FF0000;

/// No process segment may start below 2 MiB (kernel image, PMM, tables).
const MIN_SEGMENT_VADDR: u64 = 2 * 1024 * 1024;

/// Load bias applied to an ET_DYN (PIE) image's segments and entry point
/// (see `elf::Program::is_pie`). 512 MiB: comfortably clear of the kernel
/// heap, the 48 MiB convention MachaOS's own ET_EXEC test programs use
/// (`user/linker.ld`), and the 256 MiB user-stack region below, with no
/// attempt at ASLR (a fixed base is fine for a single-tenant loader).
const PIE_LOAD_BASE: u64 = 0x2000_0000;

/// Load bias for a `PT_INTERP` dynamic linker (see `spawn_linux`'s Phase
/// 4 handling): 896 MiB, clear of `PIE_LOAD_BASE` plus any realistic
/// main-binary image size below it, and clear of `MMAP_BASE` (1 GiB)
/// above it, where `ld.so` itself will go on to `mmap` every shared
/// library it loads (real `ld.so` builds are a few hundred KiB, nowhere
/// near the ~128 MiB of headroom either side gives it).
const INTERP_LOAD_BASE: u64 = 0x3800_0000;

/// Every process gets the same fixed ring-3 stack address (256 MiB — well
/// clear of the 48 MiB test-program load address and the kernel's own
/// territory) since each process has its own private page table, so
/// there's no collision between processes reusing it.
const USER_STACK_TOP: u64 = 0x1000_0000;
const USER_STACK_PAGES: u64 = 8; // 32 KiB mapped up front at spawn
const USER_STACK_BASE: u64 = USER_STACK_TOP - USER_STACK_PAGES * paging::PAGE_SIZE;
/// Hard floor stack growth (see `Process::try_grow_stack`) will map down
/// to: 1 MiB total, comfortably more than the 32 KiB mapped at spawn but
/// still small enough that a genuinely wild pointer several pages below
/// the current floor reliably lands outside it and stays fatal.
const USER_STACK_MAX_PAGES: u64 = 256; // 1 MiB
const USER_STACK_LOW_LIMIT: u64 = USER_STACK_TOP - USER_STACK_MAX_PAGES * paging::PAGE_SIZE;
/// One page right above the stack: where the ELF's `_start` lands when it
/// `ret`s normally, since ring-3 code can't `ret` straight into the
/// kernel's `exit_self` (no privilege change on a bare `ret`, and that
/// page isn't PAGE_USER anyway). Holds `mov eax, SYS_EXIT; syscall`.
const USER_EXIT_STUB_VIRT: u64 = USER_STACK_TOP;

/// Anonymous-mmap arena for the Linux ABI's `mmap` (see
/// `Process::mmap_anon`): a non-`MAP_FIXED` request gets the next free
/// address starting here and growing up, well clear of `PIE_LOAD_BASE`
/// (512 MiB) and any realistically-sized set of loaded segments, with
/// 2 GiB of room before `MMAP_CEILING` — comfortably under
/// `paging::IDENTITY_MAP_END` (4 GiB), the hard limit every virtual
/// address in this kernel is under regardless of process.
const MMAP_BASE: u64 = 0x4000_0000;
const MMAP_CEILING: u64 = 0xC000_0000;

/// One mapped chunk of a process's address space: virtual range
/// `[vaddr, vaddr+len)` backed by the physical range starting at `phys`.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub vaddr: u64,
    pub phys: u64,
    pub len: u64,
    /// `Some(shm_id)` for a `MAP_SHARED` mapping of a `shm.rs` object —
    /// those physical frames are refcounted there, owned by whichever
    /// fds (in any process) still reference the object, not by this
    /// process. `munmap` and process exit must leave them alone (no
    /// `pmm::frame_free`) for a `Some` mapping; `None` is the normal
    /// process-owned case every other mapping already was before shared
    /// memory existed.
    pub shm_id: Option<usize>,
}

/// One open fd's referent: a real file, a `memfd_create` shared-memory
/// object, or an `AF_UNIX` socket endpoint. `File` used to be the fd
/// table's only variant (`Vec<Option<vfs::FileHandle>>`); this widens it
/// for Phase 5 (`shm.rs`/`socket.rs`) without disturbing how a plain
/// file fd already worked.
pub enum FdEntry {
    File(vfs::FileHandle),
    /// `(shm.rs` object id, byte cursor)` — a memfd is readable/
    /// writable like a regular file even though real clients only ever
    /// `mmap` it; kept for completeness since it costs little.
    Shm(usize, usize),
    /// `(socket.rs` endpoint id.)
    Socket(usize),
    /// `inet.rs` endpoint id — an `AF_INET` TCP/UDP socket (Phase 10).
    Net(usize),
    /// `pipe.rs` pipe id; `true` for the read end (fd[0]).
    Pipe(usize, bool),
    /// `eventfd` counter (`eventfd2`): `read` returns the count and
    /// zeroes it; `write` adds to it. Phase 9a.
    Eventfd(u64),
    /// `timerfd` (`timerfd_create`): `(deadline_ticks, period_ticks)` on
    /// the 100 Hz clock; readable once the deadline passes. `deadline 0`
    /// is disarmed; `period 0` is a one-shot. Phase 9a.
    Timerfd(u64, u64),
    /// `epoll` instance (`epoll_create1`): the `(fd, events, data)`
    /// registrations `epoll_ctl` installed, scanned for readiness by
    /// `epoll_wait`. Phase 9a.
    Epoll(alloc::vec::Vec<(usize, u32, u64)>),
    /// Pseudo-device (`/dev/null`, `/dev/zero`, `/dev/urandom`):
    /// 0 = null (reads EOF, writes discarded), 1 = zero (reads zeros),
    /// 2 = urandom (reads PRNG bytes). Phase 9b.
    Dev(u8),
    /// A `/proc` pseudo-file's content, snapshotted at open (`/proc/
    /// self/stat`); reads consume it. Phase 9b.
    Proc(alloc::vec::Vec<u8>),
}

impl Drop for FdEntry {
    fn drop(&mut self) {
        // `File`'s own `vfs::FileHandle` has its own `Drop` (flushes if
        // dirty) that still runs automatically after this — Rust drops
        // an enum's field(s) after a custom `Drop::drop` body, the same
        // way it would for a struct.
        match self {
            FdEntry::Shm(id, _) => shm::close(*id),
            FdEntry::Socket(id) => socket::close(*id),
            FdEntry::Net(id) => crate::inet::close(*id),
            FdEntry::Pipe(id, is_read) => crate::pipe::close_end(*id, *is_read),
            FdEntry::File(_) | FdEntry::Eventfd(_) | FdEntry::Timerfd(..) | FdEntry::Epoll(_) => {}
            FdEntry::Dev(_) | FdEntry::Proc(_) => {}
        }
    }
}

impl FdEntry {
    /// Copy for `fork`. `File` deep-copies (`vfs::FileHandle::dup`);
    /// `Shm`/`Pipe` share the object (refcount bumped — `Drop` in the
    /// child then only drops the child's reference); `Socket` endpoints
    /// are single-owner in `socket.rs`, so they're not duplicated (the
    /// child gets no such fd — a documented limitation, and nothing the
    /// current test binaries exercise across a fork).
    pub fn dup(&self) -> Option<FdEntry> {
        match self {
            FdEntry::File(h) => Some(FdEntry::File(h.dup())),
            FdEntry::Shm(id, cursor) => {
                shm::dup(*id);
                Some(FdEntry::Shm(*id, *cursor))
            }
            FdEntry::Pipe(id, is_read) => {
                crate::pipe::dup_end(*id, *is_read);
                Some(FdEntry::Pipe(*id, *is_read))
            }
            FdEntry::Socket(_) | FdEntry::Net(_) => None,
            // Value semantics for the Phase 9a fds: a fork/dup gets an
            // independent copy (real Linux shares the open description —
            // fine for the event-loop workloads these serve).
            FdEntry::Eventfd(v) => Some(FdEntry::Eventfd(*v)),
            FdEntry::Timerfd(d, p) => Some(FdEntry::Timerfd(*d, *p)),
            FdEntry::Epoll(v) => Some(FdEntry::Epoll(v.clone())),
            FdEntry::Dev(kind) => Some(FdEntry::Dev(*kind)),
            FdEntry::Proc(content) => Some(FdEntry::Proc(content.clone())),
        }
    }
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
    /// The process was terminated by a signal (`kill`/`tgkill` with a
    /// default-action signal such as SIGKILL/SIGTERM). The wait status
    /// for a signal death is the signal number itself (no `<< 8`).
    Signaled { sig: u8 },
}

#[derive(Clone, Copy, PartialEq)]
pub enum ProcessState {
    Running,
    Exited,
}

/// Which syscall table `syscall::syscall_dispatch` routes a process's
/// syscalls to. `Native` is MachaOS's own 5-syscall ABI (`syscall.rs`);
/// `Linux` is the (currently skeletal — see `linux_abi.rs`) Linux x86_64
/// syscall layer this field exists to select. Independent of which
/// initial-stack layout the process got (`setup_user_stack` vs
/// `setup_linux_stack`): they're orthogonal today only because nothing
/// but `spawn_linux_test` sets `Linux`, and it happens to also want the
/// Linux-style stack — a real "run this as a Linux binary" entry point
/// (Phase 2) would set both together, but there's no requirement that
/// they always match.
#[derive(Clone, Copy, PartialEq)]
pub enum Abi {
    Native,
    Linux,
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
    /// Single-slot mailbox for `sys_send`/`sys_recv` (see `task::deliver_message`).
    pub(crate) inbox: Option<Vec<u8>>,
    /// Open-fd table for the Linux ABI layer's `openat`/`read`/`write`/
    /// `lseek`/`close`/`fstat`/`socket`/`mmap` syscalls. Indices
    /// 0..FIRST_FILE_FD stay `None` always — see `FIRST_FILE_FD`.
    fds: Vec<Option<FdEntry>>,
    /// Lowest address currently mapped for the ring-3 stack; starts at
    /// `USER_STACK_BASE` and moves down as `try_grow_stack` maps more of
    /// the reserved growth region below it.
    stack_low: u64,
    /// Physical base of this process's entire `USER_STACK_MAX_PAGES`
    /// stack allocation (see `init_stack_backing`), reserved as one
    /// contiguous block up front at spawn even though most of it isn't
    /// page-table-mapped for ring-3 access yet. That's deliberate: the
    /// kernel's own `Mapping`/`translate` bookkeeping (used to validate
    /// a *syscall* pointer via direct physical access — entirely
    /// separate from what's page-table-present for the process's own
    /// ring-3 accesses) registers the whole range as a single `Mapping`
    /// from the start, so a syscall output buffer straddling two
    /// separate `try_grow_stack` calls still resolves as one contiguous
    /// range. Per-event physical allocation (the original design) broke
    /// this the first time a real dynamically-linked binary's `ld.so`
    /// passed `fstat` a stack buffer straddling exactly such a boundary
    /// — caught by testing against one, not a hypothetical.
    stack_phys_base: u64,
    /// Which syscall table this process's syscalls dispatch to. See `Abi`.
    pub(crate) abi: Abi,
    /// Fixed once at spawn, just past the highest loaded segment
    /// (page-aligned) — the Linux ABI's `brk(0)` starting point. See
    /// `Process::brk`.
    heap_start: u64,
    /// Current program break (`brk`'s logical, possibly non-page-aligned
    /// value — see `Process::brk`).
    heap_end: u64,
    /// Next address `Process::mmap_anon` hands out for a non-`MAP_FIXED`
    /// request.
    mmap_next: u64,
    /// Parallel to `fds`: close-on-exec flag per fd (fcntl F_SETFD).
    cloexec: Vec<bool>,
    /// The task index of the process that forked this one — `wait4(-1)`
    /// must only see *this* process's children, not unrelated exited
    /// processes (a stale, never-reaped child from an earlier test made
    /// BusyBox's `waitpid(-1)` loop forever).
    parent: Option<usize>,
    /// Per-signal dispositions, indexed by signal number (1..=64; index
    /// 0 unused). `handler` is the ring-3 handler address, or 0 = SIG_DFL,
    /// 1 = SIG_IGN. Phase 8: recorded by `rt_sigaction`; default-action
    /// delivery (terminate / ignore) is implemented, handler delivery is
    /// not yet. Heap-backed (`Box`) so the `Process` struct itself stays
    /// small — a 1040-byte inline array made `fork_copy`'s kernel-stack
    /// footprint overflow the parked exit chain.
    pub(crate) sigactions: alloc::boxed::Box<[SigAction; 65]>,
    /// Blocked-signal mask (bit `sig` set = blocked), `rt_sigprocmask`.
    pub(crate) sig_blocked: u64,
    /// Signals delivered-but-not-yet-handled (bit `sig` set). Phase 8a
    /// only accumulates these for caught/blocked signals; delivery of
    /// caught handlers lands in a later phase.
    pub(crate) sig_pending: u64,
}

/// One entry of `Process::sigactions` — the Linux `struct sigaction`
/// fields this kernel keeps (x86_64: handler, flags, restorer, and the
/// kernel's 64-bit `sigset_t` mask, 32 bytes total).
#[derive(Clone, Copy)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub mask: u64,
}

impl SigAction {
    /// `SIG_DFL` — take the signal's default action (terminate, or
    /// ignore for the default-ignored signals).
    pub const fn dfl() -> SigAction {
        SigAction { handler: 0, flags: 0, mask: 0 }
    }
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

    /// Installs `entry` at the lowest free fd (never below
    /// `FIRST_FILE_FD`), returning it.
    pub fn alloc_fd(&mut self, entry: FdEntry) -> usize {
        for (i, slot) in self.fds.iter_mut().enumerate().skip(FIRST_FILE_FD) {
            if slot.is_none() {
                *slot = Some(entry);
                return i;
            }
        }
        self.fds.push(Some(entry));
        self.fds.len() - 1
    }

    pub fn fd_mut(&mut self, fd: usize) -> Option<&mut FdEntry> {
        self.fds.get_mut(fd)?.as_mut()
    }

    /// The task index of the process that forked this one (`wait4(-1)`
    /// child matching).
    pub(crate) fn parent(&self) -> Option<usize> {
        self.parent
    }

    /// Re-points or clears the parent (task reaping shifts indices; an
    /// orphaned process — its parent was reaped — gets `None`).
    pub(crate) fn set_parent(&mut self, parent: Option<usize>) {
        self.parent = parent;
    }

    /// Immutable fd access (for `dup` — the entry is copied out).
    pub fn fd(&self, fd: usize) -> Option<&FdEntry> {
        self.fds.get(fd)?.as_ref()
    }

    /// Close-on-exec flag for `fd` (fcntl F_GETFD/F_SETFD). `execve`
    /// drops any fd with this set, as real Linux does — without it a
    /// pipeline's original pipe ends leak into the exec'd program and
    /// the reader never sees EOF.
    pub fn cloexec(&self, fd: usize) -> bool {
        self.cloexec.get(fd).copied().unwrap_or(false)
    }

    /// Sets/clears the close-on-exec flag (`fcntl(F_SETFD)`).
    pub fn set_cloexec(&mut self, fd: usize, on: bool) -> bool {
        if fd >= self.fds.len() || self.fds[fd].is_none() {
            return false;
        }
        while self.cloexec.len() <= fd {
            self.cloexec.push(false);
        }
        self.cloexec[fd] = on;
        true
    }

    /// Closes every fd with the close-on-exec flag (execve).
    pub fn close_cloexec_fds(&mut self) {
        for (i, slot) in self.fds.iter_mut().enumerate() {
            if self.cloexec.get(i).copied().unwrap_or(false) && slot.is_some() {
                *slot = None;
            }
        }
    }

    /// Places `entry` at exactly `fd` (the caller closed any previous
    /// occupant); returns `fd`.
    pub fn set_fd(&mut self, fd: usize, entry: FdEntry) -> usize {
        if fd >= self.fds.len() {
            while self.fds.len() <= fd {
                self.fds.push(None);
            }
        }
        while self.cloexec.len() <= fd {
            self.cloexec.push(false);
        }
        self.fds[fd] = Some(entry);
        fd
    }

    /// Closes `fd`, flushing it (see `vfs::FileHandle::drop`). Returns
    /// `false` for a reserved (<FIRST_FILE_FD) or already-closed fd.
    pub fn close_fd(&mut self, fd: usize) -> bool {
        if fd < FIRST_FILE_FD {
            return false;
        }
        match self.fds.get_mut(fd) {
            Some(slot @ Some(_)) => {
                *slot = None;
                true
            }
            _ => false,
        }
    }

    /// Reserves this process's entire growable stack region
    /// (`[USER_STACK_LOW_LIMIT, USER_STACK_TOP)`, `USER_STACK_MAX_PAGES`
    /// pages) as one contiguous physical block and registers it as a
    /// single `Mapping`, up front — see `stack_phys_base`'s doc comment
    /// for why. Must run (via `setup_user_stack`/`setup_linux_stack`)
    /// before any `try_grow_stack` call.
    fn init_stack_backing(&mut self) -> Result<(), &'static str> {
        let pages = USER_STACK_MAX_PAGES as usize;
        let phys = pmm::alloc_contiguous(pages).ok_or("out of memory")?;
        for i in 0..pages {
            self.frames.push(phys + i * pmm::FRAME_SIZE);
        }
        unsafe {
            core::ptr::write_bytes(phys as *mut u8, 0, pages * pmm::FRAME_SIZE);
        }
        self.stack_phys_base = phys as u64;
        self.mappings.push(Mapping {
            vaddr: USER_STACK_LOW_LIMIT,
            phys: phys as u64,
            len: USER_STACK_MAX_PAGES * paging::PAGE_SIZE,
            shm_id: None,
        });
        Ok(())
    }

    /// Physical address backing `vaddr` within the growable stack region,
    /// given `init_stack_backing` already ran.
    fn stack_phys_for(&self, vaddr: u64) -> u64 {
        self.stack_phys_base + (vaddr - USER_STACK_LOW_LIMIT)
    }

    /// Grows the ring-3 stack down to cover `fault_addr`, if it is a
    /// legitimate "touched just below the current floor" access: still
    /// inside the reserved growth region (`>= USER_STACK_LOW_LIMIT`) but
    /// below `stack_low`. Maps every page from `fault_addr`'s page up to
    /// (not including) the previous floor — not just the single faulting
    /// page — so a deep one-shot descent (a large stack-local array, or a
    /// callee that skips the compiler's usual page-at-a-time stack
    /// probing) is covered in one fault instead of needing one fault per
    /// page. Returns `false` (leaving the fault to kill the process, same
    /// as before this existed) for anything outside that region.
    ///
    /// Only installs page-table entries — the physical backing is
    /// already there (`init_stack_backing`) and already registered as
    /// part of the one whole-region `Mapping`, so there's no new frame
    /// allocation or `Mapping` to push here.
    fn try_grow_stack(&mut self, pml4: u64, fault_addr: u64) -> bool {
        if fault_addr >= self.stack_low || fault_addr < USER_STACK_LOW_LIMIT {
            return false;
        }
        let new_low = fault_addr & !(paging::PAGE_SIZE - 1);
        let grow_len = self.stack_low - new_low;
        let phys = self.stack_phys_for(new_low);
        if !paging::map_range_in(
            pml4,
            new_low,
            phys,
            grow_len,
            paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER | paging::PAGE_NX,
            &mut self.frames,
        ) {
            return false;
        }
        self.stack_low = new_low;
        true
    }

    /// Translates `[vaddr, vaddr+len)` against this process's own
    /// mappings directly, without the `pid` indirection `process::translate`
    /// needs (that one goes through `task::process_mappings`, for callers
    /// that only have a pid — `Abi::Linux` syscall handlers always have
    /// `&mut self` already via `task::with_current_process_mut`).
    fn translate_local(&self, vaddr: u64, len: u64) -> Option<u64> {
        let end = vaddr.checked_add(len)?;
        // Newest mapping wins: `map_range_in` overwrites page-table entries,
        // so later `mmap(MAP_FIXED)` calls shadow earlier ones — and the VMA
        // list is push-ordered. A first-match search here returns a stale
        // (shadowed) mapping's phys instead of the one the CPU actually
        // resolves through (ld.so maps a whole shared object span first,
        // then remaps each PT_LOAD on top — first-match broke every
        // kernel-side relocation write to libc's .dynamic/.got).
        let mapping = self.mappings.iter().rev().find(|m| m.vaddr <= vaddr && end <= m.vaddr + m.len)?;
        Some(mapping.phys + (vaddr - mapping.vaddr))
    }

    /// Linux `brk`: `requested == 0` (or `< heap_start`) is the "query
    /// current break" form and changes nothing. Otherwise maps whatever
    /// additional whole pages `requested` needs beyond what's already
    /// mapped (shrinking just moves the logical boundary down — the
    /// pages already mapped for it stay mapped, simpler than unmapping
    /// them and harmless at this scale) and returns the new break; on
    /// allocation failure, returns the unchanged old break, matching
    /// real `brk`'s "never returns -1" convention (a caller compares the
    /// return value to what it asked for to detect failure).
    pub fn brk(&mut self, pml4: u64, requested: u64) -> u64 {
        if requested < self.heap_start {
            return self.heap_end;
        }
        let old_mapped = align_up(self.heap_end.max(self.heap_start) - self.heap_start, paging::PAGE_SIZE);
        let new_mapped = align_up(requested - self.heap_start, paging::PAGE_SIZE);
        if new_mapped > old_mapped {
            let grow_len = new_mapped - old_mapped;
            let pages = (grow_len / paging::PAGE_SIZE) as usize;
            let Some(phys) = pmm::alloc_contiguous(pages) else {
                return self.heap_end;
            };
            for i in 0..pages {
                self.frames.push(phys + i * pmm::FRAME_SIZE);
            }
            let map_at = self.heap_start + old_mapped;
            if !paging::map_range_in(
                pml4,
                map_at,
                phys as u64,
                grow_len,
                paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER | paging::PAGE_NX,
                &mut self.frames,
            ) {
                return self.heap_end;
            }
            // Zero through the freshly-mapped `map_at`, not `phys` — see
            // `mmap_with_content` for the identity-map collision.
            unsafe {
                core::ptr::write_bytes(map_at as *mut u8, 0, grow_len as usize);
            }
            self.mappings.push(Mapping {
                vaddr: map_at,
                phys: phys as u64,
                len: grow_len,
                shm_id: None,
            });
        }
        self.heap_end = requested;
        self.heap_end
    }

    /// Linux anonymous `mmap`: freshly zeroed frames, no initial content.
    /// See `mmap_with_content` for the shared implementation.
    pub fn mmap_anon(&mut self, pml4: u64, at: Option<u64>, len: u64, writable: bool, executable: bool) -> Option<u64> {
        self.mmap_with_content(pml4, at, len, writable, executable, None)
    }

    /// Linux file-backed `mmap`: same as `mmap_anon`, except the mapped
    /// frames start with `content`'s bytes (zero-padded if `content` is
    /// shorter than `len`, e.g. a segment's `memsz` exceeding its
    /// `filesz` — the same BSS handling `load_segments` does for the
    /// main ELF's own segments) instead of being all zero. `ld.so` needs
    /// this for real — mapping a shared library's segments from zeroed
    /// memory would load a library that's all zero bytes.
    ///
    /// Copies `content` in eagerly at mmap time rather than mapping the
    /// file's own pages directly (real `MAP_PRIVATE` semantics): no
    /// meaningful difference for a read-only/COW mapping, since each
    /// process already gets its own physical frames either way (`fork`
    /// isn't implemented, so nothing could ever share these frames'
    /// backing across processes regardless).
    pub fn mmap_file(&mut self, pml4: u64, at: Option<u64>, len: u64, writable: bool, executable: bool, content: &[u8]) -> Option<u64> {
        self.mmap_with_content(pml4, at, len, writable, executable, Some(content))
    }

    /// `at`, when `Some`, is a `MAP_FIXED` request (used verbatim,
    /// page-aligned down — `ld.so` needs this to place a shared
    /// library's segments at a chosen base); `None` hands out the next
    /// free address in the bump-allocated arena `MMAP_BASE..MMAP_CEILING`.
    /// No lazy/demand-paged anonymous memory — see Phase 1's plan notes
    /// on why that was deferred.
    fn mmap_with_content(&mut self, pml4: u64, at: Option<u64>, len: u64, writable: bool, executable: bool, content: Option<&[u8]>) -> Option<u64> {
        let len = align_up(len.max(1), paging::PAGE_SIZE);
        let addr = match at {
            Some(a) => a & !(paging::PAGE_SIZE - 1),
            None => {
                let a = self.mmap_next;
                if a.checked_add(len)? > MMAP_CEILING {
                    return None;
                }
                a
            }
        };
        let pages = (len / paging::PAGE_SIZE) as usize;
        let phys = pmm::alloc_contiguous(pages)?;
        for i in 0..pages {
            self.frames.push(phys + i * pmm::FRAME_SIZE);
        }
        let mut flags = paging::PAGE_PRESENT | paging::PAGE_USER;
        if writable {
            flags |= paging::PAGE_WRITABLE;
        }
        if !executable {
            flags |= paging::PAGE_NX;
        }
        if !paging::map_range_in(pml4, addr, phys as u64, len, flags, &mut self.frames) {
            return None;
        }
        // Zero (and copy any content) through the freshly-mapped `addr`,
        // never through `phys` as a virtual address: physical frame
        // numbers collide with user VAs in the shared identity-mapped
        // address space, so writing to `phys` clobbers whatever user
        // mapping currently occupies that VA — the same class of bug as
        // the `ls /bin` DIR corruption (the DIR's frame stayed
        // user-reachable at its own identity address).
        unsafe {
            core::ptr::write_bytes(addr as *mut u8, 0, len as usize);
            if let Some(data) = content {
                let n = data.len().min(len as usize);
                core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, n);
            }
        }
        // TEMP PROBE: dump small anon mmaps after zeroing
        if content.is_none() && len <= 0x4000 && at.is_none() {
            let mut h = alloc::string::String::new();
            for i in 0..32u64 {
                let b = unsafe { core::ptr::read_volatile((addr + i) as *const u8) };
                h.push_str(&alloc::format!("{b:02x}"));
            }
            crate::io::exception_print(crate::io::sprint(
                &mut [0u8; 256],
                format_args!("[MMZ] addr={addr:#x} phys={phys:#x} b0..32={h}\n"),
            ));
        }
        self.mappings.push(Mapping {
            vaddr: addr,
            phys: phys as u64,
            len,
            shm_id: None,
        });
        if at.is_none() {
            self.mmap_next = addr + len;
        }
        Some(addr)
    }

    /// Linux `mmap(..., MAP_SHARED, shm_fd, 0)` against a `shm.rs`
    /// object: maps that object's *existing* physical frames (whole
    /// object, offset 0 only — every real `wl_shm` caller maps the
    /// entire pool it just `ftruncate`d) instead of allocating fresh
    /// ones, so writes through this mapping are visible to whichever
    /// other process maps the same `shm_id` — the actual point of
    /// `MAP_SHARED`, unlike `mmap_file`'s eager private copy. The
    /// mapped `Mapping` doesn't own these frames (`shm_id: Some(id)`
    /// marks that for `munmap`/process exit): `shm.rs`'s own refcount,
    /// not this process, decides when they're freed.
    pub fn mmap_shared(&mut self, pml4: u64, at: Option<u64>, shm_id: usize, writable: bool, executable: bool) -> Option<u64> {
        let phys = shm::phys_of(shm_id)?;
        let pages = shm::pages_of(shm_id)?;
        let len = (pages * paging::PAGE_SIZE as usize) as u64;
        if len == 0 {
            return None;
        }
        let addr = match at {
            Some(a) => a & !(paging::PAGE_SIZE - 1),
            None => {
                let a = self.mmap_next;
                if a.checked_add(len)? > MMAP_CEILING {
                    return None;
                }
                a
            }
        };
        let mut flags = paging::PAGE_PRESENT | paging::PAGE_USER;
        if writable {
            flags |= paging::PAGE_WRITABLE;
        }
        if !executable {
            flags |= paging::PAGE_NX;
        }
        if !paging::map_range_in(pml4, addr, phys as u64, len, flags, &mut self.frames) {
            return None;
        }
        self.mappings.push(Mapping {
            vaddr: addr,
            phys: phys as u64,
            len,
            shm_id: Some(shm_id),
        });
        if at.is_none() {
            self.mmap_next = addr + len;
        }
        Some(addr)
    }

    /// Linux `munmap`. Only reclaims mappings *fully* contained in
    /// `[addr, addr+len)` — unmapping just part of an existing mapping
    /// (splitting it into a kept and a freed piece) isn't implemented;
    /// a partially-overlapping range is left mapped as-is (safe, if
    /// imprecise, and not a shape `mmap_anon`'s own callers produce).
    pub fn munmap(&mut self, pml4: u64, addr: u64, len: u64) -> bool {
        let len = align_up(len.max(1), paging::PAGE_SIZE);
        let start = addr & !(paging::PAGE_SIZE - 1);
        let end = start + len;
        let mut i = 0;
        while i < self.mappings.len() {
            let m = self.mappings[i];
            if m.vaddr >= start && m.vaddr + m.len <= end {
                paging::unmap_range_in(pml4, m.vaddr, m.len);
                // Shared-memory frames outlive this mapping — see
                // `Mapping::shm_id`'s doc comment — so only a
                // process-owned (`shm_id: None`) mapping frees them.
                if m.shm_id.is_none() {
                    let pages = (m.len / paging::PAGE_SIZE) as usize;
                    for p in 0..pages {
                        pmm::frame_free(m.phys as usize + p * pmm::FRAME_SIZE);
                    }
                    self.frames.retain(|&f| (f as u64) < m.phys || (f as u64) >= m.phys + m.len);
                }
                self.mappings.remove(i);
            } else {
                i += 1;
            }
        }
        true
    }

    /// Linux `mprotect`. Re-maps each page already owned in
    /// `[addr, addr+len)` at its existing physical address with the new
    /// permission (`map_range_in` overwrites a PTE unconditionally, so
    /// this needs no fresh frames). Fails if any page in the range isn't
    /// currently owned by this process.
    pub fn mprotect(&mut self, pml4: u64, addr: u64, len: u64, writable: bool, executable: bool) -> bool {
        let len = align_up(len.max(1), paging::PAGE_SIZE);
        let start = addr & !(paging::PAGE_SIZE - 1);
        let mut v = start;
        while v < start + len {
            let Some(phys) = self.translate_local(v, paging::PAGE_SIZE) else {
                return false;
            };
            let mut flags = paging::PAGE_PRESENT | paging::PAGE_USER;
            if writable {
                flags |= paging::PAGE_WRITABLE;
            }
            if !executable {
                flags |= paging::PAGE_NX;
            }
            if !paging::map_range_in(pml4, v, phys, paging::PAGE_SIZE, flags, &mut self.frames) {
                return false;
            }
            v += paging::PAGE_SIZE;
        }
        true
    }
}

/// Called from the #PF handler (see `interrupts::page_fault`) before it
/// falls back to `kill_current`: `true` means `cr2` was a legitimate
/// stack-growth touch that's now mapped in, so the CPU will simply
/// re-execute the faulting instruction once the handler returns. `false`
/// covers everything else (no current process, or an address outside the
/// growth region), which stays fatal exactly as before this existed.
///
/// Deliberately ignores `error_code`'s present bit rather than requiring
/// a clean not-present fault: `build_address_space` deep-copies the
/// kernel's boot identity map, which covers this whole region as
/// ordinary (non-`PAGE_USER`) 2 MiB pages, and `setup_user_stack`'s
/// initial mapping already forced a split of the 2 MiB block the top of
/// the growth region sits in — see `pt_entry_ptr_in`'s note on how a
/// split inherits the original entry's flags into every leaf it doesn't
/// explicitly overwrite. So a first ring-3 touch just below the current
/// floor, but still inside a block that's been through such a split,
/// finds an *already-present*, kernel-only page there rather than a
/// missing one: present=1, user=1, and thus a protection-violation
/// error code, even though it's exactly the legitimate growth case
/// `try_grow_stack`'s own address-range check exists to recognize. Since
/// [`USER_STACK_LOW_LIMIT`, `USER_STACK_TOP`) is reserved exclusively for
/// this stack (`validate_segments` rejects any ELF segment landing
/// there), any fault in that range — present or not — can only be this
/// case or a genuine bug in an already-mapped page, and the latter still
/// correctly falls through to `false` via `try_grow_stack`'s
/// `fault_addr >= self.stack_low` check.
pub fn handle_fault(cr2: u64, _error_code: u64) -> bool {
    let Some(pml4) = task::current_process_cr3() else {
        return false;
    };
    task::with_current_process_mut(|process| process.try_grow_stack(pml4, cr2)).unwrap_or(false)
}

/// Adds `PIE_LOAD_BASE` to every segment's `vaddr` and to `entry` in
/// place, for an ET_DYN image whose addresses are otherwise relative to a
/// link-time base of 0. A no-op for ET_EXEC (`is_pie == false`).
fn apply_pie_bias(program: &mut elf::Program) {
    if !program.is_pie {
        return;
    }
    program.entry = program.entry.wrapping_add(PIE_LOAD_BASE);
    for segment in &mut program.segments {
        segment.vaddr = segment.vaddr.wrapping_add(PIE_LOAD_BASE);
    }
}

/// Parses and loads `elf_bytes` as a new process, returning its pid.
pub fn spawn(elf_bytes: &[u8], name: &'static str) -> Result<usize, &'static str> {
    let mut program = elf::parse(elf_bytes)?;
    apply_pie_bias(&mut program);
    validate_segments(&program)?;

    let mut process = Process {
        entry: program.entry as usize,
        user_rsp: 0,
        frames: Vec::new(),
        mappings: Vec::new(),
        state: ProcessState::Running,
        exit_info: None,
        inbox: None,
        fds: (0..FIRST_FILE_FD).map(|_| None).collect(),
        stack_low: USER_STACK_BASE,
        stack_phys_base: 0, // set by init_stack_backing below
        abi: Abi::Native,
        heap_start: 0,
        heap_end: 0,
        mmap_next: MMAP_BASE,
        cloexec: Vec::new(),
        parent: None,
        sigactions: alloc::boxed::Box::new([SigAction::dfl(); 65]),
        sig_blocked: 0,
        sig_pending: 0,
    };

    let pml4 = match build_address_space(&mut process) {
        Ok(pml4) => pml4,
        Err(e) => {
            free_process(process);
            return Err(e);
        }
    };
    if let Err(e) = load_segments(&mut process, pml4, &program.segments, elf_bytes) {
        free_process(process);
        return Err(e);
    }
    if let Err(e) = setup_user_stack(&mut process, pml4) {
        free_process(process);
        return Err(e);
    }

    Ok(task::spawn_process(process_entry_trampoline, name, pml4, process))
}

/// Entry point for a Linux-ABI process: otherwise identical to `spawn`,
/// but builds an argv/envp/auxv stack (`setup_linux_stack`) instead of
/// the native ABI's single "return to the exit trampoline" slot, and
/// routes the process's syscalls through the Linux syscall table
/// (`linux_abi.rs`) instead of the native one (`Abi::Linux`). Used both
/// by the Phase 1/2 test programs and by `shell.rs`'s `runlinux` command.
pub fn spawn_linux(elf_bytes: &[u8], name: &'static str, argv: &[&str], envp: &[&str]) -> Result<usize, &'static str> {
    let (process, pml4) = load_linux_process(elf_bytes, argv, envp, (0..FIRST_FILE_FD).map(|_| None).collect())?;
    Ok(task::spawn_process(process_entry_trampoline, name, pml4, process))
}

/// The shared Linux-ELF loader behind `spawn_linux` and `execve`:
/// parses, applies the PIE bias, validates, builds the process (starting
/// with the given fd table — fresh for `spawn_linux`, the current
/// process's for `execve`), maps the segments, loads `PT_INTERP`'s
/// interpreter as the real entry point when present, and builds the
/// argv/envp/auxv stack. Returns `(process, pml4)`.
fn load_linux_process(
    elf_bytes: &[u8],
    argv: &[&str],
    envp: &[&str],
    fds: Vec<Option<FdEntry>>,
) -> Result<(Process, u64), &'static str> {
    let mut program = elf::parse(elf_bytes)?;
    apply_pie_bias(&mut program);
    validate_segments(&program)?;
    // brk(0) starts just past the highest loaded segment, page-aligned,
    // with a one-page gap so the heap can never be mistaken for part of
    // the last segment.
    let heap_start = program
        .segments
        .iter()
        .map(|s| align_up(s.vaddr + s.memsz, paging::PAGE_SIZE) + paging::PAGE_SIZE)
        .max()
        .ok_or("no loadable segments")?;

    let mut process = Process {
        entry: program.entry as usize,
        user_rsp: 0,
        frames: Vec::new(),
        mappings: Vec::new(),
        state: ProcessState::Running,
        exit_info: None,
        inbox: None,
        fds,
        stack_low: USER_STACK_BASE,
        stack_phys_base: 0, // set by init_stack_backing below
        abi: Abi::Linux,
        heap_start,
        heap_end: heap_start,
        mmap_next: MMAP_BASE,
        cloexec: Vec::new(),
        parent: None,
        sigactions: alloc::boxed::Box::new([SigAction::dfl(); 65]),
        sig_blocked: 0,
        sig_pending: 0,
    };

    let pml4 = match build_address_space(&mut process) {
        Ok(pml4) => pml4,
        Err(e) => {
            free_process(process);
            return Err(e);
        }
    };
    if let Err(e) = load_segments(&mut process, pml4, &program.segments, elf_bytes) {
        free_process(process);
        return Err(e);
    }

    // A dynamically-linked binary (almost always ET_DYN, like "program"
    // itself) names its real interpreter via PT_INTERP — load *that* as
    // the process's actual entry point instead, the same way a real
    // Linux kernel never implements the ELF relocator itself. As long as
    // AT_PHDR/AT_ENTRY (still `program`'s own, below) are correct, the
    // real ld.so this loads does its own relocation, PLT resolution, and
    // TLS setup, then jumps to `program`'s real entry itself.
    let mut entry = program.entry;
    let mut interp_base = 0u64;
    if let Some(interp_path) = program.interp.clone() {
        let interp_bytes = match fat::read_file(&interp_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                free_process(process);
                return Err("interpreter not found");
            }
        };
        let mut interp_program = match elf::parse(&interp_bytes) {
            Ok(p) => p,
            Err(e) => {
                free_process(process);
                return Err(e);
            }
        };
        if interp_program.is_pie {
            interp_base = INTERP_LOAD_BASE;
            interp_program.entry = interp_program.entry.wrapping_add(INTERP_LOAD_BASE);
            for segment in &mut interp_program.segments {
                segment.vaddr = segment.vaddr.wrapping_add(INTERP_LOAD_BASE);
            }
        }
        if let Err(e) = validate_segments(&interp_program) {
            free_process(process);
            return Err(e);
        }
        if let Err(e) = load_segments(&mut process, pml4, &interp_program.segments, &interp_bytes) {
            free_process(process);
            return Err(e);
        }
        entry = interp_program.entry;
    }
    process.entry = entry as usize;

    let user_rsp = match setup_linux_stack(&mut process, pml4, elf_bytes, &program, interp_base, argv, envp) {
        Ok(rsp) => rsp,
        Err(e) => {
            free_process(process);
            return Err(e);
        }
    };
    process.user_rsp = user_rsp;
    map_syscall_stubs(&mut process, pml4)?;

    Ok((process, pml4))
}

/// `execve`: loads `elf_bytes` into the *current* process — its fd table
/// carries over (Linux keeps fds across exec), the old address space is
/// freed, an exit chain is parked on the current task's kernel stack
/// below the running syscall's frames, and the ring-3 resume state is
/// stashed for the asm's exec-restart path, which iretq's into the new
/// program instead of sysretq'ing to the old (freed) instruction
/// pointer. Returns the new `(entry, user_rsp)`.
pub fn execve_into_current(
    elf_bytes: &[u8],
    argv: &[&str],
    envp: &[&str],
) -> Result<(u64, u64), &'static str> {
    // Take the current fd table (the new process inherits it), build the
    // new process, then swap it in and free the old address space.
    let old_parent = task::with_current_process_mut(|p| p.parent()).flatten();
    let (mut new_process, new_cr3) = {
        let taken = task::with_current_process_mut(|p| core::mem::take(&mut p.fds))
            .ok_or("not a process")?;
        load_linux_process(elf_bytes, argv, envp, taken)?
    };
    // `execve` keeps the process identity — including who forked it, so
    // the parent's `wait4(-1)` still matches this process after the exec
    // (an exec'd pipeline child whose parent vanished would never be
    // reaped, and the shell's wait loop would spin forever).
    new_process.parent = old_parent;
    // Signals: the blocked mask is preserved across `execve` like
    // Linux; the disposition table carries over too (Linux resets
    // caught handlers to SIG_DFL, but Phase 8a has no handler delivery
    // yet and the exec'd image installs its own dispositions anyway).
    let sig_state =
        task::with_current_process_mut(|p| (alloc::boxed::Box::new(*p.sigactions), p.sig_blocked))
            .unwrap_or((alloc::boxed::Box::new([crate::process::SigAction::dfl(); 65]), 0));
    new_process.sigactions = sig_state.0;
    new_process.sig_blocked = sig_state.1;
    // Close-on-exec: drop the fds the old program flagged with
    // FD_CLOEXEC (pipeline pipe ends, etc.) before the new image runs.
    new_process.close_cloexec_fds();
    let entry = new_process.entry;
    let user_rsp = new_process.user_rsp;
    let current = task::current_pid();
    if let Some(old) = task::replace_current_process(current, new_process) {
        free_process(old);
    }
    task::set_current_cr3(current, new_cr3);
    // Load the new page tables *now*: the exec-restart path iretq's into
    // the new program immediately (no context switch in between), and
    // the CPU would otherwise keep running against the old — freed —
    // address space until the next scheduler tick (reading the old
    // stack's garbage argv and looping). The kernel's identity map is
    // deep-copied into every address space, so switching mid-syscall is
    // safe.
    paging::write_cr3(new_cr3);
    // Park the exit chain at the task's fixed safe location (below the
    // syscall-stack region — see `task::repark_exit_chain`): the old
    // local-relative spot sat inside the deepest syscall frames' reach
    // and got overwritten whenever the exec'd program made deep calls.
    crate::task::repark_exit_chain(exit_self as extern "C" fn() -> ! as usize);
    // A plain execve restarts into a fresh program, so the asm's
    // exec-restart path must take the plain-iretq branch — not the
    // register-restore branch `rt_sigreturn` flags.
    crate::syscall::clear_sigreturn_restore();
    crate::syscall::set_exec_restart(entry as u64, user_rsp);
    Ok((entry as u64, user_rsp))
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
            return Err("segment beyond identity map (16 GiB)");
        }
        // The kernel reserves [USER_STACK_LOW_LIMIT, USER_EXIT_STUB_VIRT +
        // one page) in every process for its ring-3 stack — including the
        // room `try_grow_stack` grows it into — and exit trampoline (see
        // `setup_user_stack`); a segment landing there would get silently
        // overwritten (or overwrite them) once mapped.
        if start < USER_EXIT_STUB_VIRT + paging::PAGE_SIZE && end > USER_STACK_LOW_LIMIT {
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
                    // 2 MiB page: copy verbatim — but strip the USER bit.
                    // The kernel's identity map covers the whole 4 GiB,
                    // including the kernel's own page-table frames at
                    // ~3 MiB and the kernel heap at 64 MiB; leaving USER
                    // set lets ring-3 code write over the kernel's page
                    // tables (that is exactly how the Phase 6 GOT.plt
                    // fault happened: user code clobbered the kernel PD
                    // entry for 0x401d3000, and the corrupted copy was
                    // then inherited by every spawned process). User
                    // mappings replace these 2 MiB entries with their
                    // own 4 KiB page tables (map_range_in splits them,
                    // re-adding USER), so stripping it here costs
                    // nothing.
                    *pd_ptr.add(j) = pde & !paging::PAGE_USER;
                } else {
                    // 4 KiB page table: deep copy it — stripping the
                    // USER bit from every present PTE, exactly like the
                    // 2 MiB entries above. Inherited USER identity
                    // pages let ring-3 read/write whatever physical
                    // frame the kernel later hands out whose *number*
                    // coincides with that VA — the `ls /bin` DIR
                    // corruption: the DIR's frame (0x694f000) stayed
                    // reachable at its own address through a
                    // user-writable identity PTE, and a heap write
                    // landing there mutated dir->fd from 3 to 6.
                    let pt = alloc_frame(process)? as *mut u64;
                    let kernel_pt = (pde & !0xFFF) as *const u64;
                    // TEMP PROBE: the 0x680000 4K table — PTE at 0x694f000
                    let idx_694 = ((0x694f000 >> 12) & 0x1FF) as usize;
                    crate::io::exception_print(crate::io::sprint(
                        &mut [0u8; 256],
                        format_args!(
                            "[BAP] 0x680000 kernel_pte(0x694f000)={:#x} copied={:#x}\n",
                            *kernel_pt.add(idx_694),
                            *kernel_pt.add(idx_694) & !paging::PAGE_USER
                        ),
                    ));
                    for i in 0..512 {
                        let e = *kernel_pt.add(i);
                        *pt.add(i) = if e & paging::PAGE_PRESENT != 0 {
                            e & !paging::PAGE_USER
                        } else {
                            e
                        };
                    }
                    *pd_ptr.add(j) = (pt as u64) | ((pde & 0xFFF) & !paging::PAGE_USER);
                }
            }
            *pdpt_ptr.add(i) = (pd as u64) | ((entry & 0xFFF) & !paging::PAGE_USER);
        }
    }
    Ok(pml4 as u64)
}

/// Allocates physical pages for one segment, copies its file bytes
/// (zero-filling BSS), and maps them at the segment's virtual addresses
/// in the process's address space.
/// Loads every PT_LOAD segment into one contiguous physical block spanning
/// their combined page-aligned range, rather than one block per segment.
///
/// A single ELF that mixes small `.text`/`.rodata`/`.data` segments (as
/// rustc/lld output does) routinely has consecutive segments land in the
/// *same* trailing/leading 4 KiB page — e.g. `.text` ending at 0x3000030
/// and `.rodata` starting right there in the same page. Handling segments
/// independently is wrong two ways at once: each would get its own fresh
/// physical page mapped over the *same* virtual page (the later segment's
/// `map_range_in` call silently replaces the earlier one's page-table
/// entry, orphaning its frame), and each copies its file bytes to offset
/// 0 of *its own* page rather than to the segment's actual offset within
/// the shared page — so the earlier segment's bytes are simply never
/// where they need to be. Loading the whole span as one block sidesteps
/// both: there is only one frame per virtual page, and every segment
/// copies to its own precise offset within it.
///
/// This does allocate physical memory for any gap *between* segments too
/// (e.g. alignment padding), which is fine at the scale these programs
/// run at; a loader for larger binaries would want to map each segment's
/// own pages and only share frames at an explicitly-detected boundary.
fn load_segments(
    process: &mut Process,
    pml4: u64,
    segments: &[elf::Segment],
    data: &[u8],
) -> Result<(), &'static str> {
    let Some(overall_start) = segments.iter().map(|s| s.vaddr & !(paging::PAGE_SIZE - 1)).min() else {
        return Ok(()); // no segments (rejected earlier by elf::parse, but harmless)
    };
    let overall_end = segments
        .iter()
        .map(|s| align_up(s.vaddr + s.memsz, paging::PAGE_SIZE))
        .max()
        .unwrap();
    let len = overall_end - overall_start;
    let pages = (len / paging::PAGE_SIZE) as usize;

    let phys = pmm::alloc_contiguous(pages).ok_or("out of memory")?;
    for i in 0..pages {
        process.frames.push(phys + i * pmm::FRAME_SIZE);
    }

    // Map page by page rather than as one `flags` value for the whole
    // block: real linkers (unlike rust-lld's output for MachaOS's own
    // programs — see the doc comment above) page-align PT_LOAD segments
    // so `.text` and `.data` never share a page, so this gives properly
    // built binaries genuine per-segment R/W separation instead of the
    // union of every segment's flags. A page straddling two segments
    // (only happens for MachaOS's own tightly-packed test programs) still
    // safely gets the union, same as before.
    for i in 0..pages {
        let page_vaddr = overall_start + i as u64 * paging::PAGE_SIZE;
        let page_phys = (phys + i * pmm::FRAME_SIZE) as u64;
        let page_end = page_vaddr + paging::PAGE_SIZE;
        let mut flags = paging::PAGE_PRESENT | paging::PAGE_USER;
        if segments
            .iter()
            .any(|s| s.writable && s.vaddr < page_end && s.vaddr + s.memsz > page_vaddr)
        {
            flags |= paging::PAGE_WRITABLE;
        }
        if !segments
            .iter()
            .any(|s| s.executable && s.vaddr < page_end && s.vaddr + s.memsz > page_vaddr)
        {
            flags |= paging::PAGE_NX;
        }
        if !paging::map_range_in(pml4, page_vaddr, page_phys, paging::PAGE_SIZE, flags, &mut process.frames) {
            return Err("failed to map segment");
        }
    }

    unsafe {
        core::ptr::write_bytes(phys as *mut u8, 0, len as usize);
        for segment in segments {
            if segment.filesz == 0 {
                continue;
            }
            let dst = phys as usize + (segment.vaddr - overall_start) as usize;
            let src = data.as_ptr().add(segment.file_offset as usize);
            core::ptr::copy_nonoverlapping(src, dst as *mut u8, segment.filesz as usize);
        }
    }
    process.mappings.push(Mapping {
        vaddr: overall_start,
        phys: phys as u64,
        len,
        shm_id: None,
    });
    Ok(())
}

const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

/// Maps the fixed page at `USER_EXIT_STUB_VIRT` holding the two ring-3
/// syscall stubs: the exit stub (`_start`'s natural `ret` target —
/// `mov eax, SYS_EXIT; syscall`) and, at offset 16, the sigreturn
/// trampoline (`mov eax, SYS_RT_SIGRETURN; syscall`) that a signal
/// handler's `ret` lands on. Both `jmp $` after the syscall — neither
/// syscall ever returns. Mapped for native *and* Linux spawns (Linux
/// processes need the sigreturn half for Phase 8b handler delivery).
fn map_syscall_stubs(process: &mut Process, pml4: u64) -> Result<(), &'static str> {
    let stub_phys = alloc_frame(process)?;
    unsafe {
        let stubs: [u8; 25] = [
            0xB8, syscall::SYS_EXIT as u8, 0x00, 0x00, 0x00, // mov eax, SYS_EXIT
            0x0F, 0x05, // syscall
            0xEB, 0xFE, // jmp $
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
            0xB8, crate::linux_abi::SYS_RT_SIGRETURN as u8, 0x00, 0x00, 0x00, // mov eax, SYS_RT_SIGRETURN
            0x0F, 0x05, // syscall
            0xEB, 0xFE, // jmp $
        ];
        core::ptr::copy_nonoverlapping(stubs.as_ptr(), stub_phys as *mut u8, stubs.len());
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
    process.mappings.push(Mapping {
        vaddr: USER_EXIT_STUB_VIRT,
        phys: stub_phys as u64,
        len: paging::PAGE_SIZE,
        shm_id: None,
    });
    Ok(())
}

/// Maps a ring-3 stack and the syscall-stub page into the process's
/// address space, and records the initial stack pointer for
/// `process_entry_trampoline`. The stack's top slot holds a return address
/// pointing at the exit stub, so when the ELF's `_start` (an ordinary
/// `extern "C" fn`, compiled with a normal prologue/epilogue) executes its
/// closing `ret`, it lands on `mov eax, SYS_EXIT; syscall` instead of
/// falling into kernel code it has no ring-3 access to.
fn setup_user_stack(process: &mut Process, pml4: u64) -> Result<(), &'static str> {
    // Registers the whole growable region as a single Mapping (see
    // `stack_phys_base`'s doc comment for why that matters for a
    // syscall pointer's validation, not just ring-3 access) — that's
    // what makes the standalone `process.mappings.push` a previous
    // version of this function did here unnecessary now.
    process.init_stack_backing()?;
    let stack_len = USER_STACK_PAGES * paging::PAGE_SIZE;
    let stack_phys = process.stack_phys_for(USER_STACK_BASE);
    if !paging::map_range_in(
        pml4,
        USER_STACK_BASE,
        stack_phys,
        stack_len,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER | paging::PAGE_NX,
        &mut process.frames,
    ) {
        return Err("failed to map user stack");
    }

    map_syscall_stubs(process, pml4)?;

    // One return-address slot at the top of the stack. Written through
    // the frame's kernel-identity address: the process's own PML4 (where
    // USER_STACK_BASE is actually mapped) isn't active yet — CR3 only
    // switches to it when the scheduler first runs this process.
    let initial_rsp = USER_STACK_TOP - 8;
    let offset = initial_rsp - USER_STACK_BASE;
    unsafe {
        core::ptr::write_unaligned((stack_phys + offset) as *mut u64, USER_EXIT_STUB_VIRT);
    }
    process.user_rsp = initial_rsp;
    Ok(())
}

/// ELF auxiliary vector entry types `setup_linux_stack` fills in (see the
/// System V ABI x86-64 supplement, and the Linux kernel's
/// `fs/binfmt_elf.c` for what a real ELF loader provides its `_start`).
mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_FLAGS: u64 = 8;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_UID: u64 = 11;
    pub const AT_EUID: u64 = 12;
    pub const AT_GID: u64 = 13;
    pub const AT_EGID: u64 = 14;
    pub const AT_HWCAP: u64 = 16;
    pub const AT_CLKTCK: u64 = 17;
    pub const AT_SECURE: u64 = 23;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_HWCAP2: u64 = 26;
    pub const AT_EXECFN: u64 = 31;
}

/// Builds a Linux-style initial stack — argc, argv[], envp[], the auxv
/// table, and the string/random/phdr data they point into — in place of
/// `setup_user_stack`'s single "return to the exit trampoline" slot. A
/// real libc's `_start` reads exactly this layout off its initial `rsp`
/// and calls `exit`/`exit_group` explicitly, never `ret`s off the end of
/// `_start`, so unlike `setup_user_stack` this maps no exit trampoline.
///
/// Lays out, low to high address (i.e. reading forward from the returned
/// rsp): argc, argv pointers, a NULL, envp pointers, a NULL, `(type,
/// value)` auxv pairs terminated by `AT_NULL` — then, above all of that,
/// the string/phdr/random-byte data those pointers and
/// `AT_PHDR`/`AT_RANDOM`/`AT_EXECFN` reference. Reuses the native ABI's
/// stack address range (`USER_STACK_BASE`/`USER_STACK_TOP`) and leaves
/// `process.stack_low` pointing at wherever this layout's floor lands,
/// so `Process::try_grow_stack` still works if the program needs more
/// stack than this initial layout used.
fn setup_linux_stack(
    process: &mut Process,
    pml4: u64,
    elf_bytes: &[u8],
    program: &elf::Program,
    at_base: u64,
    argv: &[&str],
    envp: &[&str],
) -> Result<u64, &'static str> {
    // ---- pass 1: serialize the string/phdr/random data blob, recording
    // each item's offset within it (its final vaddr isn't known until
    // the pointer/auxv table's size — computed next — fixes where this
    // blob starts). ----
    let mut data: Vec<u8> = Vec::new();
    let push_cstr = |data: &mut Vec<u8>, s: &[u8]| -> u64 {
        let offset = data.len() as u64;
        data.extend_from_slice(s);
        data.push(0);
        offset
    };

    let mut argv_offsets = Vec::with_capacity(argv.len());
    for s in argv {
        argv_offsets.push(push_cstr(&mut data, s.as_bytes()));
    }
    let mut envp_offsets = Vec::with_capacity(envp.len());
    for s in envp {
        envp_offsets.push(push_cstr(&mut data, s.as_bytes()));
    }
    let execfn_off = argv_offsets.first().copied();

    // AT_PHDR: a normal toolchain's output (hello, busybox, ...) covers
    // the program header table within its own first PT_LOAD segment —
    // the same thing a real Linux kernel's loader assumes: it never
    // copies phdrs elsewhere, just adds the segment's load bias to
    // `e_phoff`. `program.segments` here already carries that bias (see
    // `apply_pie_bias`/`spawn_linux`), so this can point straight at the
    // real, already-mapped bytes. MachaOS's own minimal test binaries
    // (`user/linker.ld` places `.result` before anything else) are the
    // exception — nothing covers file offset `phoff` there — so those
    // fall back to embedding a copy in this stack blob instead, same as
    // before this comment. Getting this wrong isn't just cosmetic: a
    // real ld.so uses AT_PHDR to work out the main binary's own load
    // bias (comparing it against the phdrs' link-time vaddr), so a
    // wrong AT_PHDR sends it looking for the binary's other segments
    // (e.g. its PT_NOTE) at a wildly wrong address — this was a real
    // bug caught by testing against a real dynamically-linked binary,
    // not a hypothetical.
    let phoff = program.phoff;
    let phdr_len = (program.phentsize as u64) * (program.phnum as u64);
    let phdr_in_segment = program
        .segments
        .iter()
        .find(|s| s.file_offset <= phoff && phoff + phdr_len <= s.file_offset + s.filesz)
        .map(|s| s.vaddr + (phoff - s.file_offset));
    let phdr_blob_off = match phdr_in_segment {
        Some(_) => None,
        None => {
            let phdr_bytes = elf_bytes
                .get(phoff as usize..(phoff + phdr_len) as usize)
                .ok_or("program header table outside file")?;
            let off = data.len() as u64;
            data.extend_from_slice(phdr_bytes);
            Some(off)
        }
    };

    // 16 bytes for AT_RANDOM. Not cryptographically random — there's no
    // HW RNG driver yet — just distinct-enough bytes to fill the ABI
    // slot a real libc expects to be able to read (e.g. for its stack
    // protector canary).
    let random_off = data.len() as u64;
    let seed = interrupts::ticks() ^ program.entry;
    for i in 0..16u64 {
        data.push((seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i) >> 33) as u8);
    }

    // ---- pass 2: the pointer/auxv table. AT_PHDR/AT_RANDOM/AT_EXECFN's
    // values are filled in once `data_vaddr` (below) is known. ----
    let mut auxv_pairs: Vec<(u64, u64)> = alloc::vec![
        (auxv::AT_PHENT, program.phentsize as u64),
        (auxv::AT_PHNUM, program.phnum as u64),
        (auxv::AT_PAGESZ, paging::PAGE_SIZE),
        (auxv::AT_BASE, at_base),
        (auxv::AT_FLAGS, 0),
        (auxv::AT_ENTRY, program.entry),
        (auxv::AT_UID, 0),
        (auxv::AT_EUID, 0),
        (auxv::AT_GID, 0),
        (auxv::AT_EGID, 0),
        // 0: no CPU feature bits reported. A real ld.so's IFUNC
        // resolvers (glibc picks CPU-optimized memcpy/strlen/... this
        // way) use this to choose an implementation; 0 steers every
        // resolver at the most conservative/baseline one rather than
        // risking a resolver branching on an unset bit it assumed would
        // be there.
        (auxv::AT_HWCAP, 0),
        (auxv::AT_HWCAP2, 0),
        (auxv::AT_CLKTCK, 100), // matches pit.rs's 100 Hz timer
        (auxv::AT_SECURE, 0),
        (auxv::AT_PHDR, phdr_in_segment.unwrap_or(0)),
        (auxv::AT_RANDOM, 0),
    ];
    let phdr_idx = auxv_pairs.len() - 2;
    let random_idx = auxv_pairs.len() - 1;
    let execfn_idx = execfn_off.map(|_| {
        auxv_pairs.push((auxv::AT_EXECFN, 0));
        auxv_pairs.len() - 1
    });
    auxv_pairs.push((auxv::AT_NULL, 0));

    let table_words = 1 + (argv.len() + 1) + (envp.len() + 1) + auxv_pairs.len() * 2;
    let table_len = table_words as u64 * 8;

    let total_len = table_len + data.len() as u64;
    if total_len > USER_STACK_TOP {
        return Err("linux stack layout too large");
    }
    let final_rsp = (USER_STACK_TOP - total_len) & !0xF; // 16-byte align, per the SysV ABI
    let data_vaddr = final_rsp + table_len;

    if let Some(off) = phdr_blob_off {
        auxv_pairs[phdr_idx].1 = data_vaddr + off;
    }
    auxv_pairs[random_idx].1 = data_vaddr + random_off;
    if let (Some(idx), Some(off)) = (execfn_idx, execfn_off) {
        auxv_pairs[idx].1 = data_vaddr + off;
    }

    let mut table = Vec::with_capacity(table_len as usize);
    table.extend_from_slice(&(argv.len() as u64).to_le_bytes());
    for &off in &argv_offsets {
        table.extend_from_slice(&(data_vaddr + off).to_le_bytes());
    }
    table.extend_from_slice(&0u64.to_le_bytes());
    for &off in &envp_offsets {
        table.extend_from_slice(&(data_vaddr + off).to_le_bytes());
    }
    table.extend_from_slice(&0u64.to_le_bytes());
    for &(t, v) in &auxv_pairs {
        table.extend_from_slice(&t.to_le_bytes());
        table.extend_from_slice(&v.to_le_bytes());
    }
    debug_assert_eq!(table.len() as u64, table_len);

    // ---- map the region and write both blobs into it ----
    let region_start = final_rsp & !(paging::PAGE_SIZE - 1);
    if region_start < USER_STACK_LOW_LIMIT {
        return Err("linux stack layout too large");
    }
    let region_len = align_up(USER_STACK_TOP - region_start, paging::PAGE_SIZE);
    // Registers the whole growable region as a single Mapping (see
    // `stack_phys_base`'s doc comment for why that matters for a
    // syscall pointer's validation, not just ring-3 access).
    process.init_stack_backing()?;
    let phys = process.stack_phys_for(region_start);
    unsafe {
        let table_phys = phys + (final_rsp - region_start);
        core::ptr::copy_nonoverlapping(table.as_ptr(), table_phys as *mut u8, table.len());
        let data_phys = phys + (data_vaddr - region_start);
        core::ptr::copy_nonoverlapping(data.as_ptr(), data_phys as *mut u8, data.len());
    }
    if !paging::map_range_in(
        pml4,
        region_start,
        phys,
        region_len,
        paging::PAGE_PRESENT | paging::PAGE_WRITABLE | paging::PAGE_USER | paging::PAGE_NX,
        &mut process.frames,
    ) {
        return Err("failed to map linux-style stack");
    }
    process.stack_low = region_start;
    Ok(final_rsp)
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
    close_fds_on_exit();
    task::mark_current_exited(ExitInfo::Normal);
    // Phase 8b: a child's death pends SIGCHLD on its parent (Linux
    // semantics — the parent is usually wait4-polling, but a caught
    // SIGCHLD is delivered at the next timer tick). `deliver_signal`
    // drops it for SIG_DFL/SIG_IGN dispositions.
    if let Some(parent) = task::process_parent(task::current_pid()) {
        let _ = deliver_signal(parent, 17);
    }
    loop {
        interrupts::halt();
    }
}

/// Asm bridge (`enter_usermode`): re-park the exit chain at the task's
/// fixed safe location, below the syscall-stack region — but only for
/// real process tasks. A plain `run_demo` (the selftest's ring-3 smoke
/// test, running in the kernel's own main task) must keep its natural
/// chain — the ret through the caller's frame back into `run_demo`'s
/// continuation — or its demo's `exit` would jump straight to
/// `exit_self` and kill the main task.
#[unsafe(no_mangle)]
extern "C" fn ring3_repark_chain() {
    if crate::task::current_is_process() {
        task::repark_exit_chain(exit_self as extern "C" fn() -> ! as usize);
    }
}

/// Drops the current process's fd table *at exit* (not at reap), the way
/// real Linux closes fds when a process dies: a pipe's write end closing
/// is what makes a reader see EOF, and an exited-but-unreaped child must
/// not keep its pipe ends alive. Safe for the exit path — nothing after
/// this point reads the fd table (`read_result`/`wait` don't use it).
pub fn close_fds_on_exit() {
    let _ = task::with_current_process_mut(|p| {
        p.fds.clear();
    });
}

/// Exit stub for a CLONE_VM thread (see `task::spawn_child`): clears the
/// `CLONE_CHILD_CLEARTID` word and wakes the join futex (what
/// `pthread_join` blocks on), then marks just this task exited — the
/// shared address space stays alive for the owner and its other threads.
extern "C" fn thread_exit_self() -> ! {
    if let Some(addr) = task::current_child_tid() {
        if let Some(phys) = translate(task::current_pid(), addr, 8) {
            unsafe {
                core::ptr::write(phys as *mut u64, 0);
            }
            // Same futex key the joiner blocked on (sys_futex keys by
            // cr3 ^ uaddr — CLONE_VM threads share the cr3).
            let cr3 = crate::task::current_process_cr3().unwrap_or(0);
            task::wake_all(cr3 ^ addr, usize::MAX);
        }
    }
    if crate::syscall::exit_group_flag() {
        close_fds_on_exit();
        task::mark_current_exited_group(ExitInfo::Normal);
    } else {
        task::mark_current_exited(ExitInfo::Normal);
    }
    loop {
        interrupts::halt();
    }
}

/// The low 12 bits (flags) of the PTE mapping `vaddr` in the address
/// space rooted at `cr3`; 0 if unmapped. Used by `fork_copy` so the
/// child's pages get the same R/W/X attributes the parent's have
/// (`Mapping` doesn't record them).
fn read_pte_flags(cr3: u64, vaddr: u64) -> u64 {
    let lvl = |e: u64| e & 0x000f_ffff_ffff_f000;
    unsafe {
        let pml4e = core::ptr::read((cr3 as *const u64).add(((vaddr >> 39) & 0x1ff) as usize));
        if pml4e & 1 == 0 {
            return 0;
        }
        let pdpte = core::ptr::read((lvl(pml4e) as *const u64).add(((vaddr >> 30) & 0x1ff) as usize));
        if pdpte & 1 == 0 {
            return 0;
        }
        let pde = core::ptr::read((lvl(pdpte) as *const u64).add(((vaddr >> 21) & 0x1ff) as usize));
        if pde & 1 == 0 {
            return 0;
        }
        if pde & (1 << 7) != 0 {
            return 0; // 2 MiB page: never used for process mappings
        }
        let pte = core::ptr::read((lvl(pde) as *const u64).add(((vaddr >> 12) & 0x1ff) as usize));
        pte & 0xfff
    }
}

/// Deep copy of a process for `fork`: a fresh address space with every
/// mapping's contents copied into new frames (same virtual addresses,
/// same page attributes), and a duplicated fd table. `MAP_SHARED`
/// (`shm_id: Some`) mappings map the *same* frames in the child, as real
/// `fork` semantics require. The child's `user_rsp` is meaningless (a
/// fork child resumes at the parent's syscall return via
/// `task::spawn_child`), so it starts at 0. Returns `(process, cr3)`.
fn fork_copy(parent: &Process) -> Result<(Process, u64), &'static str> {
    let mut child = Process {
        entry: parent.entry,
        user_rsp: 0,
        frames: Vec::new(),
        mappings: Vec::new(),
        state: ProcessState::Running,
        exit_info: None,
        inbox: None,
        fds: parent
            .fds
            .iter()
            .map(|f| f.as_ref().and_then(FdEntry::dup))
            .collect(),
        stack_low: parent.stack_low,
        stack_phys_base: 0, // set from the copied stack mapping below
        abi: parent.abi,
        heap_start: parent.heap_start,
        heap_end: parent.heap_end,
        mmap_next: parent.mmap_next,
        cloexec: parent.cloexec.clone(),
        parent: parent.parent,
        sigactions: alloc::boxed::Box::new(*parent.sigactions),
        sig_blocked: parent.sig_blocked,
        sig_pending: 0,
    };
    let pml4 = build_address_space(&mut child)?;
    let parent_cr3 = paging::read_cr3();
    for m in &parent.mappings {
        if let Some(shm_id) = m.shm_id {
            let flags = read_pte_flags(parent_cr3, m.vaddr) | paging::PAGE_PRESENT;
            if !paging::map_range_in(pml4, m.vaddr, m.phys, m.len, flags, &mut child.frames) {
                return Err("fork: shared mapping failed");
            }
            child.mappings.push(*m);
            continue;
        }
        let pages = (m.len / paging::PAGE_SIZE) as usize;
        let Some(phys) = pmm::alloc_contiguous(pages) else {
            return Err("fork: out of memory");
        };
        for i in 0..pages {
            child.frames.push(phys + i * pmm::FRAME_SIZE);
        }
        unsafe {
            core::ptr::copy_nonoverlapping(m.phys as *const u8, phys as *mut u8, m.len as usize);
        }
        // Map *per page* with each page's own attributes: a single
        // Mapping can span text (exec, no W) and data (RW, NX) — the
        // uniform first-page flags would make the child's data pages
        // read-only (a write then faults P=1,W=1,U=1) or its text pages
        // writable. The stack mapping's demand-grown region below the
        // initial stack is the kernel 2 MiB split's supervisor copies
        // (P|W only) — those pages stay supervisor and untouched, and
        // the first user page supplies the real stack attributes.
        let mut page = m.vaddr;
        let mut page_idx = 0usize;
        while page < m.vaddr + m.len {
            let flags = read_pte_flags(parent_cr3, page);
            if flags & paging::PAGE_PRESENT != 0 {
                if !paging::map_range_in(
                    pml4,
                    page,
                    phys as u64 + (page_idx as u64) * paging::PAGE_SIZE,
                    paging::PAGE_SIZE,
                    flags,
                    &mut child.frames,
                ) {
                    return Err("fork: mapping copy failed");
                }
            }
            page += paging::PAGE_SIZE;
            page_idx += 1;
        }
        child.mappings.push(Mapping {
            vaddr: m.vaddr,
            phys: phys as u64,
            len: m.len,
            shm_id: None,
        });
        if m.vaddr == USER_STACK_LOW_LIMIT {
            child.stack_phys_base = phys as u64;
        }
    }
    Ok((child, pml4))
}

/// Writes `value` to the 8 bytes at user address `addr` of the current
/// process (used for `CLONE_PARENT_SETTID` / `CLONE_CHILD_SETTID`).
fn write_user_u64(addr: u64, value: u64) {
    if let Some(phys) = translate(task::current_pid(), addr, 8) {
        unsafe {
            core::ptr::write(phys as *mut u64, value);
        }
    }
}

/// Entry point for the asm's fork/clone/vfork special path (see
/// `syscall::sys_forkish`): creates the child task — a deep-copied
/// process for `fork`/`vfork`, a `CLONE_VM` thread sharing the parent's
/// address space for `clone` — and returns its pid for the parent. The
/// child resumes in ring 3 at the parent's syscall return point with
/// `rax = 0`, via the fabricated frame in `task::spawn_child`.
pub fn handle_forkish(
    num: u64,
    user_rip: u64,
    user_rflags: u64,
    user_rsp: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
    callee_saved: [u64; 6],
) -> Result<u64, &'static str> {
    let parent_pid = task::current_pid();
    let parent_cr3 = task::current_process_cr3().ok_or("not a process")?;
    let parent_fs = task::current_fs_base();
    match num {
        crate::linux_abi::SYS_CLONE => {
            const CLONE_VM: u64 = 0x100;
            const CLONE_SETTLS: u64 = 0x80000;
            const CLONE_PARENT_SETTID: u64 = 0x100000;
            const CLONE_CHILD_SETTID: u64 = 0x1000000;
            const CLONE_CHILD_CLEARTID: u64 = 0x200000;
            let flags = a1;
            let stack = a2;
            let parent_tid = a3;
            let child_tid = a4;
            let tls = a5;
            if flags & CLONE_VM == 0 {
                return Err("clone without CLONE_VM unsupported");
            }
            let child_fs = if flags & CLONE_SETTLS != 0 { tls } else { parent_fs };
            let pid = task::spawn_child(
                "thread",
                parent_cr3,
                Some(parent_pid),
                None,
                user_rip,
                user_rflags,
                stack,
                [a1, a2, a3, a4, a5, a6],
                callee_saved,
                child_fs,
                if flags & CLONE_CHILD_CLEARTID != 0 {
                    Some(child_tid)
                } else {
                    None
                },
                thread_exit_self as extern "C" fn() -> ! as usize,
            ) as u64;
            if flags & CLONE_PARENT_SETTID != 0 {
                write_user_u64(parent_tid, pid);
            }
            if flags & CLONE_CHILD_SETTID != 0 {
                write_user_u64(child_tid, pid);
            }
            Ok(pid)
        }
        crate::linux_abi::SYS_FORK | crate::linux_abi::SYS_VFORK => {
            let (mut child_process, child_cr3) =
                task::with_current_process_mut(|p| fork_copy(p)).ok_or("not a process")??;
            child_process.parent = Some(task::current_pid());
            let child_pid = task::spawn_child(
                "fork-child",
                child_cr3,
                None,
                Some(child_process),
                user_rip,
                user_rflags,
                user_rsp,
                [a1, a2, a3, a4, a5, a6],
                callee_saved,
                parent_fs,
                None,
                exit_self as extern "C" fn() -> ! as usize,
            ) as u64;
            Ok(child_pid)
        }
        _ => Err("not a forkish syscall"),
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
    close_fds_on_exit();
    task::mark_current_exited(ExitInfo::PageFault { cr2 });
    let rip = frame.rip;
    redirect_to_exit(frame);

    let mut buf = [0u8; 700];
    let msg = io::sprint(
        &mut buf,
        format_args!(
            "[PROC] {} killed by page fault at {:#x} (error {:#x}, rip={:#x}, r8={:#x}, r9={:#x}, r10={:#x}, r11={:#x}, r12={:#x}, r13={:#x}, r14={:#x}, r15={:#x}, rbx={:#x}, rbp={:#x}, rdx={:#x}, rcx={:#x}, rdi={:#x}, rsi={:#x}, rsp={:#x})\n",
            crate::task::current_name(), cr2, frame.error_code, rip, frame.r8, frame.r9, frame.r10, frame.r11,
            frame.r12, frame.r13, frame.r14, frame.r15, frame.rbx, frame.rbp,
            frame.rdx, frame.rcx, frame.rdi, frame.rsi, frame.rsp
        ),
    );
    io::exception_print(msg);
}

/// Signal numbers whose default disposition is to be ignored (Linux's
/// default-ignore set): SIGCHLD(17), SIGCONT(18), SIGURG(23),
/// SIGWINCH(28). Every other signal with a SIG_DFL disposition
/// terminates the process.
pub fn signal_default_ignored(sig: u8) -> bool {
    matches!(sig, 17 | 18 | 23 | 28)
}

/// Delivers signal `sig` to the process at task index `pid` (Phase 8a
/// subset — default-action delivery only):
/// - SIGKILL(9) always terminates (uncatchable, unblockable);
/// - SIG_IGN dispositions and default-ignored signals are dropped;
/// - SIG_DFL-terminate signals mark the process exited (`Signaled`);
/// - caught-handler and blocked signals are recorded pending (handler
///   delivery is a later phase).
/// Returns `Ok(())` if the target existed and was alive; `Err(())` if
/// there is no such process or it has already exited.
pub fn deliver_signal(pid: usize, sig: u8) -> Result<(), ()> {
    crate::task::with_process_mut(pid, |p| {
        if p.is_exited() {
            return Err(());
        }
        if sig == 9 {
            p.mark_exited(ExitInfo::Signaled { sig });
            return Ok(());
        }
        let blocked = (p.sig_blocked >> sig) & 1 == 1;
        let action = p.sigactions[sig as usize];
        if blocked {
            p.sig_pending |= 1u64 << sig;
        } else if action.handler == 1 {
            // SIG_IGN — dropped, like Linux
        } else if action.handler == 0 {
            // SIG_DFL: terminate unless the signal is default-ignored
            if !signal_default_ignored(sig) {
                p.mark_exited(ExitInfo::Signaled { sig });
            }
        } else {
            // A caught handler — not deliverable yet; record pending.
            p.sig_pending |= 1u64 << sig;
        }
        Ok(())
    })
    .ok_or(())?
}

/// Size of the kernel's signal frame, pushed on the ring-3 stack at
/// delivery and consumed by `rt_sigreturn`. The handler's `ret` pops the
/// restorer (offset 0 — the lowest address), so the return address sits
/// at the *bottom* of the frame:
///   +0  restorer      — the sigreturn trampoline (`USER_EXIT_STUB_VIRT + 16`)
///   +8  saved_rip
///   +16 saved_rflags
///   +24 saved_rsp     — the interrupted rsp (frame base + SIGFRAME_SIZE)
///   +32 saved_sigmask — the blocked mask to restore
///   +40 sig           — informational (the handler gets it in rdi)
///   +48..+160 the 15 GP registers (rax rbx rcx rdx rsi rdi rbp r8-r15),
///   saved because the handler — and the rt_sigreturn syscall — clobber
///   every caller-saved register: without them the interrupted code
///   resumes with the handler's leftovers (e.g. rdi = sig), which
///   busybox's `kill -CHLD` caught as a #PF at `movl 0x8c(%rdi), %eax`
///   (CR2 = sig + 0x8c).
///   Segment registers are *not* recorded: ds/es/gs are flat 0x33 for
///   every process and fs must survive sigreturn untouched (its hidden
///   base is the TLS block set via arch_prctl — see the sigreturn asm),
///   so a handler that itself changes fs would keep its change.
pub const SIGFRAME_SIZE: u64 = 168;

/// How much of the frame `rt_sigreturn` reads: the handler's `ret` has
/// already consumed the restorer slot, so the remaining 20 qwords sit
/// at the current ring-3 rsp.
pub(crate) const SIGFRAME_REMAINDER: u64 = 160;

/// The `mov eax, SYS_RT_SIGRETURN; syscall` stub in the syscall-stub page
/// (`map_syscall_stubs`) — a handler's `ret` lands here.
const SIGRETURN_TRAMPOLINE: u64 = USER_EXIT_STUB_VIRT + 16;

/// Called from the timer ISR for the current task: if it is a ring-3
/// process with a pending, unblocked signal, deliver it by redirecting
/// the interrupt frame. A caught handler gets a sigframe pushed on the
/// ring-3 stack and the frame's rip/rsp redirected into the handler
/// (the handler's `ret` lands on the sigreturn trampoline, whose
/// rt_sigreturn syscall restores the interrupted context); a SIG_DFL
/// signal terminates the process; a SIG_IGN one is dropped. One signal
/// per call — the rest stay pending for the next tick.
pub fn deliver_pending_signals(frame: &mut interrupts::InterruptFrame) {
    if frame.cs & 3 != 3 {
        return; // mid-syscall (kernel frame) — deliver when back in ring 3
    }
    let current = crate::task::current_pid();
    crate::task::with_process_mut(current, |p| {
        if p.is_exited() {
            return;
        }
        let pending = p.sig_pending & !p.sig_blocked;
        if pending == 0 {
            return;
        }
        let sig = pending.trailing_zeros() as u8;
        if !(1..=64).contains(&sig) {
            p.sig_pending = 0;
            return;
        }
        let action = p.sigactions[sig as usize];
        if action.handler == 1 {
            // SIG_IGN — drop the pending signal.
            p.sig_pending &= !(1u64 << sig);
            return;
        }
        if action.handler == 0 {
            // SIG_DFL — terminate (default-ignored signals never reach
            // here: `deliver_signal` drops them at kill time).
            p.sig_pending &= !(1u64 << sig);
            p.mark_exited(ExitInfo::Signaled { sig });
            return;
        }
        // Caught handler: push the sigframe below the interrupted rsp
        // and redirect the frame into the handler.
        let user_rsp = frame.rsp;
        let frame_base = user_rsp - SIGFRAME_SIZE;
        let Some(phys) = crate::syscall::resolve_user_buffer(frame_base, SIGFRAME_SIZE) else {
            return; // frame unmappable — leave the signal pending
        };
        let old_mask = p.sig_blocked;
        let words = [
            SIGRETURN_TRAMPOLINE, // +0  restorer — the handler's `ret` target
            frame.rip,            // +8  saved_rip
            frame.rflags,         // +16 saved_rflags
            user_rsp,             // +24 saved_rsp
            old_mask,             // +32 saved_sigmask
            sig as u64,           // +40 sig
            frame.rax,            // +48
            frame.rbx,            // +56
            frame.rcx,            // +64
            frame.rdx,            // +72
            frame.rsi,            // +80
            frame.rdi,            // +88
            frame.rbp,            // +96
            frame.r8,             // +104
            frame.r9,             // +112
            frame.r10,            // +120
            frame.r11,            // +128
            frame.r12,            // +136
            frame.r13,            // +144
            frame.r14,            // +152
            frame.r15,            // +160
        ];
        unsafe {
            core::ptr::copy_nonoverlapping(
                words.as_ptr() as *const u8,
                phys as *mut u8,
                SIGFRAME_SIZE as usize,
            );
        }
        // The signal (and the handler's sa_mask) stay blocked while the
        // handler runs; `rt_sigreturn` restores the saved mask.
        p.sig_blocked = old_mask | (1u64 << sig) | action.mask;
        p.sig_pending &= !(1u64 << sig);
        frame.rip = action.handler;
        frame.rsp = frame_base;
        frame.rdi = sig as u64;
    });
}

/// Called from #DE/#UD/#GP (and similar) when the faulting task is a
/// process: a ring-3 program hitting a divide-by-zero, invalid opcode, or
/// privileged instruction kills just that process, the same way a page
/// fault does, rather than taking down the kernel.
pub fn kill_current_exception(vector: u8, frame: &mut interrupts::InterruptFrame) {
    task::mark_current_exited(ExitInfo::Exception { vector });
    let rip = frame.rip;
    redirect_to_exit(frame);

    let mut buf = [0u8; 96];
    let message = io::sprint(
        &mut buf,
        format_args!("[PROC] killed by exception vector {} at rip={:#x}\n", vector, rip),
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
    let phys = translate(pid, PROC_RESULT_VIRT, 8)?;
    Some(unsafe { core::ptr::read_volatile(phys as *const u64) })
}

/// Translates `[vaddr, vaddr+len)` in `pid`'s address space to a physical
/// address, only if the whole range is covered by one of its mappings
/// (segments, stack, or exit trampoline — never the deep-copied kernel
/// region, which isn't in `mappings`). Used both for `read_result` and by
/// `syscall.rs` to validate every pointer a syscall receives from
/// ring-3 code before the kernel reads or writes through it: without
/// this, a process could pass a pointer into its own copy of the kernel's
/// map and have the kernel read/corrupt arbitrary kernel memory on its
/// behalf.
pub fn translate(pid: usize, vaddr: u64, len: u64) -> Option<u64> {
    let end = vaddr.checked_add(len)?;
    let mappings = task::process_mappings(pid)?;
    // Newest mapping wins — see `translate_local`; first-match would
    // resolve shadowed (pre-MAP_FIXED) VMAs to the wrong physical frames.
    let mapping = mappings.iter().rev().find(|m| m.vaddr <= vaddr && end <= m.vaddr + m.len)?;
    Some(mapping.phys + (vaddr - mapping.vaddr))
}

/// Like `translate`, but for a syscall pointer specifically: if
/// `[vaddr, vaddr+len)` isn't mapped yet, and `pid` is the *current*
/// process, grows the stack to cover it first (`Process::try_grow_stack`)
/// before giving up.
///
/// A real Linux `copy_to_user` writing to a stack buffer the caller has
/// never touched — `struct stat st; fstat(fd, &st);` is the ordinary
/// case, not an edge case — takes a real page fault the same as any
/// other user-mode access and demand-pages it in right there. This
/// kernel's syscall handlers instead write through the *physical*
/// address `translate` resolves, bypassing the process's own page table
/// (and so its #PF handler) entirely — which means an address legitimately
/// within the stack's reserved growth region, just not grown into yet,
/// previously came back EFAULT instead of succeeding. Caught by testing
/// against a real dynamically-linked binary (`ld.so`'s own `fstat` on a
/// stack-local buffer), not a hypothetical.
///
/// Doesn't handle a range that straddles the *old* stack floor (partly
/// already mapped, partly needing growth) — `translate` after growing
/// still needs a single `Mapping` to cover the whole range, and growth
/// creates its own separate entry rather than merging into the adjacent
/// one. Narrow gap: only matters for a buffer landing exactly on that
/// boundary, and the common case (the whole buffer newly needed) works.
pub fn resolve_syscall_ptr(pid: usize, vaddr: u64, len: u64) -> Option<u64> {
    if let Some(phys) = translate(pid, vaddr, len) {
        return Some(phys);
    }
    if pid != task::current_pid() {
        return None;
    }
    let cr3 = task::current_process_cr3()?;
    if !task::with_current_process_mut(|process| process.try_grow_stack(cr3, vaddr))? {
        return None;
    }
    translate(pid, vaddr, len)
}

/// Delivers a message to `pid`'s inbox from kernel context (as opposed to
/// `sys_send`, which delivers from another process). Used to hand a
/// process its first message before it starts running.
pub fn send_from_kernel(pid: usize, bytes: &[u8]) -> bool {
    task::deliver_message(pid, bytes)
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
        ExitInfo::Signaled { sig } => {
            use alloc::format;
            format!("killed by signal {}", sig)
        }
    }
}
