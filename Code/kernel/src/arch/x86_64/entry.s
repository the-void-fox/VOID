# Точка входа x86_64 — заглушка Вехи 24: стек → kmain(0, 0) → hlt.
# Настоящая загрузка (Limine: long mode, higher-half, память из карты загрузчика) — Вехи 25+.
# Синтаксис Intel (по умолчанию для global_asm! на x86_64).

.section .text.entry
.global _start
_start:
    cli
    lea rsp, [rip + _boot_stack_top]
    xor edi, edi                    # kmain(hartid = 0, …)
    xor esi, esi                    # kmain(…, boot_info = 0)
    call kmain
1:  hlt
    jmp 1b

.section .bss
.align 16
_boot_stack:
    .space 64 * 1024                # 64 КиБ загрузочного стека — как на RISC-V
_boot_stack_top:
