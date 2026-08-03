# Точка входа x86_64 (Веха 25) — PVH direct boot.
#
# QEMU умеет грузить ELF64-ядро напрямую (`-kernel`), если в PT_NOTE лежит
# XEN_ELFNOTE_PHYS32_ENTRY (18): машина стартует наш код в 32-битном protected mode
# без пейджинга (EBX = физ. адрес PVH start_info). Ни GRUB, ни ISO, ни внешних
# загрузчиков — тот же дух, что OpenSBI→ELF на RISC-V, только трамплин в long mode наш:
#   GDT → PAE → идентичные таблицы (2 МиБ страницы на первые 4 ГиБ) → EFER.LME|NXE →
#   CR0.PG|WP → далёкий прыжок в 64-битный сегмент → стек → kmain(0, start_info).
#
# Таблицы трамплина — ВРЕМЕННЫЕ (аналог «до paging::init» на RISC-V):
# kmain строит настоящие (arch::mm_init, W^X) и перещёлкивает CR3 (arch::mm_enable).
# Синтаксис Intel (по умолчанию для global_asm! на x86_64).
#
# Веха 87 — higher-half: ядро СЛИНКОВАНО по высоким адресам (окно образа
# 0xFFFF_FFFF_8000_0000 + физика, см. linker-x86_64.ld), а 32-битный трамплин
# исполняется по ФИЗИЧЕСКИМ. В отличие от riscv (medany, всё PC-относительно) x86
# в 32-битном режиме адресует символы абсолютно, поэтому КАЖДАЯ ссылка на символ
# до перехода в верхнюю половину пишется как `symbol - KVA` — то есть физически.
# Таблицы трамплина отображают память ТРИЖДЫ: identity (чтобы пережить включение
# пейджинга), direct-map с PAGE_OFFSET и окно образа — ровно те три вида адресов,
# которыми ядро пользуется дальше.
.set KVA, 0xFFFFFFFF80000000        # окно образа ядра (KIMAGE_BASE в mod.rs)

# ── PVH-нота (читает QEMU при `-kernel`) ──────────────────────────────────────
.section .note.Xen, "a", @note
.align 4
    .long 4                         # namesz = len("Xen\0")
    .long 4                         # descsz = 4 (32-битный адрес входа)
    .long 18                        # XEN_ELFNOTE_PHYS32_ENTRY
    .asciz "Xen"
    .long _start32_phys             # поле 32-битное — только физический адрес (linker-x86_64.ld)

# ── Multiboot2-заголовок (читает GRUB — путь для РЕАЛЬНОГО железа, Веха 41) ─────
# Одна ELF-сборка грузится и QEMU (PVH-нотой), и GRUB'ом (этим заголовком): на ноутбуке
# PVH недоступен, а GRUB есть. Почему MB2, а не MB1: QEMU `-kernel` понимает multiboot1 и
# грузил бы ИМ (а он 32-битный → отвергает наш ELF64); multiboot2 QEMU не знает → падает на
# PVH-ноту (cargo run цел), а GRUB грузит по MB2. Тип различаем по magic в eax: multiboot2
# кладёт 0x36D76289 (и mb2-инфо в ebx), PVH — start_info в ebx.
# Заголовок — в СВОЮ секцию .multiboot, которую линкер кладёт САМОЙ ПЕРВОЙ: GRUB ищет magic
# в первых 8 КиБ ФАЙЛА, а .text (с трамплином) выровнен на страницу и уезжает за границу.
.set MB2_MAGIC, 0xE85250D6
.set MB2_ARCH, 0                    # 0 = i386 (32-битный protected mode на входе)
.section .multiboot, "a"
.align 8
mb2_header:
    .long MB2_MAGIC
    .long MB2_ARCH
    .long mb2_header_end - mb2_header
    .long -(MB2_MAGIC + MB2_ARCH + (mb2_header_end - mb2_header))

    # Веха 87: тег АДРЕСОВ (type 2). Без него GRUB грузит ELF по p_vaddr, а у нас они
    # высокие (higher-half) — ядро уезжало в никуда и машина уходила в ребут. С тегом
    # GRUB игнорирует ELF-заголовки и кладёт файл СЫРЫМ по физическим адресам ниже.
    # Образ в файле непрерывен (paddr = file_offset + 0xFF000), поэтому это корректно.
    .align 8
    .short 2
    .short 0
    .long 24
    .long mb2_header - KVA          # header_addr: физ. адрес ЭТОГО заголовка
    .long _kernel_start - KVA       # load_addr: с какого физ. адреса лить файл
    .long _bss_start - KVA          # load_end_addr: конец «сырых» данных в файле
    .long _kernel_end - KVA         # bss_end_addr: досюда GRUB обнуляет (.bss)

    # Тег ТОЧКИ ВХОДА (type 3) — тоже физический: ELF entry GRUB здесь уже не смотрит.
    .align 8
    .short 3
    .short 0
    .long 12
    .long _start32 - KVA

    # Веха 96: тег ФРЕЙМБУФЕРА (type 5) — просим GRUB поставить ГРАФИЧЕСКИЙ режим и вернуть
    # линейный буфер инфо-тегом 8 (разбор — arch/x86_64/mod.rs, консоль — fb.rs). Причина не
    # косметическая: знакогенератор VGA держит 256 глифов, и настоящих шрифтов там быть не может.
    # Нули в width/height = «на усмотрение загрузчика» (панель/видео-BIOS решают сами), глубину
    # просим 32 бита — с ней проще всего писать пиксели.
    # flags bit0 = ОПЦИОНАЛЬНОСТЬ: если режим поставить нельзя, GRUB грузит нас всё равно (и мы
    # останемся на текстовом VGA), а не отказывается загружаться.
    .align 8
    .short 5
    .short 1
    .long 20
    .long 0                         # width  — без предпочтения
    .long 0                         # height — без предпочтения
    .long 32                        # depth (бит на пиксель)

    # обязательный завершающий тег (type 0, size 8)
    .align 8
    .short 0
    .short 0
    .long 8
mb2_header_end:

# ── 32-битный трамплин ────────────────────────────────────────────────────────
.section .text.entry
.code32
.global _start32
_start32:
    cli
    mov esi, ebx                    # info ptr (PVH start_info | multiboot info) → 2-й арг kmain
    mov [boot_magic - KVA], eax     # magic загрузки → глобал (edi клобберит пейджинг ниже)

    # PVH ABI: esp НЕ определён — свой стек до первого push (retf ниже).
    mov esp, offset _boot_stack_top - KVA

    lgdt [gdt64_ptr - KVA]

    # PAE — обязательна для long mode. OSFXSR|OSXMMEXCPT (Веха 36) — SSE для
    # userspace: без них любой SSE-опкод в ring3 даёт #UD (ядро само собрано
    # с soft-float и XMM не трогает; контекст процессов носит fxsave64-область
    # trap-кадра, см. trap.rs).
    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax

    # Таблицы: pdpt[0..3] → 4 PD; PD — 2048 × 2 МиБ = первые 4 ГиБ физической памяти.
    # Верхние половины записей нули: RAM QEMU обнулена, пишем только младшие dword'ы.
    #
    # Этот же PDPT вешаем в ДВА слота PML4:
    #   [0]   — identity (VA == PA): без него следующая инструкция после включения CR0.PG
    #           ушла бы в никуда;
    #   [256] — direct-map (VA == PA + PAGE_OFFSET = 0xFFFF_8000_0000_0000): слот покрывает
    #           512 ГиБ с её начала, и те же 4 ГиБ ложатся ровно куда надо.
    mov edi, offset boot_pdpt - KVA
    or edi, 3                       # P | RW
    mov [boot_pml4 - KVA], edi              # PML4[0]   — identity
    mov [boot_pml4 - KVA + 256*8], edi      # PML4[256] — direct-map

    # Окно ОБРАЗА ядра (0xFFFF_FFFF_8000_0000 = PML4[511], PDPT[510]) — отдельная ветка,
    # ведущая на первый PD (физические 0..1 ГиБ): образ ядра лежит там.
    mov edi, offset boot_pdpt_hi - KVA
    or edi, 3
    mov [boot_pml4 - KVA + 511*8], edi
    mov edi, offset boot_pd - KVA
    or edi, 3
    mov [boot_pdpt_hi - KVA + 510*8], edi

    mov edi, offset boot_pdpt - KVA
    mov eax, offset boot_pd - KVA
    or eax, 3                       # P | RW
    mov ecx, 4
1:  mov [edi], eax
    add edi, 8
    add eax, 4096
    loop 1b

    mov edi, offset boot_pd - KVA
    mov eax, 0x83                   # P | RW | PS (2 МиБ)
    mov ecx, 2048
2:  mov [edi], eax
    add edi, 8
    add eax, 0x200000
    loop 2b

    mov eax, offset boot_pml4 - KVA
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

    # Далёкий «прыжок» сменой CS через retf: в стек CS:EIP, far return. Целевой адрес
    # 32-битный, поэтому прыгаем в НИЗКИЙ 64-битный огрызок, а он уже уходит наверх.
    # (`push offset …` LLVM собирает 16-битной релокацией — кладём через регистр.)
    push 0x08
    mov eax, offset _start64_low - KVA
    push eax
    retf

# ── 64-битный вход ───────────────────────────────────────────────────────────
.code64
# Ещё по физическим адресам: единственный способ уйти в верхнюю половину — абсолютный
# 64-битный адрес (movabs даёт ЛИНКОВОЧНЫЙ, то есть высокий) и косвенный jmp.
_start64_low:
    movabs rax, offset _start64
    jmp rax

_start64:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esi, esi                    # обнулить верхнюю половину rsi (после смены режима — мусор)
    # Дальше PC высокий, и rip-относительная адресация сама даёт высокие адреса.
    lea rsp, [rip + _boot_stack_top]
    mov edi, [rip + boot_magic]     # 1-й арг kmain = magic загрузки (0x2BADB002 → multiboot)
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
    .long gdt64 - KVA               # 32-битный lgdt: word limit + dword base (физический!)

.section .bss
.align 8
boot_magic:                         # magic загрузки (eax при входе): 0x2BADB002 = multiboot
    .space 8
.align 4096
boot_pml4:
    .space 4096
boot_pdpt:
    .space 4096
boot_pdpt_hi:                       # ветка окна образа ядра (PML4[511])
    .space 4096
boot_pd:
    .space 4 * 4096                 # 4 PD × 512 записей × 2 МиБ = 4 ГиБ
.align 16
_boot_stack:
    .space 64 * 1024                # 64 КиБ загрузочного стека — как на RISC-V
_boot_stack_top:
