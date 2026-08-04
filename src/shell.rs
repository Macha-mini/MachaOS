use alloc::string::String;
use alloc::vec::Vec;

use crate::{cpuid, interrupts, io, keyboard, mouse, multiboot, port, serial, vga};

const BANNER: &str = "MachaOS v0.1.0";

pub fn run() -> ! {
    loop {
        vga::set_color(vga::colors::LIGHT_GREEN);
        print!("machaos> ");
        vga::reset_color();

        let line = read_line();
        execute(&line);
    }
}

fn read_line() -> String {
    let mut line = String::new();
    loop {
        while let Some(event) = keyboard::next_event() {
            match event {
                keyboard::Event::Char(c) => {
                    if line.len() < 256 {
                        line.push(c);
                        vga::print_char(c as u8);
                        serial::write_byte(c as u8);
                    }
                }
                keyboard::Event::Backspace => {
                    if line.pop().is_some() {
                        vga::erase_char();
                        serial::write_byte(0x08);
                    }
                }
                keyboard::Event::Enter => {
                    println!();
                    return line;
                }
                keyboard::Event::Tab => {
                    for _ in 0..4 {
                        line.push(' ');
                        vga::print_char(b' ');
                        serial::write_byte(b' ');
                    }
                }
            }
        }
        interrupts::halt();
    }
}

pub fn execute(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    let mut words = line.split_whitespace();
    let command = words.next().unwrap();
    let args: Vec<&str> = words.collect();

    match command {
        "help" => cmd_help(),
        "clear" | "cls" => io::clear_active(),
        "echo" => {
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    print!(" ");
                }
                print!("{}", arg);
            }
            println!();
        }
        "time" => {
            let ticks = interrupts::ticks();
            println!("system ticks: {}", ticks);
        }
        "uptime" => {
            let ticks = interrupts::ticks();
            println!("uptime: {} seconds ({} ticks)", ticks / 100, ticks);
        }
        "meminfo" => cmd_meminfo(),
        "heap" => cmd_heap(),
        "cpuinfo" => cmd_cpuinfo(),
        "version" | "ver" => println!("{}", BANNER),
        "reboot" => {
            println!("rebooting...");
            reboot();
        }
        "shutdown" => {
            println!("power off (QEMU isa-debug-exit)...");
            power_off();
        }
        "crash" => {
            println!("triggering a divide-by-zero exception (#DE)...");
            crash();
        }
        "breakpoint" => {
            println!("triggering an int3 breakpoint (#BP)...");
            breakpoint_demo();
            println!("...returned from the breakpoint");
        }
        "fault" => {
            println!("triggering a page fault (#PF)...");
            fault_demo();
        }
        "panic" => panic!("user-requested kernel panic"),
        "mousetest" => cmd_mousetest(),
        _ => println!("unknown command: '{}' (type 'help')", command),
    }
}

fn cmd_help() {
    println!("Available commands:");
    println!("  help        show this help");
    println!("  clear       clear the screen");
    println!("  echo <txt>  print text");
    println!("  time        print timer ticks");
    println!("  uptime      print time since boot");
    println!("  meminfo     print memory information");
    println!("  heap        exercise the heap allocator");
    println!("  cpuinfo     print CPU information");
    println!("  version     print kernel version");
    println!("  reboot      reboot the machine");
    println!("  shutdown    power off (QEMU only)");
    println!("  crash       trigger a divide-by-zero exception");
    println!("  breakpoint  trigger an int3 breakpoint");
    println!("  fault       trigger a page fault");
    println!("  panic       trigger a kernel panic");
    println!("  mousetest   poll the PS/2 mouse for a few seconds");
}

fn cmd_meminfo() {
    let info = multiboot::info();

    println!("boot loader: {}", info.boot_loader_name().unwrap_or("unknown"));
    if let (Some(lower), Some(upper)) = (info.memory_lower_kb(), info.memory_upper_kb()) {
        println!(
            "memory: {} KiB lower + {} KiB upper = {} KiB total",
            lower,
            upper,
            lower + upper
        );
    }

    let mut total_usable: u64 = 0;
    let mut largest: u64 = 0;
    let mut region_count: usize = 0;
    for region in info.memory_map() {
        region_count += 1;
        if region.region_type == 1 {
            total_usable += region.length;
            if region.length > largest {
                largest = region.length;
            }
        }
    }
    println!("memory map ({} regions):", region_count);
    for region in info.memory_map() {
        println!(
            "  [{:#x} - {:#x}] {} KiB ({})",
            region.base,
            region.base + region.length,
            region.length / 1024,
            region.type_name()
        );
    }
    println!(
        "usable: {} KiB total, largest region {} KiB",
        total_usable / 1024,
        largest / 1024
    );
    println!(
        "heap: {} bytes total, {} allocated, {} free",
        crate::allocator::total_bytes(),
        crate::allocator::allocated_bytes(),
        crate::allocator::free_bytes()
    );
}

fn cmd_heap() {
    let mut vector: Vec<u64> = Vec::new();
    for i in 0..4096 {
        vector.push((i as u64).wrapping_mul(31).wrapping_add(7));
    }
    let sum: u64 = vector.iter().sum();
    let max = vector.iter().max().unwrap();
    println!(
        "Vec<u64> with {} elements: sum = {}, max = {}",
        vector.len(),
        sum,
        max
    );

    let s = String::from("hello from the MachaOS heap allocator!");
    println!("String on the heap: \"{}\"", s);

    drop(vector);
    drop(s);
    println!(
        "after free: {} bytes allocated",
        crate::allocator::allocated_bytes()
    );
}

fn cmd_cpuinfo() {
    let vendor = cpuid::vendor_id();
    println!(
        "vendor: {}",
        core::str::from_utf8(&vendor).unwrap_or("????")
    );
    if let Some(brand) = cpuid::brand_string() {
        let brand = core::str::from_utf8(&brand).unwrap_or("");
        println!("brand: {}", brand.trim_end());
    }
    println!("logical cores: {}", cpuid::cores());
    let features = cpuid::features();
    println!("features: {}", features.join(" "));
}

fn cmd_mousetest() {
    println!("polling PS/2 mouse for 5 seconds...");
    let deadline = interrupts::ticks() + 500;
    while interrupts::ticks() < deadline {
        while let Some(event) = mouse::next_event() {
            println!(
                "mouse: dx={} dy={} left={} right={} middle={}",
                event.dx, event.dy, event.left, event.right, event.middle
            );
        }
        interrupts::halt();
    }
    println!("mousetest done");
}

fn reboot() -> ! {
    unsafe { port::outb(0x64, 0xFE) } // 8042 reset
    interrupts::halt_forever()
}

fn power_off() -> ! {
    unsafe { port::outb(0xF4, 0) } // QEMU isa-debug-exit
    interrupts::halt_forever()
}

fn crash() -> ! {
    unsafe {
        core::arch::asm!(
            "xor rax, rax",
            "div rax",
            options(nostack, nomem)
        );
    }
    unreachable!()
}

fn breakpoint_demo() {
    unsafe {
        core::arch::asm!("int3", options(nostack, nomem));
    }
}

fn fault_demo() -> ! {
    unsafe {
        core::arch::asm!(
            "mov rax, {0}",
            "mov qword ptr [rax], rax",
            const 0x1_0000_0000u64,
            options(nostack)
        );
    }
    unreachable!()
}

pub fn selftest() -> ! {
    println!();
    println!("== MachaOS selftest ==");
    execute("version");
    execute("echo MachaOS is running in kernel mode");
    execute("time");
    execute("uptime");
    execute("meminfo");
    execute("heap");
    execute("cpuinfo");
    println!("[SELFTEST OK]");
    unsafe { port::outb(0xF4, 0) }
    interrupts::halt_forever()
}

/// Non-blocking, one-character-at-a-time line editor. The GUI desktop
/// loop (`desktop.rs`) feeds it keyboard events for whichever terminal
/// window has focus, since (unlike `read_line`'s blocking loop) it must
/// keep returning control so the compositor and other windows keep
/// running between keystrokes.
pub struct LineEditor {
    line: String,
}

pub enum Feed {
    Pending,
    Line(String),
}

impl LineEditor {
    pub const fn new() -> Self {
        Self { line: String::new() }
    }

    pub fn feed(&mut self, event: keyboard::Event) -> Feed {
        match event {
            keyboard::Event::Char(c) => {
                if self.line.len() < 256 {
                    self.line.push(c);
                    print!("{}", c);
                }
                Feed::Pending
            }
            keyboard::Event::Backspace => {
                if self.line.pop().is_some() {
                    print!("\x08");
                }
                Feed::Pending
            }
            keyboard::Event::Enter => {
                println!();
                Feed::Line(core::mem::take(&mut self.line))
            }
            keyboard::Event::Tab => {
                for _ in 0..4 {
                    self.line.push(' ');
                    print!(" ");
                }
                Feed::Pending
            }
        }
    }
}
