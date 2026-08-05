//! Power control: safe shutdown and reboot.
//!
//! Both paths save the system settings first (the caller — the window
//! manager — persists the desktop session before handing over), then
//! perform the hardware action:
//!
//! - `shutdown` writes to QEMU's `isa-debug-exit` port (0xF4), which
//!   makes the emulator exit with a success status; on real hardware
//!   this would be an ACPI S5 transition, which this OS does not
//!   implement yet.
//! - `reboot` pulses the 8042 keyboard controller reset line (0x64 /
//!   0xFE), the classic PC reboot mechanism.
//!
//! Neither ever returns: after the port write the CPU halts forever as
//! a fallback (in case the port write was ignored).

use crate::port;
use crate::settings::Settings;

/// Saves settings and powers the machine off (QEMU: isa-debug-exit).
/// Never returns.
pub fn shutdown(settings: &Settings) -> ! {
    let _ = settings.save();
    let _ = crate::fat::remove("/system/dirty"); // clean shutdown marker
    unsafe { port::outb(0xF4, 0) }; // QEMU isa-debug-exit
    crate::interrupts::halt_forever()
}

/// Saves settings and reboots the machine (8042 reset). Never returns.
pub fn reboot(settings: &Settings) -> ! {
    let _ = settings.save();
    let _ = crate::fat::remove("/system/dirty"); // clean shutdown marker
    unsafe { port::outb(0x64, 0xFE) }; // 8042 reset
    crate::interrupts::halt_forever()
}
