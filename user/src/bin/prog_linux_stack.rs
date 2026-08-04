//! prog_linux_stack: verifies `process::setup_linux_stack`'s Linux-style
//! initial stack layout (argc/argv/envp/auxv) by reading it back exactly
//! as a real libc's `_start` would, off the raw entry `rsp` — not through
//! a normal Rust `extern "C" fn _start()`, whose own prologue may have
//! already adjusted `rsp` by the time any Rust code runs. `_start` here
//! is hand-written asm (matching how `syscall.rs`'s `enter_usermode`/
//! `syscall_entry` are defined in this codebase) that captures the
//! untouched entry `rsp` and hands it to `rust_entry` as an argument.
//!
//! Never falls through to a `ret`: unlike MachaOS's native-ABI test
//! programs, a Linux-style entry point has no return address on the
//! stack at all (`argc` sits there instead) — it must exit via a syscall,
//! never `ret`.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::arch::global_asm;
use core::sync::atomic::Ordering;

const SYS_EXIT: u64 = 1;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

global_asm!(
    r#"
.section .text
.global _start
.type _start, @function
_start:
    mov rdi, rsp
    call rust_entry
"#
);

unsafe fn read_u64(ptr: u64) -> u64 {
    unsafe { core::ptr::read(ptr as *const u64) }
}

/// Reads a NUL-terminated string at `ptr` and compares it to `expected`.
unsafe fn str_eq(ptr: u64, expected: &[u8]) -> bool {
    for (i, &want) in expected.iter().enumerate() {
        if unsafe { core::ptr::read((ptr + i as u64) as *const u8) } != want {
            return false;
        }
    }
    unsafe { core::ptr::read((ptr + expected.len() as u64) as *const u8) == 0 }
}

#[unsafe(no_mangle)]
extern "C" fn rust_entry(initial_rsp: u64) -> ! {
    let mut ok = 0u64;
    unsafe {
        let argc = read_u64(initial_rsp);
        if argc == 2 {
            ok |= 1 << 0;
        }
        let argv0 = read_u64(initial_rsp + 8);
        let argv1 = read_u64(initial_rsp + 16);
        if str_eq(argv0, b"prog_linux_stack") {
            ok |= 1 << 1;
        }
        if str_eq(argv1, b"hello") {
            ok |= 1 << 2;
        }
        let argv_null = read_u64(initial_rsp + 24);
        if argv_null == 0 {
            ok |= 1 << 3;
        }

        let envp_base = initial_rsp + 32;
        let envp0 = read_u64(envp_base);
        if str_eq(envp0, b"FOO=bar") {
            ok |= 1 << 4;
        }
        let envp_null = read_u64(envp_base + 8);
        if envp_null == 0 {
            ok |= 1 << 5;
        }

        let mut cursor = envp_base + 16;
        let mut pagesz_ok = false;
        let mut phent_ok = false;
        let mut phnum_ok = false;
        let mut phdr_ok = false;
        let mut random_ok = false;
        let mut execfn_ok = false;
        let mut saw_null = false;
        for _ in 0..64 {
            let at_type = read_u64(cursor);
            let at_val = read_u64(cursor + 8);
            match at_type {
                AT_NULL => {
                    saw_null = true;
                    break;
                }
                AT_PAGESZ if at_val == 4096 => pagesz_ok = true,
                AT_PHENT if at_val == 56 => phent_ok = true,
                AT_PHNUM if at_val >= 1 => phnum_ok = true,
                AT_PHDR if at_val != 0 => {
                    // The copied program header table's first entry
                    // should be a real PT_LOAD (p_type == 1) at offset 0.
                    let p_type = core::ptr::read(at_val as *const u32);
                    phdr_ok = p_type == 1;
                }
                AT_RANDOM if at_val != 0 => random_ok = true,
                AT_EXECFN if at_val != 0 => {
                    execfn_ok = str_eq(at_val, b"prog_linux_stack");
                }
                AT_ENTRY => {
                    let _ = at_val; // just confirm the entry is present
                }
                _ => {}
            }
            cursor += 16;
        }
        if saw_null {
            ok |= 1 << 6;
        }
        if pagesz_ok {
            ok |= 1 << 7;
        }
        if phent_ok {
            ok |= 1 << 8;
        }
        if phnum_ok {
            ok |= 1 << 9;
        }
        if phdr_ok {
            ok |= 1 << 10;
        }
        if random_ok {
            ok |= 1 << 11;
        }
        if execfn_ok {
            ok |= 1 << 12;
        }
    }

    common::RESULT.store(ok, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT, 0, 0, 0, 0);
    }
    loop {}
}
