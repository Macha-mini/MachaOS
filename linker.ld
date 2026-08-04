ENTRY(boot)

SECTIONS
{
    . = 1M;
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
