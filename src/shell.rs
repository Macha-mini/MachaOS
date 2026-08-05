use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::sync::SpinLock;
use crate::{ata, cpuid, fat, fat::FatError, interrupts, io, keyboard, mouse, multiboot, port, process, rtc, serial, task, vga};

const BANNER: &str = "MachaOS v0.1.0";
pub const PROMPT: &str = "machaos> ";

// Current working directory on the mounted disk (absolute, normalized).
// Empty means "not set yet": the shell starts in the user's home.
static CWD: SpinLock<String> = SpinLock::new(String::new());

/// Returns the current working directory (always starts with `/`).
pub fn cwd() -> String {
    let dir = CWD.lock().clone();
    if dir.is_empty() {
        crate::users::home()
    } else {
        dir
    }
}

fn set_cwd(path: &str) {
    *CWD.lock() = path.to_string();
}

/// The prompt, showing the working directory when it is not the root.
pub fn prompt() -> String {
    let dir = cwd();
    if dir == "/" {
        String::from(PROMPT)
    } else {
        format!("machaos:{}> ", dir)
    }
}

/// Collapses `.` and `..` and duplicate slashes in an absolute path.
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::from("/");
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    out
}

/// Resolves `path` against the current working directory. A leading
/// `~` (or `~/`) expands to the user's home directory.
fn abs_path(path: &str) -> String {
    let expanded = if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            format!("{}{}", crate::users::home(), rest)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    if expanded.starts_with('/') {
        normalize_path(&expanded)
    } else {
        normalize_path(&format!("{}/{}", cwd(), expanded))
    }
}

/// Splits a command line into arguments. Text inside double quotes is kept
/// as a single argument (quotes are stripped, no escape sequences).
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(core::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// Command names, for Tab completion in LineEditor. Kept in sync with the
// match in `execute()` and the descriptions in `cmd_help()` by hand — if
// you add a command there, add it here too.
pub const COMMANDS: &[&str] = &[
    "help", "clear", "cls", "echo", "time", "date", "uptime", "meminfo", "heap", "cpuinfo",
    "version", "ver", "reboot", "shutdown", "crash", "breakpoint", "fault", "panic", "mousetest",
    "tasks", "ls", "cat", "fatinfo", "write", "mkdir", "rm", "mv", "run", "runlinux",
];

pub fn run() -> ! {
    loop {
        vga::set_color(vga::colors::LIGHT_GREEN);
        print!("{}", prompt());
        vga::reset_color();

        let line = read_line();
        execute(&line);
    }
}

const HISTORY_CAPACITY: usize = 32;
static FALLBACK_HISTORY: SpinLock<Vec<String>> = SpinLock::new(Vec::new());

fn redraw_line(line: &str, clear: usize) {
    for _ in 0..clear {
        vga::erase_char();
        serial::write_byte(0x08);
    }
    for c in line.chars() {
        vga::print_char(c as u8);
        serial::write_byte(c as u8);
    }
}

fn complete_fallback(line: &mut String) {
    if line.is_empty() || line.contains(' ') {
        return;
    }
    let matches: Vec<&str> =
        COMMANDS.iter().copied().filter(|c| c.starts_with(line.as_str())).collect();
    match matches.as_slice() {
        [] => {}
        [only] => {
            let completion = &only[line.len()..];
            for c in completion.chars() {
                line.push(c);
                vga::print_char(c as u8);
                serial::write_byte(c as u8);
            }
        }
        multiple => {
            println!();
            println!("{}", multiple.join("  "));
            print!("{}{}", prompt(), line);
        }
    }
}

fn read_line() -> String {
    let mut line = String::new();
    let mut browse: Option<usize> = None;
    let mut draft = String::new();
    loop {
        while let Some(event) = keyboard::next_event() {
            match event {
                keyboard::Event::Char(c) => {
                    if line.len() < 256 {
                        line.push(c);
                        vga::print_char(c as u8);
                        serial::write_byte(c as u8);
                    }
                    browse = None;
                }
                keyboard::Event::Backspace => {
                    if line.pop().is_some() {
                        vga::erase_char();
                        serial::write_byte(0x08);
                    }
                    browse = None;
                }
                keyboard::Event::Enter => {
                    println!();
                    {
                        let mut history = FALLBACK_HISTORY.lock();
                        if !line.is_empty() && history.last().map(String::as_str) != Some(&line) {
                            if history.len() >= HISTORY_CAPACITY {
                                history.remove(0);
                            }
                            history.push(line.clone());
                        }
                    }
                    return line;
                }
                keyboard::Event::Tab => {
                    complete_fallback(&mut line);
                    browse = None;
                }
                keyboard::Event::Up => {
                    let history = FALLBACK_HISTORY.lock();
                    if history.is_empty() {
                        continue;
                    }
                    let next = match browse {
                        None => {
                            draft = line.clone();
                            history.len() - 1
                        }
                        Some(i) => i.saturating_sub(1),
                    };
                    browse = Some(next);
                    let entry = history[next].clone();
                    drop(history);
                    redraw_line(&entry, line.chars().count());
                    line = entry;
                }
                keyboard::Event::Down => {
                    if let Some(i) = browse {
                        let history = FALLBACK_HISTORY.lock();
                        let entry = if i + 1 < history.len() {
                            browse = Some(i + 1);
                            history[i + 1].clone()
                        } else {
                            browse = None;
                            core::mem::take(&mut draft)
                        };
                        drop(history);
                        redraw_line(&entry, line.chars().count());
                        line = entry;
                    }
                }
                // The plain VGA fallback shell has no cursor movement.
                keyboard::Event::Left | keyboard::Event::Right => {}
                keyboard::Event::Escape | keyboard::Event::F2 | keyboard::Event::AltTab => {}
                keyboard::Event::Ctrl(_) => {}
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

    let words = tokenize(line);
    if words.is_empty() {
        return;
    }
    let command = words[0].as_str();
    let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();

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
        "mv" => cmd_mv(&args),
        "run" => cmd_run(&args),
        "runlinux" => cmd_runlinux(&args),
        "cd" => cmd_cd(&args),
        "pwd" => println!("{}", cwd()),
        "whoami" => println!("{}", crate::users::USER),
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
    println!("  ls [path]   list files (default: current directory)");
    println!("  cat <path>  print a file's contents");
    println!("  fatinfo     show mounted volume information");
    println!("  write <path> <text>");
    println!("               write text to a file (LFN supported)");
    println!("  mkdir <path> create a directory");
    println!("  rm <path>   remove a file or empty directory");
    println!("  mv <path> <new-name>");
    println!("               rename a file or directory in place");
    println!("  run <path>  load and run an ELF program as a process");
    println!("  runlinux <path> [args...]");
    println!("               load and run a Linux ELF binary (Linux ABI, see linux_abi.rs)");
    println!("  cd [path]   change directory (default: home, .. goes up)");
    println!("  pwd         print the current directory");
    println!("  whoami      print the current user");
    println!("Paths may be relative to the current directory; '~' means the");
    println!("home directory; quote arguments containing spaces: write notes.txt \"hello world\"");
}

fn cmd_date() {
    let now = rtc::now();
    println!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    );
}

fn cmd_ls(args: &[&str]) {
    let path = if args.is_empty() {
        cwd()
    } else {
        abs_path(&args.join(" "))
    };
    match fat::list_dir(&path) {
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
        Err(e) => println!("ls: {}: {}", path, e),
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() {
        println!("usage: cat <path>");
        return;
    }
    // The shell splits on whitespace, so a path containing spaces must be
    // reassembled here (or quoted: cat "hello world.txt").
    let path = abs_path(&args.join(" "));
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
        Err(e) => println!("cat: {}: {}", path, e),
    }
}

fn cmd_write(args: &[&str]) {
    if args.len() < 2 {
        println!("usage: write <path> <text>");
        return;
    }
    // Reassemble the text: the shell splits on whitespace unless it is
    // quoted (write hello.txt "hello world").
    let text = args[1..].join(" ");
    let path = abs_path(args[0]);
    match fat::write_file(&path, text.as_bytes()) {
        Ok(()) => println!("wrote {} bytes to {}", text.len(), path),
        Err(e) => println!("write: {}: {}", path, e),
    }
}

fn cmd_mkdir(args: &[&str]) {
    if args.is_empty() {
        println!("usage: mkdir <path>");
        return;
    }
    let path = abs_path(&args.join(" "));
    match fat::make_dir(&path) {
        Ok(()) => println!("created directory {}", path),
        Err(e) => println!("mkdir: {}: {}", path, e),
    }
}

fn cmd_rm(args: &[&str]) {
    if args.is_empty() {
        println!("usage: rm <path>");
        return;
    }
    let path = abs_path(&args.join(" "));
    match fat::remove(&path) {
        Ok(()) => println!("removed {}", path),
        Err(e) => println!("rm: {}: {}", path, e),
    }
}

fn cmd_mv(args: &[&str]) {
    if args.len() != 2 {
        println!("usage: mv <path> <new-name>");
        return;
    }
    let path = abs_path(args[0]);
    let new_name = args[1].to_string();
    match fat::rename(&path, &new_name) {
        Ok(()) => println!("renamed {} to {}", path, new_name),
        Err(e) => println!("mv: {}: {}", path, e),
    }
}

fn cmd_cd(args: &[&str]) {
    // No argument: back to the user's home. Quoted paths may contain
    // spaces, and `~` expands to the home directory.
    let target = if args.is_empty() {
        crate::users::home()
    } else {
        abs_path(&args.join(" "))
    };
    match fat::is_dir(&target) {
        Ok(true) => set_cwd(&target),
        Ok(false) => println!("cd: {}: not a directory", target),
        Err(e) => println!("cd: {}: {}", target, e),
    }
}

/// Loads an ELF file from the disk, spawns it as a process, waits for it
/// to exit, and reports its result (up to one second).
fn cmd_run(args: &[&str]) {
    if args.is_empty() {
        println!("usage: run <path>");
        return;
    }
    // A path containing spaces must be reassembled (or quoted).
    let path = abs_path(&args.join(" "));
    match fat::read_file(&path) {
        Ok(elf) => match process::spawn(&elf, "app") {
            Ok(pid) => {
                println!(
                    "spawned process {} from {} ({} byte ELF)",
                    pid,
                    path,
                    elf.len()
                );
                match process::wait(pid, 100) {
                    Some(info) => {
                        println!("process {} {}", pid, process::describe_exit(&info));
                        if let Some(result) = process::read_result(pid) {
                            println!("process {} result: {:#x}", pid, result);
                        }
                        process::reap(pid);
                        println!("process {} reaped", pid);
                    }
                    None => println!("process {} did not exit within 1s", pid),
                }
            }
            Err(e) => println!("run: {}: {}", path, e),
        },
        Err(e) => println!("run: {}: {}", path, e),
    }
}

/// Like `cmd_run`, but for a Linux binary: loads the ELF from disk and
/// spawns it via `process::spawn_linux` (Linux-style argv/envp/auxv
/// stack, syscalls routed through `linux_abi.rs`) instead of the native
/// ABI's `process::spawn`. `argv[0]` is the path as given (matching what
/// a real shell passes); any further words become `argv[1..]`.
fn cmd_runlinux(args: &[&str]) {
    let Some(&path) = args.first() else {
        println!("usage: runlinux <path> [args...]");
        return;
    };
    let abs = abs_path(path);
    match fat::read_file(&abs) {
        Ok(elf) => {
            let mut argv = alloc::vec![path];
            argv.extend_from_slice(&args[1..]);
            match process::spawn_linux(&elf, "app", &argv, &["PATH=/bin"]) {
                Ok(pid) => {
                    println!("spawned Linux process {} from {} ({} byte ELF)", pid, abs, elf.len());
                    match process::wait(pid, 500) {
                        Some(info) => {
                            println!("process {} {}", pid, process::describe_exit(&info));
                            process::reap(pid);
                            println!("process {} reaped", pid);
                        }
                        None => println!("process {} did not exit within 5s", pid),
                    }
                }
                Err(e) => println!("runlinux: {}: {}", abs, e),
            }
        }
        Err(e) => println!("runlinux: {}: {}", abs, e),
    }
}

fn cmd_fatinfo() {
    match fat::info() {
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
        let label = match task::process_state_label(i) {
            Some(state) => format!(" [proc, {}]", state),
            None => String::new(),
        };
        println!("  [{}] {}{}", i, task::task_name(i), label);
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
    crate::power::reboot(&crate::settings::Settings::load())
}

fn power_off() -> ! {
    crate::power::shutdown(&crate::settings::Settings::load())
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
    execute("ls /users/macha/Documents");
    execute("cat /users/macha/Documents/hello world.txt");
    execute("cat /users/macha/Documents/greetings.txt");
    execute("cat /users/macha/Documents/readme.txt");
    execute("whoami");

    // FAT32 read verification: the fixture files were placed on the disk
    // image by `make disk` (mtools), so this exercises LFN parsing, the
    // FAT cluster chain, and subdirectory traversal. Fixtures live in
    // the user's Documents folder.
    match fat::list_dir("/users/macha/Documents") {
        Ok(entries) => {
            let fixture = entries.iter().find(|e| e.name == "hello world.txt");
            match fixture {
                Some(entry) if !entry.is_dir && entry.size == 20 => {
                    println!("[OK] FAT32 Documents listing finds fixture (20 bytes)")
                }
                _ => selftest_fail("FAT32 fixture file missing or wrong size in Documents"),
            }
        }
        Err(_) => selftest_fail("FAT32 Documents listing failed"),
    }
    match fat::read_file("/users/macha/Documents/readme.txt") {
        Ok(data) if data == b"hello from the host\n" => {
            println!("[OK] FAT32 read Documents/readme.txt matches fixture")
        }
        _ => selftest_fail("FAT32 subdirectory read mismatch"),
    }

    // FAT32 write verification: create a directory, write a file into it
    // (LFN >8.3), read it back, overwrite it, then delete both. Exercising
    // remove also proves that free clusters are recycled.
    match fat::make_dir("/selftest") {
        Ok(()) => println!("[OK] FAT32 mkdir /selftest"),
        Err(_e) => selftest_fail("FAT32 mkdir failed"),
    }
    let payload = "selftest payload line 1\nline 2 (2 KiB+ to force multi-cluster)\n".repeat(64);
    if let Err(_e) = fat::write_file("/selftest/multicluster payload.txt", payload.as_bytes()) {
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
    if let Err(_e) = fat::write_file("/selftest/multicluster payload.txt", overwrite.as_bytes()) {
        selftest_fail("FAT32 overwrite failed");
    }
    match fat::read_file("/selftest/multicluster payload.txt") {
        Ok(data) if data == overwrite.as_bytes() => {
            println!("[OK] FAT32 overwrite shrinks file")
        }
        _ => selftest_fail("FAT32 overwrite mismatch"),
    }
    if let Err(_e) = fat::write_file("/selftest/movable.txt", b"move payload") {
        selftest_fail("FAT32 move source write failed");
    }
    if let Err(_e) = fat::make_dir("/selftest/destination") {
        selftest_fail("FAT32 move destination mkdir failed");
    }
    if let Err(_e) = fat::move_file("/selftest/movable.txt", "/selftest/destination/movable.txt") {
        selftest_fail("FAT32 move failed");
    }
    match fat::read_file("/selftest/destination/movable.txt") {
        Ok(data) if data == b"move payload" && fat::read_file("/selftest/movable.txt").is_err() => {
            println!("[OK] FAT32 move file round trip")
        }
        _ => selftest_fail("FAT32 move result mismatch"),
    }
    if let Err(_e) = fat::remove("/selftest/destination/movable.txt") {
        selftest_fail("FAT32 move cleanup file failed");
    }
    if let Err(_e) = fat::remove("/selftest/destination") {
        selftest_fail("FAT32 move cleanup directory failed");
    }
    if let Err(_e) = fat::remove("/selftest/multicluster payload.txt") {
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

    // Trash: a file moved to the trash is gone from its original
    // location, still readable from the trash, and can be restored to
    // its exact original path (then emptied). The FAT test above
    // deleted /selftest, so recreate it first.
    if let Err(e) = fat::make_dir("/selftest") {
        selftest_fail(&format!("trash: setup mkdir failed: {}", e));
    }
    if let Err(e) = fat::write_file("/selftest/to-trash.txt", b"trash payload") {
        selftest_fail(&format!("trash setup write failed: {}", e));
    }
    match crate::trash::trash_file("/selftest/to-trash.txt") {
        Ok(name) if name == "to-trash.txt" => println!("[OK] trash: moved file into trash"),
        Ok(name) => selftest_fail(&format!("trash: unexpected name '{}'", name)),
        Err(e) => selftest_fail(&format!("trash: move failed: {}", e)),
    }
    if fat::read_file("/selftest/to-trash.txt").is_ok() {
        selftest_fail("trash: original path still exists after trashing");
    }
    match fat::read_file("/system/Trash/to-trash.txt") {
        Ok(data) if data == b"trash payload" => println!("[OK] trash: payload preserved in trash"),
        _ => selftest_fail("trash: payload not preserved"),
    }
    match crate::trash::list_original_paths().as_slice() {
        [path] if path == "/selftest/to-trash.txt" => println!("[OK] trash: meta records original path"),
        _ => selftest_fail("trash: meta missing original path"),
    }
    match crate::trash::restore(0) {
        Ok(path) if path == "/selftest/to-trash.txt" => println!("[OK] trash: restored to original path"),
        Ok(path) => selftest_fail(&format!("trash: restored to wrong path '{}'", path)),
        Err(e) => selftest_fail(&format!("trash: restore failed: {}", e)),
    }
    match fat::read_file("/selftest/to-trash.txt") {
        Ok(data) if data == b"trash payload" => println!("[OK] trash: restore round trip"),
        _ => selftest_fail("trash: restore did not bring the payload back"),
    }
    if let Err(e) = crate::trash::trash_file("/selftest/to-trash.txt") {
        selftest_fail(&format!("trash: re-trash failed: {}", e));
    }
    match crate::trash::empty_trash() {
        Ok(()) => println!("[OK] trash: empty deletes entries"),
        Err(e) => selftest_fail(&format!("trash: empty failed: {}", e)),
    }
    if fat::read_file("/system/Trash/to-trash.txt").is_ok() {
        selftest_fail("trash: entry survived empty");
    }
    if fat::read_file("/selftest/to-trash.txt").is_ok() {
        selftest_fail("trash: emptied entry reappeared at original path");
    }
    if let Err(e) = fat::remove("/selftest") {
        selftest_fail(&format!("trash: selftest dir cleanup failed: {}", e));
    }

    // Desktop session persistence: serialize a representative set of
    // open windows and remembered geometries, save it to the disk,
    // load it back and check every field survives the round trip.
    // Also proves the parser is lenient (garbage lines are skipped).
    use crate::session::{self, SessionData, SessionWindow};
    let original = SessionData {
        windows: vec![
            SessionWindow {
                app: "terminal".into(),
                x: 100,
                y: 80,
                cols: 80,
                rows: 24,
                minimized: false,
                maximized: false,
            },
            SessionWindow {
                app: "notepad".into(),
                x: 300,
                y: 200,
                cols: 60,
                rows: 12,
                minimized: true,
                maximized: false,
            },
            SessionWindow {
                app: "settings".into(),
                x: 20,
                y: 40,
                cols: 0,
                rows: 0,
                minimized: false,
                maximized: true,
            },
            SessionWindow {
                app: "sysinfo".into(),
                x: -5,
                y: 700,
                cols: 38,
                rows: 16,
                minimized: false,
                maximized: false,
            },
        ],
        remembered: vec![
            SessionWindow {
                app: "calculator".into(),
                x: 140,
                y: 100,
                cols: 0,
                rows: 0,
                minimized: false,
                maximized: false,
            },
            SessionWindow {
                app: "paint".into(),
                x: 512,
                y: 384,
                cols: 0,
                rows: 0,
                minimized: false,
                maximized: true,
            },
        ],
    };
    let text = session::serialize(&original);
    let reparsed = session::parse(&text);
    if reparsed != original {
        selftest_fail("desktop session serialize/parse round trip mismatch");
    }
    println!(
        "[OK] desktop session serialize/parse round trip ({} bytes)",
        text.len()
    );

    // Disk round trip through the real persistence path: save_to_disk
    // writes /system/desktop.session, load_from_disk reads it back —
    // exactly what happens across a reboot.
    if !session::save_to_disk(&original) {
        selftest_fail("desktop session disk write failed");
    }
    let loaded = session::load_from_disk();
    if loaded != original {
        selftest_fail("desktop session save/load round trip mismatch");
    }
    println!("[OK] desktop session survives save/load round trip via /system/desktop.session");
    if fat::remove("/system/desktop.session").is_err() {
        selftest_fail("desktop session test cleanup failed");
    }

    // Missing file: loading yields an empty session, never an error.
    let missing = session::load_from_disk();
    if !missing.windows.is_empty() || !missing.remembered.is_empty() {
        selftest_fail("desktop session missing-file load is not empty");
    }
    println!("[OK] desktop session missing file restores an empty session");

    // Lenient parsing: unknown/malformed lines are skipped, the rest
    // still loads.
    let junk = "# comment\nwindow bogus notanumber 2 3 4 0 0\nnot a session\nremembered\nwindow terminal 50 60 40 10 0 1\n";
    let parsed = session::parse(junk);
    if parsed.windows.len() != 1
        || parsed.windows[0].app != "terminal"
        || parsed.windows[0].x != 50
        || !parsed.windows[0].maximized
    {
        selftest_fail("desktop session lenient parsing failed");
    }
    println!("[OK] desktop session parsing skips malformed lines");

    // File rename (FAT32 rename-impl): a file keeps its contents under
    // the new name and disappears from the old one; a non-empty
    // directory renames in place too (its children move with it).
    if fat::make_dir("/selftest").is_err() {
        selftest_fail("rename test mkdir failed");
    }
    if let Err(_e) = fat::write_file("/selftest/old name.txt", b"rename payload") {
        selftest_fail("rename test source write failed");
    }
    if let Err(e) = fat::rename("/selftest/old name.txt", "new name.txt") {
        selftest_fail(&format!("file rename failed: {}", e));
    }
    match fat::read_file("/selftest/new name.txt") {
        Ok(data) if data == b"rename payload" && fat::read_file("/selftest/old name.txt").is_err() => {
            println!("[OK] FAT32 rename keeps file contents under the new name")
        }
        _ => selftest_fail("file rename result mismatch"),
    }
    if let Err(_e) = fat::write_file("/selftest/taken.txt", b"taken") {
        selftest_fail("rename collision setup failed");
    }
    match fat::rename("/selftest/new name.txt", "taken.txt") {
        Err(FatError::AlreadyExists) => println!("[OK] FAT32 rename rejects an existing name"),
        _ => selftest_fail("rename collision was not rejected"),
    }
    if fat::make_dir("/selftest/dir").is_err() {
        selftest_fail("rename dir setup mkdir failed");
    }
    if let Err(_e) = fat::write_file("/selftest/dir/child.txt", b"child") {
        selftest_fail("rename dir child write failed");
    }
    if let Err(e) = fat::rename("/selftest/dir", "moved-dir") {
        selftest_fail(&format!("directory rename failed: {}", e));
    }
    match fat::read_file("/selftest/moved-dir/child.txt") {
        Ok(data) if data == b"child" && fat::list_dir("/selftest/dir").is_err() => {
            println!("[OK] FAT32 rename moves a non-empty directory in place")
        }
        _ => selftest_fail("directory rename result mismatch"),
    }

    // Empty files: writing zero bytes creates a 0-byte entry that reads
    // back empty — what the explorer's "New File" button produces.
    if let Err(e) = fat::write_file("/selftest/empty file.txt", b"") {
        selftest_fail(&format!("empty file write failed: {}", e));
    }
    match fat::read_file("/selftest/empty file.txt") {
        Ok(data) if data.is_empty() => println!("[OK] FAT32 empty file round trip"),
        _ => selftest_fail("empty file read mismatch"),
    }

    // Overwriting an existing empty file (first_cluster == 0) must not
    // free reserved cluster 0: the file survives, both as an empty
    // overwrite and then as a content write. Removing an empty file
    // must succeed for the same reason.
    if fat::write_file("/selftest/empty file.txt", b"").is_err() {
        selftest_fail("empty file overwrite failed");
    }
    match fat::read_file("/selftest/empty file.txt") {
        Ok(data) if data.is_empty() => println!("[OK] FAT32 empty file overwrite keeps the file"),
        _ => selftest_fail("empty file overwrite lost the file"),
    }
    if fat::write_file("/selftest/empty file.txt", b"now has content").is_err() {
        selftest_fail("empty file content write failed");
    }
    match fat::read_file("/selftest/empty file.txt") {
        Ok(data) if data == b"now has content" => {
            println!("[OK] FAT32 empty file grows content on overwrite")
        }
        _ => selftest_fail("empty file content overwrite mismatch"),
    }
    if fat::remove("/selftest/empty file.txt").is_err() {
        selftest_fail("empty file remove failed");
    }
    match fat::read_file("/selftest/empty file.txt") {
        Err(FatError::NotFound) => println!("[OK] FAT32 empty file removes cleanly"),
        _ => selftest_fail("empty file remove left it behind"),
    }

    // The shell's `mv` command drives the same rename path (quote paths
    // with spaces, like `write`).
    if let Err(e) = fat::write_file("/selftest/mv source.txt", b"mv me") {
        selftest_fail(&format!("mv test setup failed: {}", e));
    }
    execute("mv \"/selftest/mv source.txt\" \"renamed-empty.txt\"");
    if fat::read_file("/selftest/renamed-empty.txt").is_err() || fat::read_file("/selftest/mv source.txt").is_ok() {
        selftest_fail("shell mv did not rename the file");
    }
    println!("[OK] shell mv command renames files");

    // Clean up every rename-test artifact.
    for leftover in ["/selftest/renamed-empty.txt", "/selftest/taken.txt", "/selftest/new name.txt"] {
        let _ = fat::remove(leftover);
    }
    if let Err(_e) = fat::remove("/selftest/moved-dir/child.txt") {
        selftest_fail("rename test cleanup file failed");
    }
    if let Err(_e) = fat::remove("/selftest/moved-dir") {
        selftest_fail("rename test cleanup dir failed");
    }
    if let Err(_e) = fat::remove("/selftest") {
        selftest_fail("rename test cleanup root failed");
    }

    // Shell path handling: cd/pwd, `..`, home expansion, and relative access.
    execute("cd /users/macha/Documents");
    if cwd() != "/users/macha/Documents" {
        selftest_fail("cd Documents did not update cwd");
    }
    execute("pwd");
    execute("ls");
    execute("cat readme.txt"); // relative to cwd
    execute("cd ..");
    if cwd() != "/users/macha" {
        selftest_fail("cd .. did not return to the home directory");
    }
    execute("pwd");
    execute("cd ~/Documents");
    if cwd() != "/users/macha/Documents" {
        selftest_fail("~ expansion did not resolve to Documents");
    }
    execute("cd");
    if cwd() != "/users/macha" {
        selftest_fail("cd with no argument did not return home");
    }
    println!("[OK] shell cd/pwd and relative paths");

    // Quoting: a double-quoted argument keeps embedded spaces whole.
    execute("write ~/Documents/qt.txt \"hello world from quotes\"");
    match fat::read_file("/users/macha/Documents/qt.txt") {
        Ok(data) if data == b"hello world from quotes" => {
            println!("[OK] shell quoting keeps spaces in one argument")
        }
        _ => selftest_fail("shell quoting mismatch"),
    }
    execute("rm ~/Documents/qt.txt");

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
    // Restore the identity map: the PMM hands freed frames straight out
    // again, and fresh allocations are expected to be identity-mapped
    // (the unmap test above proved the teardown works; this returns the
    // frame to the normal contract).
    if !crate::paging::map_page(
        frame as u64,
        frame as u64,
        crate::paging::PAGE_PRESENT | crate::paging::PAGE_WRITABLE,
    ) {
        selftest_fail("restoring the identity map after the unmap test");
    }
    if value != 0x1234_5678 {
        selftest_fail("paging map/write/read/unmap round trip mismatch");
    }
    println!("[OK] paging 4 KiB map/split/unmap round trip (frame {:#x})", frame);

    // Process management, part 1: a clean run. The embedded ELF computes
    // sum(1..=1000), stores it in its .result page, and returns from
    // `_start`; the trampoline then exits the process normally. Waiting
    // proves the scheduler switched to and back from the process (which
    // runs under its own page tables / CR3), and reading the result
    // proves the ELF was loaded, mapped, and executed.
    let frames_before = crate::pmm::free_frames();
    let pid = crate::process::spawn(crate::user_prog::PROG_EXIT, "sum")
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    println!("[OK] process {} spawned from embedded ELF", pid);
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] process exited normally"),
        Some(other) => selftest_fail(&format!("unexpected exit: {:?}", other)),
        None => selftest_fail("process did not exit in time"),
    }
    match crate::process::read_result(pid) {
        Some(500500) => println!("[OK] process computed sum(1..=1000) = 500500"),
        other => selftest_fail(&format!("process result mismatch: {:?}", other)),
    }
    execute("tasks");
    crate::process::reap(pid);

    // Process management, part 2: fault isolation. A process writing to
    // an unmapped address (4 GiB) must be killed by the #PF handler —
    // leaving the kernel and every other task alive.
    let pid = crate::process::spawn(crate::user_prog::PROG_FAULT, "faulty")
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::PageFault { cr2 }) if cr2 == 0x1_0000_0000 => {
            println!("[OK] page fault in process killed it (cr2 = {:#x}), kernel survived", cr2)
        }
        other => selftest_fail(&format!("faulting process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(0xFACE_FEED) => println!("[OK] faulting process stored its result before dying"),
        other => selftest_fail(&format!("faulting process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 2b: stack growth. The embedded ELF recurses
    // deep enough to blow well past the 32 KiB mapped for a fresh
    // process's stack; without `process::try_grow_stack` this would be
    // indistinguishable from part 2's fault and the process would be
    // killed instead of returning sum(0..=2000) = 2001000.
    let pid = crate::process::spawn(crate::user_prog::PROG_STACK, "deep-stack")
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 200) {
        Some(process::ExitInfo::Normal) => println!("[OK] deep recursion exited normally (stack grew instead of faulting)"),
        other => selftest_fail(&format!("stack-growth process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(2_001_000) => println!("[OK] deep recursion computed sum(0..=2000) = 2001000"),
        other => selftest_fail(&format!("stack-growth process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 2c: Linux-style initial stack layout.
    // `process::spawn_linux` builds a real argv/argc/envp/auxv stack
    // (`process::setup_linux_stack`) instead of the native ABI's single
    // return-address slot; the embedded ELF reads it straight off its
    // entry `rsp` (the way a real libc's `_start` does) and reports which
    // of 13 checks (argc, argv[], envp[], the auxv terminator, and each
    // auxv value including a round trip through the copied program
    // header table) passed as a bitmask.
    let pid = crate::process::spawn_linux(
        crate::user_prog::PROG_LINUX_STACK,
        "linux-stack",
        &["prog_linux_stack", "hello"],
        &["FOO=bar"],
    )
    .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] Linux-style stack layout process exited normally"),
        other => selftest_fail(&format!("linux-stack process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(0x1FFF) => println!("[OK] Linux-style stack layout: argv/envp/auxv all read back correctly"),
        other => selftest_fail(&format!("linux-stack process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 2d: Linux syscall dispatch routing.
    // `process::Abi::Linux` (also set by `spawn_linux`) makes
    // `syscall::syscall_dispatch` route to `linux_abi::syscall_dispatch`
    // instead of the native table; the embedded ELF calls Linux syscall
    // 39 (getpid), an unrecognized number (expecting `-ENOSYS` per the
    // real Linux errno convention), and Linux syscall 1 (`write`, not
    // the native ABI's exit — proves `syscall_entry`'s ABI-aware exit
    // check keeps the two numbering schemes from colliding), then exits
    // via the real Linux `exit` (60).
    let pid = crate::process::spawn_linux(crate::user_prog::PROG_LINUX_SYSCALL, "linux-syscall", &[], &[])
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] Linux syscall dispatch process exited normally"),
        other => selftest_fail(&format!("linux-syscall process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(0b111) => println!("[OK] Linux syscall dispatch: getpid, -ENOSYS, and real write(1, ...) all correct"),
        other => selftest_fail(&format!("linux-syscall process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 2e: Phase 3 syscalls (futex/rseq/
    // prlimit64/sched_getaffinity/sysinfo) a static glibc binary's
    // startup needs beyond Phase 2's musl-oriented set. No glibc
    // toolchain was available to build a real test binary against, so
    // this checks each syscall's documented (simplified — see
    // linux_abi.rs) behavior directly; 7 checks, one bit each.
    let pid = crate::process::spawn_linux(crate::user_prog::PROG_LINUX_PHASE3, "linux-phase3", &[], &[])
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] Linux Phase 3 syscalls process exited normally"),
        other => selftest_fail(&format!("linux-phase3 process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(0x7F) => println!("[OK] Linux Phase 3: futex/rseq/prlimit64/sched_getaffinity/sysinfo all correct"),
        other => selftest_fail(&format!("linux-phase3 process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 3: the full disk path. The same program
    // read back from the FAT32 image (copied there by `make disk`) must
    // run identically to the embedded copy.
    match fat::read_file("/bin/prog_exit.elf") {
        Ok(elf) => {
            let pid = crate::process::spawn(&elf, "disk-sum")
                .unwrap_or_else(|e| selftest_fail(&format!("disk process spawn failed: {e}")));
            match crate::process::wait(pid, 100) {
                Some(process::ExitInfo::Normal) => match crate::process::read_result(pid) {
                    Some(500500) => println!(
                        "[OK] process loaded from disk ({} byte ELF) ran correctly",
                        elf.len()
                    ),
                    _ => selftest_fail("disk-loaded process gave wrong result"),
                },
                _ => selftest_fail("disk-loaded process did not exit normally"),
            }
            crate::process::reap(pid);
        }
        Err(e) => selftest_fail(&format!("reading /bin/prog_exit.elf failed: {e}")),
    }

    // Process management, part 4: real multi-argument syscalls. The
    // embedded ELF writes a buffer via sys_write(stdout, ptr, len) and
    // stores the sys_clock return value in its result — nonzero only if
    // both syscalls actually ran and returned through the normal SYSRET
    // path (not just the sys_exit unwind the other tests exercise).
    let pid = crate::process::spawn(crate::user_prog::PROG_SYSCALL, "syscall")
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] process syscall test exited normally"),
        other => selftest_fail(&format!("syscall test process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(ticks) if ticks > 0 => {
            println!("[OK] process sys_write + sys_clock round trip (clock = {} ticks)", ticks)
        }
        other => selftest_fail(&format!("syscall test process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    // Process management, part 5: IPC. The kernel delivers a message to
    // the process's inbox right after spawning it — nothing yields
    // between `spawn` and `send_from_kernel` here, so it's guaranteed to
    // land before the process can possibly have run — and the process
    // reads it back via sys_recv and reports the first byte.
    let pid = crate::process::spawn(crate::user_prog::PROG_IPC, "ipc")
        .unwrap_or_else(|e| selftest_fail(&format!("process spawn failed: {e}")));
    if !crate::process::send_from_kernel(pid, b"ping") {
        selftest_fail("send_from_kernel failed to deliver to a freshly spawned process");
    }
    match crate::process::wait(pid, 100) {
        Some(process::ExitInfo::Normal) => println!("[OK] IPC process exited normally"),
        other => selftest_fail(&format!("IPC process gave unexpected exit: {:?}", other)),
    }
    match crate::process::read_result(pid) {
        Some(n) if n == b'p' as u64 => {
            println!("[OK] sys_recv delivered the message sent via send_from_kernel")
        }
        other => selftest_fail(&format!("IPC process result mismatch: {:?}", other)),
    }
    crate::process::reap(pid);

    let frames_after = crate::pmm::free_frames();
    if frames_after == frames_before {
        println!("[OK] all process frames returned to the PMM");
    } else {
        println!("[WARN] free frames changed across process tests: {} -> {}", frames_before, frames_after);
    }

    // Process management, part 5: a real Linux binary, if one happens to
    // be on the disk (`make disk-linux`, see the Makefile — not part of
    // the plain `disk` target `make test` itself uses, so this quietly
    // skips rather than needing network access for the normal build).
    // BusyBox's `echo` applet exercises the Linux ABI layer end to end:
    // real musl _start (brk/arch_prctl/mmap during startup), argv
    // dispatch, and a real write(1, ...) syscall — not just a MachaOS-
    // native test program that happens to use Linux syscall numbers.
    match fat::read_file("/bin/busybox.elf") {
        Ok(elf) => {
            let pid = crate::process::spawn_linux(&elf, "busybox", &["busybox", "echo", "hello-from-busybox"], &["PATH=/bin"])
                .unwrap_or_else(|e| selftest_fail(&format!("busybox spawn failed: {e}")));
            match crate::process::wait(pid, 500) {
                Some(process::ExitInfo::Normal) => println!("[OK] real busybox binary (echo applet) exited normally"),
                other => selftest_fail(&format!("busybox gave unexpected exit: {:?}", other)),
            }
            crate::process::reap(pid);
        }
        Err(_) => println!("[SKIP] /bin/busybox.elf not present (run `make disk-linux` to fetch it)"),
    }

    // Process management, part 6: a real *dynamically-linked* glibc
    // binary (GNU Hello), if present — exercises Phase 4/6's PT_INTERP
    // handling for real: the process's actual entry point is the real
    // /lib64/ld-linux-x86-64.so.2, which is expected to mmap and relocate
    // the real /lib/x86_64-linux-gnu/libc.so.6 itself before ever
    // reaching hello's own main. Not fetched by any Makefile target
    // (getting a real glibc + ld.so pair needs extracting Debian
    // packages, done by hand for this — see the Phase 4 commit) and not
    // a hard selftest failure either way: testing against the real
    // binary is what found and fixed several real gaps, most recently
    // (Phase 6) a page fault during `ld.so`'s TLS/rseq setup traced to
    // `syscall_entry` never restoring the caller's rdi/rsi/rdx/r10/r8/r9
    // after `syscall_dispatch` — real Linux's syscall ABI guarantees
    // those survive a syscall unchanged, and real glibc (unlike this
    // repo's own hand-written test programs, whose `common::syscall`
    // deliberately marks them clobbered to match this kernel's old,
    // non-compliant behavior) relies on that guarantee. Fixing it
    // removed the crash entirely, but `ld.so` still never calls `mmap`
    // on the fd it opens for `libc.so.6` — see `linux_abi.rs`'s module
    // docs for the current diagnosis of what's left.
    match fat::read_file("/bin/hello.elf") {
        Ok(elf) => {
            let pid = crate::process::spawn_linux(&elf, "hello", &["hello"], &["PATH=/bin"])
                .unwrap_or_else(|e| selftest_fail(&format!("hello spawn failed: {e}")));
            match crate::process::wait(pid, 500) {
                Some(info) => println!("[INFO] real dynamically-linked hello binary: {}", process::describe_exit(&info)),
                None => println!("[INFO] real dynamically-linked hello binary: did not exit within 5s"),
            }
            crate::process::reap(pid);
        }
        Err(_) => println!("[SKIP] /bin/hello.elf not present"),
    }

    // Same real-binary methodology against real coreutils `true`/`cat`
    // (Debian coreutils 9.1-1) — confirms the remaining gap above isn't
    // specific to GNU Hello's own build: both fail identically
    // ("undefined symbol: __libc_start_main, version GLIBC_2.34"),
    // ruling out a version-mismatched test fixture as the explanation.
    match fat::read_file("/bin/true.elf") {
        Ok(elf) => {
            let pid = crate::process::spawn_linux(&elf, "true", &["true"], &["PATH=/bin"])
                .unwrap_or_else(|e| selftest_fail(&format!("true spawn failed: {e}")));
            match crate::process::wait(pid, 500) {
                Some(info) => println!("[INFO] real coreutils true: {}", process::describe_exit(&info)),
                None => println!("[INFO] real coreutils true: did not exit within 5s"),
            }
            crate::process::reap(pid);
        }
        Err(_) => println!("[SKIP] /bin/true.elf not present"),
    }
    match fat::read_file("/bin/cat.elf") {
        Ok(elf) => {
            let pid = crate::process::spawn_linux(&elf, "cat", &["cat", "/dev/null"], &["PATH=/bin"])
                .unwrap_or_else(|e| selftest_fail(&format!("cat spawn failed: {e}")));
            match crate::process::wait(pid, 500) {
                Some(info) => println!("[INFO] real coreutils cat: {}", process::describe_exit(&info)),
                None => println!("[INFO] real coreutils cat: did not exit within 5s"),
            }
            crate::process::reap(pid);
        }
        Err(_) => println!("[SKIP] /bin/cat.elf not present"),
    }

    // Process management, part 7 (Phase 5): a real Linux-ABI client
    // process drawing through `wayland.rs`'s kernel-native compositor —
    // memfd_create/ftruncate/mmap(MAP_SHARED), socket/connect, and
    // sendmsg with SCM_RIGHTS to hand the compositor task a real shared
    // frame of pixels over a real AF_UNIX socket. Checking the client's
    // own exit code only proves its own syscalls succeeded; the pixel
    // check below is what proves the whole chain — two independently
    // scheduled tasks sharing physical memory through a kernel-mediated
    // fd handoff — actually worked, not just returned success codes.
    {
        // Must match `prog_linux_wayland_client.rs`'s TEST_COLOR/WIDTH.
        const TEST_COLOR: u32 = 0x00_FF10_C0;
        const SURFACE_X: u32 = 40;
        const SURFACE_Y: u32 = 40;

        let frames_before = crate::wayland::FRAMES_RENDERED.load(core::sync::atomic::Ordering::Acquire);
        let pid = crate::process::spawn_linux(crate::user_prog::PROG_LINUX_WAYLAND_CLIENT, "wl-client", &["wl-client"], &[])
            .unwrap_or_else(|e| selftest_fail(&format!("wayland client spawn failed: {e}")));
        match crate::process::wait(pid, 200) {
            Some(process::ExitInfo::Normal) => println!("[OK] Wayland client process exited normally"),
            other => selftest_fail(&format!("wayland client gave unexpected exit: {:?}", other)),
        }
        match crate::process::read_result(pid) {
            Some(0x7F) => println!("[OK] Wayland client: memfd/mmap/socket/connect/sendmsg all correct"),
            other => selftest_fail(&format!("wayland client result mismatch: {:?}", other)),
        }
        crate::process::reap(pid);

        if !crate::wayland::wait_for_frame(frames_before, 100) {
            selftest_fail("compositor never rendered a frame for the wayland client");
        }
        match crate::fb::get_pixel(SURFACE_X, SURFACE_Y) {
            Some(color) if color == TEST_COLOR => {
                println!("[OK] compositor blitted the client's shared-memory buffer onto the real framebuffer")
            }
            other => selftest_fail(&format!("framebuffer pixel after wayland commit: {:?} (want {:#x})", other, TEST_COLOR)),
        }
    }

    // Ring 3 round trip: run a hand-assembled user-mode program (mapped
    // PAGE_USER, entered via `enter_usermode`'s iretq) that calls the
    // sys_write syscall once per character and then sys_exit. Reaching the
    // line after `run_demo` at all proves SYSCALL/SYSRET and the manual
    // "return to kernel" unwind both worked; the write count confirms
    // every syscall was dispatched (not just the first).
    let message = b"hello from ring3\n";
    let before = crate::syscall::write_count();
    crate::syscall::run_demo(message);
    let written = crate::syscall::write_count() - before;
    if written != message.len() as u64 {
        selftest_fail("ring3 syscall round trip: wrong write count");
    }
    println!("[OK] ring3 syscall round trip ({} sys_write calls via SYSCALL/SYSRET)", written);

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
                print!("{}{}", prompt(), self.line);
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
            keyboard::Event::Left
            | keyboard::Event::Right
            | keyboard::Event::Escape
            | keyboard::Event::F2
            | keyboard::Event::AltTab => Feed::Pending,
            keyboard::Event::Ctrl('v') => {
                // Paste the clipboard at the end of the line (no
                // mid-line cursor yet). Pasting a multi-line command
                // would confuse the single-line editor, so only the
                // first line of the clipboard is inserted.
                if let Some(text) = crate::clipboard::paste_text() {
                    let first = text.lines().next().unwrap_or("");
                    if self.line.len() + first.len() <= 256 {
                        self.line.push_str(first);
                        print!("{}", first);
                    }
                }
                self.history_index = None;
                Feed::Pending
            }
            keyboard::Event::Ctrl(_) => Feed::Pending,
        }
    }
}
