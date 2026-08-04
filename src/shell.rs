use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::{ata, cpuid, fat, fat::FatError, interrupts, io, keyboard, mouse, multiboot, port, rtc, serial, task, vga};

const BANNER: &str = "MachaOS v0.1.0";
pub const PROMPT: &str = "machaos> ";

// Command names, for Tab completion in LineEditor. Kept in sync with the
// match in `execute()` and the descriptions in `cmd_help()` by hand — if
// you add a command there, add it here too.
pub const COMMANDS: &[&str] = &[
    "help", "clear", "cls", "echo", "time", "date", "uptime", "meminfo", "heap", "cpuinfo",
    "version", "ver", "reboot", "shutdown", "crash", "breakpoint", "fault", "panic", "mousetest",
    "tasks", "ls", "cat", "fatinfo", "write", "mkdir", "rm",
];

pub fn run() -> ! {
    loop {
        vga::set_color(vga::colors::LIGHT_GREEN);
        print!("{}", PROMPT);
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
                // History/cursor movement are GUI-only (LineEditor, used by
                // the desktop's Terminal window); the plain VGA fallback
                // shell doesn't support them.
                keyboard::Event::Up
                | keyboard::Event::Down
                | keyboard::Event::Left
                | keyboard::Event::Right => {}
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
        "date" => cmd_date(),
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
        "tasks" => cmd_tasks(),
        "ls" => cmd_ls(&args),
        "cat" => cmd_cat(&args),
        "fatinfo" => cmd_fatinfo(),
        "write" => cmd_write(&args),
        "mkdir" => cmd_mkdir(&args),
        "rm" => cmd_rm(&args),
        _ => println!("unknown command: '{}' (type 'help')", command),
    }
}

fn cmd_help() {
    println!("Available commands:");
    println!("  help        show this help");
    println!("  clear       clear the screen");
    println!("  echo <txt>  print text");
    println!("  time        print timer ticks");
    println!("  date        print the current date and time (RTC)");
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
    println!("  tasks       list scheduler tasks and their counters");
    println!("  ls [path]   list files on the mounted disk");
    println!("  cat <path>  print a file's contents");
    println!("  fatinfo     show mounted volume information");
    println!("  write <path> <text>");
    println!("               write text to a file (LFN supported)");
    println!("  mkdir <path> create a directory");
    println!("  rm <path>   remove a file or empty directory");
}

fn cmd_date() {
    let now = rtc::now();
    println!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    );
}

fn cmd_ls(args: &[&str]) {
    let path = args.first().copied().unwrap_or("/");
    match fat::list_dir(path) {
        Ok(entries) => {
            if entries.is_empty() {
                println!("(empty)");
                return;
            }
            for entry in entries {
                if entry.is_dir {
                    println!("{:<32} <dir>", entry.name);
                } else {
                    println!("{:<32} {:>8} bytes", entry.name, entry.size);
                }
            }
        }
        Err(e) => println!("ls: {}", e),
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() {
        println!("usage: cat <path>");
        return;
    }
    // The shell splits on whitespace, so a path containing spaces must be
    // reassembled here.
    let path = args.join(" ");
    match fat::read_file(&path) {
        Ok(data) => {
            let text: alloc::string::String = data
                .iter()
                .map(|&b| if b.is_ascii() { b as char } else { '?' })
                .collect();
            print!("{}", text);
            if !text.ends_with('\n') {
                println!();
            }
        }
        Err(e) => println!("cat: {}", e),
    }
}

fn cmd_write(args: &[&str]) {
    if args.len() < 2 {
        println!("usage: write <path> <text>");
        return;
    }
    // Reassemble the text: the shell splits on whitespace.
    let text = args[1..].join(" ");
    match fat::write_file(args[0], text.as_bytes()) {
        Ok(()) => println!("wrote {} bytes to {}", text.len(), args[0]),
        Err(e) => println!("write: {}", e),
    }
}

fn cmd_mkdir(args: &[&str]) {
    if args.is_empty() {
        println!("usage: mkdir <path>");
        return;
    }
    let path = args.join(" ");
    match fat::make_dir(&path) {
        Ok(()) => println!("created directory {}", path),
        Err(e) => println!("mkdir: {}", e),
    }
}

fn cmd_rm(args: &[&str]) {
    if args.is_empty() {
        println!("usage: rm <path>");
        return;
    }
    let path = args.join(" ");
    match fat::remove(&path) {
        Ok(()) => println!("removed {}", path),
        Err(e) => println!("rm: {}", e),
    }
}

fn cmd_fatinfo() {    match fat::info() {
        Some(info) => {
            let cluster_bytes = info.sectors_per_cluster as u32 * 512;
            println!(
                "FAT32 volume: {} sectors ({} MiB), {} bytes/cluster",
                info.total_sectors,
                info.total_sectors / 2048,
                cluster_bytes
            );
            println!(
                "  fat: {} sectors, root cluster {}, free: {} clusters ({} KiB)",
                info.fat_sectors,
                info.root_cluster,
                info.free_clusters,
                info.free_clusters * cluster_bytes / 1024
            );
        }
        None => println!("no FAT32 volume mounted"),
    }
    for i in 0..ata::count() {
        if let Some(description) = ata::describe(i) {
            println!("  {}", description);
        }
    }
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
    println!(
        "pmm: {} frames ({} MiB) total, {} used, {} free",
        crate::pmm::total_frames(),
        crate::pmm::total_frames() * crate::pmm::FRAME_SIZE / (1024 * 1024),
        crate::pmm::used_frames(),
        crate::pmm::free_frames()
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

fn cmd_tasks() {
    let count = task::task_count();
    println!("scheduler tasks: {}", count);
    for i in 0..count {
        println!("  [{}] {}", i, task::task_name(i));
    }
    for (i, counter) in task::COUNTERS.iter().enumerate() {
        println!("  bg-{} counter: {}", i, counter.load(Ordering::Relaxed));
    }
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
    execute("date");
    execute("uptime");
    execute("meminfo");
    execute("heap");
    execute("cpuinfo");
    execute("tasks");
    execute("fatinfo");
    execute("ls");
    execute("ls /docs");
    execute("cat /hello world.txt");
    execute("cat /greetings.txt");
    execute("cat /docs/readme.txt");

    // FAT32 read verification: the fixture files were placed on the disk
    // image by `make disk` (mtools), so this exercises LFN parsing, the
    // FAT cluster chain, and subdirectory traversal.
    match fat::list_dir("/") {
        Ok(entries) => {
            let fixture = entries.iter().find(|e| e.name == "hello world.txt");
            match fixture {
                Some(entry) if !entry.is_dir && entry.size == 20 => {
                    println!("[OK] FAT32 root listing finds fixture (20 bytes)")
                }
                _ => selftest_fail("FAT32 fixture file missing or wrong size in /"),
            }
        }
        Err(_) => selftest_fail("FAT32 root listing failed"),
    }
    match fat::read_file("/docs/readme.txt") {
        Ok(data) if data == b"hello from the host\n" => {
            println!("[OK] FAT32 read /docs/readme.txt matches fixture")
        }
        _ => selftest_fail("FAT32 subdirectory read mismatch"),
    }

    // FAT32 write verification: create a directory, write a file into it
    // (LFN >8.3), read it back, overwrite it, then delete both. Exercising
    // remove also proves that free clusters are recycled.
    match fat::make_dir("/selftest") {
        Ok(()) => println!("[OK] FAT32 mkdir /selftest"),
        Err(e) => selftest_fail("FAT32 mkdir failed"),
    }
    let payload = "selftest payload line 1\nline 2 (2 KiB+ to force multi-cluster)\n".repeat(64);
    if let Err(e) = fat::write_file("/selftest/multicluster payload.txt", payload.as_bytes()) {
        selftest_fail("FAT32 write failed");
    }
    match fat::read_file("/selftest/multicluster payload.txt") {
        Ok(data) if data == payload.as_bytes() => {
            println!("[OK] FAT32 write+read round trip ({} bytes, LFN)", payload.len())
        }
        Ok(_) => selftest_fail("FAT32 write+read round trip mismatch"),
        Err(_) => selftest_fail("FAT32 write+read round trip failed"),
    }
    let overwrite = "shorter overwrite";
    if let Err(e) = fat::write_file("/selftest/multicluster payload.txt", overwrite.as_bytes()) {
        selftest_fail("FAT32 overwrite failed");
    }
    match fat::read_file("/selftest/multicluster payload.txt") {
        Ok(data) if data == overwrite.as_bytes() => {
            println!("[OK] FAT32 overwrite shrinks file")
        }
        _ => selftest_fail("FAT32 overwrite mismatch"),
    }
    if let Err(e) = fat::remove("/selftest/multicluster payload.txt") {
        selftest_fail("FAT32 rm file failed");
    }
    if let Err(e) = fat::remove("/selftest") {
        selftest_fail(&format!("FAT32 rm dir failed: {}", e));
    }
    match fat::list_dir("/selftest") {
        Err(FatError::NotFound) => println!("[OK] FAT32 rm file+dir cleans up"),
        _ => selftest_fail("FAT32 rm did not remove entries"),
    }
    let info = fat::info();
    if let Some(info) = info {
        println!(
            "     volume free: {} clusters after write/rm cycle",
            info.free_clusters
        );
    }

    // Physical memory manager: allocate two frames, scribble on the
    // first, free both, then confirm the first allocation hands the
    // same frame back (a full alloc/free round trip).
    let frame_a = crate::pmm::frame_alloc().unwrap_or_else(|| selftest_fail("pmm alloc"));
    let frame_b = crate::pmm::frame_alloc().unwrap_or_else(|| selftest_fail("pmm alloc"));
    unsafe {
        let p = frame_a as *mut u8;
        core::ptr::write_volatile(p, 0xAB);
        core::ptr::write_volatile(p.add(1), 0xCD);
        if core::ptr::read_volatile(p) != 0xAB || core::ptr::read_volatile(p.add(1)) != 0xCD {
            selftest_fail("pmm frame contents did not survive");
        }
    }
    crate::pmm::frame_free(frame_a);
    crate::pmm::frame_free(frame_b);
    let frame_c = crate::pmm::frame_alloc().unwrap_or_else(|| selftest_fail("pmm re-alloc"));
    if frame_c != frame_a && frame_c != frame_b {
        selftest_fail("pmm did not reuse a freed frame");
    }
    crate::pmm::frame_free(frame_c);
    println!("[OK] pmm alloc/free round trip (frames {:#x}, {:#x}, {:#x})", frame_a, frame_b, frame_c);

    // Page table surgery: split the 2 MiB region containing a freshly
    // allocated frame, map that single 4 KiB page onto itself, verify
    // it is reachable, then tear the mapping down again.
    let frame = crate::pmm::frame_alloc().unwrap_or_else(|| selftest_fail("paging frame alloc"));
    let mapped = crate::paging::map_page(
        frame as u64,
        frame as u64,
        crate::paging::PAGE_PRESENT | crate::paging::PAGE_WRITABLE,
    );
    if !mapped {
        selftest_fail("map_page refused the request");
    }
    unsafe {
        core::ptr::write_volatile(frame as *mut u32, 0x1234_5678);
    }
    let value = unsafe { core::ptr::read_volatile(frame as *mut u32) };
    if !crate::paging::unmap_page(frame as u64) {
        selftest_fail("unmap_page refused the request");
    }
    crate::pmm::frame_free(frame);
    if value != 0x1234_5678 {
        selftest_fail("paging map/write/read/unmap round trip mismatch");
    }
    println!("[OK] paging 4 KiB map/split/unmap round trip (frame {:#x})", frame);

    let before: Vec<u64> = task::COUNTERS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let deadline = interrupts::ticks() + 30; // spans several scheduler quanta (5 ticks each)
    while interrupts::ticks() < deadline {
        interrupts::halt();
    }
    execute("tasks");
    let progressed = task::COUNTERS
        .iter()
        .zip(before.iter())
        .all(|(counter, &prior)| counter.load(Ordering::Relaxed) > prior);

    if progressed {
        println!("[SELFTEST OK]");
    } else {
        println!("[SELFTEST FAIL] background task counters did not advance");
    }
    unsafe { port::outb(0xF4, 0) }
    interrupts::halt_forever()
}

fn selftest_fail(reason: &str) -> ! {
    println!("[SELFTEST FAIL] {}", reason);
    unsafe { port::outb(0xF4, 0) }
    interrupts::halt_forever()
}

/// Non-blocking, one-character-at-a-time line editor. The GUI desktop
/// loop (`desktop.rs`) feeds it keyboard events for whichever terminal
/// window has focus, since (unlike `read_line`'s blocking loop) it must
/// keep returning control so the compositor and other windows keep
/// running between keystrokes.
const HISTORY_CAPACITY: usize = 32;

pub struct LineEditor {
    line: String,
    history: Vec<String>,
    // Some(i): browsing history[i]; None: editing fresh input (`line` is
    // the source of truth). `draft` is what was being typed before the
    // user pressed Up, restored when Down walks past the newest entry.
    history_index: Option<usize>,
    draft: String,
}

pub enum Feed {
    Pending,
    Line(String),
}

impl LineEditor {
    pub const fn new() -> Self {
        Self {
            line: String::new(),
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
        }
    }

    pub fn current_line(&self) -> &str {
        &self.line
    }

    /// Erases whatever is currently visible on screen for this line and
    /// prints `new_line` in its place. Relies only on `print!`, like the
    /// rest of this type, so it works wherever the caller has pointed the
    /// active console sink.
    fn set_line(&mut self, new_line: String) {
        for _ in 0..self.line.chars().count() {
            print!("\x08");
        }
        self.line = new_line;
        print!("{}", self.line);
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.draft = self.line.clone();
                self.history.len() - 1
            }
            Some(i) => i.saturating_sub(1),
        };
        self.history_index = Some(next);
        let entry = self.history[next].clone();
        self.set_line(entry);
    }

    fn history_down(&mut self) {
        match self.history_index {
            Some(i) if i + 1 < self.history.len() => {
                self.history_index = Some(i + 1);
                let entry = self.history[i + 1].clone();
                self.set_line(entry);
            }
            Some(_) => {
                self.history_index = None;
                let draft = core::mem::take(&mut self.draft);
                self.set_line(draft);
            }
            None => {}
        }
    }

    fn complete(&mut self) {
        // Argument completion is out of scope; only complete the command
        // name itself, before the first space.
        if self.line.is_empty() || self.line.contains(' ') {
            return;
        }
        let matches: Vec<&str> =
            COMMANDS.iter().copied().filter(|c| c.starts_with(self.line.as_str())).collect();
        match matches.as_slice() {
            [] => {}
            [only] => {
                let completion = &only[self.line.len()..];
                self.line.push_str(completion);
                print!("{}", completion);
            }
            multiple => {
                println!();
                println!("{}", multiple.join("  "));
                print!("{}{}", PROMPT, self.line);
            }
        }
    }

    pub fn feed(&mut self, event: keyboard::Event) -> Feed {
        match event {
            keyboard::Event::Char(c) => {
                if self.line.len() < 256 {
                    self.line.push(c);
                    print!("{}", c);
                }
                self.history_index = None;
                Feed::Pending
            }
            keyboard::Event::Backspace => {
                if self.line.pop().is_some() {
                    print!("\x08");
                }
                self.history_index = None;
                Feed::Pending
            }
            keyboard::Event::Enter => {
                println!();
                let line = core::mem::take(&mut self.line);
                self.history_index = None;
                self.draft.clear();
                if !line.is_empty() && self.history.last().map(String::as_str) != Some(&line) {
                    if self.history.len() >= HISTORY_CAPACITY {
                        self.history.remove(0);
                    }
                    self.history.push(line.clone());
                }
                Feed::Line(line)
            }
            keyboard::Event::Tab => {
                self.complete();
                Feed::Pending
            }
            keyboard::Event::Up => {
                self.history_up();
                Feed::Pending
            }
            keyboard::Event::Down => {
                self.history_down();
                Feed::Pending
            }
            // Not supported yet: LineEditor only ever appends/removes at
            // the end of `line`, it has no notion of a cursor within it.
            keyboard::Event::Left | keyboard::Event::Right => Feed::Pending,
        }
    }
}
