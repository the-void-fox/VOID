#!/usr/bin/env python3
"""textrun.py — прогнать команды в ТЕКСТОВОМ шелле VOID и забрать serial-журнал (Веха 225.5).

Использование: textrun.py <образ.img> <каталог-выхода> <команда> [ещё команда…]

## Зачем отдельный инструмент

`screenrun.py` ведёт ОКОННЫЙ сеанс: мышь, аккорды, снимки кадра. Но зонды (`sysfuzz`, `greed`,
`lx-fuzz`) печатают сотни и тысячи строк, и в окне терминала это картинка, которую не прочесть
машиной. В текстовом режиме (`-vga none`) тот же вывод идёт в serial целиком — его и надо
грепать.

## Главное здесь — ОЖИДАНИЕ ПРИГЛАШЕНИЯ, а не задержка

**Шелл теряет ввод, пришедший пока он занят.** Это стоило двух ложно чистых прогонов подряд
(Веха 225.4): первый слал команды через треть секунды — в журнале остались обрывки `003` и
`elf009`, у команд съело начало; второй ждал своей метки и встал на первом же шаге, потому что
метку тоже съело.

Поэтому следующая строка уходит только тогда, когда шелл напечатал НОВОЕ приглашение. Это и
самоподстройка под скорость машины, и обнаружение зависания: не дождались — значит встали, и об
этом говорится прямо.

Ложно чистый прогон хуже упавшего: он закрывает вопрос, не ответив на него.
"""

import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
MACHINE_SH = os.path.join(HERE, "qemu-machine.sh")
# Приглашение vvsh кончается сбросом цвета и «> ». Считать его — надёжнее любой своей метки:
# саму метку надо ещё суметь послать, а приглашение печатает шелл сам.
PROMPT = "\x1b[0m> "
SHELL_UP = "— шелл VOID (ADR"


def stand(*args):
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не вышло собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    img, out, cmds = sys.argv[1], sys.argv[2], sys.argv[3:]
    os.makedirs(out, exist_ok=True)
    log_path = os.path.join(out, "serial.log")

    qemu = [
        "qemu-system-x86_64",
        *stand("machine", img, os.environ.get("VOID_QEMU_MEM", "1280M")),
        "-accel", "kvm" if os.access("/dev/kvm", os.W_OK) else "tcg",
        "-snapshot",
        *stand("net", "user", "", "", "", ""),
        # Без фреймбуфера композитор честно отказывается, и init поднимает шелл в serial.
        "-vga", "none", "-display", "none", "-serial", "stdio",
    ]
    log = open(log_path, "wb")
    p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)

    def text():
        return open(log_path, "rb").read().decode("utf-8", "replace")

    started = time.monotonic()
    while time.monotonic() - started < 180:
        if SHELL_UP in text():
            break
        time.sleep(0.5)
    else:
        p.kill()
        sys.exit("шелл не поднялся — смотри " + log_path)

    deadline = float(os.environ.get("VOID_TEXTRUN_DEADLINE", "300"))
    stalled = None
    for cmd in cmds:
        was = text().count(PROMPT)
        p.stdin.write((cmd + "\n").encode())
        p.stdin.flush()
        t0 = time.monotonic()
        while time.monotonic() - t0 < deadline:
            if text().count(PROMPT) > was:
                break
            time.sleep(0.1)
        else:
            stalled = cmd
            break
    time.sleep(1)
    p.kill()
    p.wait()

    body = text()
    print(f"журнал: {log_path} ({len(body.splitlines())} строк)")
    bad = [m for m in ("[PANIC]", "FATAL TRAP") if m in body]
    if bad:
        print(f"  ЯДРО НЕ ПЕРЕЖИЛО: {', '.join(bad)}")
        return 1
    if stalled is not None:
        print(f"  ШЕЛЛ НЕ ОТВЕТИЛ после «{stalled}» за {deadline:.0f} с — зависание")
        return 1
    print("  ядро живо, шелл ответил на все команды")
    return 0


if __name__ == "__main__":
    sys.exit(main())
