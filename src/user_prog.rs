//! Freestanding test programs embedded into the kernel image. Built by
//! the `user` crate (see user/) and copied to `target/` by `make build`;
//! the selftest loads them via `process::spawn`.

pub static PROG_EXIT: &[u8] = include_bytes!("../target/user-exit.elf");
pub static PROG_FAULT: &[u8] = include_bytes!("../target/user-fault.elf");
pub static PROG_SYSCALL: &[u8] = include_bytes!("../target/user-syscall.elf");
pub static PROG_IPC: &[u8] = include_bytes!("../target/user-ipc.elf");
pub static PROG_WINDOW: &[u8] = include_bytes!("../target/user-window.elf");
