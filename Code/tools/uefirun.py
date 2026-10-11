#!/usr/bin/env python3
"""uefirun.py — загрузить VOID по UEFI в QEMU с прошивкой OVMF и забрать журнал (Веха 226).

Использование: uefirun.py <образ.iso|образ.img> <каталог-выхода> [--без-сеанса] [-- аргументы QEMU]

## Зачем отдельный стенд

`qemu-machine.sh` поднимает машину с BIOS от SeaBIOS — то есть ровно тот путь, который у нас
работает с Вехи 41. UEFI — ДРУГОЙ путь, и проверять его надо отдельно: прошивка иная, экран ей
даёт GOP, а не VBE, и легаси-области памяти (EBDA, `0xE0000`) у неё может не быть вовсе.

## Переменные прошивки — КОПИЕЙ, а не оригиналом

`OVMF_VARS.fd` — это энергонезависимая память машины: прошивка туда ПИШЕТ (порядок загрузки,
записи о носителях). Файл в nix-сторе только для чтения, и прошивка на этом молча встанет.
Поэтому копия на каждый прогон; заодно прогоны не наследуют друг другу состояние.

## Чего здесь не будет: живого железа

Проверка UEFI сделана в эмуляторе по решению владельца (11.10.2026). Это значит, что веха
закрывается с пометкой **«в QEMU работает, на железе НЕ проверено»** — и пометка не формальная:
QEMU отвечает мгновенно и всегда, а на железе те же циклы жгут секунды (см. заметку
`void-qemu-hides-bugs`). OVMF к тому же прошивка образцовая, а живые прошивки — нет.
"""

import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# Отметки, по которым видно, докуда дошло. Порядок — порядок загрузки, и это важнее списка:
# по последней достигнутой отметке сразу видно, где встало.
#
# Отметки взяты ИЗ ЖУРНАЛА удачного прогона, а не придуманы. Первая редакция их угадывала
# («память», «— шелл VOID») — и объявила «ВСТАЛО» о прогоне, который дошёл до композитора, панели
# и сети. Стенд, который врёт в обе стороны, хуже отсутствия стенда.
#
# Третий столбец — нужна ли отметка в режиме `--без-сеанса`. Разделение не для красоты: под
# эмуляцией инструкций (CI, где KVM нет) подъём ОКОННОГО сеанса не укладывается в разумный срок —
# по той же причине дымовой прогон там идёт в текстовом режиме. Но всё, что ломается ИМЕННО от
# UEFI, случается до композитора: конфиг, карта памяти, кадр от прошивки, ACPI и соседние ядра.
STAGES = [
    ("GRUB прочитал НАШ конфиг", "Booting `VOID'", True),
    ("ядро разобрало карту памяти", "карта RAM", True),
    ("ядро получило кадр от прошивки", "[видео] фреймбуфер", True),
    ("ACPI нашлась — ядра подняты", "[smp]  ядер", True),
    ("композитор поднялся", "[wm] композитор VOID", False),
    ("сеанс живой (жесты разобраны)", "[wm] жесты:", False),
]
BAD = ("[PANIC]", "FATAL TRAP", "ПРОЦЕСС УБИТ")

# Где прошивку кладут дистрибутивы. Списком, а не выражением внутри функции, — чтобы его можно
# было подменить в проверке: на NixOS ни одного из этих каталогов нет, и убедиться, что разбор
# имён верный, иначе нечем.
CANDIDATE_DIRS = (
    "/usr/share/OVMF",       # Debian/Ubuntu: OVMF_CODE_4M.fd + OVMF_VARS_4M.fd
    "/usr/share/ovmf",
    "/usr/share/ovmf/x64",   # Arch
    "/usr/share/edk2/ovmf",  # Fedora
    "/usr/share/edk2/x64",
    "/usr/share/qemu",
)


def firmware():
    """Найти прошивку: сперва то, что назвали, потом — где её кладут дистрибутивы.

    Зашивать путь нельзя: в nix-сторе он с хэшем, у Ubuntu это `OVMF_CODE_4M.fd`, у Fedora —
    `/usr/share/edk2/ovmf/`. Первая редакция требовала `VOID_OVMF` и упала в CI ровно на этом:
    пакет стоял, прошивка лежала, а стенд сказал «войди в nix-shell».

    Код и переменные берутся ПАРОЙ из одного каталога: `OVMF_CODE_4M.fd` и `OVMF_VARS.fd` имеют
    разный размер pflash, и смешать их — получить прошивку, которая не стартует.
    """
    named = os.environ.get("VOID_OVMF")
    if named and os.path.exists(named):
        v = os.environ.get("VOID_OVMF_VARS") or named.replace("_CODE", "_VARS")
        if os.path.exists(v):
            return named, v
    tried = []
    for d in CANDIDATE_DIRS:
        if not os.path.isdir(d):
            continue
        for c in sorted(os.listdir(d)):
            if not (c.startswith("OVMF_CODE") and c.endswith(".fd")):
                continue
            v = os.path.join(d, c.replace("_CODE", "_VARS"))
            tried.append(os.path.join(d, c))
            if os.path.exists(v):
                return os.path.join(d, c), v
    sys.exit(
        "прошивки UEFI не нашлось. Назови её в VOID_OVMF (+ VOID_OVMF_VARS) или поставь пакет:\n"
        "  NixOS: уже в shell.nix (OVMF)   ·   Debian/Ubuntu: apt install ovmf\n"
        + (f"  нашлось без пары переменных: {', '.join(tried)}\n" if tried else "")
    )


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    args = [a for a in sys.argv[1:] if a != "--без-сеанса"]
    # «Без сеанса» — не послабление, а другой вопрос: проверяем путь загрузки, а не оболочку.
    # Отметка про `[smp]  ядер` при этом обязательна, и именно она ловит дыру этой вехи.
    core_only = "--без-сеанса" in sys.argv
    stages = [s for s in STAGES if s[2] or not core_only]
    img, out = os.path.abspath(args[0]), args[1]
    extra = args[args.index("--") + 1:] if "--" in args else []
    os.makedirs(out, exist_ok=True)

    code, varsrc = firmware()
    print(f"прошивка: {code}")
    nvram = os.path.join(out, "OVMF_VARS.fd")
    shutil.copyfile(varsrc, nvram)
    os.chmod(nvram, 0o644)

    media = (["-cdrom", img, "-boot", "d"] if img.endswith(".iso")
             else ["-drive", f"file={img},format=raw,if=ide"])
    qemu = [
        "qemu-system-x86_64", "-machine", "q35",
        "-m", os.environ.get("VOID_QEMU_MEM", "1280M"),
        "-accel", "kvm" if os.access("/dev/kvm", os.W_OK) else "tcg",
        "-cpu", "host" if os.access("/dev/kvm", os.W_OK) else "qemu64",
        # Прошивка двумя файлами: код только для чтения, переменные — своей копией.
        "-drive", f"if=pflash,format=raw,readonly=on,file={code}",
        "-drive", f"if=pflash,format=raw,file={nvram}",
        *media,
        "-device", "virtio-rng-pci,disable-legacy=on",
        "-display", "none", "-serial", "stdio",
        *extra,
    ]
    log_path = os.path.join(out, "serial.log")
    with open(log_path, "wb") as log:
        p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=log,
                             stderr=subprocess.STDOUT)
        deadline = float(os.environ.get("VOID_UEFI_DEADLINE", "180"))
        started = time.monotonic()
        while time.monotonic() - started < deadline:
            body = open(log_path, "rb").read().decode("utf-8", "replace")
            if stages[-1][1] in body or any(m in body for m in BAD):
                break
            if p.poll() is not None:
                break
            time.sleep(0.5)
        time.sleep(1)
        p.kill()
        p.wait()

    body = open(log_path, "rb").read().decode("utf-8", "replace")
    print(f"журнал: {log_path} ({len(body.splitlines())} строк)")
    reached = -1
    for i, (name, mark, _) in enumerate(stages):
        got = mark in body
        print(f"  [{'да ' if got else 'НЕТ'}] {name}")
        if got:
            reached = i
    bad = [m for m in BAD if m in body]
    if bad:
        print(f"  ЯДРО НЕ ПЕРЕЖИЛО: {', '.join(bad)}")
        return 1
    if reached < len(stages) - 1:
        # Последние строки — самое ценное в неудачном прогоне: на них всё и встало.
        tail = [l for l in body.splitlines() if l.strip()][-6:]
        print("  ВСТАЛО. Последние строки журнала:")
        for l in tail:
            print(f"    {l[:150]}")
        return 1
    print("  загрузка по UEFI дошла до " + ("ядер" if core_only else "сеанса"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
