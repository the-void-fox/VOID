# Точка входа x86_64 (Веха 25) — PVH direct boot.
#
# QEMU умеет грузить ELF64-ядро напрямую (`-kernel`), если в PT_NOTE лежит
# XEN_ELFNOTE_PHYS32_ENTRY (18): машина стартует наш код в 32-битном protected mode
# без пейджинга (EBX = физ. адрес PVH start_info). Ни GRUB, ни ISO, ни внешних
# загрузчиков — тот же дух, что OpenSBI→ELF на RISC-V, только трамплин в long mode наш:
#   GDT → PAE → идентичные таблицы (2 МиБ страницы на первые 4 ГиБ) → EFER.LME|NXE →
#   CR0.PG|WP → далёкий прыжок в 64-битный сегмент → стек → kmain(0, start_info).
#
# Идентичные таблицы трамплина — ВРЕМЕННЫЕ (аналог «до paging::init» на RISC-V):
# kmain строит настоящие (arch::mm_init, W^X) и перещёлкивает CR3 (arch::mm_enable).
# Синтаксис Intel (по умолчанию для global_asm! на x86_64).

# ── PVH-нота (читает QEMU) ────────────────────────────────────────────────────
.section .note.Xen, "a", @note
.align 4
    .long 4                         # namesz = len("Xen\0")
    .long 4                         # descsz = 4 (32-битный адрес входа)
    .long 18                        # XEN_ELFNOTE_PHYS32_ENTRY
    .asciz "Xen"
    .long _start32

# ── 32-битный трамплин ────────────────────────────────────────────────────────
.section .text.entry
.code32
.global _start32
_start32:
    cli
    mov esi, ebx                    # PVH start_info → 2-й аргумент kmain (как dtb на RISC-V)

    # PVH ABI: esp НЕ определён — свой стек до первого push (retf ниже).
    mov esp, offset _boot_stack_top

    lgdt [gdt64_ptr]

    # PAE — обязательна для long mode.
    mov eax, cr4
    or eax, 1 << 5
    mov cr4, eax

    # Идентичные таблицы: pml4[0] → pdpt; pdpt[0..3] → 4 PD; PD — 2048 × 2 МиБ = 4 ГиБ.
    # Верхние половины записей нули: RAM QEMU обнулена, пишем только младшие dword'ы.
    mov edi, offset boot_pdpt
    or edi, 3                       # P | RW
    mov [boot_pml4], edi

    mov edi, offset boot_pdpt
    mov eax, offset boot_pd
    or eax, 3                       # P | RW
    mov ecx, 4
1:  mov [edi], eax
    add edi, 8
    add eax, 4096
    loop 1b

    mov edi, offset boot_pd
    mov eax, 0x83                   # P | RW | PS (2 МиБ)
    mov ecx, 2048
2:  mov [edi], eax
    add edi, 8
    add eax, 0x200000
    loop 2b

    mov eax, offset boot_pml4
    mov cr3, eax

    # EFER: LME (long mode) + NXE (бит NX в PTE — нужен W^X настоящих таблиц).
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr

    # CR0: PG (пейджинг) + WP (ядро уважает read-only страницы — половина W^X) + PE.
    mov eax, cr0
    or eax, 0x80010001
    mov cr0, eax

    # Далёкий «прыжок» сменой CS через retf: в стек CS:EIP, far return.
    # (`push offset …` LLVM собирает 16-битной релокацией — кладём через регистр.)
    push 0x08
    mov eax, offset _start64
    push eax
    retf

# ── 64-битный вход ───────────────────────────────────────────────────────────
.code64
_start64:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esi, esi                    # обнулить верхнюю половину rsi (после смены режима — мусор)
    lea rsp, [rip + _boot_stack_top]
    xor edi, edi                    # hartid = 0 (нет аналога — один процессор)
    call kmain
3:  hlt
    jmp 3b

# ── данные трамплина ─────────────────────────────────────────────────────────
.section .rodata
.align 8
gdt64:
    .quad 0                         # null
    .quad 0x00AF9A000000FFFF        # 0x08: код ring0, L=1 (64-бит)
    .quad 0x00CF92000000FFFF        # 0x10: данные ring0
gdt64_end:
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .long gdt64                     # 32-битный lgdt: word limit + dword base

.section .bss
.align 4096
boot_pml4:
    .space 4096
boot_pdpt:
    .space 4096
boot_pd:
    .space 4 * 4096                 # 4 PD × 512 записей × 2 МиБ = 4 ГиБ
.align 16
_boot_stack:
    .space 64 * 1024                # 64 КиБ загрузочного стека — как на RISC-V
_boot_stack_top:
