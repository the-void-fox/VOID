# Ассемблерный трамплин обработки trap'ов (S-mode).
#
# При любом trap'е процессор кладёт sepc/scause/stval, переносит SIE->SPIE и обнуляет SIE,
# сохраняет прежний режим в sstatus.SPP, и прыгает по stvec (сюда, режим Direct).
#
# Переключение стека (Веха 10). Инвариант: sscratch = вершина ЯДЕРНОГО trap-стека, пока
# исполняется U-mode, и 0, пока исполняется ядро (S-mode).
#   - trap из U: нельзя строить кадр на пользовательском стеке → переключаемся на ядерный.
#   - trap из S: стек уже ядерный → работаем на нём (как раньше).
#
# Раскладка TrapFrame (см. trap.rs, repr(C)) — 67 ячеек по 8 байт = 536 (кадр 544, 16-выровнен):
#   слот i (i=0..31) -> регистр xi   (x2 = исходный sp виновника trap'а)
#   слот 32          -> sepc
#   слот 33          -> sstatus
#   слоты 34..65     -> f0..f31 (только на trap'е из U — ядро float'ов не имеет)
#   слот 66          -> fcsr

.section .text
.p2align 2                      # stvec требует выравнивания адреса по 4 байта
.global trap_entry
trap_entry:
    csrrw sp, sscratch, sp      # sp <-> sscratch
    bnez  sp, .Lon_stack        # sp != 0 -> trap из U: sp = вершина ядерного trap-стека
    csrrw sp, sscratch, sp      # trap из S: вернуть sp, sscratch снова 0
.Lon_stack:
    addi  sp, sp, -544          # выделить TrapFrame на (теперь точно ядерном) стеке

    sd    x1,   1*8(sp)         # ra (дальше x1 используем как scratch — оригинал уже сохранён)
    # Сохранить исходный sp виновника в слот x2:
    #   trap из S: это sp+544 ; trap из U: он сейчас в sscratch (туда попал при первом swap).
    csrr  x1, sstatus
    andi  x1, x1, 0x100         # бит SPP (1<<8): 1 = trap из S, 0 = trap из U
    bnez  x1, .Lsp_from_s
    csrr  x1, sscratch          # trap из U: исходный sp = user sp
    j     .Lsp_store
.Lsp_from_s:
    addi  x1, sp, 544           # trap из S: исходный sp = sp+544
.Lsp_store:
    sd    x1,   2*8(sp)

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

    csrr  t0, sepc
    sd    t0, 32*8(sp)
    csrr  t0, sstatus
    sd    t0, 33*8(sp)

    # FP-контекст (Веха 32): сохранять только для trap'а из U — там sstatus.FS
    # процесса ≥ Initial (fsd легален), а ядро float-регистры не трогает.
    andi  t1, t0, 0x100         # SPP: 1 = из S (пропустить), 0 = из U (сохранить)
    bnez  t1, .Lno_fsave
    # Таргет ядра riscv64gc-unknown-none-elf собран без D (ядро не должно
    # ЭМИТИТЬ float) — включаем расширение только для этих ручных инструкций.
    .option push
    .option arch, +d
    fsd   f0,  34*8(sp)
    fsd   f1,  35*8(sp)
    fsd   f2,  36*8(sp)
    fsd   f3,  37*8(sp)
    fsd   f4,  38*8(sp)
    fsd   f5,  39*8(sp)
    fsd   f6,  40*8(sp)
    fsd   f7,  41*8(sp)
    fsd   f8,  42*8(sp)
    fsd   f9,  43*8(sp)
    fsd   f10, 44*8(sp)
    fsd   f11, 45*8(sp)
    fsd   f12, 46*8(sp)
    fsd   f13, 47*8(sp)
    fsd   f14, 48*8(sp)
    fsd   f15, 49*8(sp)
    fsd   f16, 50*8(sp)
    fsd   f17, 51*8(sp)
    fsd   f18, 52*8(sp)
    fsd   f19, 53*8(sp)
    fsd   f20, 54*8(sp)
    fsd   f21, 55*8(sp)
    fsd   f22, 56*8(sp)
    fsd   f23, 57*8(sp)
    fsd   f24, 58*8(sp)
    fsd   f25, 59*8(sp)
    fsd   f26, 60*8(sp)
    fsd   f27, 61*8(sp)
    fsd   f28, 62*8(sp)
    fsd   f29, 63*8(sp)
    fsd   f30, 64*8(sp)
    fsd   f31, 65*8(sp)
    csrr  t1, fcsr
    sd    t1, 66*8(sp)
    .option pop
.Lno_fsave:

    mv    a0, sp                # arg0 = указатель на TrapFrame
    call  trap_handler

    ld    t0, 32*8(sp)         # sepc мог быть изменён обработчиком
    csrw  sepc, t0
    ld    t0, 33*8(sp)         # sstatus
    andi  t1, t0, 0x100        # SPP
    bnez  t1, .Lret_s          # SPP=1 -> возврат в S: sscratch оставляем 0
    addi  t1, sp, 544          # SPP=0 -> возврат в U: sscratch = вершина trap-стека (sp+544)
    csrw  sscratch, t1
.Lret_s:
    csrw  sstatus, t0

    ld    x1,   1*8(sp)
    ld    x3,   3*8(sp)
    ld    x4,   4*8(sp)
    ld    x5,   5*8(sp)
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

    ld    x2,   2*8(sp)        # sp последним: user sp (возврат в U) или kernel sp (возврат в S)
    sret                       # вернуться на sepc, восстановив режим из SPP и SIE из SPIE
