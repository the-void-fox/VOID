/* Раскладка памяти userspace-программы VOID (Веха 19.1).
 *
 * Линкуется на фиксированную базу 0x4000_0000 — начало региона VPN[2]=1, отведённого ядром под
 * адресное пространство процесса (см. `proc::USER_REGION_START`, kernel/src/proc.rs). Стек
 * процессу даёт ядро как обычно (растёт вниз от 0x8000_0000) — здесь его не резервируем.
 *
 * Три PT_LOAD-сегмента с РАЗНЫМИ правами (флаги ELF: R=4, W=2, X=1) — ядерный загрузчик
 * (kernel/src/elf.rs) читает `p_flags` каждого сегмента и маппит его страницы буквально этими
 * правами. ALIGN(4K) перед каждым сегментом обязателен: если бы два сегмента с разными правами
 * делили одну 4-КиБ страницу, загрузчик замаппил бы её дважды с разными флагами (последний
 * побеждает — тихая порча прав). W^X: ни один сегмент не несёт W и X одновременно.
 *
 * `_start` — обычная `extern "C" fn` (см. src/main.rs), НЕ ассемблерный трамплин: в отличие от
 * ядра (которое OpenSBI прыгает по фиксированному физическому адресу первым байтом образа), сюда
 * приходят через `e_entry` из ELF-заголовка — адрес символа `_start` неважен, важно, что ENTRY()
 * его туда положит. Стек (sp) и аргумент (a0) уже выставлены ядром в trap-кадре до входа. */
/* OUTPUT_ARCH не задаём: скрипт один на обе архитектуры (riscv64 и x86_64, Веха 26) —
 * архитектуру объектов знает линкер, а раскладка/PHDRS/W^X от неё не зависят. */
ENTRY(_start)

BASE_ADDRESS = 0x40000000;

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R+X — код, никогда W */
    rodata PT_LOAD FLAGS(4); /* R   — константы, никогда W и никогда X */
    data   PT_LOAD FLAGS(6); /* R+W — данные+bss, никогда X (W^X) */
    tls    PT_TLS  FLAGS(4); /* шаблон TLS (Веха 35): .tdata+.tbss, R (только читается) */
}

SECTIONS
{
    . = BASE_ADDRESS;

    .text : ALIGN(4K) {
        *(.text .text.*)
    } :text

    . = ALIGN(4K);
    .rodata : {
        *(.rodata .rodata.*)
        *(.srodata .srodata.*)
    } :rodata

    /* Веха 36 — C-мир: массивы конструкторов/деструкторов gcc. Указатели резолвятся
       статически (не-PIE) — им хватает R-only сегмента; KEEP — от них никто не
       ссылается, без KEEP gc-sections их выкинул бы. crt0 void-libc обходит их сам
       (crti/crtbegin не линкуются: -nostartfiles). Rust-объекты их не эмитят —
       для Rust-программ секции пусты и безвредны. */
    .init_array : ALIGN(8) {
        __init_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array))
        __init_array_end = .;
    } :rodata
    .fini_array : ALIGN(8) {
        __fini_array_start = .;
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*)))
        KEEP(*(.fini_array))
        __fini_array_end = .;
    } :rodata

    . = ALIGN(4K);
    .data : {
        *(.data .data.*)
        /* riscv gcc адресует малые данные относительно gp (linker relaxation):
           классическая точка gp — начало .sdata + 0x800 (охват ±2 КиБ в обе стороны).
           crt0 void-libc грузит его в gp; Rust/lld символ не трогают. */
        PROVIDE(__global_pointer$ = . + 0x800);
        *(.sdata .sdata.*)
        *(.got .got.*)      /* x86: редкие GOT-ссылки статики — чтобы не осиротели */
    } :data

    /* Веха 35 — шаблон TLS. Модель local-exec: линкер резолвит TPREL-смещения переменных
       ОТНОСИТЕЛЬНО начала PT_TLS (нет динамических релокаций). .tdata несёт инициализаторы
       (грузится и как часть data-сегмента), .tbss — нулевой хвост (NOBITS). Рантайм pal
       (sys/pal/void/tls.rs) копирует шаблон в per-thread блок и ставит tp/база %fs.
       __tls_align = p_align сегмента (по нему рантайм округляет размер блока для x86). */
    .tdata : {
        __tls_start = .;
        *(.tdata .tdata.*)
        __tdata_end = .;
    } :data :tls
    .tbss : {
        *(.tbss .tbss.*)
        *(.tcommon)
        __tls_end = .;
    } :data :tls
    __tls_align = MAX(ALIGNOF(.tdata), ALIGNOF(.tbss));

    .bss : ALIGN(8) {
        _bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        *(COMMON)
        _bss_end = .;
    } :data

    . = ALIGN(4K);
    _end = .;

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
