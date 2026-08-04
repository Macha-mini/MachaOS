use core::arch::asm;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const TSS_SELECTOR: u16 = 0x18;

// SYSRET (64-bit) requires a specific layout: given a base selector B in
// IA32_STAR[63:48], it loads CS = (B+16)|3 and SS = (B+8)|3. That forces
// three *consecutive* GDT slots — [B]=unused, [B+8]=user data, [B+16]=user
// code — which can't start at 0x18 because the TSS descriptor (a 16-byte
// system descriptor) already occupies both 0x18 and 0x20. So the unused
// slot starts right after the TSS instead, at 0x28.
pub const SYSRET_BASE: u16 = 0x28;
pub const USER_DATA: u16 = SYSRET_BASE + 8; // 0x30, RPL added by callers
pub const USER_CODE: u16 = SYSRET_BASE + 16; // 0x38, RPL added by callers

#[repr(C, packed)]
struct TaskStateSegment {
    reserved1: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved2: u64,
    ist: [u64; 7],
    reserved3: u64,
    reserved4: u16,
    iomap_base: u16,
}

impl TaskStateSegment {
    const fn new() -> Self {
        Self {
            reserved1: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved2: 0,
            ist: [0; 7],
            reserved3: 0,
            reserved4: 0,
            iomap_base: 0xFFFF,
        }
    }
}

static mut TSS: TaskStateSegment = TaskStateSegment::new();
const DOUBLE_FAULT_STACK_SIZE: usize = 16384;
static mut DOUBLE_FAULT_STACK: [u8; DOUBLE_FAULT_STACK_SIZE] = [0; DOUBLE_FAULT_STACK_SIZE];
// Index: 0 null, 1 kernel code (0x08), 2 kernel data (0x10), 3-4 TSS
// (0x18, a 16-byte descriptor), 5 unused (0x28, see SYSRET_BASE above),
// 6 user data (0x30), 7 user code (0x38).
static mut GDT: [u64; 8] = [
    0,
    0x00209A0000000000, // kernel code: P DPL0 S exec/read, L=1
    0x0000920000000000, // kernel data: P DPL0 S read/write
    0,
    0,
    0, // never loaded as a real selector; SYSRET only uses it for arithmetic
    0x0000F20000000000, // user data: P DPL3 S read/write
    0x0020FA0000000000, // user code: P DPL3 S exec/read, L=1
];

#[repr(C, packed)]
struct GdtDescriptor {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    static stack_top: u8;
}

pub fn init() {
    unsafe {
        TSS.rsp0 = &stack_top as *const u8 as u64;
        TSS.ist[1] =
            (core::ptr::addr_of!(DOUBLE_FAULT_STACK) as usize + DOUBLE_FAULT_STACK_SIZE) as u64;

        let base = core::ptr::addr_of!(TSS) as u64;
        let mut descriptor: u64 = 0x67; // limit = 0x67 (104 bytes)
        descriptor |= (base & 0xFFFF) << 16; // base[15:0]
        descriptor |= ((base >> 16) & 0xFF) << 32; // base[23:16]
        descriptor |= 0x89 << 40; // type: 64-bit TSS, available
        descriptor |= ((base >> 24) & 0xFF) << 56; // base[31:24]
        GDT[3] = descriptor;
        GDT[4] = base >> 32;

        let gdt = GdtDescriptor {
            limit: (core::mem::size_of::<[u64; 8]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };
        asm!("lgdt [{}]", in(reg) &gdt, options(nostack, readonly, preserves_flags));

        asm!(
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            options(nostack, readonly, preserves_flags)
        );

        asm!(
            "mov ax, 0x18",
            "ltr ax",
            options(nostack, readonly, preserves_flags)
        );
    }
}
