/* Раскладка памяти ядра VOID для QEMU virt (RISC-V).
 * OpenSBI (-bios default) грузится в 0x80000000 и прыгает в наш ELF по ФИЗИЧЕСКОМУ
 * 0x80200000 в S-mode, передавая a0=hartid, a1=указатель на DTB.
 *
 * Веха 87 — higher-half: секции ЛИНКУЮТСЯ по высоким адресам (VMA), а ГРУЗЯТСЯ по
 * физическим (LMA = VMA − KERNEL_OFFSET; QEMU кладёт сегменты ELF по p_paddr).
 * Пока трансляция не включена, entry.s исполняется по физическим адресам — это
 * законно, потому что riscv собран с medany и всё внутри ядра адресуется
 * PC-относительно; после `satp` тот же код продолжается по высоким адресам.
 * KERNEL_OFFSET = начало верхней половины Sv39, поэтому phys_to_virt(0) попадает
 * ровно в её начало, а потолок RAM — вся половина (256 ГиБ). См. ADR 0010.
 *
 * Веха 3: секции выровнены по 4 КиБ и обрамлены символами _*_start/_*_end, чтобы
 * страницы кода/констант/данных не пересекались — это нужно для постраничного W^X
 * (код R+X, константы R, данные R+W) в paging.rs. */
OUTPUT_ARCH(riscv)
/* Точка входа ELF — ФИЗИЧЕСКИЙ адрес `_start`: трансляции при старте ещё нет.
 * (QEMU и так прыгает на минимальный LMA, но полагаться на это не хочется.) */
ENTRY(_start_phys)

KERNEL_OFFSET = 0xFFFFFFC000000000;
PHYS_BASE = 0x80200000;
BASE_ADDRESS = KERNEL_OFFSET + PHYS_BASE;

SECTIONS
{
    . = BASE_ADDRESS;
    _kernel_start = .;

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
        *(.srodata .srodata.*)
        . = ALIGN(4K);
        _rodata_end = .;
    }

    /* Секции `.user` больше нет (Веха 23): код U-mode приходит ТОЛЬКО из ELF в объектном
     * store (см. programs/user, kernel/src/elf.rs) — в образе ядра нет ни одной страницы,
     * исполняемой из пользовательского режима. */

    /* .data + .bss + стек — всё RW, попадают под direct map как RW. */
    .data : AT(ADDR(.data) - KERNEL_OFFSET) ALIGN(4K) {
        _data_start = .;
        *(.data .data.*)
        *(.sdata .sdata.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) ALIGN(8) {
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

    _start_phys = _start - KERNEL_OFFSET;   /* см. ENTRY выше */

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
