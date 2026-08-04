/* MachaOS user-program linker script. The fixed addresses below are a
 * contract with the kernel's ELF loader (src/process.rs):
 *
 *   .result lives at 0x2FF0000 (PROC_RESULT_VIRT in process.rs). The
 *   kernel maps this page, and after the process exits it reads the u64
 *   the program stores here as its result value.
 *
 *   .text starts at 0x3000000 (48 MiB), deliberately above the kernel
 *   image, the 40 MiB heap, and the PMM structures, so a process's
 *   private pages shadow no kernel-critical memory in its own page
 *   table. The loader rejects programs whose segments land anywhere
 *   below ~2 MiB or inside the heap range.
 */

ENTRY(_start)

SECTIONS
{
    .result 0x2FF0000 : {
        KEEP(*(.result))
    }

    . = 0x3000000;
    .text : {
        *(.text .text.*)
    }

    . = ALIGN(16);
    .rodata : {
        *(.rodata .rodata.*)
    }

    . = ALIGN(16);
    .data : {
        *(.data .data.*)
    }

    . = ALIGN(16);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
    }
}
