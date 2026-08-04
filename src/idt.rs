use core::arch::asm;
use core::mem::MaybeUninit;

use crate::gdt::KERNEL_CODE;
use crate::interrupts;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    fn new(offset: u64, selector: u16, ist: u8) -> Self {
        Self {
            offset_low: (offset & 0xFFFF) as u16,
            selector,
            ist,
            type_attr: 0x8E, // present, DPL 0, 64-bit interrupt gate
            offset_mid: ((offset >> 16) & 0xFFFF) as u16,
            offset_high: ((offset >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }
}

#[repr(C)]
struct Idt {
    entries: [IdtEntry; 256],
}

impl Idt {
    fn new() -> Self {
        let mut entries = [IdtEntry::new(0, 0, 0); 256];
        for (i, entry) in entries.iter_mut().enumerate() {
            *entry = IdtEntry::new(
                crate::isr_table::ISR_ENTRIES[i] as u64,
                KERNEL_CODE,
                if i == 8 { 1 } else { 0 }, // double fault uses IST 1
            );
        }
        Self { entries }
    }
}

#[repr(C, packed)]
struct IdtDescriptor {
    limit: u16,
    base: u64,
}

static mut IDT_STORAGE: MaybeUninit<Idt> = MaybeUninit::uninit();

pub fn init() {
    interrupts::register_default_handlers();
    unsafe {
        let idt = core::ptr::addr_of_mut!(IDT_STORAGE).cast::<Idt>();
        idt.write(Idt::new());
        let descriptor = IdtDescriptor {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &descriptor, options(nostack, readonly, preserves_flags));
    }
}
