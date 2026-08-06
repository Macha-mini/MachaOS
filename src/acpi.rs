//! Minimal ACPI support for a real S5 (power-off) transition.
//!
//! A full AML interpreter is out of scope (see the roadmap): S5 only
//! needs three pieces — the RSDP -> RSDT/XSDT -> FADT chain for the
//! PM1a_CNT I/O port, the sleep type from the DSDT's `\_S5` package,
//! and the SMI-command ACPI enable that gates the PM registers on the
//! PIIX/ICH9 chipsets QEMU emulates. QEMU, UTM and Parallels all build
//! standard tables, so this works on the VM targets; the same parsing
//! applies to real firmware.
//!
//! The S5 write is `outw(PM1a_CNT, (SLP_TYP << 10) | SLP_EN)` with
//! SLP_EN = bit 13 (PM1x_CNT layout: SLP_TYP bits 10-12, SLP_EN bit
//! 13). The sleep type comes from `\_S5`'s first integer, parsed by
//! scanning the DSDT for the `_S5`/`_S5_` name and decoding the
//! following `Package` (the classic no-AML approach).

use crate::port;

/// `RSD PTR ` — the RSDP signature, checked at 16-byte-aligned
/// addresses in the EBDA and the BIOS ROM area.
const RSDP_SIG: [u8; 8] = *b"RSD PTR ";
/// The FADT's signature is `FACP`.
const FADT_SIG: [u8; 4] = *b"FACP";
/// `_S5` in AML — the sleep-state package name we scan for.
const S5_NAME: [u8; 3] = [0x5F, 0x53, 0x35]; // "_S5"
/// SLP_EN bit in the PM1x_CNT register.
const SLP_EN: u16 = 1 << 13;
/// SLP_TYP field position in the PM1x_CNT register.
const SLP_TYP_SHIFT: u16 = 10;

/// Everything `s5_shutdown` needs, decoded from the tables.
pub struct AcpiS5 {
    /// PM1a control-block I/O port.
    pub pm1a_cnt: u16,
    /// S5 sleep type from `\_S5` (PM1x_CNT.SLP_TYP).
    pub slp_typa: u16,
    /// SMI command port for enabling ACPI (0 = none).
    pub smi_cmd: u16,
    /// Value to write to the SMI command port to enable ACPI (0 = none).
    pub acpi_enable: u8,
}

// --- physical-memory reads (the kernel identity-maps all of RAM) ----

fn r8(p: u64) -> u8 {
    unsafe { core::ptr::read_volatile(p as *const u8) }
}
fn r16(p: u64) -> u16 {
    unsafe { core::ptr::read_volatile(p as *const u16) }
}
fn r32(p: u64) -> u32 {
    unsafe { core::ptr::read_volatile(p as *const u32) }
}
fn r64(p: u64) -> u64 {
    unsafe { core::ptr::read_volatile(p as *const u64) }
}

fn sig(p: u64, want: &[u8]) -> bool {
    for (i, &b) in want.iter().enumerate() {
        if r8(p + i as u64) != b {
            return false;
        }
    }
    true
}

/// Locates the RSDP (root system description pointer): first in the
/// EBDA (its segment is a word at 0x40E), then in the BIOS ROM area
/// 0xE0000-0xFFFFF, at 16-byte alignment. Verifies the v1 checksum
/// (the first 20 bytes sum to zero).
fn find_rsdp() -> Option<u64> {
    let ebda_seg = unsafe { core::ptr::read_volatile(0x40E as *const u16) } as u64;
    let ebda = ebda_seg << 4;
    if ebda >= 0x400 {
        for a in (ebda..ebda + 0x400).step_by(16) {
            if valid_rsdp(a) {
                return Some(a);
            }
        }
    }
    for a in (0xE0000u64..0x100000).step_by(16) {
        if valid_rsdp(a) {
            return Some(a);
        }
    }
    None
}

fn valid_rsdp(p: u64) -> bool {
    if !sig(p, &RSDP_SIG) {
        return false;
    }
    let mut sum: u32 = 0;
    for i in 0..20 {
        sum += r8(p + i as u64) as u32;
    }
    sum & 0xFF == 0
}

/// True when `p` looks like a sane ACPI table header: the expected
/// signature and a length within the plausible range (the header is 36
/// bytes and a table list won't be megabytes). Guards the entry scans
/// against a garbage RSDP pointer.
fn valid_table(p: u64, want: &[u8]) -> bool {
    sig(p, want) && {
        let len = r32(p + 4) as u64;
        (36..0x10000).contains(&len)
    }
}

/// Scans the RSDT/XSDT entry lists for the FADT. Prefers the XSDT
/// (64-bit entries) when the RSDP revision says it exists, then falls
/// back to the RSDT (32-bit entries).
fn fadt_addr() -> Option<u64> {
    let rsdp = find_rsdp()?;
    if r8(rsdp + 15) >= 2 {
        let xsdt = r64(rsdp + 24);
        if valid_table(xsdt, b"XSDT") {
            let len = r32(xsdt + 4) as u64;
            let mut off = 36u64;
            while off + 8 <= len {
                let ent = r64(xsdt + off);
                if sig(ent, &FADT_SIG) {
                    return Some(ent);
                }
                off += 8;
            }
        }
    }
    let rsdt = r32(rsdp + 16) as u64;
    if valid_table(rsdt, b"RSDT") {
        let len = r32(rsdt + 4) as u64;
        let mut off = 36u64;
        while off + 4 <= len {
            let ent = r32(rsdt + off) as u64;
            if sig(ent, &FADT_SIG) {
                return Some(ent);
            }
            off += 4;
        }
    }
    None
}

/// Decodes an AML PkgLength at `p` (returns the length and how many
/// bytes it occupied). One byte when bit 7 is clear (length in bits
/// 0-5); otherwise bits 6-7 say how many extra bytes follow and bits
/// 0-3 of the first byte hold the most-significant nibble.
fn pkg_length(p: u64) -> Option<(u64, u64)> {
    let b0 = r8(p);
    if b0 & 0x80 == 0 {
        return Some((b0 as u64 & 0x3F, 1));
    }
    let nbytes = ((b0 >> 6) + 1) as u64;
    if nbytes > 4 {
        return None;
    }
    let mut len = (b0 & 0x0F) as u64;
    for k in 1..nbytes {
        len = (len << 8) | r8(p + k) as u64;
    }
    Some((len, nbytes))
}

/// Reads an AML integer constant at `*q` (advanced past it), bounded
/// by `end`: Zero (0x00), One (0x01), BytePrefix (0x0B + 1),
/// DWordPrefix (0x0A + 4) or QWordPrefix (0x0C + 8). Returns the
/// value, or None on a malformed/oversized element.
fn aml_int(p: u64, q: &mut u64, end: u64) -> Option<u16> {
    if *q + 1 > end {
        return None;
    }
    let op = r8(p + *q);
    let (val, consumed): (u64, u64) = match op {
        0x00 => (0, 1),
        0x01 => (1, 1),
        0x0B => {
            if *q + 2 > end {
                return None;
            }
            (r8(p + *q + 1) as u64, 2)
        }
        0x0A => {
            if *q + 5 > end {
                return None;
            }
            (r32(p + *q + 1) as u64, 5)
        }
        0x0C => {
            if *q + 9 > end {
                return None;
            }
            (r64(p + *q + 1), 9)
        }
        _ => return None,
    };
    *q += consumed;
    Some(val.min(u16::MAX as u64) as u16)
}

/// Finds the S5 sleep type by scanning the DSDT for the `_S5` name and
/// decoding the `Package` that follows it (`Name (_S5, Package (...) {
/// a, b, ... })`). QEMU names the object `_S5_` (trailing underscore)
/// and its package elements are Zero/One/Byte/DWord constants. Returns
/// `a` (the PM1a sleep type); `Some(0)` when the object exists but its
/// first element is zero (QEMU's DSDT does exactly that), `None` when
/// the DSDT has no parseable `_S5` at all.
fn dsdt_s5(dsdt: u64) -> Option<u16> {
    let len = r32(dsdt + 4) as u64;
    // DSDT header is 36 bytes; scan the body for the `_S5` name whose
    // following bytes are the package: `_S5 <0x12 ...>` or
    // `_S5_ <0x12 ...>` (a trailing underscore before the PackageOp).
    let mut i = 36u64;
    while i + 5 < len {
        if sig(dsdt + i, &S5_NAME) {
            let mut q = i + 3;
            if r8(dsdt + q) == 0x5F {
                q += 1; // "_S5_" — skip the trailing underscore
            }
            if r8(dsdt + q) != 0x12 {
                // Not a Name(_S5, Package...) — a method *reference* to
                // \_S5 elsewhere; keep scanning.
                i += 1;
                continue;
            }
            q += 1; // past the PackageOp
            let (plen, n) = pkg_length(dsdt + q)?;
            let body = q + n;
            let pkg_end = body + plen;
            if body >= pkg_end {
                return None;
            }
            let count = r8(dsdt + body);
            if count >= 1 {
                let mut e = body + 1;
                if let Some(a) = aml_int(dsdt, &mut e, pkg_end) {
                    return Some(a);
                }
            }
            // The object exists but its elements don't parse as plain
            // constants — fall through to the caller's default.
            return Some(0);
        }
        i += 1;
    }
    None
}

/// Decodes the ACPI parameters needed for an S5 transition.
pub fn s5_params() -> Option<AcpiS5> {
    let fadt = fadt_addr()?;
    let pm1a_cnt = r32(fadt + 0x40) as u16; // FADT.PM1a_CNT_BLK
    let dsdt = r32(fadt + 0x28) as u64; // FADT.DSDT
    let smi_cmd = r32(fadt + 0x30) as u16; // FADT.SMI_CMD
    let acpi_enable = r8(fadt + 0x34); // FADT.ACPI_ENABLE
    let slp_typa = dsdt_s5(dsdt)?;
    Some(AcpiS5 {
        pm1a_cnt,
        slp_typa,
        smi_cmd,
        acpi_enable,
    })
}

/// Waits a few scheduler ticks so a failed SLP write can be retried
/// with a different sleep type (only relevant on VM targets where the
/// chipset ignores unknown values; on real firmware the first write
/// either powers the machine off — after which nothing here runs — or
/// is simply not honoured by the firmware, in which case the fallback
/// debug-exit still ends the run).
fn wait_ticks(n: u64) {
    let deadline = crate::interrupts::ticks() + n;
    while crate::interrupts::ticks() < deadline {
        crate::interrupts::halt();
    }
}

/// Performs a real S5 (power-off) transition. Enables ACPI through the
/// SMI command port (QEMU's PIIX/ICH9 gate the PM registers on it),
/// then writes `(SLP_TYP << 10) | SLP_EN` to the PM1a control port —
/// first with the `\_S5` sleep type from the tables, then with the
/// common 7 and 5 values (QEMU's DSDT returns 0, which the chipset
/// ignores). Returns true once a write was issued; on success the
/// machine powers off asynchronously and nothing after runs. Returns
/// false when ACPI tables are missing or malformed, so the caller can
/// fall back to QEMU's isa-debug-exit.
pub fn s5_shutdown() -> bool {
    let Some(s5) = s5_params() else {
        return false;
    };
    // Gate: PIIX/ICH9 ignore PM writes until ACPI is enabled via the
    // SMI command port. Only write when the FADT supplies a port+value.
    if s5.smi_cmd != 0 && s5.acpi_enable != 0 {
        unsafe { port::outb(s5.smi_cmd, s5.acpi_enable) };
    }
    // The `\_S5` value first (correct on real firmware / UTM / Parallels),
    // then the values QEMU's chipset reacts to when the table says 0.
    let mut val = (s5.slp_typa << SLP_TYP_SHIFT) | SLP_EN;
    for typ in [s5.slp_typa, 7, 5] {
        if typ != s5.slp_typa && s5.slp_typa != 0 {
            continue; // the table's own value is authoritative; 7/5 are QEMU-only
        }
        val = (typ << SLP_TYP_SHIFT) | SLP_EN;
        crate::io::exception_print(crate::io::sprint(
            &mut [0u8; 96],
            format_args!(
                "[ACPI] S5: PM1a_CNT=0x{:x} <- 0x{:x} (SLP_TYP={})\n",
                s5.pm1a_cnt, val, typ
            ),
        ));
        unsafe { port::outw(s5.pm1a_cnt, val) };
        wait_ticks(10); // let a real transition take effect before retrying
    }
    true
}
