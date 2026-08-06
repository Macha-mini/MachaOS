ENTRY(boot)

SECTIONS
{
    /* Loaded at 8 MiB, not 1 MiB: the kernel image (code + a 1 MiB
       PMM bitmap + page tables) now grows past 4 MiB, which is exactly
       where real (non-PIE) Linux binaries such as BusyBox are linked to
       load. Processes map their ELF segments straight onto physical
       addresses (identity map), so a kernel that overlaps 4 MiB gets
       silently overwritten by a running process. 8 MiB keeps the kernel
       clear of both the 4 MiB Linux-load zone and the 48 MiB
       native-ABI load zone, and is still inside LOW_RESERVED_END
       (64 MiB), so the PMM never hands those frames out. */
    . = 8M;
    _kernel_start = .;

    .text : {
        KEEP(*(.multiboot))
        *(.text .text.*)
    }

    . = ALIGN(4K);
    .rodata : {
        *(.rodata .rodata.*)
    }

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
    }

    . = ALIGN(4K);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
    }

    _kernel_end = .;
}
