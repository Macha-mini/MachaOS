#![no_std]
#![no_main]
#![allow(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod allocator;
mod ata;
mod calculator;
mod console;
mod cpuid;
mod desktop;
mod elf;
mod fb;
mod fat;
mod font;
mod gdt;
mod gfx;
mod idt;
mod interrupts;
#[macro_use]
mod io;
mod isr_table;
mod keyboard;
mod mouse;
mod multiboot;
mod paging;
mod pic;
mod pit;
mod pmm;
mod port;
mod process;
mod rtc;
mod serial;
mod shell;
mod sync;
mod syscall;
mod task;
mod user_prog;
mod vga;
mod wm;

use core::arch::global_asm;
use core::panic::PanicInfo;

global_asm!(
    r#"
.section .multiboot, "a"
.align 4
multiboot_header:
    .long 0x1BADB002   # magic
    .long 0x00000007   # flags: align | meminfo | video request(bit2)
    .long 0xE4524FF7   # checksum = -(magic+flags)
    .long 0x00000000   # header_addr (unused, flags[16] not set)
    .long 0x00000000   # load_addr
    .long 0x00000000   # load_end_addr
    .long 0x00000000   # bss_end_addr
    .long 0x00000000   # entry_addr
    .long 0x00000000   # mode_type = 0 (linear graphics), offset 32 per spec
    .long 1920         # width
    .long 1080         # height
    .long 32           # depth

.section .bss
.balign 4096
page_table_pml4:
    .skip 4096
page_table_pdp:
    .skip 4096
page_table_pd:
    .skip 4096 * 4
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

    leal page_table_pdp, %edi
    leal page_table_pd, %eax
    orl $0x3, %eax
    movl $4, %ecx
1:
    movl %eax, 0(%edi)
    addl $4096, %eax
    addl $8, %edi
    loop 1b

    leal page_table_pd, %edi
    xorl %eax, %eax
    movl $2048, %ecx
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

    println!("[OK] initializing physical memory manager...");
    pmm::init(&info);
    println!(
        "[OK] memory: {} MiB usable ({} KiB heap reserved)",
        (pmm::total_frames() * pmm::FRAME_SIZE) / (1024 * 1024),
        allocator::HEAP_SIZE / 1024
    );

    println!("[OK] initializing heap allocator...");
    allocator::init();

    println!("[OK] loading GDT (kernel code/data/TSS)...");
    gdt::init();

    println!("[OK] loading IDT (256 interrupt vectors)...");
    idt::init();

    println!("[OK] enabling SYSCALL/SYSRET (EFER.SCE, STAR/LSTAR/SFMASK)...");
    syscall::init();

    println!("[OK] remapping PIC (IRQ0-15 -> 0x20-0x2F)...");
    pic::remap();

    println!("[OK] programming PIT timer at 100 Hz...");
    pit::init();

    println!("[OK] starting preemptive task scheduler...");
    task::init();

    println!("[OK] enabling PS/2 mouse (IRQ12)...");
    mouse::init();

    println!("[OK] probing ATA devices...");
    ata::init();
    for index in 0..ata::count() {
        println!("[OK] {}", ata::describe(index).unwrap_or_default());
    }
    if ata::count() == 0 {
        println!("[WARN] no ATA devices found");
    }

    match fat::mount() {
        Ok(()) => println!("[OK] FAT32 volume mounted"),
        Err(e) => println!("[WARN] no FAT32 volume: {}", e),
    }

    if fb::init(&info) {
        let (width, height) = fb::dimensions();
        println!("[OK] graphics mode: {}x{}x32", width, height);
    } else {
        println!("[WARN] no usable linear framebuffer; staying in VGA text mode");
    }
    if let Some(framebuffer) = info.framebuffer() {
        // Keep the physical memory manager from ever handing out the
        // framebuffer's MMIO region, in case it wasn't flagged reserved
        // in the memory map.
        pmm::mark_reserved(
            framebuffer.addr as usize,
            framebuffer.pitch as usize * framebuffer.height as usize,
        );
    }

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

    interrupts::enable_interrupts();

    let selftest_mode = info
        .cmdline()
        .is_some_and(|cmdline| cmdline.split_whitespace().any(|word| word == "selftest"));
    if selftest_mode {
        shell::selftest();
    }

    println!();
    println!("System ready.");
    println!();

    if fb::is_graphics_mode() {
        desktop::run();
    } else {
        println!("No usable framebuffer; falling back to the text shell.");
        println!("Type 'help' for available commands.");
        println!();
        shell::run();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut buf = [0u8; 256];
    let message = io::sprint(&mut buf, format_args!("KERNEL PANIC: {}", info.message()));
    io::exception_print("\n");
    io::exception_print(message);
    io::exception_print("\n");

    let mut loc_buf = [0u8; 128];
    let location = info.location().map(|location| {
        io::sprint(
            &mut loc_buf,
            format_args!("  at {}:{}", location.file(), location.line()),
        )
    });
    if let Some(location) = location {
        io::exception_print(location);
        io::exception_print("\n");
    }
    io::exception_print("System halted.\n");

    if fb::is_graphics_mode() {
        // No locks, no heap: the allocator or another CPU-visible lock
        // holder may be in a bad state, but a panic never returns so
        // there is no concurrent access to race against.
        fb::emergency_fill(0x00_7F0000);
        fb::emergency_draw_string(16, 16, message, 0x00_FFFFFF);
        if let Some(location) = location {
            fb::emergency_draw_string(16, 16 + font::GLYPH_HEIGHT as u32 + 4, location, 0x00_FFFFFF);
        }
        fb::emergency_draw_string(
            16,
            16 + 2 * (font::GLYPH_HEIGHT as u32 + 4),
            "System halted.",
            0x00_FFFFFF,
        );
    }

    interrupts::halt_forever()
}
