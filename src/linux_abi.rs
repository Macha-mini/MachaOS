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
//!   `process::Process::munmap`); `mprotect` only ever grants/revokes
//!   write access (no NX enforcement — `paging.rs` has none yet).
//! - `futex` never actually blocks or wakes anything (see `sys_futex`'s
//!   doc comment) — fine for the single-threaded, uncontended-lock case
//!   glibc's own startup hits, but `clone`/real threading aren't
//!   implemented at all, so this is the extent of it for now.
//! - A real dynamically-linked glibc binary (tested against GNU Hello +
//!   a real `ld.so`/`libc.so.6` pair — see the Phase 4 commit and
//!   `shell.rs`'s selftest hook) doesn't run to completion yet: `ld.so`
//!   opens `libc.so.6`, `pread64`s its program headers (verified via
//!   tracing to return the correct byte count) and `fstat`s it (verified
//!   to report the correct real file size), then goes straight into
//!   symbol version processing and faults — without ever calling `mmap`
//!   on that fd to actually load its segments. Metadata about the file
//!   is reaching `ld.so` correctly, but whatever `ld.so` does with that
//!   metadata between "headers parsed" and "map the PT_LOAD segments" is
//!   still going wrong in a way this kernel's own tracing can't see
//!   further into without instrumenting `ld.so`'s disassembly directly.
//!   Testing against that real binary is exactly what found and fixed
//!   the AT_PHDR/`pread64`/`AT_EMPTY_PATH`/stack-buffer/`access`(21)
//!   bugs the rest of this file's history documents; this is the next
//!   one, left for follow-up rather than resolved here — consistent
//!   with this plan's own framing of Phase 4 as roadmap-level rigor
//!   rather than Phase 1/2's full-completion bar.

use alloc::format;
use alloc::string::{String, ToString};

use crate::vfs;

// ---- errno -----------------------------------------------------------

const EBADF: i32 = 9;
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
    if fd == 1 || fd == 2 {
        for &b in bytes {
            crate::io::print(core::format_args!("{}", b as char));
        }
        return bytes.len() as u64;
    }
    if fd < 3 {
        return err(EBADF); // fd 0 (stdin) isn't writable
    }
    with_process(|p| match p.fd_mut(fd as usize) {
        Some(h) => match h.write(bytes) {
            Ok(n) => n as u64,
            Err(e) => vfs_err(e),
        },
        None => err(EBADF),
    })
    .unwrap_or(err(EBADF))
}

fn sys_read(fd: u64, buf: u64, count: u64) -> u64 {
    let Some(phys) = resolve(buf, count) else {
        return err(EFAULT);
    };
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
    if fd < 3 {
        return err(EBADF); // fd 1/2 (stdout/stderr) aren't readable
    }
    with_process(|p| match p.fd_mut(fd as usize) {
        Some(h) => {
            let out = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, count as usize) };
            h.read(out) as u64
        }
        None => err(EBADF),
    })
    .unwrap_or(err(EBADF))
}

fn sys_openat(_dirfd: u64, pathname: u64, flags: u64, _mode: u64) -> u64 {
    let Some(path) = read_cstr(pathname, 256) else {
        return err(EFAULT);
    };
    let path = normalize_path(&path);
    let vfs_flags = translate_open_flags(flags);
    match vfs::FileHandle::open(&path, vfs_flags) {
        Ok(handle) => with_process(|p| p.alloc_fd(handle) as u64).unwrap_or(err(EBADF)),
        Err(e) => vfs_err(e),
    }
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
        Some(h) => h.seek(from),
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
    let Some(phys) = resolve(buf, count) else {
        return err(EFAULT);
    };
    with_process(|p| {
        let Some(h) = p.fd_mut(fd as usize) else {
            return err(EBADF);
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

/// Writes a (mostly zeroed, minimally plausible) Linux `struct stat` to
/// `buf_ptr`: only the fields a typical startup path or `ls`-like
/// listing actually inspects (`st_mode`, `st_size`, `st_blksize`,
/// `st_blocks`) are filled in; timestamps and ownership stay zero.
fn write_stat(buf_ptr: u64, mode: u32, size: u64) -> u64 {
    let Some(phys) = resolve(buf_ptr, STAT_SIZE) else {
        return err(EFAULT);
    };
    unsafe {
        let p = phys as *mut u8;
        core::ptr::write_bytes(p, 0, STAT_SIZE as usize);
        core::ptr::write_unaligned(p.add(16) as *mut u64, 1); // st_nlink
        core::ptr::write_unaligned(p.add(24) as *mut u32, mode); // st_mode
        core::ptr::write_unaligned(p.add(48) as *mut u64, size); // st_size
        core::ptr::write_unaligned(p.add(56) as *mut u64, 4096); // st_blksize
        core::ptr::write_unaligned(p.add(64) as *mut u64, size.div_ceil(512)); // st_blocks
    }
    0
}

fn sys_fstat(fd: u64, statbuf: u64) -> u64 {
    if fd < 3 {
        return write_stat(statbuf, S_IFCHR | 0o666, 0);
    }
    let stat = with_process(|p| p.fd_mut(fd as usize).map(|h| h.stat()));
    match stat {
        Some(Some(st)) => {
            let mode = if st.is_dir { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
            write_stat(statbuf, mode, st.size)
        }
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
            write_stat(statbuf, mode, st.size)
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

const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const PROT_WRITE: u64 = 2;

/// `fd`/`offset` only matter for a file-backed request (`MAP_ANONYMOUS`
/// clear and `fd` a real, non-negative descriptor) — `ld.so` uses this to
/// map each segment of a shared library it's loading straight from the
/// library's own file bytes (see `process::Process::mmap_file`).
fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 {
        return err(EINVAL);
    }
    let Some(pml4) = crate::task::current_process_cr3() else {
        return err(ENOMEM);
    };
    let at = if flags & MAP_FIXED != 0 { Some(addr) } else { None };
    let writable = prot & PROT_WRITE != 0;

    if flags & MAP_ANONYMOUS != 0 || (fd as i64) < 0 {
        return with_process(|p| p.mmap_anon(pml4, at, len, writable)).flatten().unwrap_or(err(ENOMEM));
    }

    let content = with_process(|p| {
        let h = p.fd_mut(fd as usize)?;
        h.seek(vfs::SeekFrom::Start(offset));
        let mut buf = alloc::vec![0u8; len as usize];
        let n = h.read(&mut buf);
        buf.truncate(n);
        Some(buf)
    });
    match content {
        Some(Some(bytes)) => with_process(|p| p.mmap_file(pml4, at, len, writable, &bytes)).flatten().unwrap_or(err(ENOMEM)),
        _ => err(EBADF),
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
    if with_process(|p| p.mprotect(pml4, addr, len, writable)).unwrap_or(false) {
        0
    } else {
        err(EINVAL)
    }
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
            with_process(|p| p.fs_base = addr);
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

/// This kernel has no threading (`clone` isn't implemented) and is
/// single-CPU, so nothing else could ever be running concurrently to
/// either contend for or wake a futex — glibc's malloc arena lock and
/// similar internal locks still take this path once at startup even
/// single-threaded, just always uncontended. `FUTEX_WAIT` therefore
/// either finds the lock already free (the value at `uaddr` no longer
/// matches `val`, meaning whoever held it already released it — return
/// success immediately rather than actually blocking, since there's
/// nothing that could ever wake a real block) or matches (genuinely
/// uncontended acquisition; real futex would still block waiting for a
/// wake that, again, can never come here — return success as if a
/// spurious wake happened, which is always a legal futex outcome).
/// `FUTEX_WAKE` has no real waiters to wake and just reports zero.
fn sys_futex(uaddr: u64, futex_op: u64, val: u64, _val2_or_timeout: u64) -> u64 {
    match futex_op & FUTEX_CMD_MASK {
        FUTEX_WAIT => {
            let Some(phys) = resolve(uaddr, 4) else {
                return err(EFAULT);
            };
            let current = unsafe { core::ptr::read(phys as *const u32) };
            if current as u64 == (val & 0xFFFF_FFFF) {
                0
            } else {
                err(EAGAIN)
            }
        }
        FUTEX_WAKE => 0,
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
    match num {
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
        SYS_UNAME => sys_uname(arg1),
        SYS_ARCH_PRCTL => sys_arch_prctl(arg1, arg2),
        SYS_SET_TID_ADDRESS => crate::task::current_pid() as u64,
        SYS_CLOCK_GETTIME => sys_clock_gettime(arg1, arg2),
        SYS_OPENAT => sys_openat(arg1, arg2, arg3, arg4),
        SYS_NEWFSTATAT => sys_newfstatat(arg1, arg2, arg3, arg4),
        SYS_SET_ROBUST_LIST => 0,
        SYS_GETRANDOM => sys_getrandom(arg1, arg2, arg3),
        SYS_FUTEX => sys_futex(arg1, arg2, arg3, arg4),
        SYS_RSEQ => sys_rseq(arg1, arg2, arg3, arg4),
        SYS_PRLIMIT64 => sys_prlimit64(arg1, arg2, arg3, arg4),
        SYS_SCHED_GETAFFINITY => sys_sched_getaffinity(arg1, arg2, arg3),
        SYS_SYSINFO => sys_sysinfo(arg1),
        _ => {
            let mut buf = [0u8; 64];
            let msg = crate::io::sprint(&mut buf, format_args!("[UNKSYSCALL] num={}\n", num));
            crate::io::exception_print(msg);
            err(ENOSYS)
        }
    }
}
