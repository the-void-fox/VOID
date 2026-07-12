/* Раскладка памяти ядра VOID для x86_64 — ЗАГЛУШКА Вехи 24.
 *
 * Цель — линковаться (сборка под оба таргета из одного дерева); настоящая раскладка
 * (Limine, higher-half по 0xffffffff80000000, W^X постранично) придёт с bring-up (Вехи 25+).
 * База 1 МиБ — классическая нижняя загрузка; символы _*_start/_end и _kernel_end
 * повторяют RISC-V-скрипт: на них ссылается общий код (frame.rs). */
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

BASE_ADDRESS = 0x100000;

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
