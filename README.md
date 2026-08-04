# MachaOS

A small x86_64 Multiboot kernel written in Rust. It currently includes:

- Multiboot 1 boot entry and 32-bit to long-mode transition
- A statically identity-mapped 4 GiB address space (2 MiB pages)
- A linear VBE framebuffer desktop (1920x1080x32, requested via the Multiboot video header) with a window manager: draggable/resizable/minimizable/closable windows, a taskbar, and a mouse cursor, alongside VGA text output and COM1 serial logging
- GDT, TSS, IST, IDT, exception handlers, PIC, PIT, PS/2 keyboard, and PS/2 mouse IRQs
- A lock-protected 8 MiB free-list heap allocator
- Preemptive kernel-thread multitasking: a round-robin scheduler switches tasks from the PIT timer interrupt (50 ms quanta), sharing the single address space
- CPUID information and a small interactive shell, running inside a Terminal window on the desktop
- A polled ATA PIO driver and a FAT32 filesystem: `ls`, `cat`, `write`, `mkdir`, and `rm` commands (long filenames supported), with the Notepad saving/loading files to the disk
- A graphical (and text-mode) panic screen
- QEMU self-test coverage for heap allocation, interrupts, CPU information, background task scheduling, and FAT32 read/write round trips

## Requirements

- Rust nightly with the `rust-src` component
- QEMU
- A BIOS-capable GRUB rescue tool (`i686-elf-grub-mkrescue` or `grub-mkrescue`)
- `xorriso` and `mtools`
- `dosfstools` (`mkfs.fat`) to create the FAT32 disk image

On macOS with Homebrew:

```sh
brew install qemu i686-elf-grub xorriso mtools dosfstools
```

The `rust-toolchain.toml` file selects nightly and the `x86_64-unknown-none` target.

## Build and Run

```sh
make build
make run
```

For serial-only output:

```sh
make run-nographic
```

Run the automated QEMU self-test:

```sh
make test
```

The kernel binary is written to `target/x86_64-unknown-none/release/machaos` and the GRUB ISO to `target/machaos.iso`.

## Disk

`make run` (and `make test`) first build `target/disk.img`, a 64 MiB FAT32
disk image containing a few fixture files (`/hello world.txt`,
`/greetings.txt`, `/docs/readme.txt`). QEMU exposes it as the primary ATA
disk. MachaOS mounts it at boot and the shell's `ls`/`cat`/`write`/`mkdir`/`rm`
commands operate on it. Changes are written back to `target/disk.img`, so
files you create with `write` (or save from the Notepad) persist between
runs. The disk only holds fixtures if you recreate it with `make disk`.

## Desktop

On normal boot, MachaOS switches to a 1920x1080 graphical desktop with four
windows: a **Terminal** (the interactive shell), a **System Info** panel, a
**Calculator**, and a **Notepad** text editor. Click a title bar to
focus/raise a window and drag it around; click `_` to minimize it (it stays
in the taskbar, click it there to restore) or the red `x` to close it.
Terminal and Notepad have a resize grip in their bottom-right corner — drag
it to resize the window. The taskbar at the bottom lists open windows,
three counters (`bg: ...`) incrementing in the background scheduler tasks,
and uptime. If no linear framebuffer is available, MachaOS falls back to
the plain VGA text shell automatically.

The Notepad is a text editor with arrow-key cursor movement and
Enter/Backspace line editing. Press **Ctrl+S** to save the document to
`/notepad.txt` and **Ctrl+O** to load it back (the bottom row shows a
status message for both). The Calculator does integer-only four-function
arithmetic via mouse clicks.

## Shell

Type `help` at the `machaos>` prompt (inside the Terminal window, or at the
text-mode fallback). Available commands: `help`, `clear`/`cls`, `echo`,
`time`, `date`, `uptime`, `meminfo`, `heap`, `cpuinfo`, `version`/`ver`,
`reboot`, `shutdown`, `crash`, `breakpoint`, `fault`, `panic`, `mousetest`,
`tasks` (lists scheduler tasks and their background counters), and the
filesystem commands `ls [path]`, `cat <path>`, `write <path> <text>`,
`mkdir <path>`, `rm <path>`, `fatinfo`, `cd [path]`, and `pwd`. Paths may
be relative to the current directory (the prompt shows it), and arguments
containing spaces can be double-quoted: `write notes.txt "hello world"`.
Up/Down recall command history and Tab completes command names (in both
the Terminal window and the text-mode fallback shell).
