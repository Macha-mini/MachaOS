//! Power control: safe shutdown and reboot.
//!
//! Both paths save the system settings first (the caller — the window
//! manager — persists the desktop session before handing over), then
//! perform the hardware action:
//!
//! - `shutdown` performs a real ACPI S5 transition (RSDP/FADT ->
//!   PM1a_CNT write, see `acpi.rs`) so the machine — VM or real
//!   hardware — actually powers off; if the tables are missing it
//!   falls back to QEMU's `isa-debug-exit` port (0xF4).
//! - `reboot` pulses the 8042 keyboard controller reset line (0x64 /
//!   0xFE), the classic PC reboot mechanism.
//!
//! Neither ever returns: after the port write the CPU halts forever as
//! a fallback (in case the port write was ignored).

use crate::port;
use crate::settings::Settings;

/// Saves settings and powers the machine off (real ACPI S5, with a
/// QEMU isa-debug-exit fallback). Never returns.
pub fn shutdown(settings: &Settings) -> ! {
    let _ = settings.save();
    let _ = crate::fat::remove("/system/dirty"); // clean shutdown marker
    crate::acpi::s5_shutdown();
    // Only reached if ACPI ignored the write (no tables / not S5-capable).
    unsafe { port::outb(0xF4, 0) }; // QEMU isa-debug-exit fallback
    crate::interrupts::halt_forever()
}

/// Saves settings and reboots the machine (8042 reset). Never returns.
pub fn reboot(settings: &Settings) -> ! {
    let _ = settings.save();
    let _ = crate::fat::remove("/system/dirty"); // clean shutdown marker
    unsafe { port::outb(0x64, 0xFE) }; // 8042 reset
    crate::interrupts::halt_forever()
}
