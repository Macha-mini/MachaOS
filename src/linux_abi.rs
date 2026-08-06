//! Linux x86_64 syscall ABI layer. Phase 2 of the approved Linux ABI
//! emulation plan: enough real syscalls for a statically-linked musl
//! binary's startup and basic I/O to work.
//!
//! Selected per-process via `process::Abi::Linux` (see
//! `process::spawn_linux`) and dispatched by `syscall::syscall_dispatch`,
//! which checks the current process's `Abi` before falling into the
//! native table.
//!
//! `syscall_entry`'s asm forwards `num` plus all six real Linux syscall
//! arguments (five in registers, the sixth — e.g. `mmap`'s file offset —
//! pushed on the stack the way a real 7-argument SysV call would be) to
//! whichever dispatch function handles a syscall.
//!
//! Phase 3 adds the handful of extra syscalls a static **glibc** binary's
//! startup wants beyond musl's smaller set: `futex` (glibc's malloc/loader
//! locks take this path even single-threaded, at least once, to acquire
//! an uncontended lock), `rseq` (glibc probes for it and falls back
//! cleanly if it's refused), `prlimit64`, `sched_getaffinity`, `sysinfo`.
//!
//! Phase 4 makes `mmap` genuinely file-backed (needed for `ld.so` to map
//! a shared library's segments from their real file bytes rather than
//! zeroed anonymous memory) and adds `PT_INTERP` support to
//! `process::spawn_linux` so a dynamically-linked binary's *real*
//! interpreter runs it, the same way an actual Linux kernel never
//! implements the ELF relocator itself.
//!
//! Phase 5 adds `memfd_create`/`ftruncate` (`shm.rs`) and `socket`/
//! `connect`/`sendmsg`/`recvmsg` (`socket.rs`, `AF_UNIX`/`SOCK_STREAM`
//! only) plus `mmap`'s `MAP_SHARED` path (`process::Process::
//! mmap_shared`) — real shared memory and real `SCM_RIGHTS` fd-passing,
//! the two pieces a `wl_shm`-based GUI client needs beyond everything
//! Phase 2-4 already had. See `wayland.rs` for the kernel-native
//! compositor task these exist for, and its own module docs for how far
//! "GUI apps" actually goes here (a hand-designed, Wayland-*inspired*
//! wire subset, not the real protocol). Every syscall this phase adds
//! is non-blocking by construction — see `socket.rs`'s module docs on
//! why a syscall handler can never wait the way `process::wait` does.
//!
//! Phase 6 (finishing real dynamic linking — see the plan file's
//! extended roadmap) found and fixed the real root cause of the crash
//! documented below in earlier phases' commits: `syscall_entry`
//! (`syscall.rs`) never restored the caller's rdi/rsi/rdx/r10/r8/r9
//! after `syscall_dispatch` returned — only rax (the result) and rcx/r11
//! (clobbered by the `syscall`/`sysretq` instructions themselves) made
//! it back to user space unchanged; the other five held whatever
//! `syscall_dispatch`'s own internal computation last left in those
//! physical registers. Real Linux's syscall ABI guarantees all of them
//! survive unchanged (the kernel saves/restores the full register set),
//! and real glibc code relies on that guarantee in ways this repo's own
//! hand-written test programs never exercised — this repo's own
//! `common::syscall` wrapper (`user/src/common.rs`) has always marked
//! them clobbered, matching this kernel's old, non-compliant behavior,
//! which is exactly why nothing caught this until testing against a
//! real, unmodified glibc binary's `ld.so`. Confirmed by tracing: ld.so
//! caches `__rseq_offset` live in r8 across the `rseq(2)` syscall during
//! its own TLS setup and reuses it immediately afterward on the error
//! path without reloading it; this kernel's dispatch clobbered r8
//! computing something unrelated, and the stale value (observed
//! directly in the CPU register at fault time via `frame.r8`, now
//! logged permanently in `process::kill_current`) sent the fallback
//! write to a wild address. Fixing this removed the crash entirely.
//!
//! Phase 6 also turns on real NX enforcement: `EFER.NXE` (`syscall::init`)
//! plus `paging::PAGE_NX` on every mapped page this module doesn't mark
//! executable (`mmap`'s `PROT_EXEC`, `mprotect`'s `PROT_EXEC`, and
//! `process::load_segments` reading each ELF segment's real `PF_X` —
//! previously every present page was silently executable regardless of
//! what was asked for, since the bit was never set and the CPU was never
//! told to check it either).
//!
//! KNOWN GAPS left beyond Phase 4:
//! - No signal delivery: `rt_sigaction`/`rt_sigprocmask` just record
//!   nothing and return success.
//! - File-backed `mmap` is read-only-effectively: writes to a
//!   `MAP_PRIVATE` file mapping are never written back (copy-on-write
//!   without the "write" part mattering to anything but the process's
//!   own view, which is already true since frames aren't shared between
//!   processes) — fine for loading code/rodata, which is all `ld.so`
//!   needs this for. `MAP_SHARED` isn't distinguished from
//!   `MAP_PRIVATE` at all.
//! - `munmap` only reclaims a mapping it fully contains (see
//!   `process::Process::munmap`).
//! - `futex` never actually blocks or wakes anything (see `sys_futex`'s
//!   doc comment) — fine for the single-threaded, uncontended-lock case
//!   glibc's own startup hits, but `clone`/real threading aren't
//!   implemented at all, so this is the extent of it for now.
//! - A real dynamically-linked glibc binary (tested against GNU Hello,
//!   real coreutils `true`/`cat`, and a real `ld.so`/`libc.so.6` pair —
//!   see `shell.rs`'s selftest hook) still doesn't run to completion, but
//!   two real bugs found and fixed this phase got it *substantially*
//!   further than before:
//!   1. The register-preservation fix above removed a crash during
//!      `ld.so`'s TLS/rseq setup.
//!   2. `st_dev`/`st_ino` were always `0` for every file (`fstat`/
//!      `newfstatat` zeroed the whole `struct stat` and only ever filled
//!      in `st_mode`/`st_size`/etc.) — `ld.so` dedups a shared library
//!      it's about to load against every already-loaded map by comparing
//!      `(st_dev, st_ino)`, and with every file reporting the same
//!      (zero) pair, `libc.so.6` looked like the *same file* as the main
//!      executable already sitting in `ld.so`'s loaded-map list — so
//!      `ld.so` silently reused hello's own `link_map` instead of ever
//!      mapping `libc.so.6`'s real segments (explaining, in hindsight,
//!      the `"hello: hello: no version information available (required
//!      by hello)"` output an earlier round of this investigation found:
//!      the *library* name in that message really was hello's own).
//!      Fixed by giving every open file a synthetic but stable, distinct
//!      `st_ino` (a path hash — see `vfs::Stat::ino`'s doc comment) and a
//!      fixed nonzero `st_dev` (there's only one filesystem here).
//!
//!   With both fixed, `ld.so` now does what it never did in any prior
//!   session: it genuinely `mmap`s all four of `libc.so.6`'s real
//!   `PT_LOAD` segments at the correct addresses with the correct
//!   permissions (traced directly — reservation `mmap` picks a base via
//!   this kernel's own bump allocator, then each segment lands at
//!   `base + file-declared-offset` via `MAP_FIXED`, matching
//!   `libc.so.6`'s real program headers exactly) and walks into real
//!   `GNU_HASH`-based symbol table lookup for `hello`'s first undefined
//!   symbol, `__libc_start_main` — further than this project has ever
//!   gotten a real glibc binary.
//!
//!   It still doesn't complete: it now faults *inside* that symbol
//!   lookup (disassembled — `llvm-objdump` — down to the exact
//!   instruction, and cross-checked against real glibc source,
//!   `elf/dl-lookup.c`'s `do_lookup_x`/`check_match`, `glibc-2.36` tag).
//!   The faulting pointer is provably `libc.so.6`'s own real dynamic
//!   symbol table base (`0x8a50`, confirmed against this exact
//!   `libc.so.6`'s `.dynsym` section address) plus a plausible symbol
//!   index times `sizeof(Elf64_Sym)` — i.e. `do_lookup_x`'s
//!   `D_PTR(map, l_info[DT_SYMTAB])` computation, which real glibc
//!   defines as `map->l_addr + <link-time symtab address>`, but *without
//!   `l_addr` (`libc.so.6`'s real load bias, `0x40000000` — confirmed
//!   correct at `mmap` time, from the segment addresses above) added* —
//!   as if `map->l_addr` reads back `0` at the exact moment this
//!   specific lookup runs, despite the same map's `GNU_HASH` bucket/
//!   chain walk (which *did* need a correctly-biased pointer to find a
//!   plausible symbol index at all) apparently working. A scan of
//!   `ld.so`'s own data/BSS region turned up no stored copy of
//!   `0x40000000` anywhere, arguing against "read from the wrong
//!   struct" and toward "the assignment never ran for whatever
//!   `link_map` this particular access reads." This is real glibc's own
//!   (unmodified, stripped — no symbol table to set a named breakpoint
//!   on) machine code, so further progress here most likely needs
//!   interactive debugging (QEMU's own `-s`/gdbstub plus a GDB-remote-
//!   capable debugger, e.g. `lldb`) to actually watch `do_lookup_x`'s
//!   `map` argument rather than continuing to infer it from disassembly
//!   and raw memory scans — a bigger investment than this round's
//!   static-analysis approach, left as the concrete next step. Every
//!   fault reproduces identically (same faulting instruction, same
//!   unbiased pointer, same symbol) across three independent real
//!   binaries (GNU Hello, coreutils `true`/`cat`), ruling out anything
//!   fixture-specific. Testing against real binaries is exactly what
//!   found and fixed the AT_PHDR/`pread64`/`AT_EMPTY_PATH`/stack-buffer/
//!   `access`(21)/register-preservation/`st_dev`+`st_ino` bugs the rest
//!   of this file's history documents; this is the next one, left for
//!   follow-up — consistent with the plan's own framing of Phase 4/6
//!   dynamic-linking work as roadmap-level rigor rather than Phase 1/2's
//!   full-completion bar.
//! - `socket`/`sendmsg`/`recvmsg` only support `AF_UNIX`/`SOCK_STREAM`,
//!   don't expose `bind`/`listen`/`accept` at all (only `socket.rs`'s
//!   Rust API does — `wayland.rs`'s compositor task is the only thing
//!   that ever calls them, deliberately kernel-native rather than a
//!   second syscall-driven process — see that module's docs), and
//!   `recvmsg` collapses message boundaries (a `recv` that arrives after
//!   several `send`s can return them all concatenated) rather than
//!   honoring real datagram-style framing.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::process::FdEntry;
use crate::shm;
use crate::socket;
use crate::vfs;

// ---- errno -----------------------------------------------------------

const EBADF: i32 = 9;
/// No child processes (or all already reaped) — `waitpid`'s terminal
/// condition; a shell's `waitpid(-1)` loop ends here.
const ECHILD: i32 = 10;
const EFAULT: i32 = 14;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EINVAL: i32 = 22;
const ENOTTY: i32 = 25;
const ESPIPE: i32 = 29;
const ENOSYS: i32 = 38;
const ENOENT: i32 = 2;
const EAGAIN: i32 = 11;
const EIO: i32 = 5;
const ENOMEM: i32 = 12;
const ENOEXEC: i32 = 8;

/// Negates and sign-extends `errno` into the raw `u64` a syscall returns
/// on failure, matching the real Linux convention (small negative values
/// close to 0, e.g. `-38` for `ENOSYS`) rather than MachaOS's native
/// ABI's `u64::MAX` sentinel (`syscall::SYSCALL_ERROR`).
fn err(errno: i32) -> u64 {
    (-(errno as i64)) as u64
}

fn vfs_err(e: vfs::VfsError) -> u64 {
    match e {
        vfs::VfsError::NotFound => err(ENOENT),
        vfs::VfsError::NotDir | vfs::VfsError::IsDir => err(ENOTDIR),
        vfs::VfsError::AlreadyExists => err(EEXIST),
        _ => err(EIO),
    }
}

// ---- syscall numbers (x86_64) -----------------------------------------

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_FSTAT: u64 = 5;
const SYS_LSEEK: u64 = 8;
const SYS_ACCESS: u64 = 21;
const SYS_PREAD64: u64 = 17;
const SYS_MMAP: u64 = 9;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_BRK: u64 = 12;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_IOCTL: u64 = 16;
const SYS_WRITEV: u64 = 20;
const SYS_SYSINFO: u64 = 99;
const SYS_GETPID: u64 = 39;
pub const SYS_EXIT: u64 = 60;
const SYS_UNAME: u64 = 63;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_FUTEX: u64 = 202;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_CLOCK_GETTIME: u64 = 228;
pub const SYS_EXIT_GROUP: u64 = 231;
const SYS_OPENAT: u64 = 257;
const SYS_NEWFSTATAT: u64 = 262;
const SYS_SET_ROBUST_LIST: u64 = 273;
const SYS_PRLIMIT64: u64 = 302;
const SYS_GETRANDOM: u64 = 318;
const SYS_RSEQ: u64 = 334;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_SENDMSG: u64 = 46;
const SYS_RECVMSG: u64 = 47;
const SYS_FTRUNCATE: u64 = 77;
const SYS_MEMFD_CREATE: u64 = 319;
// Phase 7: clone/fork/vfork are intercepted by the asm special path (see
// syscall.rs is_forkish_syscall / sys_forkish) before syscall_dispatch;
// the numbers are still declared here for the dispatch table and the
// asm's checks.
pub const SYS_CLONE: u64 = 56;
pub const SYS_FORK: u64 = 57;
pub const SYS_VFORK: u64 = 58;
const SYS_EXECVE: u64 = 59;
const SYS_WAIT4: u64 = 61;
const SYS_DUP: u64 = 32;
const SYS_DUP2: u64 = 33;
/// x86_64's own `dup2` number (32/33 are the i386 numbers; musl uses 63
/// on this arch).
const SYS_DUP2_64: u64 = 63;
const SYS_GETTID: u64 = 186;
const SYS_DUP3: u64 = 292;
const SYS_PIPE2: u64 = 293;
/// Legacy `pipe(pipefd)` — BusyBox ash uses this instead of `pipe2`.
const SYS_PIPE: u64 = 22;
/// Legacy `open(path, flags, mode)` — static musl binaries call this
/// instead of `openat` (musl's `open` wrapper uses SYS_open = 2).
const SYS_OPEN: u64 = 2;
/// Legacy `stat(path, buf)` / `lstat(path, buf)` — same layout as
/// `newfstatat`, which is what the modern wrapper ends up calling.
const SYS_STAT: u64 = 4;
const SYS_LSTAT: u64 = 6;
const SYS_GETPPID: u64 = 110;
const AT_FDCWD: u64 = 0xffff_ffff_ffff_ff9c; // -100
/// `fcntl(2)` — BusyBox uses F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL.
const SYS_FCNTL: u64 = 72;
/// `fcntl64` — glibc's 64-bit-offset variant; the commands this kernel
/// implements are identical to `fcntl`.
const SYS_FCNTL64: u64 = 221;

// ---- helpers ------------------------------------------------------------

/// Runs `f` on the current process. Only meaningful for a syscall (the
/// current task is always a process when `syscall_dispatch` is reached
/// through the normal `syscall` path), so `unwrap_or`'s fallback below
/// only matters for `run_demo`'s non-process caller, which never routes
/// here in the first place (see `syscall::syscall_dispatch`).
fn with_process<R>(f: impl FnOnce(&mut crate::process::Process) -> R) -> Option<R> {
    crate::task::with_current_process_mut(f)
}

fn resolve(ptr: u64, len: u64) -> Option<u64> {
    crate::syscall::resolve_user_buffer(ptr, len)
}

/// Reads a NUL-terminated string from user memory, byte at a time (each
/// byte independently validated against the process's mappings — simple
/// and safe, and paths are short, so the extra per-byte lookup cost
/// doesn't matter).
fn read_cstr(ptr: u64, max_len: usize) -> Option<String> {
    let mut bytes = alloc::vec::Vec::new();
    for i in 0..max_len as u64 {
        let phys = resolve(ptr + i, 1)?;
        let b = unsafe { core::ptr::read(phys as *const u8) };
        if b == 0 {
            return core::str::from_utf8(&bytes).ok().map(|s| s.to_string());
        }
        bytes.push(b);
    }
    None
}

/// Normalizes a syscall path argument to the absolute form the FAT32 VFS
/// expects. There's no per-process current-working-directory tracked for
/// a Linux process yet, so a relative path is just anchored at the root
/// — fine for the common case (a program given an absolute path, or run
/// from what it assumes is `/`), wrong for genuine relative-to-cwd use;
/// `dirfd` (openat/newfstatat's first argument) is likewise ignored
/// rather than honored when it isn't `AT_FDCWD`.
fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{}", path)
    }
}

fn translate_open_flags(linux_flags: u64) -> u32 {
    let mut f = 0u32;
    match linux_flags & 0b11 {
        1 => f |= vfs::O_WRONLY,
        2 => f |= vfs::O_RDWR,
        _ => {}
    }
    if linux_flags & 0o100 != 0 {
        f |= vfs::O_CREAT;
    }
    if linux_flags & 0o1000 != 0 {
        f |= vfs::O_TRUNC;
    }
    if linux_flags & 0o2000 != 0 {
        f |= vfs::O_APPEND;
    }
    f
}

// ---- file I/O -----------------------------------------------------------

fn sys_write(fd: u64, buf: u64, count: u64) -> u64 {
    let Some(phys) = resolve(buf, count) else {
        return err(EFAULT);
    };
    let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, count as usize) };
    // An fd-table entry wins over the stdio fallback: a shell redirect
    // dup2's the target onto fd 1/2, and fd 1/2 only mean "the serial"
    // when nothing is installed there.
    let routed = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::File(h)) => Some(match h.write(bytes) {
            Ok(n) => n as u64,
            Err(e) => vfs_err(e),
        }),
        Some(FdEntry::Shm(id, cursor)) => Some(shm_write(*id, cursor, bytes) as u64),
        Some(FdEntry::Socket(id)) => {
            Some(socket::send(*id, bytes, &[]).map(|n| n as u64).unwrap_or(err(EPIPE)))
        }
        Some(FdEntry::Pipe(id, _)) => {
            let n = crate::pipe::write(*id, bytes);
            Some(if n == 0 { err(EPIPE) } else { n as u64 })
        }
        None => None,
    });
    let routed = routed.flatten();
    if let Some(r) = routed {
        return r;
    }
    if fd == 1 || fd == 2 {
        for &b in bytes {
            crate::io::print(core::format_args!("{}", b as char));
        }
        return bytes.len() as u64;
    }
    err(EBADF) // fd 0 (stdin) isn't writable, and nothing else exists
}

fn sys_read(fd: u64, buf: u64, count: u64) -> u64 {
    let Some(phys) = resolve(buf, count) else {
        return err(EFAULT);
    };
    // An fd-table entry wins over the stdio fallback (same rule as
    // `sys_write`): `cat < file` / `echo x | cat` install a file or
    // pipe onto fd 0, and fd 0 only means "the keyboard" when nothing
    // is installed there.
    let routed = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::File(h)) => {
            let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
            Some(h.read(out) as u64)
        }
        Some(FdEntry::Shm(id, cursor)) => {
            let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
            Some(shm_read(*id, cursor, out) as u64)
        }
        Some(FdEntry::Pipe(id, _)) => {
            let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
            Some(crate::pipe::read(*id, out) as u64)
        }
        Some(FdEntry::Socket(id)) => {
            let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
            let (n, fds) = socket::recv(*id, out);
            // A plain `read()` can't carry `SCM_RIGHTS` — any fds that
            // happened to be queued on this message are lost, same as
            // real Linux. Nothing in this kernel's own protocol
            // (`wayland.rs`) ever mixes a data-only `read()` with an
            // fd-bearing message, so this path is a defensive fallback,
            // not a real one.
            for fid in fds {
                shm::close(fid);
            }
            Some(n as u64)
        }
        None => None,
    });
    let routed = routed.flatten();
    if let Some(r) = routed {
        return r;
    }
    if fd == 0 {
        let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
        let mut n = 0usize;
        while n < out.len() {
            match crate::keyboard::next_event() {
                Some(crate::keyboard::Event::Char(c)) => {
                    out[n] = c as u8;
                    n += 1;
                }
                Some(_) => {}
                None => break,
            }
        }
        return n as u64;
    }
    err(EBADF) // fd 1/2 (stdout/stderr) aren't readable
}

fn sys_openat(_dirfd: u64, pathname: u64, flags: u64, _mode: u64) -> u64 {
    let Some(path) = read_cstr(pathname, 256) else {
        return err(EFAULT);
    };
    let path = normalize_path(&path);
    let vfs_flags = translate_open_flags(flags);
    match vfs::FileHandle::open(&path, vfs_flags) {
        Ok(handle) => {
            let fd = with_process(|p| p.alloc_fd(FdEntry::File(handle)) as u64).unwrap_or(err(EBADF));
            fd
        }
        Err(e) => vfs_err(e),
    }
}

/// Copies up to `out.len()` bytes from `id`'s backing starting at
/// `*cursor`, advancing it — `read()`/`pread`-style access to a memfd,
/// direct against its physical frames (identity-mapped, like everything
/// else this kernel touches straight from ring 0).
fn shm_read(id: usize, cursor: &mut usize, out: &mut [u8]) -> usize {
    let (Some(phys), Some(size)) = (shm::phys_of(id), shm::size_of(id)) else {
        return 0;
    };
    let avail = size.saturating_sub(*cursor);
    let n = avail.min(out.len());
    if n > 0 {
        unsafe { core::ptr::copy_nonoverlapping((phys + *cursor) as *const u8, out.as_mut_ptr(), n) };
        *cursor += n;
    }
    n
}

fn shm_write(id: usize, cursor: &mut usize, data: &[u8]) -> usize {
    let (Some(phys), Some(size)) = (shm::phys_of(id), shm::size_of(id)) else {
        return 0;
    };
    let avail = size.saturating_sub(*cursor);
    let n = avail.min(data.len());
    if n > 0 {
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), (phys + *cursor) as *mut u8, n) };
        *cursor += n;
    }
    n
}

fn seek_cursor(cursor: usize, size: usize, from: vfs::SeekFrom) -> usize {
    let base = match from {
        vfs::SeekFrom::Start(p) => p as i64,
        vfs::SeekFrom::Current(p) => cursor as i64 + p,
        vfs::SeekFrom::End(p) => size as i64 + p,
    };
    base.max(0) as usize
}

/// `access(pathname, mode)`: this kernel doesn't track file permissions
/// (`mode`, beyond `F_OK`) meaningfully, so this only ever checks
/// existence — `ld.so` uses `access` for a handful of legacy tunable
/// files (e.g. `/etc/ld.so.nohwcap`) that are expected to usually not
/// exist; ENOENT is the normal, successful-probe answer there, not a
/// failure.
fn sys_access(pathname: u64, _mode: u64) -> u64 {
    let Some(path) = read_cstr(pathname, 256) else {
        return err(EFAULT);
    };
    let path = normalize_path(&path);
    match vfs::FileHandle::open(&path, 0) {
        Ok(_) => 0,
        Err(e) => vfs_err(e),
    }
}

/// Reads a NUL-terminated string from user memory (the old address
/// space — safe to call before an execve frees it).
fn read_user_cstr(ptr: u64) -> Option<alloc::string::String> {
    let mut out = alloc::vec::Vec::new();
    let mut i = 0u64;
    loop {
        let phys = resolve(ptr + i, 1)?;
        let b = unsafe { core::ptr::read(phys as *const u8) };
        if b == 0 {
            break;
        }
        out.push(b);
        i += 1;
    }
    Some(alloc::string::String::from_utf8_lossy(&out).into_owned())
}

/// Reads a NULL-terminated array of string pointers from user memory.
fn read_user_argv(ptr: u64) -> Option<Vec<alloc::string::String>> {
    let mut v = Vec::new();
    let mut i = 0u64;
    loop {
        let phys = resolve(ptr + i * 8, 8)?;
        let p = unsafe { core::ptr::read_unaligned(phys as *const u64) };
        if p == 0 {
            break;
        }
        v.push(read_user_cstr(p)?);
        i += 1;
    }
    Some(v)
}

/// `execve(path, argv, envp)`: replaces the current process's address
/// space with the ELF at `path` and restarts it at the new entry. On
/// success returns the `EXECVE_RESTART_MAGIC` sentinel the asm uses to
/// iretq into the new program instead of returning to the old rip.
fn sys_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    let Some(path) = read_user_cstr(path_ptr) else {
        return err(EFAULT);
    };
    let argv = match read_user_argv(argv_ptr) {
        Some(v) => v,
        None => return err(EFAULT),
    };
    let envp = match read_user_argv(envp_ptr) {
        Some(v) => v,
        None => return err(EFAULT),
    };
    let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let envp_refs: Vec<&str> = envp.iter().map(|s| s.as_str()).collect();
    let bytes = match crate::fat::read_file(&path) {
        Ok(b) => b,
        Err(_) => return err(ENOENT),
    };
    match crate::process::execve_into_current(&bytes, &argv_refs, &envp_refs) {
        Ok((_entry, _rsp)) => crate::syscall::EXECVE_RESTART_MAGIC_VALUE,
        Err(e) => {
            crate::io::exception_print(crate::io::sprint(
                &mut [0u8; 256],
                format_args!("[EXECVE] {}: {}\n", path, e),
            ));
            err(ENOEXEC)
        }
    }
}

/// `wait4(pid, wstatus, options, rusage)`: waits for a process to exit
/// and reports it. `pid > 0` waits for that exact pid; `-1`/`0` waits
/// for any exited process (scanning in pid order). `WNOHANG` returns
/// immediately with 0 instead of blocking. The wait is a poll loop
/// yielding between checks — the child's exit is marked by its own
/// exit path, and the parent simply re-checks after each reschedule.
/// The status is Linux's wait encoding: `exit_code << 8` for a normal
/// exit, the signal number for a signal/exception death.
fn sys_wait4(pid: u64, wstatus: u64, options: u64, _rusage: u64) -> u64 {
    const WNOHANG: u64 = 1;
    const WIFEXITED_SHIFT: u64 = 8;
    let status_ptr = wstatus;
    let target = pid as i64;
    loop {
        // Find an exited process matching the request, and remember
        // whether *any* child matches at all (so "all my children are
        // reaped zombies" can return ECHILD like Linux instead of
        // spinning — BusyBox's `waitpid(-1)` loop ends exactly there).
        let mut found: Option<(usize, crate::process::ExitInfo)> = None;
        let mut any_child = false;
        let mut any_alive = false;
        let count = crate::task::task_count();
        for cand in 0..count {
            // `-1`/`0` wait for any *child* of the caller (pid 0 also
            // means "same process group" — there are no groups, so it
            // behaves like -1). Only the caller's own fork children
            // count: an unrelated exited-but-unreaped process must not
            // satisfy the wait, or a shell's `waitpid(-1)` loop would
            // spin on a stale pid forever.
            let is_match = if target == -1 || target == 0 {
                crate::task::process_parent(cand) == Some(crate::task::current_pid())
            } else {
                cand as i64 == target
            };
            if !is_match {
                continue;
            }
            any_child = true;
            if !crate::task::process_is_exited(cand) {
                any_alive = true;
            }
            if let Some(info) = crate::task::process_exit_status(cand) {
                found = Some((cand, info));
                break;
            }
        }
        if found.is_none() && any_child && !any_alive {
            // Every matching child is an already-reaped zombie (or
            // never existed): `waitpid` reports ECHILD.
            return err(ECHILD);
        }
        if let Some((cand, info)) = found {
            // Reap the exit state: like Linux, `wait4` consumes the
            // child's zombie — a later `wait4` must not see the same
            // child again (BusyBox's `waitpid(-1)` loop spun on the
            // never-reaped first child of its pipeline).
            crate::task::reap_exit_status(cand);
            let status = match info {
                crate::process::ExitInfo::Normal => 0u64 << WIFEXITED_SHIFT,
                crate::process::ExitInfo::PageFault { .. } => 11, // SIGSEGV
                crate::process::ExitInfo::Exception { .. } => 6,  // SIGABRT-ish
            };
            if status_ptr != 0 {
                if let Some(phys) = resolve(status_ptr, 4) {
                    unsafe {
                        core::ptr::write_unaligned(phys as *mut u32, status as u32);
                    }
                }
            }
            return cand as u64;
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        // Not exited yet: yield (round-robin) and re-check. If the
        // target pid doesn't exist at all this would spin forever, but
        // the only callers wait on pids they forked.
        crate::task::yield_rr();
    }
}

/// `dup(oldfd)`: a new fd referring to the same open description.
fn sys_dup(oldfd: u64) -> u64 {
    with_process(|p| {
        let old = p.fd(oldfd as usize)?.dup()?;
        Some(p.alloc_fd(old) as u64)
    })
    .flatten()
    .unwrap_or(err(EBADF))
}

/// `dup2(oldfd, newfd)`: like `dup`, but places the copy at exactly
/// `newfd` (closing whatever was there).
fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    with_process(|p| {
        if oldfd == newfd {
            return Some(newfd);
        }
        let old = p.fd(oldfd as usize)?.dup()?;
        p.close_fd(newfd as usize);
        Some(p.set_fd(newfd as usize, old) as u64)
    })
    .flatten()
    .unwrap_or(err(EBADF))
}

/// `fcntl(fd, cmd, arg)` — the subset a shell needs: fd duplication
/// (F_DUPFD), the close-on-exec flag (F_GETFD/F_SETFD), and the file
/// status flags (F_GETFL/F_SETFL — only the access mode and O_APPEND
/// are tracked; the rest are accepted and ignored).
fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const FD_CLOEXEC: u64 = 1;
    const O_ACCMODE: u64 = 0b11;
    const O_APPEND: u64 = 0o2000;
    match cmd {
        F_DUPFD => {
            let min = arg as usize;
            with_process(|p| {
                let entry = p.fd(fd as usize)?;
                let dup = entry.dup()?;
                let mut slot = min;
                while p.fd(slot).is_some() {
                    slot += 1;
                }
                let clo = p.cloexec(fd as usize);
                let new = p.alloc_fd(dup);
                p.set_cloexec(new, clo);
                Some(new as u64)
            })
            .flatten()
            .unwrap_or(err(EBADF))
        }
        F_GETFD => with_process(|p| {
            if p.fd(fd as usize).is_some() {
                Some(if p.cloexec(fd as usize) { FD_CLOEXEC } else { 0 })
            } else {
                None
            }
        })
        .flatten()
        .unwrap_or(err(EBADF)),
        F_SETFD => with_process(|p| {
            if p.set_cloexec(fd as usize, arg & FD_CLOEXEC != 0) {
                Some(0)
            } else {
                None
            }
        })
        .flatten()
        .unwrap_or(err(EBADF)),
        F_GETFL => with_process(|p| {
            // Only the access mode is reported; O_APPEND/O_NONBLOCK are
            // not tracked per-fd yet, so 2 (O_RDWR) is the safe answer
            // for anything callers are likely to probe.
            if p.fd(fd as usize).is_some() {
                Some(2u64)
            } else {
                None
            }
        })
        .flatten()
        .unwrap_or(err(EBADF)),
        F_SETFL => 0, // O_NONBLOCK etc.: accepted, not implemented
        _ => err(EINVAL),
    }
}

/// `pipe2(pipefd, flags)`: creates a pipe and writes the read-end fd to
/// `pipefd[0]` and the write-end fd to `pipefd[1]`.
fn sys_pipe2(pipefd: u64, _flags: u64) -> u64 {
    let Some(phys) = resolve(pipefd, 16) else {
        return err(EFAULT);
    };
    let id = crate::pipe::create();
    match with_process(|p| {
        let rd = p.alloc_fd(FdEntry::Pipe(id, true));
        let wr = p.alloc_fd(FdEntry::Pipe(id, false));
        Some((rd, wr))
    })
    .flatten()
    {
        Some((rd, wr)) => {
            unsafe {
                // Real Linux writes an `int[2]` (8 bytes total) — writing
                // u64s here would overflow a caller's `int pipefd[2]`
                // (the second u64 lands past the array) and corrupt
                // whatever follows on the stack.
                core::ptr::write_unaligned(phys as *mut u32, rd as u32);
                core::ptr::write_unaligned((phys as *mut u8).add(4) as *mut u32, wr as u32);
            }
            0
        }
        None => err(EBADF),
    }
}

fn sys_close(fd: u64) -> u64 {
    if fd < 3 {
        return 0; // stdio: no real fd table entry to remove
    }
    with_process(|p| if p.close_fd(fd as usize) { 0 } else { err(EBADF) }).unwrap_or(err(EBADF))
}

fn sys_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    if fd < 3 {
        return err(ESPIPE);
    }
    let from = match whence {
        0 => vfs::SeekFrom::Start(offset),
        1 => vfs::SeekFrom::Current(offset as i64),
        2 => vfs::SeekFrom::End(offset as i64),
        _ => return err(EINVAL),
    };
    with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::File(h)) => h.seek(from),
        Some(FdEntry::Shm(id, cursor)) => {
            let size = shm::size_of(*id).unwrap_or(0);
            *cursor = seek_cursor(*cursor, size, from);
            *cursor as u64
        }
        Some(FdEntry::Socket(_)) => err(ESPIPE),
        Some(FdEntry::Pipe(_, _)) => err(ESPIPE), // pipes aren't seekable
        None => err(EBADF),
    })
    .unwrap_or(err(EBADF))
}

/// `pread64`: reads without disturbing the fd's own cursor (real `pread`
/// semantics) — `ld.so` uses this to peek at an ELF header before
/// deciding how to `mmap` the rest of the file, and would otherwise have
/// its later sequential reads thrown off by this one. Implemented on top
/// of `FileHandle`'s cursor-based `read`/`seek` (there's no separate
/// positioned-read primitive in vfs.rs) by saving and restoring the
/// cursor around a normal seek + read.
fn sys_pread64(fd: u64, buf: u64, count: u64, offset: u64) -> u64 {
    if fd < 3 {
        return err(ESPIPE);
    }
    // Phase 6 note: 0x38032ad8 is ld.so's `_rtld_global_ro+0x18` =
    // `_dl_pagesize`, whose file-initial value 0x1000 is CORRECT and must
    // NOT be touched: ld.so negates it (0x6759) to form the page-align
    // mask used when building loadcmds (0x6ae0/0x6b03/0x6b23). Writing a
    // phdr pointer there (as earlier debugging attempts did) corrupted the
    // mask, skewing every loadcmd's vaddr/offset and making the mmap
    // wrapper reject the (unaligned) offset -> "failed to map segment".
    let Some(phys) = resolve(buf, count) else {
        return err(EFAULT);
    };
    with_process(|p| {
        let Some(FdEntry::File(h)) = p.fd_mut(fd as usize) else {
            return err(EBADF); // pread64 against a memfd/socket isn't needed by anything this kernel runs yet
        };
        let saved = h.seek(vfs::SeekFrom::Current(0));
        h.seek(vfs::SeekFrom::Start(offset));
        let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
        let n = h.read(out);
        h.seek(vfs::SeekFrom::Start(saved));
        n as u64
    })
    .unwrap_or(err(EBADF))
}

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;
const STAT_SIZE: u64 = 144; // sizeof(struct stat), x86_64 Linux

/// Every file this kernel serves lives on the one FAT32 volume — real
/// Linux uses `st_dev` to tell *which filesystem* an inode number is
/// relative to, and since there's only ever one here, any fixed nonzero
/// value is a faithful enough answer.
const ST_DEV: u64 = 1;

/// Writes a (mostly zeroed, minimally plausible) Linux `struct stat` to
/// `buf_ptr`: `st_dev`/`st_ino` (see their doc comments — `ld.so` uses
/// these to dedup a shared library it's about to load against every
/// already-loaded map), `st_mode`, `st_size`, `st_blksize`, `st_blocks`
/// are filled in; timestamps and ownership stay zero.
fn write_stat(buf_ptr: u64, mode: u32, size: u64, ino: u64) -> u64 {
    let Some(phys) = resolve(buf_ptr, STAT_SIZE) else {
        return err(EFAULT);
    };
    unsafe {
        let p = phys as *mut u8;
        core::ptr::write_bytes(p, 0, STAT_SIZE as usize);
        core::ptr::write_unaligned(p as *mut u64, ST_DEV); // st_dev
        core::ptr::write_unaligned(p.add(8) as *mut u64, ino); // st_ino
        core::ptr::write_unaligned(p.add(16) as *mut u64, 1); // st_nlink
        core::ptr::write_unaligned(p.add(24) as *mut u32, mode); // st_mode
        core::ptr::write_unaligned(p.add(48) as *mut u64, size); // st_size
        core::ptr::write_unaligned(p.add(56) as *mut u64, 4096); // st_blksize
        core::ptr::write_unaligned(p.add(64) as *mut u64, size.div_ceil(512)); // st_blocks
    }
    0
}

const S_IFSOCK: u32 = 0o140000;
const S_IFIFO: u32 = 0o010000;

fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    if fd < 3 {
        return write_stat(statbuf, S_IFCHR | 0o666, 0, 0);
    }
    let result = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::File(h)) => {
            let st = h.stat();
            let mode = if st.is_dir { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
            Some((mode, st.size, st.ino))
        }
        Some(FdEntry::Shm(id, _)) => Some((S_IFREG | 0o600, shm::size_of(*id).unwrap_or(0) as u64, *id as u64 + 1)),
        Some(FdEntry::Socket(id)) => Some((S_IFSOCK | 0o777, 0, *id as u64 + 1)),
        Some(FdEntry::Pipe(id, _)) => Some((S_IFIFO | 0o600, crate::pipe::buffered(*id) as u64, *id as u64 + 1)),
        None => None,
    });
    match result {
        Some(Some((mode, size, ino))) => write_stat(statbuf, mode, size, ino),
        _ => err(EBADF),
    }
}

const AT_EMPTY_PATH: u64 = 0x1000;

/// `ld.so` (and modern glibc's plain `fstat` wrapper) commonly calls this
/// as `newfstatat(fd, "", statbuf, AT_EMPTY_PATH)` — "stat the fd itself,
/// ignore pathname" — rather than plain `fstat`. Handling only the
/// path-based form (and, worse, trying to resolve/read `pathname` even
/// when the caller never meant it to be read) made this fail with EFAULT
/// against a real dynamically-linked binary: caught by testing against
/// one, not a hypothetical gap.
fn sys_newfstatat(dirfd: u64, pathname: u64, statbuf: u64, flags: u64) -> u64 {
    if flags & AT_EMPTY_PATH != 0 {
        return sys_fstat(dirfd, statbuf);
    }
    let Some(path) = read_cstr(pathname, 256) else {
        return err(EFAULT);
    };
    let path = normalize_path(&path);
    match vfs::FileHandle::open(&path, 0) {
        Ok(h) => {
            let st = h.stat();
            let mode = if st.is_dir { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
            write_stat(statbuf, mode, st.size, st.ino)
        }
        Err(e) => vfs_err(e),
    }
}

fn sys_writev(fd: u64, iov: u64, iovcnt: u64) -> u64 {
    let mut total = 0u64;
    for i in 0..iovcnt {
        let Some(entry) = resolve(iov + i * 16, 16) else {
            return err(EFAULT);
        };
        let base = unsafe { core::ptr::read_unaligned(entry as *const u64) };
        let len = unsafe { core::ptr::read_unaligned((entry as *const u8).add(8) as *const u64) };
        if len == 0 {
            continue;
        }
        let n = sys_write(fd, base, len);
        if (n as i64) < 0 {
            return n;
        }
        total += n;
    }
    total
}

// ---- memory ---------------------------------------------------------------

fn sys_brk(addr: u64) -> u64 {
    let Some(pml4) = crate::task::current_process_cr3() else {
        return 0;
    };
    with_process(|p| p.brk(pml4, addr)).unwrap_or(0)
}

const MAP_SHARED: u64 = 0x01;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;

enum MmapFdKind {
    File,
    Shm(usize),
}

/// `fd`/`offset` only matter for a file-backed request (`MAP_ANONYMOUS`
/// clear and `fd` a real, non-negative descriptor). Two real fd kinds
/// reach here: a plain file — `ld.so` uses this to map each segment of a
/// shared library it's loading straight from the library's own file
/// bytes (see `process::Process::mmap_file`), a private eager copy — or
/// a `memfd_create` shared-memory object with `MAP_SHARED`, which needs
/// `process::Process::mmap_shared` instead: the whole reason `wl_shm`
/// works is that the compositor's mapping of the same object sees the
/// client's writes, which an eager copy could never give it.
/// `MAP_PRIVATE` of a memfd isn't supported (see module docs) — nothing
/// this kernel runs needs it.
fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 {
        return err(EINVAL);
    }
    let Some(pml4) = crate::task::current_process_cr3() else {
        return err(ENOMEM);
    };
    let at = if flags & MAP_FIXED != 0 { Some(addr) } else { None };
    let writable = prot & PROT_WRITE != 0;
    let executable = prot & PROT_EXEC != 0;

    if flags & MAP_ANONYMOUS != 0 || (fd as i64) < 0 {
        let r = with_process(|p| p.mmap_anon(pml4, at, len, writable, executable)).flatten().unwrap_or(err(ENOMEM));
        return r;
    }

    let kind = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::File(_)) => Some(MmapFdKind::File),
        Some(FdEntry::Shm(id, _)) => Some(MmapFdKind::Shm(*id)),
        _ => None,
    })
    .flatten();

    match kind {
        Some(MmapFdKind::Shm(shm_id)) if flags & MAP_SHARED != 0 => {
            with_process(|p| p.mmap_shared(pml4, at, shm_id, writable, executable)).flatten().unwrap_or(err(ENOMEM))
        }
        Some(MmapFdKind::Shm(_)) => err(EINVAL),
        Some(MmapFdKind::File) => {
            let content = with_process(|p| {
                let Some(FdEntry::File(h)) = p.fd_mut(fd as usize) else {
                    return None;
                };
                h.seek(vfs::SeekFrom::Start(offset));
                let mut buf = alloc::vec![0u8; len as usize];
                let n = h.read(&mut buf);
                buf.truncate(n);
                Some(buf)
            });
            match content {
                Some(Some(bytes)) => {
                    let r = with_process(|p| p.mmap_file(pml4, at, len, writable, executable, &bytes)).flatten().unwrap_or(err(ENOMEM));
                    // Phase 6: libc.so.6's dynamic relocations (ADJUST,
                    // RELR, GLOB_DAT, JUMP_SLOT) are all handled by ld.so's
                    // own _dl_relocate_object now that translate() resolves
                    // the newest (PTE-active) VMA — the kernel-side
                    // workarounds double-biased (ADJUST/RELR) or fought
                    // ld.so's RELATIVE pass over the GOT (SYMBOLIC/PLT).
                    // No workaround remains for the 0x1cf000 RW segment.
                    r
                }
                _ => err(EBADF),
            }
        }
        None => err(EBADF),
    }
}

fn sys_munmap(addr: u64, len: u64) -> u64 {
    let Some(pml4) = crate::task::current_process_cr3() else {
        return err(EINVAL);
    };
    if with_process(|p| p.munmap(pml4, addr, len)).unwrap_or(false) {
        0
    } else {
        err(EINVAL)
    }
}

fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    let Some(pml4) = crate::task::current_process_cr3() else {
        return err(EINVAL);
    };
    let writable = prot & PROT_WRITE != 0;
    let executable = prot & PROT_EXEC != 0;
    let r = with_process(|p| p.mprotect(pml4, addr, len, writable, executable)).unwrap_or(false);
    if r {
        0
    } else {
        err(EINVAL)
    }
}

// ---- shared memory (memfd_create / ftruncate) ----------------------------

fn sys_memfd_create(_name: u64, _flags: u64) -> u64 {
    let id = shm::create();
    with_process(|p| p.alloc_fd(FdEntry::Shm(id, 0)) as u64).unwrap_or(err(EBADF))
}

/// Only meaningful against a `memfd_create` fd — this kernel's regular
/// files aren't sparse/pre-sizeable the way `ftruncate` implies, and
/// nothing that runs against them needs it (only `wl_shm`-style shared
/// memory ever calls this).
fn sys_ftruncate(fd: u64, len: u64) -> u64 {
    let shm_id = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::Shm(id, _)) => Some(*id),
        _ => None,
    })
    .flatten();
    match shm_id {
        Some(id) if shm::truncate(id, len as usize) => 0,
        Some(_) => err(ENOMEM),
        None => err(EBADF),
    }
}

// ---- AF_UNIX sockets ------------------------------------------------------
//
// See `socket.rs`'s module docs for why every syscall here is
// non-blocking (interrupts are off for a syscall's whole duration) and
// what "connect" actually does (synchronously pairs this endpoint with
// a freshly created one on the listener's backlog — nothing here ever
// calls `socket::bind`/`listen`/`accept`, since those are only ever
// used by `wayland.rs`'s kernel-native compositor task directly, not
// through a syscall).

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;
const EPIPE: i32 = 32;
const EAFNOSUPPORT: i32 = 97;
const ECONNREFUSED: i32 = 111;

unsafe fn read_u64(addr: u64) -> u64 {
    unsafe { core::ptr::read_unaligned(addr as *const u64) }
}

unsafe fn read_u32(addr: u64) -> u32 {
    unsafe { core::ptr::read_unaligned(addr as *const u32) }
}

unsafe fn write_u64(addr: u64, v: u64) {
    unsafe { core::ptr::write_unaligned(addr as *mut u64, v) }
}

unsafe fn write_u32(addr: u64, v: u32) {
    unsafe { core::ptr::write_unaligned(addr as *mut u32, v) }
}

fn sys_socket(domain: u64, ty: u64, _protocol: u64) -> u64 {
    if domain != AF_UNIX || (ty & 0xf) != SOCK_STREAM {
        return err(EAFNOSUPPORT);
    }
    let id = socket::create();
    with_process(|p| p.alloc_fd(FdEntry::Socket(id)) as u64).unwrap_or(err(EBADF))
}

/// Reads a `struct sockaddr_un` (`sa_family: u16` then a NUL-terminated
/// `sun_path`) from user memory — the only address family these sockets
/// support.
fn read_sockaddr_un(addr: u64, addrlen: u64) -> Option<String> {
    let len = addrlen.min(2 + 108) as usize;
    if len < 2 {
        return None;
    }
    let phys = resolve(addr, len as u64)?;
    let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, len) };
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    if family as u64 != AF_UNIX {
        return None;
    }
    let path_bytes = &bytes[2..];
    let nul = path_bytes.iter().position(|&b| b == 0).unwrap_or(path_bytes.len());
    core::str::from_utf8(&path_bytes[..nul]).ok().map(|s| s.to_string())
}

fn sys_connect(fd: u64, addr: u64, addrlen: u64) -> u64 {
    let Some(path) = read_sockaddr_un(addr, addrlen) else {
        return err(EINVAL);
    };
    let socket_id = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::Socket(id)) => Some(*id),
        _ => None,
    })
    .flatten();
    let Some(socket_id) = socket_id else {
        return err(EBADF);
    };
    if socket::connect(socket_id, &path) {
        0
    } else {
        err(ECONNREFUSED)
    }
}

/// `sendmsg(fd, msg, flags)`: a real `struct msghdr`/`iovec`/`cmsghdr`
/// layout, but only ever reads a single `iovec` and at most one
/// `SCM_RIGHTS` `cmsghdr` — everything `wayland.rs`'s wire protocol
/// needs to pass. Each ancillary fd is resolved from *this* (the
/// sender's) fd table to a `shm.rs` object id before handing it to
/// `socket::send`, which is what actually crosses it over to the peer.
fn sys_sendmsg(fd: u64, msg: u64, _flags: u64) -> u64 {
    let Some(msg_phys) = resolve(msg, 56) else {
        return err(EFAULT);
    };
    let socket_id = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::Socket(id)) => Some(*id),
        _ => None,
    })
    .flatten();
    let Some(socket_id) = socket_id else {
        return err(EBADF);
    };

    let iov_ptr = unsafe { read_u64(msg_phys + 16) };
    let iov_len_count = unsafe { read_u64(msg_phys + 24) };
    let control_ptr = unsafe { read_u64(msg_phys + 32) };
    let control_len = unsafe { read_u64(msg_phys + 40) };

    let mut bytes: Vec<u8> = Vec::new();
    if iov_len_count > 0 {
        if let Some(iov_phys) = resolve(iov_ptr, 16) {
            let base = unsafe { read_u64(iov_phys) };
            let blen = unsafe { read_u64(iov_phys + 8) };
            if blen > 0 {
                if let Some(data_phys) = resolve(base, blen) {
                    bytes = unsafe { core::slice::from_raw_parts(data_phys as *const u8, blen as usize) }.to_vec();
                }
            }
        }
    }

    let mut shm_ids: Vec<usize> = Vec::new();
    if control_len >= 16 {
        if let Some(ctrl_phys) = resolve(control_ptr, control_len) {
            let cmsg_len = unsafe { read_u64(ctrl_phys) } as usize;
            let cmsg_level = unsafe { read_u32(ctrl_phys + 8) };
            let cmsg_type = unsafe { read_u32(ctrl_phys + 12) };
            if cmsg_level == SOL_SOCKET && cmsg_type == SCM_RIGHTS && cmsg_len > 16 {
                let n_fds = (cmsg_len - 16) / 4;
                for i in 0..n_fds {
                    let client_fd = unsafe { read_u32(ctrl_phys + 16 + (i as u64) * 4) } as u64;
                    let shm_id = with_process(|p| match p.fd_mut(client_fd as usize) {
                        Some(FdEntry::Shm(id, _)) => Some(*id),
                        _ => None,
                    })
                    .flatten();
                    if let Some(id) = shm_id {
                        shm_ids.push(id);
                    }
                }
            }
        }
    }

    match socket::send(socket_id, &bytes, &shm_ids) {
        Some(n) => n as u64,
        None => err(EPIPE),
    }
}

/// `recvmsg(fd, msg, flags)`: the receiving half of `sys_sendmsg` — never
/// blocks (see `socket.rs`'s module docs), returns `-EAGAIN` immediately
/// when nothing is queued rather than waiting for it.
fn sys_recvmsg(fd: u64, msg: u64, _flags: u64) -> u64 {
    let Some(msg_phys) = resolve(msg, 56) else {
        return err(EFAULT);
    };
    let socket_id = with_process(|p| match p.fd_mut(fd as usize) {
        Some(FdEntry::Socket(id)) => Some(*id),
        _ => None,
    })
    .flatten();
    let Some(socket_id) = socket_id else {
        return err(EBADF);
    };
    if !socket::has_data(socket_id) {
        return err(EAGAIN);
    }

    let iov_ptr = unsafe { read_u64(msg_phys + 16) };
    let iov_len_count = unsafe { read_u64(msg_phys + 24) };
    let control_ptr = unsafe { read_u64(msg_phys + 32) };
    let control_cap = unsafe { read_u64(msg_phys + 40) };

    let (buf_phys, buf_len) = if iov_len_count > 0 {
        match resolve(iov_ptr, 16) {
            Some(iov_phys) => {
                let base = unsafe { read_u64(iov_phys) };
                let blen = unsafe { read_u64(iov_phys + 8) };
                (resolve(base, blen), blen)
            }
            None => (None, 0),
        }
    } else {
        (None, 0)
    };

    let (n, recv_fds) = match buf_phys {
        Some(bp) => {
            let out = unsafe { core::slice::from_raw_parts_mut(bp as *mut u8, buf_len as usize) };
            socket::recv(socket_id, out)
        }
        None => socket::recv(socket_id, &mut []),
    };

    let mut controllen_used = 0u64;
    match resolve(control_ptr, control_cap).filter(|_| !recv_fds.is_empty() && control_cap >= 16) {
        Some(ctrl_phys) => {
            let max_fds = ((control_cap - 16) / 4).min(recv_fds.len() as u64) as usize;
            let cmsg_len = 16 + (max_fds as u64) * 4;
            unsafe {
                write_u64(ctrl_phys, cmsg_len);
                write_u32(ctrl_phys + 8, SOL_SOCKET);
                write_u32(ctrl_phys + 12, SCM_RIGHTS);
            }
            for (i, &shm_id) in recv_fds.iter().take(max_fds).enumerate() {
                let new_fd = with_process(|p| p.alloc_fd(FdEntry::Shm(shm_id, 0))).unwrap_or(0);
                unsafe { write_u32(ctrl_phys + 16 + (i as u64) * 4, new_fd as u32) };
            }
            controllen_used = cmsg_len;
            // Any fds beyond what the caller's buffer could fit are
            // simply dropped (their `shm.rs` reference released) — real
            // recvmsg sets MSG_CTRUNC instead; not worth the extra
            // plumbing when this module's one real caller always sizes
            // its buffer correctly.
            for &shm_id in recv_fds.iter().skip(max_fds) {
                shm::close(shm_id);
            }
        }
        None => {
            for &shm_id in &recv_fds {
                shm::close(shm_id);
            }
        }
    }
    unsafe { write_u64(msg_phys + 40, controllen_used) };
    n as u64
}

// ---- TLS (arch_prctl) -------------------------------------------------

const ARCH_SET_FS: u64 = 0x1002;
const MSR_FS_BASE: u32 = 0xC000_0100;

fn sys_arch_prctl(code: u64, addr: u64) -> u64 {
    match code {
        ARCH_SET_FS => {
            unsafe {
                crate::syscall::wrmsr(MSR_FS_BASE, addr);
            }
            crate::task::set_current_fs_base(addr);
            0
        }
        _ => err(EINVAL),
    }
}

// ---- misc startup/runtime syscalls ---------------------------------------

fn sys_uname(buf: u64) -> u64 {
    const FIELD: usize = 65;
    const TOTAL: u64 = FIELD as u64 * 6;
    let Some(phys) = resolve(buf, TOTAL) else {
        return err(EFAULT);
    };
    let write_field = |offset: usize, s: &[u8]| unsafe {
        core::ptr::write_bytes((phys as *mut u8).add(offset), 0, FIELD);
        core::ptr::copy_nonoverlapping(s.as_ptr(), (phys as *mut u8).add(offset), s.len());
    };
    write_field(0, b"Linux");
    write_field(FIELD, b"machaos");
    write_field(FIELD * 2, b"6.1.0-machaos");
    write_field(FIELD * 3, b"#1 SMP");
    write_field(FIELD * 4, b"x86_64");
    write_field(FIELD * 5, b"");
    0
}

fn sys_getrandom(buf: u64, buflen: u64, _flags: u64) -> u64 {
    let Some(phys) = resolve(buf, buflen) else {
        return err(EFAULT);
    };
    let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, buflen as usize) };
    // Not cryptographically random — no HW RNG driver exists yet — just
    // distinct-enough bytes to satisfy a libc that refuses to start
    // without them (musl's stack-protector canary, malloc hardening).
    let mut seed = crate::interrupts::ticks() ^ buf;
    for (i, b) in out.iter_mut().enumerate() {
        seed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i as u64);
        *b = (seed >> 33) as u8;
    }
    buflen
}

fn sys_clock_gettime(_clockid: u64, ts: u64) -> u64 {
    let Some(phys) = resolve(ts, 16) else {
        return err(EFAULT);
    };
    let ticks = crate::interrupts::ticks(); // 100 Hz (see pit.rs)
    unsafe {
        core::ptr::write_unaligned(phys as *mut u64, ticks / 100);
        core::ptr::write_unaligned((phys as *mut u8).add(8) as *mut u64, (ticks % 100) * 10_000_000);
    }
    0
}

// ---- Phase 3: extra syscalls a static glibc binary's startup wants ----

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_CMD_MASK: u64 = !128; // clears FUTEX_PRIVATE_FLAG

/// Real blocking futex: `FUTEX_WAIT` blocks the calling task on
/// `(cr3, uaddr)` when the word at `uaddr` still equals `val` (the
/// single-CPU syscall path — IF cleared for the whole syscall — makes
/// the check-and-register sequence atomic, so no wake can be lost
/// between them), and `FUTEX_WAKE` marks up to `val` blocked tasks
/// runnable again. This is what pthread mutexes and pthread_join are
/// built on.
fn sys_futex(uaddr: u64, futex_op: u64, val: u64, _val2_or_timeout: u64) -> u64 {
    let cr3 = crate::task::current_process_cr3().unwrap_or(0);
    match futex_op & FUTEX_CMD_MASK {
        FUTEX_WAIT => {
            let Some(phys) = resolve(uaddr, 4) else {
                return err(EFAULT);
            };
            let current = unsafe { core::ptr::read(phys as *const u32) };
            if current as u64 != (val & 0xFFFF_FFFF) {
                return err(EAGAIN);
            }
            // Value still matches: block until FUTEX_WAKE on this word.
            // `yield_blocked` switches away; when woken, the caller
            // re-reads the word (pthread loops on the futex value).
            crate::task::yield_blocked(cr3 ^ uaddr);
            0
        }
        FUTEX_WAKE => crate::task::wake_all(cr3 ^ uaddr, val as usize) as u64,
        _ => err(ENOSYS),
    }
}

/// Always refuses (matches a kernel that doesn't support `rseq`): glibc
/// probes for it once at startup and falls back to its non-rseq path
/// cleanly when this fails, the same as running on an old real kernel.
fn sys_rseq(_rseq: u64, _rseq_len: u64, _flags: u64, _sig: u64) -> u64 {
    err(EINVAL)
}

const RLIM_INFINITY: u64 = u64::MAX;

/// Reports every resource limit as unlimited — this kernel doesn't track
/// or enforce per-process limits at all, so "unlimited" is the only
/// answer that can't be wrong in a way a caller would notice.
fn sys_prlimit64(_pid: u64, _resource: u64, _new_limit: u64, old_limit: u64) -> u64 {
    if old_limit != 0 {
        let Some(phys) = resolve(old_limit, 16) else {
            return err(EFAULT);
        };
        unsafe {
            core::ptr::write_unaligned(phys as *mut u64, RLIM_INFINITY); // rlim_cur
            core::ptr::write_unaligned((phys as *mut u8).add(8) as *mut u64, RLIM_INFINITY); // rlim_max
        }
    }
    0
}

/// Reports a single CPU (bit 0 of the mask), matching this kernel's
/// single-CPU reality.
fn sys_sched_getaffinity(_pid: u64, cpusetsize: u64, mask_ptr: u64) -> u64 {
    let Some(phys) = resolve(mask_ptr, cpusetsize) else {
        return err(EFAULT);
    };
    unsafe {
        core::ptr::write_bytes(phys as *mut u8, 0, cpusetsize as usize);
        if cpusetsize >= 1 {
            core::ptr::write(phys as *mut u8, 1);
        }
    }
    cpusetsize.min(8)
}

/// `struct sysinfo` (Linux x86_64, 112 bytes including trailing padding —
/// see <sys/sysinfo.h>). Only uptime/totalram/freeram/mem_unit are filled
/// in with real values; load averages, swap, and process count stay
/// zero (this kernel doesn't track any of those).
fn sys_sysinfo(info_ptr: u64) -> u64 {
    const SIZE: u64 = 112;
    let Some(phys) = resolve(info_ptr, SIZE) else {
        return err(EFAULT);
    };
    unsafe {
        let p = phys as *mut u8;
        core::ptr::write_bytes(p, 0, SIZE as usize);
        core::ptr::write_unaligned(p as *mut i64, (crate::interrupts::ticks() / 100) as i64); // uptime
        let total = (crate::pmm::total_frames() * crate::pmm::FRAME_SIZE) as u64;
        let free = (crate::pmm::free_frames() * crate::pmm::FRAME_SIZE) as u64;
        core::ptr::write_unaligned(p.add(32) as *mut u64, total); // totalram
        core::ptr::write_unaligned(p.add(40) as *mut u64, free); // freeram
        core::ptr::write_unaligned(p.add(104) as *mut u32, 1); // mem_unit
    }
    0
}

/// Dispatches one Linux-numbered syscall.
pub fn syscall_dispatch(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64, arg6: u64) -> u64 {
    // Snapshot the ring-3 rsp into this task (see `syscall_save_user_rsp`
    // in syscall.rs) — the asm's sysretq return reads it back per task.
    crate::syscall::snapshot_saved_user_rsp();
    let ret = match num {
        SYS_READ => sys_read(arg1, arg2, arg3),
        SYS_WRITE => sys_write(arg1, arg2, arg3),
        SYS_CLOSE => sys_close(arg1),
        SYS_FSTAT => sys_fstat(arg1, arg2),
        SYS_LSEEK => sys_lseek(arg1, arg2, arg3),
        SYS_ACCESS => sys_access(arg1, arg2),
        SYS_PREAD64 => sys_pread64(arg1, arg2, arg3, arg4),
        SYS_MMAP => sys_mmap(arg1, arg2, arg3, arg4, arg5, arg6),
        SYS_MPROTECT => sys_mprotect(arg1, arg2, arg3),
        SYS_MUNMAP => sys_munmap(arg1, arg2),
        SYS_BRK => sys_brk(arg1),
        SYS_RT_SIGACTION => 0,   // no signal delivery — see module docs
        SYS_RT_SIGPROCMASK => 0, // ditto
        SYS_IOCTL => err(ENOTTY), // every fd reports "not a tty"
        SYS_WRITEV => sys_writev(arg1, arg2, arg3),
        SYS_GETPID => crate::task::current_pid() as u64,
        SYS_GETTID => crate::task::current_pid() as u64,
        SYS_UNAME => sys_uname(arg1),
        SYS_ARCH_PRCTL => sys_arch_prctl(arg1, arg2),
        SYS_SET_TID_ADDRESS => crate::task::current_pid() as u64,
        SYS_CLOCK_GETTIME => sys_clock_gettime(arg1, arg2),
        SYS_OPENAT => sys_openat(arg1, arg2, arg3, arg4),
        SYS_OPEN => sys_openat(AT_FDCWD, arg1, arg2, arg3),
        SYS_NEWFSTATAT => sys_newfstatat(arg1, arg2, arg3, arg4),
        SYS_STAT => sys_newfstatat(AT_FDCWD, arg1, arg2, 0),
        SYS_LSTAT => sys_newfstatat(AT_FDCWD, arg1, arg2, 0),
        SYS_SET_ROBUST_LIST => 0,
        SYS_GETRANDOM => sys_getrandom(arg1, arg2, arg3),
        SYS_FUTEX => sys_futex(arg1, arg2, arg3, arg4),
        SYS_RSEQ => sys_rseq(arg1, arg2, arg3, arg4),
        SYS_PRLIMIT64 => sys_prlimit64(arg1, arg2, arg3, arg4),
        SYS_SCHED_GETAFFINITY => sys_sched_getaffinity(arg1, arg2, arg3),
        SYS_SYSINFO => sys_sysinfo(arg1),
        SYS_MEMFD_CREATE => sys_memfd_create(arg1, arg2),
        SYS_FTRUNCATE => sys_ftruncate(arg1, arg2),
        SYS_SOCKET => sys_socket(arg1, arg2, arg3),
        SYS_CONNECT => sys_connect(arg1, arg2, arg3),
        SYS_SENDMSG => sys_sendmsg(arg1, arg2, arg3),
        SYS_RECVMSG => sys_recvmsg(arg1, arg2, arg3),
        // Phase 7: process control — fork/clone/vfork are intercepted by
        // the asm special path before dispatch; the rest live here.
        SYS_EXECVE => sys_execve(arg1, arg2, arg3),
        SYS_WAIT4 => sys_wait4(arg1, arg2, arg3, arg4),
        SYS_DUP => sys_dup(arg1),
        SYS_DUP2 => sys_dup2(arg1, arg2),
        SYS_DUP2_64 => sys_dup2(arg1, arg2),
        SYS_DUP3 => {
            // dup3(oldfd, newfd, flags): flags are all O_CLOEXEC-ish,
            // which this kernel doesn't track — behave like dup2.
            sys_dup2(arg1, arg2)
        }
        SYS_PIPE2 => sys_pipe2(arg1, arg2),
        SYS_PIPE => sys_pipe2(arg1, 0),
        SYS_FCNTL => sys_fcntl(arg1, arg2, arg3),
        SYS_FCNTL64 => sys_fcntl(arg1, arg2, arg3),
        SYS_GETPPID => 0, // no parent tracking yet — 0 is a valid "no parent"
        _ => {
            let mut buf = [0u8; 64];
            let msg = crate::io::sprint(&mut buf, format_args!("[UNKSYSCALL] num={}\n", num));
            crate::io::exception_print(msg);
            err(ENOSYS)
        }
    };
    ret
}
