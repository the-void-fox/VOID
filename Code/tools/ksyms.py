#!/usr/bin/env python3
"""Разрешить адреса ядра в имена функций (Веха 126.4).

Аварийный дамп ядра печатает голые адреса: таблицы символов на целевой машине нет и быть
не должно. Здесь она есть — в собранном ELF, — и этот скрипт превращает обратный след из
`FATAL TRAP` в цепочку имён.

    python3 Code/tools/ksyms.py 0xffffffff8010abcd ...   # адреса аргументами
    ... | python3 Code/tools/ksyms.py                    # или весь лог на вход

Во втором виде из текста выбираются все шестнадцатеричные адреса ядра — можно просто
вставить кусок serial-лога целиком.

Ядро линкуется по фиксированному адресу (KASLR нет), поэтому адреса из дампа сравнимы с
символами ELF напрямую. ВАЖНО: разрешать надо тем самым образом, который упал, — после
пересборки адреса сдвигаются, и имена станут враньём.
"""

import re
import struct
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_KERNEL = ROOT / "target/x86_64-unknown-none/release/void-kernel"


def read_symbols(path):
    """(адрес, размер, имя) всех символов ELF64 — из .symtab, иначе .dynsym."""
    f = path.read_bytes()
    if f[:4] != b"\x7fELF":
        sys.exit(f"не ELF: {path}")
    (e_shoff,) = struct.unpack_from("<Q", f, 0x28)
    e_shentsize, e_shnum = struct.unpack_from("<HH", f, 0x3A)

    secs = []
    for i in range(e_shnum):
        o = e_shoff + i * e_shentsize
        _, typ, _, _, off, size, link, _, _, entsize = struct.unpack_from("<IIQQQQIIQQ", f, o)
        secs.append((typ, off, size, link, entsize))

    syms = []
    for typ, off, size, link, entsize in secs:
        if typ not in (2, 11) or not entsize:  # SHT_SYMTAB / SHT_DYNSYM
            continue
        _, stroff, _, _, _ = secs[link]
        for i in range(size // entsize):
            o = off + i * entsize
            nm, _, _, _, val, sz = struct.unpack_from("<IBBHQQ", f, o)
            if not nm or not val:
                continue
            s = stroff + nm
            name = f[s : f.index(b"\0", s)].decode(errors="replace")
            syms.append((val, sz, name))
    return sorted(syms)


def demangle(name):
    """Грубое разворачивание v0-манглинга Rust: вытащить сегменты пути.

    Полный разбор v0 здесь не нужен и был бы лишним риском ошибиться: для чтения
    обратного следа хватает пути `крейт::модуль::функция`. Хэш крейта (`Cs<...>_`)
    выбрасываем — он только мешает читать.
    """
    if not name.startswith("_R"):
        return name
    # Хэш крейта `Cs<base62>_` — не сегмент пути: без этого шага его куски попадают в имя
    # мусорными «модулями» (`F01SBRE9::_11vo::…`).
    name = re.sub(r"Cs[0-9A-Za-z]{4,}_", "C", name)
    parts, i = [], 2
    while i < len(name):
        if not name[i].isdigit():
            i += 1
            continue
        j = i
        while j < len(name) and name[j].isdigit():
            j += 1
        n = int(name[i:j])
        seg = name[j : j + n]
        i = j + n
        if seg and not seg.startswith("Cs") and re.fullmatch(r"[A-Za-z0-9_.]+", seg):
            parts.append(seg)
    return "::".join(parts) if parts else name


def resolve(syms, addr):
    """Символ, содержащий адрес: последний с началом <= addr (и, если размер известен, накрывающий его)."""
    lo, hi = 0, len(syms)
    while lo < hi:
        mid = (lo + hi) // 2
        if syms[mid][0] <= addr:
            lo = mid + 1
        else:
            hi = mid
    if lo == 0:
        return None
    val, sz, name = syms[lo - 1]
    if sz and addr >= val + sz:
        return None  # адрес в дырке между символами — врать не будем
    return name, addr - val


def main():
    args = sys.argv[1:]
    kernel = DEFAULT_KERNEL
    if args and not args[0].lower().startswith("0x") and Path(args[0]).exists():
        kernel = Path(args[0])
        args = args[1:]
    if not kernel.exists():
        sys.exit(f"нет образа ядра: {kernel}\nсоберите ядро или укажите путь первым аргументом")

    text = " ".join(args) if args else sys.stdin.read()
    addrs = [int(a, 16) for a in re.findall(r"0x[0-9a-fA-F]{6,16}", text)]
    addrs = [a for a in addrs if a >= 0xFFFFFFFF80000000]
    if not addrs:
        sys.exit("адресов ядра во входных данных нет")

    syms = read_symbols(kernel)
    print(f"# {kernel}  ({len(syms)} символов)\n")
    for a in addrs:
        hit = resolve(syms, a)
        if hit:
            name, off = hit
            print(f"{a:#018x}  {demangle(name)}+{off:#x}")
        else:
            print(f"{a:#018x}  ?")


if __name__ == "__main__":
    main()
