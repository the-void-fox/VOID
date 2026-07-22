/* Раскладка памяти ядра VOID для x86_64 (Веха 25 — PVH direct boot).
 *
 * QEMU (`-kernel`) грузит ELF64 по физическим адресам сегментов и стартует 32-битный
 * вход из PVH-ноты (см. arch/x86_64/entry.s). База 1 МиБ — ниже только BIOS/EBDA-дыры.
 * Ядро живёт identity-mapped (VA == PA), как на RISC-V: та же модель direct map, те же
 * символы _*_start/_end и _kernel_end для W^X (arch/x86_64/paging.rs) и frame.rs.
 * Higher-half не используется сознательно — см. ADR 0005/0006. */
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start32)

BASE_ADDRESS = 0x100000;

SECTIONS
{
    . = BASE_ADDRESS;
    _kernel_start = .;

    /* Multiboot1-заголовок (Веха 41) — САМЫМ ПЕРВЫМ в файле: GRUB ищет magic в первых
     * 8 КиБ файла. .text выровнен на страницу и уезжает за 8 КиБ, поэтому заголовок —
     * в отдельной секции здесь, у самого начала первого PT_LOAD. */
    .multiboot : ALIGN(4) {
        KEEP(*(.multiboot))
    }

    /* PVH-нота — отдельной секцией типа note: LLD сам заведёт под неё PT_NOTE,
     * в котором её ищет QEMU. Прочие ноты игнорируются (/DISCARD/ ниже). */
    .note : ALIGN(4) {
        *(.note.Xen)
    }

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
        . = ALIGN(4K);
        _rodata_end = .;
    }

    .data : ALIGN(4K) {
        _data_start = .;
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss : ALIGN(8) {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _bss_end = .;
    }

    . = ALIGN(4K);
    _kernel_end = .;            /* конец образа; дальше свободная RAM под фреймы */

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
