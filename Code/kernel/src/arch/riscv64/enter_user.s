# Вход в процесс: переключить адресное пространство и возобновить его trap-кадр (Веха 10.2).
#   a0 = *TrapFrame (в памяти ядра; она отображена и в таблице процесса), a1 = satp, a2 = trap_top
#
# Раскладка TrapFrame (см. trap.rs, repr(C)): слот i -> регистр xi (i=0..31),
#   слот 32 -> sepc, слот 33 -> sstatus, слоты 34..65 -> f0..f31, слот 66 -> fcsr.
#   a0 (x10) загружаем ПОСЛЕДНИМ (это наша база).

.section .text
.p2align 2
.global enter_user_frame
enter_user_frame:
    csrw  satp, a1              # переключиться в адресное пространство процесса
    sfence.vma                  # сбросить TLB
    csrw  sscratch, a2          # trap из U переключится на ядерный trap-стек

    ld    t0, 33*8(a0)          # sstatus (SPP=0 -> sret уйдёт в U-mode, SUM=1)
    csrw  sstatus, t0
    ld    t0, 32*8(a0)          # sepc (точка входа/продолжения)
    csrw  sepc, t0

    # FP-контекст процесса (Веха 32). fld легален: sstatus.FS уже ≥ Initial —
    # только что записан из кадра (new_user ставит Initial, дальше живёт своё).
    .option push
    .option arch, +d
    ld    t0, 66*8(a0)
    csrw  fcsr, t0
    fld   f0,  34*8(a0)
    fld   f1,  35*8(a0)
    fld   f2,  36*8(a0)
    fld   f3,  37*8(a0)
    fld   f4,  38*8(a0)
    fld   f5,  39*8(a0)
    fld   f6,  40*8(a0)
    fld   f7,  41*8(a0)
    fld   f8,  42*8(a0)
    fld   f9,  43*8(a0)
    fld   f10, 44*8(a0)
    fld   f11, 45*8(a0)
    fld   f12, 46*8(a0)
    fld   f13, 47*8(a0)
    fld   f14, 48*8(a0)
    fld   f15, 49*8(a0)
    fld   f16, 50*8(a0)
    fld   f17, 51*8(a0)
    fld   f18, 52*8(a0)
    fld   f19, 53*8(a0)
    fld   f20, 54*8(a0)
    fld   f21, 55*8(a0)
    fld   f22, 56*8(a0)
    fld   f23, 57*8(a0)
    fld   f24, 58*8(a0)
    fld   f25, 59*8(a0)
    fld   f26, 60*8(a0)
    fld   f27, 61*8(a0)
    fld   f28, 62*8(a0)
    fld   f29, 63*8(a0)
    fld   f30, 64*8(a0)
    fld   f31, 65*8(a0)
    .option pop

    ld    x1,   1*8(a0)
    ld    x2,   2*8(a0)         # sp процесса
    ld    x3,   3*8(a0)
    ld    x4,   4*8(a0)
    ld    x5,   5*8(a0)
    ld    x6,   6*8(a0)
    ld    x7,   7*8(a0)
    ld    x8,   8*8(a0)
    ld    x9,   9*8(a0)
    # x10 (a0) — база, загрузим последним
    ld    x11, 11*8(a0)
    ld    x12, 12*8(a0)
    ld    x13, 13*8(a0)
    ld    x14, 14*8(a0)
    ld    x15, 15*8(a0)
    ld    x16, 16*8(a0)
    ld    x17, 17*8(a0)
    ld    x18, 18*8(a0)
    ld    x19, 19*8(a0)
    ld    x20, 20*8(a0)
    ld    x21, 21*8(a0)
    ld    x22, 22*8(a0)
    ld    x23, 23*8(a0)
    ld    x24, 24*8(a0)
    ld    x25, 25*8(a0)
    ld    x26, 26*8(a0)
    ld    x27, 27*8(a0)
    ld    x28, 28*8(a0)
    ld    x29, 29*8(a0)
    ld    x30, 30*8(a0)
    ld    x31, 31*8(a0)
    ld    x10, 10*8(a0)         # наконец a0

    sret                        # в U-mode на sepc с восстановленным sp
