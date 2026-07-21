/* Вход C-программы VOID (Веха 36). Ядро приходит по e_entry (ELF-загрузчик,
 * kernel/src/elf.rs) со стеком, уже выставленным в trap-кадре, — как у Rust-программ
 * (`_start` там обычная extern "C" fn). C-миру перед С-кодом нужно чуть больше:
 *
 * - riscv64: установить gp = __global_pointer$ (символ даёт linker.ld) — gcc адресует
 *   .sdata относительно gp (linker relaxation); без gp любая глобальная переменная
 *   промахнётся. `.option norelax` обязателен: иначе линкер «срелаксирует» само
 *   вычисление gp через gp, которого ещё нет.
 * - обе: выровнять стек по 16 (ABI требует до первого call) и обнулить кадровый
 *   регистр — дно цепочки кадров для отладчика.
 *
 * Дальше — C (`_start_c` в void.c): куча, argv/env, init_array, main, exit.
 */
#if defined(__riscv)

    .section .text._start, "ax"
    .globl _start
_start:
.option push
.option norelax
    la   gp, __global_pointer$
.option pop
    andi sp, sp, -16
    li   fp, 0
    call _start_c
1:  j    1b                     /* _start_c не возвращается */

#elif defined(__x86_64__)

    .section .text._start, "ax"
    .globl _start
_start:
    and  $-16, %rsp
    xor  %rbp, %rbp
    call _start_c
1:  jmp  1b

    /* legacy-пара из crti/crtn (мы их не линкуем: -nostartfiles); newlib x86_64
     * зовёт _fini из __libc_fini_array — пустышек достаточно, конструкторы ходят
     * через .init_array. riscv-newlib собран без них вовсе. */
    .section .text._init, "ax"
    .globl _init
_init:
    ret
    .globl _fini
_fini:
    ret

#endif
