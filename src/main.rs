#![no_std]
#![no_main]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod allocator;
mod cpuid;
mod gdt;
mod idt;
mod interrupts;
#[macro_use]
mod io;
mod isr_table;
mod keyboard;
mod multiboot;
mod pic;
mod pit;
mod port;
mod serial;
mod shell;
mod sync;
mod vga;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(
    r#"
.section .multiboot, "a"
.align 4
multiboot_header:
    .long 0x1BADB002
    .long 0x00000003
    .long 0xE4524FFB

.section .bss
.balign 4096
page_table_pml4:
    .skip 4096
page_table_pdp:
    .skip 4096
page_table_pd:
    .skip 4096
stack_bottom:
    .skip 65536
    .balign 16
stack_top:

.section .text
.code32

.globl boot
.type boot, @function
boot:
    cli
    cld
    movl %eax, %ebp

    movl %cr0, %eax
    orl $0x2, %eax
    movl %eax, %cr0
    movl %cr4, %eax
    orl $0x600, %eax
    movl %eax, %cr4

    leal page_table_pml4, %edi
    leal page_table_pdp, %eax
    orl $0x3, %eax
    movl %eax, 0(%edi)

    leal page_table_pd, %eax
    orl $0x3, %eax
    movl %eax, 0x1000(%edi)

    leal page_table_pd, %edi
    xorl %eax, %eax
    movl $8, %ecx
1:
    movl %eax, %esi
    shll $21, %esi
    orl $0x83, %esi
    movl %esi, 0(%edi)
    addl $8, %edi
    incl %eax
    loop 1b

    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    movl $0xC0000080, %ecx
    rdmsr
    orl $0x100, %eax
    wrmsr

    movl $page_table_pml4, %eax
    movl %eax, %cr3
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0

    lgdt gdt_descriptor
    ljmp $0x08, $start64

.code64
start64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %fs
    movw %ax, %gs
    movw %ax, %ss

    movq $stack_top, %rsp

    movl %ebp, %edi
    movl %ebx, %esi
    call kmain

1:
    hlt
    jmp 1b

.section .data
gdt_descriptor:
    .word gdt_end - gdt_start - 1
    .long gdt_start
gdt_start:
    .quad 0x0000000000000000
    .quad 0x00209A0000000000
    .quad 0x0000920000000000
    .quad 0x0000000000000000
gdt_end:
"#,
    options(att_syntax)
);

#[unsafe(no_mangle)]
pub extern "C" fn kmain(magic: u32, multiboot_info: u32) -> ! {
    serial::init();
    vga::init();

    vga::set_color(vga::color(vga::colors::LIGHT_CYAN, vga::colors::BLACK));
    println!();
    println!("M   M  AAA   CCC  H   H  AAA   OOO   SSS");
    println!("MM MM A   A C    H   H A   A O   O S");
    println!("M M M AAAAA C    HHHHH AAAAA O   O  SSS");
    println!("M   M A   A C    H   H A   A O   O    S");
    println!("M   M A   A  CCC H   H A   A  OOO   SSS");
    println!("MachaOS v0.1.0 - a small x86_64 OS written in Rust");
    println!();
    vga::reset_color();

    let info = multiboot::MultibootInfo::new(multiboot_info as usize);
    multiboot::set_info(multiboot_info as usize);

    if magic == multiboot::MAGIC {
        println!("[OK] multiboot boot protocol (magic {:#x})", magic);
    } else {
        println!("[WARN] invalid multiboot magic: {:#x}", magic);
    }
    if let Some(name) = info.boot_loader_name() {
        println!("[OK] boot loader: {}", name);
    }
    if let Some(cmdline) = info.cmdline() {
        println!("[OK] kernel cmdline: '{}'", cmdline);
    }
    if let (Some(lower), Some(upper)) = (info.memory_lower_kb(), info.memory_upper_kb()) {
        println!("[OK] memory: {} KiB lower, {} KiB upper", lower, upper);
    }
    println!("[OK] memory map: {} regions", info.memory_map().count());

    println!("[OK] initializing heap allocator...");
    allocator::init();

    println!("[OK] loading GDT (kernel code/data/TSS)...");
    gdt::init();

    println!("[OK] loading IDT (256 interrupt vectors)...");
    idt::init();

    println!("[OK] remapping PIC (IRQ0-15 -> 0x20-0x2F)...");
    pic::remap();

    println!("[OK] programming PIT timer at 100 Hz...");
    pit::init();

    let vendor = cpuid::vendor_id();
    println!(
        "[OK] CPU: {} ({} logical cores)",
        core::str::from_utf8(&vendor).unwrap_or("unknown"),
        cpuid::cores()
    );
    if let Some(brand) = cpuid::brand_string() {
        println!("     brand: {}", core::str::from_utf8(&brand).unwrap_or("").trim_end());
    }
    let features = cpuid::features();
    println!("     features: {}", features.join(" "));

    let selftest_mode = info
        .cmdline()
        .is_some_and(|cmdline| cmdline.split_whitespace().any(|word| word == "selftest"));
    if selftest_mode {
        shell::selftest();
    }

    println!();
    println!("System ready. Type 'help' for available commands.");
    println!();

    interrupts::enable_interrupts();
    shell::run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut buf = [0u8; 256];
    io::exception_print(io::sprint(
        &mut buf,
        format_args!("\nKERNEL PANIC: {}\n", info.message()),
    ));
    if let Some(location) = info.location() {
        let mut buf = [0u8; 128];
        io::exception_print(io::sprint(
            &mut buf,
            format_args!("  at {}:{}\n", location.file(), location.line()),
        ));
    }
    io::exception_print("System halted.\n");
    interrupts::halt_forever()
}
