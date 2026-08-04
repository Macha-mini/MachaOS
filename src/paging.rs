//! The boot assembly (see `boot` in main.rs) identity-maps the first 4 GiB
//! of physical memory using 2 MiB pages. Any physical address the kernel
//! wants to access directly (RAM, or MMIO such as the VBE linear
//! framebuffer) must fall below this bound.

pub const IDENTITY_MAP_END: u64 = 4 * 1024 * 1024 * 1024;

pub fn is_identity_mapped(addr: u64, len: u64) -> bool {
    match addr.checked_add(len) {
        Some(end) => end <= IDENTITY_MAP_END,
        None => false,
    }
}
