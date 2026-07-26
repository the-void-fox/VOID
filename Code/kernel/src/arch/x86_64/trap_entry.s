# Стабы trap'ов x86_64 (Веха 25). Аналог riscv64/trap_entry.s.
#
# Аппаратно в 64-битном режиме CPU кладёт на стек ss:rsp, rflags, cs, rip (+ err для части
# исключений). Стаб доводит кадр до единой формы TrapFrame: подкладывает err=0, если его
# нет, кладёт номер вектора и все 15 GPR (порядок = раскладка структуры), затем зовёт
# x86_trap_handler(rdi = &TrapFrame). Обратный путь — симметричный pop + iretq.
# Синтаксис Intel.

.macro TRAP_STUB vec, has_err
trap_stub_\vec:
.if \has_err == 0
    push 0                          # выровнять кадр: err всегда присутствует
.endif
    push \vec
    jmp trap_common
.endm

.section .text

# Исключения 0–31. Код ошибки аппаратно кладут: 8, 10–14, 17, 21.
TRAP_STUB 0, 0
TRAP_STUB 1, 0
TRAP_STUB 2, 0
TRAP_STUB 3, 0
TRAP_STUB 4, 0
TRAP_STUB 5, 0
TRAP_STUB 6, 0
TRAP_STUB 7, 0
TRAP_STUB 8, 1
TRAP_STUB 9, 0
TRAP_STUB 10, 1
TRAP_STUB 11, 1
TRAP_STUB 12, 1
TRAP_STUB 13, 1
TRAP_STUB 14, 1
TRAP_STUB 15, 0
TRAP_STUB 16, 0
TRAP_STUB 17, 1
TRAP_STUB 18, 0
TRAP_STUB 19, 0
TRAP_STUB 20, 0
TRAP_STUB 21, 1
TRAP_STUB 22, 0
TRAP_STUB 23, 0
TRAP_STUB 24, 0
TRAP_STUB 25, 0
TRAP_STUB 26, 0
TRAP_STUB 27, 0
TRAP_STUB 28, 0
TRAP_STUB 29, 0
TRAP_STUB 30, 0
TRAP_STUB 31, 0
TRAP_STUB 32, 0                     # LAPIC-таймер
TRAP_STUB 33, 0                     # консоль: IOAPIC GSI4 (Веха 27)
TRAP_STUB 34, 0                     # диск: MSI-X virtio-blk (Веха 27)
TRAP_STUB 35, 0                     # userspace-драйвер: IOAPIC IRQ устройства (Веха 52)
TRAP_STUB 255, 0                    # spurious
TRAP_STUB 128, 0                    # int 0x80 — syscall (шлюз DPL=3, Веха 26)

trap_common:
    push rax                        # regs[14]
    push rcx                        # regs[13]
    push rdx                        # regs[12]
    push rbx                        # regs[11]
    push rbp                        # regs[10]
    push rsi                        # regs[9]
    push rdi                        # regs[8]
    push r8                         # regs[7]
    push r9                         # regs[6]
    push r10                        # regs[5]
    push r11                        # regs[4]
    push r12                        # regs[3]
    push r13                        # regs[2]
    push r14                        # regs[1]
    push r15                        # regs[0]
    mov rdi, rsp                    # &TrapFrame
    call x86_trap_handler
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rbp
    pop rbx
    pop rdx
    pop rcx
    pop rax
    add rsp, 16                     # vector + err
    iretq

# Таблица адресов стабов для заполнения IDT из Rust ([0..=34] + spurious + syscall).
.section .rodata
.align 8
.global TRAP_STUBS
TRAP_STUBS:
    .quad trap_stub_0
    .quad trap_stub_1
    .quad trap_stub_2
    .quad trap_stub_3
    .quad trap_stub_4
    .quad trap_stub_5
    .quad trap_stub_6
    .quad trap_stub_7
    .quad trap_stub_8
    .quad trap_stub_9
    .quad trap_stub_10
    .quad trap_stub_11
    .quad trap_stub_12
    .quad trap_stub_13
    .quad trap_stub_14
    .quad trap_stub_15
    .quad trap_stub_16
    .quad trap_stub_17
    .quad trap_stub_18
    .quad trap_stub_19
    .quad trap_stub_20
    .quad trap_stub_21
    .quad trap_stub_22
    .quad trap_stub_23
    .quad trap_stub_24
    .quad trap_stub_25
    .quad trap_stub_26
    .quad trap_stub_27
    .quad trap_stub_28
    .quad trap_stub_29
    .quad trap_stub_30
    .quad trap_stub_31
    .quad trap_stub_32
    .quad trap_stub_33
    .quad trap_stub_34
    .quad trap_stub_35
    .quad trap_stub_255
    .quad trap_stub_128
