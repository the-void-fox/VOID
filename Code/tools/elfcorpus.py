#!/usr/bin/env python3
"""elfcorpus.py — КОРПУС ВРАЖДЕБНЫХ ОБРАЗОВ для обстрела загрузчика ELF (Веха 225.4).

Использование: elfcorpus.py <целый-elf> <каталог-выхода> [сколько]

## Зачем

Загрузчик образов разбирает ЧУЖИЕ БАЙТЫ ВНУТРИ ЯДРА: читает заголовки, ходит по программным
заголовкам и отображает сегменты. Паника там — смерть машины. При этом его не обстреливал никто:
`sysfuzz` бьёт по аргументам родных вызовов, `lx-fuzz` — по аргументам Linux, а до загрузчика
доходит только то, что уже лежит в store целым файлом.

А положить туда можно что угодно: `store:rwx` есть у шелла, и модель угроз (ADR 0023) прямо
называет это слабостью. То есть «битый образ в store» — не выдумка, а то, что бывает.

## Почему корпус делается ЗДЕСЬ, а не фаззером на хосте

Разбор заголовка можно было бы проверить и на хосте, но интересное начинается дальше: сегменты
ОТОБРАЖАЮТСЯ, и ошибка ждёт в паре «адрес + размер», а не в чтении полей. Проверять это можно
только там, где есть таблицы страниц, — то есть прогоном на VOID.

Поэтому здесь только ПОДГОТОВКА: взять целый образ, испортить его по-разному и разложить
файлами. Кладёт их в store и запускает — `tools/elfrun.py`.

## Что в корпусе

Половина — наводка руками по известным местам (их ломают в первую очередь во всяком загрузчике),
половина — случайные мутации заголовочной области. Руками — потому что слепая мутация редко
попадает в `e_phnum` и почти никогда не ставит `p_memsz` в терабайт.
"""

import os
import struct
import sys

# Смещения полей ELF64 — те, что ломают загрузчик, если им верить.
E_TYPE, E_MACHINE, E_ENTRY = 16, 18, 24
E_PHOFF, E_SHOFF = 32, 40
E_PHENTSIZE, E_PHNUM = 54, 56
E_SHENTSIZE, E_SHNUM = 58, 60
PH_TYPE, PH_FLAGS, PH_OFFSET, PH_VADDR = 0, 4, 8, 16
PH_FILESZ, PH_MEMSZ, PH_ALIGN = 32, 40, 48


def w64(b, off, v):
    b[off:off + 8] = struct.pack("<Q", v & 0xFFFF_FFFF_FFFF_FFFF)


def w32(b, off, v):
    b[off:off + 4] = struct.pack("<I", v & 0xFFFF_FFFF)


def w16(b, off, v):
    b[off:off + 2] = struct.pack("<H", v & 0xFFFF)


def handmade(src: bytes):
    """Наводка руками: каждый случай — известный способ обмануть загрузчик."""
    out = []

    def case(name, fn):
        b = bytearray(src)
        fn(b)
        out.append((name, bytes(b)))

    # Обрезки: половина ошибок разбора — чтение за концом.
    for n in (0, 1, 4, 15, 16, 63, 64, 0x40, 0x100):
        out.append((f"trunc{n}", src[:n]))

    case("phnum-max", lambda b: w16(b, E_PHNUM, 0xFFFF))
    case("phnum-0", lambda b: w16(b, E_PHNUM, 0))
    case("phoff-max", lambda b: w64(b, E_PHOFF, 0xFFFF_FFFF_FFFF_FFFF))
    case("phoff-past", lambda b: w64(b, E_PHOFF, len(src) + 8))
    case("phentsize-0", lambda b: w16(b, E_PHENTSIZE, 0))
    case("phentsize-max", lambda b: w16(b, E_PHENTSIZE, 0xFFFF))
    case("shnum-max", lambda b: w16(b, E_SHNUM, 0xFFFF))
    case("shoff-max", lambda b: w64(b, E_SHOFF, 0xFFFF_FFFF_FFFF_FFFF))
    case("entry-0", lambda b: w64(b, E_ENTRY, 0))
    # Точка входа В ЯДРЕ: если загрузчик ей поверит, процесс прыгнет в higher-half.
    case("entry-kernel", lambda b: w64(b, E_ENTRY, 0xFFFF_8000_0000_0000))
    case("entry-max", lambda b: w64(b, E_ENTRY, 0xFFFF_FFFF_FFFF_FFFF))
    case("type-0", lambda b: w16(b, E_TYPE, 0))
    case("machine-0", lambda b: w16(b, E_MACHINE, 0))

    # Программные заголовки: берём первый и ломаем его поля по очереди.
    phoff = struct.unpack_from("<Q", src, E_PHOFF)[0]
    if 0 < phoff < len(src) - 56:
        p = phoff

        def ph(name, off, size, val):
            def fn(b):
                if size == 8:
                    w64(b, p + off, val)
                elif size == 4:
                    w32(b, p + off, val)
            case(name, fn)

        ph("ph-memsz-tb", PH_MEMSZ, 8, 1 << 40)
        ph("ph-memsz-max", PH_MEMSZ, 8, 0xFFFF_FFFF_FFFF_FFFF)
        # filesz > memsz — копировать больше, чем отображено.
        ph("ph-filesz-max", PH_FILESZ, 8, 0xFFFF_FFFF_FFFF_FFFF)
        ph("ph-filesz-past", PH_FILESZ, 8, len(src) * 4)
        ph("ph-offset-max", PH_OFFSET, 8, 0xFFFF_FFFF_FFFF_FFFF)
        ph("ph-offset-past", PH_OFFSET, 8, len(src) + 1)
        # Адрес сегмента В ЯДРЕ и под началом области процесса.
        ph("ph-vaddr-kernel", PH_VADDR, 8, 0xFFFF_8000_0000_0000)
        ph("ph-vaddr-0", PH_VADDR, 8, 0)
        ph("ph-vaddr-low", PH_VADDR, 8, 0x1000)
        ph("ph-vaddr-max", PH_VADDR, 8, 0xFFFF_FFFF_FFFF_FFFF)
        ph("ph-align-0", PH_ALIGN, 8, 0)
        ph("ph-align-max", PH_ALIGN, 8, 0xFFFF_FFFF_FFFF_FFFF)
        ph("ph-type-max", PH_TYPE, 4, 0xFFFF_FFFF)
        ph("ph-flags-max", PH_FLAGS, 4, 0xFFFF_FFFF)
    return out


def mutations(src: bytes, n: int, seed: int):
    """Случайные правки ЗАГОЛОВОЧНОЙ области: там живёт всё, чему загрузчик верит."""
    state = seed | 1

    def rnd():
        nonlocal state
        x = state
        x ^= (x >> 12) & 0xFFFF_FFFF_FFFF_FFFF
        x ^= (x << 25) & 0xFFFF_FFFF_FFFF_FFFF
        x ^= (x >> 27) & 0xFFFF_FFFF_FFFF_FFFF
        state = x & 0xFFFF_FFFF_FFFF_FFFF
        return (state * 0x2545_F491_4F6C_DD1D) & 0xFFFF_FFFF_FFFF_FFFF

    head = min(len(src), 0x400)
    out = []
    for i in range(n):
        b = bytearray(src)
        for _ in range(1 + rnd() % 4):
            off = rnd() % head
            b[off] = rnd() & 0xFF
        out.append((f"mut{i}", bytes(b)))
    return out


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    src = open(sys.argv[1], "rb").read()
    outdir = sys.argv[2]
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    os.makedirs(outdir, exist_ok=True)
    cases = handmade(src) + mutations(src, count, 2026)
    for i, (name, data) in enumerate(cases):
        with open(os.path.join(outdir, f"elf{i:03d}"), "wb") as f:
            f.write(data)
    with open(os.path.join(outdir, "ИМЕНА.txt"), "w") as f:
        for i, (name, data) in enumerate(cases):
            f.write(f"elf{i:03d}\t{name}\t{len(data)} Б\n")
    print(f"корпус: {len(cases)} образов в {outdir} (имена — в ИМЕНА.txt)")


if __name__ == "__main__":
    main()
