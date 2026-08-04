//! Linux x86_64 syscall ABI layer — a skeleton. Per the approved Linux
//! ABI emulation plan, Phase 1 only needs the dispatch-routing mechanism
//! and the errno return convention to exist and be selectable per
//! process; Phase 2 is where real syscalls (openat/mmap/brk/futex/...)
//! get implemented here.
//!
//! Selected per-process via `process::Abi::Linux` (see
//! `process::spawn_linux_test`, currently the only path that sets it)
//! and dispatched by `syscall::syscall_dispatch`, which checks the
//! current process's `Abi` before falling into the native table.
//!
//! Reuses the same argument registers MachaOS's own `syscall_entry` asm
//! already decodes (rdi/rsi/rdx/r10/r8/r9 on the user side) — this
//! happens to be identical to the real Linux x86-64 syscall ABI's
//! argument registers, so no asm changes were needed to add this second
//! table. That asm currently only forwards the first *four* of them
//! (num, arg1..arg4) to whichever dispatch function ends up handling a
//! syscall; a real syscall like `mmap` needs six. Widening that is
//! Phase 2's problem, once a syscall that actually needs args 5/6 is
//! implemented here — this skeleton's own `getpid` needs none, and an
//! unrecognized number just returns `-ENOSYS` regardless of its
//! arguments.
//!
//! KNOWN GAP for Phase 2: `syscall_entry`'s asm special-cases syscall
//! number 1 as the *native* SYS_EXIT before any dispatch table (native
//! or Linux) ever sees it (see syscall.rs's module docs) — but Linux
//! syscall 1 is `write`, not `exit` (`exit` is 60, `exit_group` is 231).
//! A Linux-ABI process calling `write` today would be silently hijacked
//! into the native exit path instead of reaching this module. Fixing
//! that needs the asm itself to check the current process's `Abi` before
//! deciding what "1" means; not addressed here since no Linux process
//! can meaningfully reach syscall number 1 yet (nothing in this skeleton
//! implements `write`).

/// Function not implemented — returned (as `-ENOSYS`, the raw `syscall`
/// return value a real libc's wrapper turns into "return -1, set
/// `errno = ENOSYS`") for any syscall number this skeleton doesn't
/// recognize.
const ENOSYS: i32 = 38;

const SYS_GETPID: u64 = 39;

/// Negates and sign-extends `errno` into the raw `u64` a syscall returns
/// on failure, matching the real Linux convention (small negative values
/// close to 0, e.g. `-38` for `ENOSYS`, rather than MachaOS's native ABI's
/// `u64::MAX` sentinel — see `syscall::SYSCALL_ERROR`).
fn err(errno: i32) -> u64 {
    (-(errno as i64)) as u64
}

/// Dispatches one Linux-numbered syscall. `getpid` is wired up for real
/// (proving argument-free syscalls round-trip correctly through this
/// table); everything else — the vast majority of a real libc's startup
/// requirements (`arch_prctl`, `brk`, `mmap`, `openat`, ...) — returns
/// `ENOSYS` until Phase 2 implements it.
pub fn syscall_dispatch(num: u64, _arg1: u64, _arg2: u64, _arg3: u64, _arg4: u64) -> u64 {
    match num {
        SYS_GETPID => crate::task::current_pid() as u64,
        _ => err(ENOSYS),
    }
}
