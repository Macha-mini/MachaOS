# MachaOS

A small x86_64 Multiboot kernel written in Rust. It currently includes:

- Multiboot 1 boot entry and 32-bit to long-mode transition
- A statically identity-mapped 4 GiB address space (2 MiB pages)
- A linear VBE framebuffer desktop (1024x768x32, requested via the Multiboot video header) with a window manager: draggable/closable windows, a taskbar, and a mouse cursor, alongside VGA text output and COM1 serial logging
- GDT, TSS, IST, IDT, exception handlers, PIC, PIT, PS/2 keyboard, and PS/2 mouse IRQs
- A lock-protected 8 MiB free-list heap allocator
- Preemptive kernel-thread multitasking: a round-robin scheduler switches tasks from the PIT timer interrupt (50 ms quanta), sharing the single address space
- CPUID information and a small interactive shell, running inside a Terminal window on the desktop
- A graphical (and text-mode) panic screen
- QEMU self-test coverage for heap allocation, interrupts, CPU information, and background task scheduling

## Requirements

- Rust nightly with the `rust-src` component
- QEMU
- A BIOS-capable GRUB rescue tool (`i686-elf-grub-mkrescue` or `grub-mkrescue`)
- `xorriso` and `mtools`

On macOS with Homebrew:

```sh
brew install qemu i686-elf-grub xorriso mtools
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

## Desktop

On normal boot, MachaOS switches to a 1024x768 graphical desktop with two
windows: a **Terminal** (the interactive shell) and a **System Info** panel.
Click a title bar to focus/raise a window and drag it around; click the red
`x` to close it. The taskbar at the bottom lists open windows, three
counters (`bg: ...`) incrementing in the background scheduler tasks, and
uptime. If no linear framebuffer is available, MachaOS falls back to the
plain VGA text shell automatically.

## Shell

Type `help` at the `machaos>` prompt (inside the Terminal window, or at the
text-mode fallback). Available commands: `help`, `clear`/`cls`, `echo`,
`time`, `uptime`, `meminfo`, `heap`, `cpuinfo`, `version`/`ver`, `reboot`,
`shutdown`, `crash`, `breakpoint`, `fault`, `panic`, `mousetest`, and
`tasks` (lists scheduler tasks and their background counters).
