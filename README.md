# MachaOS

A small x86_64 Multiboot kernel written in Rust. It currently includes:

- Multiboot 1 boot entry and 32-bit to long-mode transition
- VGA text output and COM1 serial logging
- GDT, TSS, IST, IDT, exception handlers, PIC, PIT, and keyboard IRQs
- A lock-protected 1 MiB free-list heap allocator
- CPUID information and a small interactive shell
- QEMU self-test coverage for heap allocation, interrupts, and CPU information

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

## Shell

After normal boot, type `help` at the `machaos>` prompt. Available commands: `help`, `clear`/`cls`, `echo`, `time`, `uptime`, `meminfo`, `heap`, `cpuinfo`, `version`/`ver`, `reboot`, `shutdown`, `crash`, `breakpoint`, `fault`, and `panic`.
