# Точка входа ядра. OpenSBI прыгает сюда в S-mode:
#   a0 = hartid, a1 = указатель на device tree (DTB).
# Сохраняем a0/a1 нетронутыми и передаём их в kmain.
.section .text.entry
.global _start
_start:
    # На многоядерной системе паркуем все харты, кроме hart 0.
    bnez    a0, park

    # Указатель стека.
    la      sp, _stack_top

    # Обнулить .bss (границы выровнены по 8 в linker.ld).
    la      t0, _bss_start
    la      t1, _bss_end
1:
    bgeu    t0, t1, 2f
    sd      zero, 0(t0)
    addi    t0, t0, 8
    j       1b
2:
    # a0 (hartid), a1 (dtb) всё ещё в регистрах аргументов.
    call    kmain

park:
    wfi
    j       park
