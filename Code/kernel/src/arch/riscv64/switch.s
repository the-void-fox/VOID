# Переключение контекста между задачами (кооперативное) и трамплин запуска.
#
# Сохраняем ТОЛЬКО callee-saved регистры (ra, sp, s0..s11). Caller-saved (a0..a7,
# t0..t6) сохранять не нужно: переключение всегда происходит в точке вызова
# context_switch как обычной функции, а вокруг вызова компилятор уже сохранил всё
# нужное сам. Это классический приём (как swtch в xv6).
#
#   context_switch(a0 = old: *mut Context, a1 = new: *const Context)
#
# Раскладка Context (repr(C), см. context.rs): ra@0, sp@8, s0..s11 @ 16..104.

.section .text
.globl context_switch
context_switch:
    sd   ra,   0*8(a0)
    sd   sp,   1*8(a0)
    sd   s0,   2*8(a0)
    sd   s1,   3*8(a0)
    sd   s2,   4*8(a0)
    sd   s3,   5*8(a0)
    sd   s4,   6*8(a0)
    sd   s5,   7*8(a0)
    sd   s6,   8*8(a0)
    sd   s7,   9*8(a0)
    sd   s8,  10*8(a0)
    sd   s9,  11*8(a0)
    sd   s10, 12*8(a0)
    sd   s11, 13*8(a0)

    ld   ra,   0*8(a1)
    ld   sp,   1*8(a1)
    ld   s0,   2*8(a1)
    ld   s1,   3*8(a1)
    ld   s2,   4*8(a1)
    ld   s3,   5*8(a1)
    ld   s4,   6*8(a1)
    ld   s5,   7*8(a1)
    ld   s6,   8*8(a1)
    ld   s7,   9*8(a1)
    ld   s8,  10*8(a1)
    ld   s9,  11*8(a1)
    ld   s10, 12*8(a1)
    ld   s11, 13*8(a1)
    ret                        # «возврат» в ra нового контекста

# Точка, куда «возвращается» новая задача при самом первом запуске.
# context_switch восстановил s0 = адрес функции задачи (мы так настроили Context).
.globl task_trampoline
task_trampoline:
    csrsi sstatus, 2           # SIE=1: задача должна быть вытесняема с самого старта
                               #        (мы попали сюда из обработчика таймера, где SIE=0)
    jalr s0                    # вызвать функцию задачи
    call task_exit             # если функция вернулась — завершить (не возвращается)
