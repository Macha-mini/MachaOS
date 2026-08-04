use alloc::vec::Vec;
use core::arch::asm;

pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let mut eax = leaf;
    let mut ebx: u32 = 0;
    let mut ecx: u32 = 0;
    let mut edx: u32 = 0;
    unsafe {
        // rbx is callee-saved and cannot be used as a direct asm operand
        asm!(
            "mov {saved:r}, rbx",
            "cpuid",
            "mov {res:e}, ebx",
            "mov rbx, {saved:r}",
            saved = out(reg) _,
            res = out(reg) ebx,
            inout("eax") eax,
            inout("ecx") ecx,
            inout("edx") edx,
            options(nostack, preserves_flags)
        );
    }
    (eax, ebx, ecx, edx)
}

pub fn vendor_id() -> [u8; 12] {
    let (_, ebx, ecx, edx) = cpuid(0);
    let mut id = [0u8; 12];
    id[0..4].copy_from_slice(&ebx.to_le_bytes());
    id[4..8].copy_from_slice(&edx.to_le_bytes());
    id[8..12].copy_from_slice(&ecx.to_le_bytes());
    id
}

pub fn max_extended_leaf() -> u32 {
    cpuid(0x8000_0000).0
}

pub fn brand_string() -> Option<[u8; 48]> {
    if max_extended_leaf() < 0x8000_0004 {
        return None;
    }
    let mut brand = [0u8; 48];
    for leaf in 0..3u32 {
        let (a, b, c, d) = cpuid(0x8000_0002 + leaf);
        let chunk = [a, b, c, d];
        let offset = leaf as usize * 16;
        for (i, value) in chunk.iter().enumerate() {
            brand[offset + i * 4..offset + i * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    Some(brand)
}

pub fn cores() -> u32 {
    let (_, ebx, _, _) = cpuid(1);
    ((ebx >> 16) & 0xFF) + 1
}

pub fn features() -> Vec<&'static str> {
    let mut out = Vec::new();
    let (_, _, ecx, edx) = cpuid(1);
    if edx & (1 << 0) != 0 {
        out.push("FPU");
    }
    if edx & (1 << 6) != 0 {
        out.push("PAE");
    }
    if edx & (1 << 7) != 0 {
        out.push("PGE");
    }
    if edx & (1 << 25) != 0 {
        out.push("SSE");
    }
    if edx & (1 << 26) != 0 {
        out.push("SSE2");
    }
    if edx & (1 << 29) != 0 {
        out.push("LM");
    }
    if ecx & (1 << 0) != 0 {
        out.push("SSE3");
    }
    if ecx & (1 << 9) != 0 {
        out.push("SSSE3");
    }
    if ecx & (1 << 19) != 0 {
        out.push("SSE4.1");
    }
    if ecx & (1 << 20) != 0 {
        out.push("SSE4.2");
    }
    if ecx & (1 << 28) != 0 {
        out.push("AVX");
    }
    out
}
