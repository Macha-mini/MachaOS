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
        /* Orphan sections lld can emit depending on what a program's code
         * references (e.g. an indirect call through a bounds-check panic
         * path pulls in a GOT entry) even under relocation-model=static.
         * Without a landing spot here, lld gives each of these its own
         * PT_LOAD at vaddr 0 instead — which the kernel's loader rejects
         * ("segment below 2 MiB") since 0 is nowhere near this script's
         * 0x3000000 base.
         */
        *(.got .got.plt .data.rel.ro .data.rel.ro.*)
    }

    . = ALIGN(16);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note .note.*)
    }
}
