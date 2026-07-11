/* Раскладка памяти ядра VOID для QEMU virt (RISC-V).
 * OpenSBI (-bios default) грузится в 0x80000000 и прыгает в наш ELF по 0x80200000
 * в S-mode, передавая a0=hartid, a1=указатель на DTB.
 *
 * Веха 3: секции выровнены по 4 КиБ и обрамлены символами _*_start/_*_end, чтобы
 * страницы кода/констант/данных не пересекались — это нужно для постраничного W^X
 * (код R+X, константы R, данные R+W) в paging.rs. */
OUTPUT_ARCH(riscv)
ENTRY(_start)

BASE_ADDRESS = 0x80200000;

SECTIONS
{
    . = BASE_ADDRESS;
    _kernel_start = .;

    .text : ALIGN(4K) {
        _text_start = .;
        *(.text.entry)          /* _start обязан быть первым */
        *(.text .text.*)
        . = ALIGN(4K);
        _text_end = .;
    }

    .rodata : ALIGN(4K) {
        _rodata_start = .;
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        _rodata_end = .;
    }

    /* Секции `.user` больше нет (Веха 23): код U-mode приходит ТОЛЬКО из ELF в объектном
     * store (см. programs/user, kernel/src/elf.rs) — в образе ядра нет ни одной страницы,
     * исполняемой из пользовательского режима. */

    /* .data + .bss + стек — всё RW, попадают под direct map как RW. */
    .data : ALIGN(4K) {
        _data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : ALIGN(8) {
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

    . = ALIGN(4K);
    _kernel_end = .;            /* конец образа; дальше свободная RAM под фреймы */

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
