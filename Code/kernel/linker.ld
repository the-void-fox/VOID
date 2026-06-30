/* Раскладка памяти ядра VOID для QEMU virt (RISC-V).
 * OpenSBI (-bios default) грузится в 0x80000000 и прыгает в наш ELF по 0x80200000
 * в S-mode, передавая a0=hartid, a1=указатель на DTB. */
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;

SECTIONS
{
    . = BASE_ADDRESS;

    .text : {
        *(.text.entry)          /* _start обязан быть первым */
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    }

    .data : {
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : {
        . = ALIGN(8);
        _bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _bss_end = .;
    }

    . = ALIGN(16);
    . += 64K;                   /* стек ядра */
    _stack_top = .;

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
