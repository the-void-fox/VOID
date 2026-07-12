# Переключение контекстов ядерных задач, x86_64 (Веха 25). Аналог riscv64/switch.s.
#
# Кооперативная модель: сохраняются только callee-saved (rbx, rbp, r12–r15) и rsp;
# адрес возврата НЕ хранится в структуре — он лежит на стеке задачи, и финальный `ret`
# возобновляет её сам (у новой задачи на дне стека заранее лежит адрес входа —
# см. Context::new_task/new_kernel). Раскладка структуры Context строго совпадает.
# Синтаксис Intel.

.section .text
.global context_switch
context_switch:                     # rdi = *old, rsi = *new
    mov [rdi + 0],  rsp
    mov [rdi + 8],  rbx
    mov [rdi + 16], rbp
    mov [rdi + 24], r12
    mov [rdi + 32], r13
    mov [rdi + 40], r14
    mov [rdi + 48], r15

    mov rsp, [rsi + 0]
    mov rbx, [rsi + 8]
    mov rbp, [rsi + 16]
    mov r12, [rsi + 24]
    mov r13, [rsi + 32]
    mov r14, [rsi + 40]
    mov r15, [rsi + 48]
    ret                             # прыжок по адресу с вершины стека новой задачи

# Первый запуск задачи планировщика: rbx = функция задачи (см. Context::new_task);
# по её возврату — штатное завершение через sched::task_exit.
.global x86_task_trampoline
x86_task_trampoline:
    call rbx
    call task_exit
