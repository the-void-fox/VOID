/* Раскладка памяти ядра VOID для x86_64 (Веха 25 — PVH direct boot).
 *
 * QEMU (`-kernel`) грузит ELF64 по ФИЗИЧЕСКИМ адресам сегментов (p_paddr) и стартует
 * 32-битный вход из PVH-ноты (см. arch/x86_64/entry.s); GRUB на металле — так же, по
 * multiboot2. База 1 МиБ — ниже только BIOS/EBDA-дыры.
 *
 * Веха 87 — higher-half: секции линкуются по высоким VMA при физических LMA. Окно
 * образа — ПОСЛЕДНИЕ 2 ГиБ адресного пространства, потому что `x86_64-unknown-none`
 * собран с моделью кода `kernel` (знаковая 32-битная адресация от -2 ГиБ): никуда
 * больше слинковать ядро нельзя, не пересобирая core. Поэтому окон ДВА, как в Linux:
 * это (`__START_KERNEL_map`) — под сам образ, и direct-map RAM с начала верхней
 * половины (`PAGE_OFFSET`, arch/x86_64/mod.rs) — под всю физическую память.
 * См. ADR 0010. */
OUTPUT_ARCH(i386:x86-64)
/* Точка входа ELF — ФИЗИЧЕСКИЙ адрес: стартуем в 32-битном режиме без пейджинга. */
ENTRY(_start32_phys)

KERNEL_OFFSET = 0xFFFFFFFF80000000;
PHYS_BASE = 0x100000;
BASE_ADDRESS = KERNEL_OFFSET + PHYS_BASE;

SECTIONS
{
    . = BASE_ADDRESS;
    _kernel_start = .;

    /* Multiboot1-заголовок (Веха 41) — САМЫМ ПЕРВЫМ в файле: GRUB ищет magic в первых
     * 8 КиБ файла. .text выровнен на страницу и уезжает за 8 КиБ, поэтому заголовок —
     * в отдельной секции здесь, у самого начала первого PT_LOAD. */
    .multiboot : AT(ADDR(.multiboot) - KERNEL_OFFSET) ALIGN(4) {
        KEEP(*(.multiboot))
    }

    /* PVH-нота — отдельной секцией типа note: LLD сам заведёт под неё PT_NOTE,
     * в котором её ищет QEMU. Прочие ноты игнорируются (/DISCARD/ ниже). */
    .note : AT(ADDR(.note) - KERNEL_OFFSET) ALIGN(4) {
        *(.note.Xen)
    }

    .text : AT(ADDR(.text) - KERNEL_OFFSET) ALIGN(4K) {
        _text_start = .;
        *(.text.entry)          /* _start обязан быть первым */
        *(.text .text.*)
        . = ALIGN(4K);
        _text_end = .;
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) ALIGN(4K) {
        _rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(4K);
        _rodata_end = .;
    }

    .data : AT(ADDR(.data) - KERNEL_OFFSET) ALIGN(4K) {
        _data_start = .;
        *(.data .data.*)
        /* Веха 126.4 — у GOT появились имена, чтобы её было ВИДНО в дизассемблере. Через неё
         * идут косвенные вызовы memcpy/memset, и без границ такой вызов подписывается ближайшим
         * символом слева — при разборе падения он выглядел как `callq *<TSS+0x70>`, то есть
         * уводил в заведомо ложную сторону. С границами это честное `<_got_start+0x8>`. */
        _got_start = .;
        *(.got .got.*)
        _got_end = .;
    }

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) ALIGN(8) {
        _bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _bss_end = .;
    }

    . = ALIGN(4K);
    _kernel_end = .;            /* конец образа; дальше свободная RAM под фреймы */

    _start32_phys = _start32 - KERNEL_OFFSET;   /* см. ENTRY выше и PVH-ноту в entry.s */

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
