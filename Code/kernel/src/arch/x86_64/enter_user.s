# Вход в процесс x86_64 (Веха 26) — аналог riscv64/enter_user.s.
#   rdi = *TrapFrame (подготовленная копия: cs/ss/rflags уже выставлены Rust-стороной,
#   см. mod.rs::enter_user; CR3 и TSS.rsp0 переключены там же).
#
# Кадр iretq строится на ТЕКУЩЕМ ядерном стеке (мы на trap-стеке; следующий трап из U
# начнёт его заново с TSS.rsp0 — живых кадров под нами к тому моменту не будет).
# rdi восстанавливаем последним — он наша база (как a0/x10 на RISC-V).
# Синтаксис Intel; раскладку TrapFrame см. в trap.rs (regs[15], vector, err, rip, cs,
# rflags, rsp, ss — слоты 0..21).

.section .text
.global x86_enter_user
x86_enter_user:
    push qword ptr [rdi + 21*8]     # ss  (ring3)
    push qword ptr [rdi + 20*8]     # rsp процесса
    push qword ptr [rdi + 19*8]     # rflags (IF=1 — прерывания в U включены)
    push qword ptr [rdi + 18*8]     # cs  (ring3)
    push qword ptr [rdi + 17*8]     # rip (вход/продолжение)

    mov r15, [rdi + 0*8]
    mov r14, [rdi + 1*8]
    mov r13, [rdi + 2*8]
    mov r12, [rdi + 3*8]
    mov r11, [rdi + 4*8]
    mov r10, [rdi + 5*8]
    mov r9,  [rdi + 6*8]
    mov r8,  [rdi + 7*8]
    mov rsi, [rdi + 9*8]
    mov rbp, [rdi + 10*8]
    mov rbx, [rdi + 11*8]
    mov rdx, [rdi + 12*8]
    mov rcx, [rdi + 13*8]
    mov rax, [rdi + 14*8]
    mov rdi, [rdi + 8*8]            # последним — база больше не нужна

    iretq                           # в ring3 на rip с восстановленным rsp
