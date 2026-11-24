# Веха 243.4 — ТРАМПЛИН ПРОБУЖДЕНИЯ из сна S3, с возвратом видеорежима.
#
# Прошивка будит процессор в РЕАЛЬНОМ РЕЖИМЕ и прыгает по адресу, который мы заранее положили в
# FACS. Это единственное место во всей системе, где нам снова доступен видео-BIOS: он лежит в
# памяти (она сон пережила), прерывание `int 0x10` работает, а таблица векторов цела.
#
# ## Зачем это вообще
#
# В S3 с графики снимается питание, и конвейер развёртки гаснет. Поставить режим заново значит
# поднять PLL, тайминги панели, канал к ней и её питание — в Linux на это уходят тысячи строк
# (`i915`), и своего такого у нас нет. Зато есть чужой, написанный производителем именно под эту
# панель: видео-BIOS. Тот же приём в Linux называется `acpi_sleep=s3_bios`.
#
# Поэтому здесь, ДО перехода в длинный режим:
#   1. (по просьбе) вызвать POST видео-BIOS — «подними графику как при включении»;
#   2. найти в списке VBE режим с нашими шириной, высотой и глубиной и поставить его с
#      линейным кадром.
#
# Порядок важен: сперва режим без POST. POST — средство тяжёлое (он заново инициализирует
# устройство целиком) и зовётся только если без него не вышло.
#
# ## Почему отдельный трамплин, а не тот, что у прикладных ядер
#
# Прикладное ядро поднимается СРЕДИ РАБОТЫ системы, и звать там видео-BIOS нельзя: экраном в
# этот момент владеет композитор. Пробуждение — другое дело: графики в этот миг нет ни у кого.
#
# Раскладка задана числами по той же причине, что в `ap.s`: код копируется по фиксированному
# физическому адресу, и ссылки внутри него считаются от него же.

.set S3_BASE,    0x9000
.set OFF_PROT,   0x180          # вход в 32-битный защищённый режим
.set OFF_LONG,   0x1E0          # вход в длинный режим
.set OFF_GDTR,   0x220
.set OFF_GDT,    0x230
.set OFF_CR3,    0x260
.set OFF_STACK,  0x268
.set OFF_ENTRY,  0x270
.set OFF_W,      0x278          # ширина, точек (u16)
.set OFF_H,      0x27A          # высота, точек (u16)
.set OFF_BPP,    0x27C          # бит на точку (u16)
.set OFF_FLAGS,  0x27E          # 1 — звать POST, 2 — ставить режим
.set OFF_TRY,    0x290          # номер попытки поиска: 0 наш режим, 1 режим загрузчика
.set OFF_POSTED, 0x298          # звали ли уже POST видео-BIOS в этом пробуждении
.set OFF_PCI,    0x29A          # где графика на шине: AH шина, AL устройство+функция
.set OFF_WHY,    0x29C          # чем кончился поиск: 2 ПЗУ молчит, 6 режима нет в списке
.set OFF_PTR,    0x292          # где стоим в списке режимов (смещение и сегмент)
.set OFF_MODE,   0x296          # номер режима, который сейчас разглядываем
.set OFF_W2,     0x28C          # ЗАПАСНОЙ режим: ширина (его ставил загрузчик)
.set OFF_H2,     0x28E          # запасной: высота
.set OFF_RESULT, 0x280          # что вышло: 1 наш режим, 3 режим загрузчика (+4 — после POST), 2 ПЗУ молчит, 6 режима нет
.set OFF_LFB,    0x284          # физический адрес кадра у найденного режима (u32)
.set OFF_PITCH,  0x288          # шаг строки найденного режима (u16)
.set OFF_VBE,    0x400          # сюда ложится информация VBE (512 байт)
.set OFF_MI,     0x600          # сюда — информация о режиме (256 байт)
.set OFF_END,    0x700

# Стек реального режима: ниже трамплина, в свободной низкой памяти. Своей страницы не хватит —
# видео-BIOS кладёт на стек изрядно.
.set RM_STACK,   0x7F00

.section .rodata.s3tramp, "a"
.code16
.global s3_tramp_start
s3_tramp_start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, RM_STACK

    # ── видеорежим ──────────────────────────────────────────────────────────
    mov ax, [S3_BASE + OFF_FLAGS]
    test ax, 2
    jz s3_video_done
    mov word ptr [S3_BASE + OFF_POSTED], 0

s3_set_mode:
    # VBE: общая информация (в ней — список номеров режимов).
    mov di, S3_BASE + OFF_VBE
    mov ax, 0x4F00
    int 0x10
    # Разница важна для разбора: «ПЗУ молчит» и «режима нет в списке» лечатся разным, а в
    # журнале до сих пор выглядели одинаково.
    mov word ptr [S3_BASE + OFF_WHY], 2
    cmp ax, 0x004F
    jne s3_video_fail
    # Ищем ДВАЖДЫ. Сперва режим, в котором система работает сейчас; если его в списке нет —
    # тот, что ставил ЗАГРУЗЧИК. Разница на живой машине решающая: панель у владельца
    # 1366×768, но это НАШ режим (на родной размер ядро переходит само, Веха 232), а
    # видео-BIOS знает только свои — там 1024×768.
    #
    # ВСЁ СОСТОЯНИЕ ПОИСКА — В ПАМЯТИ, и это не стиль, а цена ошибки. Регистры через `int 0x10`
    # не переживают НИЧЕГО: видео-BIOS волен портить любой, и первая версия с номером попытки
    # в `dx` подвесила машину прямо в трамплине — пробуждение не доходило даже до первой
    # строки журнала. В памяти держим и номер попытки, и место в списке режимов.
    mov word ptr [S3_BASE + OFF_TRY], 0

s3_try_list:
    # Начать список сначала: дальний указатель лежит в сведениях VBE по смещению 0x0E.
    mov ax, [S3_BASE + OFF_VBE + 0x0E]
    mov [S3_BASE + OFF_PTR], ax
    mov ax, [S3_BASE + OFF_VBE + 0x10]
    mov [S3_BASE + OFF_PTR + 2], ax

s3_next_mode:
    # Взять очередной номер режима (gs:si восстанавливаем из памяти каждый раз).
    mov ax, [S3_BASE + OFF_PTR + 2]
    mov gs, ax
    mov si, [S3_BASE + OFF_PTR]
    mov cx, gs:[si]
    add si, 2
    mov [S3_BASE + OFF_PTR], si
    cmp cx, 0xFFFF
    je s3_list_end
    mov [S3_BASE + OFF_MODE], cx
    mov di, S3_BASE + OFF_MI
    mov ax, 0x4F01
    int 0x10
    cmp ax, 0x004F
    jne s3_next_mode
    # Атрибуты: бит 0 — режим есть, бит 4 — графический, бит 7 — линейный кадр.
    mov ax, [S3_BASE + OFF_MI + 0x00]
    and ax, 0x0091
    cmp ax, 0x0091
    jne s3_next_mode
    # Ширину и высоту сравниваем с парой, на которую указывает номер попытки.
    mov bx, S3_BASE + OFF_W
    cmp word ptr [S3_BASE + OFF_TRY], 0
    je s3_have_pair
    mov bx, S3_BASE + OFF_W2
s3_have_pair:
    mov ax, [S3_BASE + OFF_MI + 0x12]
    cmp ax, [bx]
    jne s3_next_mode
    mov ax, [S3_BASE + OFF_MI + 0x14]
    cmp ax, [bx + 2]
    jne s3_next_mode
    xor ax, ax
    mov al, [S3_BASE + OFF_MI + 0x19]
    cmp ax, [S3_BASE + OFF_BPP]
    jne s3_next_mode
    # Нашли. Запомнить, что обещает режим, и поставить его с линейным кадром (бит 14) и
    # битом «не чистить память» (бит 15): в ней живой кадр, переживший сон.
    mov ax, [S3_BASE + OFF_MI + 0x10]
    mov [S3_BASE + OFF_PITCH], ax
    mov ax, [S3_BASE + OFF_MI + 0x28]
    mov [S3_BASE + OFF_LFB], ax
    mov ax, [S3_BASE + OFF_MI + 0x2A]
    mov [S3_BASE + OFF_LFB + 2], ax
    mov bx, [S3_BASE + OFF_MODE]
    or bx, 0xC000
    mov ax, 0x4F02
    int 0x10
    # 1 — поставлен НАШ режим, 3 — режим загрузчика (ядру придётся вернуть свой поверх него).
    mov ax, 1
    cmp word ptr [S3_BASE + OFF_TRY], 0
    je s3_posted_bit
    mov ax, 3
s3_posted_bit:
    # Старший бит ответа — «пришлось звать POST»: ядру это стоит сказать вслух, потому что
    # POST переинициализирует графику целиком, и всё, что мы о ней помним, после него чужое.
    cmp word ptr [S3_BASE + OFF_POSTED], 0
    je s3_say_result
    add ax, 4
s3_say_result:
    mov [S3_BASE + OFF_RESULT], ax
    jmp s3_video_done

s3_list_end:
    # Список кончился. Если искали свой режим — попробовать режим загрузчика.
    mov word ptr [S3_BASE + OFF_WHY], 6
    cmp word ptr [S3_BASE + OFF_TRY], 0
    jne s3_video_fail
    mov ax, [S3_BASE + OFF_W2]
    cmp ax, 0
    je s3_video_fail
    mov word ptr [S3_BASE + OFF_TRY], 1
    jmp s3_try_list

s3_video_fail:
    # Режима не нашлось НИ НАШЕГО, НИ ЗАГРУЗЧИКА. Второй заход: позвать POST видео-BIOS —
    # «подними графику как при включении» — и искать заново.
    #
    # Почему не с него: POST тяжёл, он заново инициализирует устройство целиком, и на машине,
    # где режим находится и так, звать его незачем. А на машине владельца без него не вышло
    # ничего: после сна питания у графики нет, и ПЗУ отвечает отказом на всякий вопрос о
    # режимах — ровно это и видно в журнале («не нашёл режима»).
    # ПО ПРОСЬБЕ, а не всегда: на X54C этот вызов ПОДВЕСИЛ машину наглухо — пробуждение не
    # дошло ни до одной строки журнала, и выключать пришлось удержанием кнопки. Поэтому POST
    # теперь зовётся только по явной просьбе: `suspend post` в шелле, отдельным опытом.
    test word ptr [S3_BASE + OFF_FLAGS], 1
    jz s3_video_gone
    cmp word ptr [S3_BASE + OFF_POSTED], 0
    jne s3_video_gone
    mov word ptr [S3_BASE + OFF_POSTED], 1
    # AX — ГДЕ на шине живёт эта графика: `AH` номер шины, `AL` устройство с функцией. Так
    # велит соглашение о точке входа ПЗУ устройства PCI, и это первое подозрение на причину
    # зависания: без него ПЗУ не знает, какую карту поднимать.
    mov ax, [S3_BASE + OFF_PCI]
    # Дальний вызов записан БАЙТАМИ: встроенный ассемблер в Intel-синтаксисе
    # `lcall сегмент:смещение` не принимает, а ошибка здесь не диагностируется ничем —
    # машина уходит в тройной сбой без единого слова. 0x9A — CALL ptr16:16.
    .byte 0x9A
    .short 0x0003
    .short 0xC000
    # ПОСЛЕ POST восстановить всё своё: сегменты и стек он волен испортить, как и регистры.
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, RM_STACK
    jmp s3_set_mode

s3_video_gone:
    mov ax, [S3_BASE + OFF_WHY]
    mov [S3_BASE + OFF_RESULT], ax

s3_video_done:
    # ── дальше как у прикладного ядра: 16 → 32 → 64 ─────────────────────────
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    lgdt [S3_BASE + OFF_GDTR]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    .byte 0x66, 0xEA
    .long S3_BASE + OFF_PROT
    .short 0x18

.org OFF_PROT
.code32
s3_prot:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov eax, cr4
    or eax, (1 << 5) | (1 << 9) | (1 << 10)
    mov cr4, eax
    mov eax, [S3_BASE + OFF_CR3]
    mov cr3, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr
    mov eax, cr0
    or eax, 0x80010001
    mov cr0, eax
    .byte 0xEA
    .long S3_BASE + OFF_LONG
    .short 0x08

.org OFF_LONG
.code64
s3_long:
    mov rsp, [S3_BASE + OFF_STACK]
    mov rax, [S3_BASE + OFF_ENTRY]
    jmp rax

.org OFF_GDTR
s3_gdtr:
    .short (4 * 8) - 1
    .long S3_BASE + OFF_GDT

.org OFF_GDT
# Номера совпадают с GDT ядра — по той же причине, что в `ap.s`: `CS`/`SS`, с которыми ядро
# вышло из трамплина, остаются в регистрах навсегда.
s3_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF        # 0x08: код 64 бита
    .quad 0x00CF92000000FFFF        # 0x10: данные
    .quad 0x00CF9A000000FFFF        # 0x18: код 32 бита

.org OFF_CR3
.global s3_cr3
s3_cr3:
    .long 0

.org OFF_STACK
.global s3_stack
s3_stack:
    .quad 0

.org OFF_ENTRY
.global s3_entry
s3_entry:
    .quad 0

.org OFF_W
.global s3_mode_w
s3_mode_w:
    .short 0
.global s3_mode_h
s3_mode_h:
    .short 0
.global s3_mode_bpp
s3_mode_bpp:
    .short 0
.global s3_mode_flags
s3_mode_flags:
    .short 0

.org OFF_RESULT
.global s3_mode_result
s3_mode_result:
    .short 0

.org OFF_LFB
.global s3_mode_lfb
s3_mode_lfb:
    .long 0

.org OFF_PITCH
.global s3_mode_pitch
s3_mode_pitch:
    .short 0

.org OFF_W2
.global s3_mode_w2
s3_mode_w2:
    .short 0

.org OFF_H2
.global s3_mode_h2
s3_mode_h2:
    .short 0

.org OFF_TRY
s3_try_no:
    .short 0

.org OFF_PTR
s3_list_ptr:
    .long 0

.org OFF_MODE
s3_mode_cur:
    .short 0

.org OFF_POSTED
s3_posted:
    .short 0

.org OFF_PCI
.global s3_video_bdf
s3_video_bdf:
    .short 0

.org OFF_WHY
s3_video_why:
    .short 0

.org OFF_END
.global s3_tramp_end
s3_tramp_end:
