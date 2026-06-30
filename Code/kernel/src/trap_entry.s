# Ассемблерный трамплин обработки trap'ов (S-mode).
#
# При любом trap'е (исключение или прерывание) процессор:
#   - кладёт адрес виновной инструкции в sepc, причину в scause, доп.инфо в stval;
#   - переносит sstatus.SIE -> sstatus.SPIE и обнуляет SIE (прерывания выключены
#     на время обработки — поэтому вложенных trap'ов того же типа не будет);
#   - прыгает по адресу из stvec (сюда, т.к. режим Direct).
#
# Мы пришли в S-mode из S-mode (ядро), стек тот же. Сохраняем ВСЕ регистры в
# структуру TrapFrame на стеке, передаём указатель на неё в Rust-диспетчер
# trap_handler(&mut TrapFrame), затем восстанавливаем регистры и возвращаемся
# инструкцией sret (она же восстановит SIE из SPIE).
#
# Раскладка TrapFrame (см. trap.rs, repr(C)) — 34 ячейки по 8 байт = 272:
#   слот i (i=0..31) -> регистр xi   (x0 не нужен, но слот держим для простоты индексации)
#   слот 32          -> sepc
#   слот 33          -> sstatus

.section .text
.p2align 2                      # stvec требует выравнивания адреса по 4 байта
.global trap_entry
trap_entry:
    addi  sp, sp, -272          # выделить TrapFrame на стеке

    sd    x1,   1*8(sp)         # ra
    addi  x1, sp, 272           # x1 := исходный sp (до выделения кадра)
    sd    x1,   2*8(sp)         # сохранить исходный sp в слот x2
    sd    x3,   3*8(sp)
    sd    x4,   4*8(sp)
    sd    x5,   5*8(sp)
    sd    x6,   6*8(sp)
    sd    x7,   7*8(sp)
    sd    x8,   8*8(sp)
    sd    x9,   9*8(sp)
    sd    x10, 10*8(sp)
    sd    x11, 11*8(sp)
    sd    x12, 12*8(sp)
    sd    x13, 13*8(sp)
    sd    x14, 14*8(sp)
    sd    x15, 15*8(sp)
    sd    x16, 16*8(sp)
    sd    x17, 17*8(sp)
    sd    x18, 18*8(sp)
    sd    x19, 19*8(sp)
    sd    x20, 20*8(sp)
    sd    x21, 21*8(sp)
    sd    x22, 22*8(sp)
    sd    x23, 23*8(sp)
    sd    x24, 24*8(sp)
    sd    x25, 25*8(sp)
    sd    x26, 26*8(sp)
    sd    x27, 27*8(sp)
    sd    x28, 28*8(sp)
    sd    x29, 29*8(sp)
    sd    x30, 30*8(sp)
    sd    x31, 31*8(sp)

    csrr  t0, sepc              # t0 == x5, уже сохранён выше — можно использовать как scratch
    sd    t0, 32*8(sp)
    csrr  t0, sstatus
    sd    t0, 33*8(sp)

    mv    a0, sp                # arg0 = указатель на TrapFrame
    call  trap_handler

    ld    t0, 32*8(sp)         # sepc мог быть изменён обработчиком (напр. перешагнуть ebreak)
    csrw  sepc, t0
    ld    t0, 33*8(sp)
    csrw  sstatus, t0

    ld    x1,   1*8(sp)
    # x2 (sp) НЕ восстанавливаем из кадра — вернём его через addi ниже
    ld    x3,   3*8(sp)
    ld    x4,   4*8(sp)
    ld    x5,   5*8(sp)        # вернёт настоящий t0
    ld    x6,   6*8(sp)
    ld    x7,   7*8(sp)
    ld    x8,   8*8(sp)
    ld    x9,   9*8(sp)
    ld    x10, 10*8(sp)
    ld    x11, 11*8(sp)
    ld    x12, 12*8(sp)
    ld    x13, 13*8(sp)
    ld    x14, 14*8(sp)
    ld    x15, 15*8(sp)
    ld    x16, 16*8(sp)
    ld    x17, 17*8(sp)
    ld    x18, 18*8(sp)
    ld    x19, 19*8(sp)
    ld    x20, 20*8(sp)
    ld    x21, 21*8(sp)
    ld    x22, 22*8(sp)
    ld    x23, 23*8(sp)
    ld    x24, 24*8(sp)
    ld    x25, 25*8(sp)
    ld    x26, 26*8(sp)
    ld    x27, 27*8(sp)
    ld    x28, 28*8(sp)
    ld    x29, 29*8(sp)
    ld    x30, 30*8(sp)
    ld    x31, 31*8(sp)

    addi  sp, sp, 272          # освободить TrapFrame
    sret                       # вернуться на sepc, восстановив SIE из SPIE
