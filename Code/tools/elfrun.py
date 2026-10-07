#!/usr/bin/env python3
"""elfrun.py — ОБСТРЕЛ ЗАГРУЗЧИКА ОБРАЗОВ: положить корпус в store и запустить каждый (Веха 225.4).

Использование: elfrun.py <образ.img> <каталог-корпуса> <каталог-выхода>

## Что проверяется

Загрузчик ELF работает ВНУТРИ ЯДРА: читает заголовки, ходит по программным заголовкам, отображает
сегменты. Паника там — смерть машины, а не отказ запуска. Корпус (`tools/elfcorpus.py`) — битые
образы: обрезки, `e_phnum = 0xffff`, `p_memsz` в терабайт, адрес сегмента В ЯДРЕ, точка входа в
higher-half.

**Находка — только смерть ядра** (`[PANIC]`, `FATAL TRAP`) или зависание. Отказ запуска
правильный ответ, и смерть САМОГО процесса тоже: образ, которому отображён мусор, вправе умереть
— ядро при этом обязано сказать «ПРОЦЕСС УБИТ (ядро живо)» и жить дальше.

## Почему прогон текстовый

Вывод шелла идёт в serial целиком, а его и надо читать: при гибели ядра последняя строка журнала
называет виновный образ. В оконном режиме вывод уехал бы в окно терминала, то есть в картинку.
"""

import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
IMPORT = os.path.join(HERE, "void-store-import")


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    img, corpus, out = sys.argv[1], sys.argv[2], sys.argv[3]
    os.makedirs(out, exist_ok=True)

    names = sorted(f for f in os.listdir(corpus) if f.startswith("elf"))
    if not names:
        sys.exit(f"корпус пуст: {corpus}")
    print(f"кладу в store {len(names)} образов…")
    for n in names:
        r = subprocess.run(
            ["cargo", "run", "--quiet", "--release", "--",
             os.path.abspath(img), "put", os.path.join(corpus, n), f"bin/x86_64/{n}"],
            cwd=IMPORT, capture_output=True, text=True,
        )
        if r.returncode != 0:
            sys.exit(f"не положить {n}: {r.stderr.strip()}")

    def stand(*a):
        r = subprocess.run(["bash", os.path.join(HERE, "qemu-machine.sh"), *a],
                           capture_output=True, text=True)
        if r.returncode:
            sys.exit(r.stderr)
        return r.stdout.splitlines()

    qemu = ["qemu-system-x86_64", *stand("machine", img, "1280M"), "-accel", "kvm", "-snapshot",
            *stand("net", "user", "", "", "", ""), "-vga", "none", "-display", "none",
            "-serial", "stdio"]
    log_path = os.path.join(out, "serial.log")
    log = open(log_path, "wb")
    p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)

    # Ждём приглашение, а не «сколько-нибудь секунд»: слать команды в пустоту значит получить
    # прогон, который ничего не проверил.
    started = time.monotonic()
    while time.monotonic() - started < 120:
        if "— шелл VOID (ADR" in open(log_path, "rb").read().decode("utf-8", "replace"):
            break
        time.sleep(0.5)
    else:
        p.kill()
        sys.exit("шелл не поднялся — смотри " + log_path)

    # Шлём по шагу и ЖДЁМ ОТВЕТА, а не «через треть секунды следующую».
    #
    # Первый заход слал всё подряд, и ввод терялся: в журнале остались обрывки `003` и `elf009`
    # — у команд съело начало. Прогон при этом рапортовал «находок нет», проверив на треть
    # меньше, чем назвал. Ложно чистый отчёт хуже упавшего: он закрывает вопрос, не ответив.
    #
    # Отметка перед запуском нужна по-прежнему: если машина встанет, последняя строка назовёт
    # виновный образ (тот же довод, что у `sysfuzz` и `lx-fuzz`).
    def text_now():
        return open(log_path, "rb").read().decode("utf-8", "replace")

    # Приглашение кончается сбросом цвета и «> ». Считать его — самый надёжный признак того,
    # что шелл освободился: своей метки для этого мало, потому что саму метку надо ещё суметь
    # послать.
    PROMPT = "\x1b[0m> "

    def step(cmd, deadline=25.0):
        """Послать строку и дождаться НОВОГО приглашения. `False` — не дождались."""
        was = text_now().count(PROMPT)
        p.stdin.write((cmd + "\n").encode())
        p.stdin.flush()
        started = time.monotonic()
        while time.monotonic() - started < deadline:
            if text_now().count(PROMPT) > was:
                return True
            time.sleep(0.05)
        return False

    # Почему по приглашению, а не «послал и жду метку»: второй заход слал `run` и `echo`
    # подряд, и у второй команды съедало НАЧАЛО (`echo ELFD` → `ONE000`). Шелл теряет ввод,
    # пришедший пока он занят, — значит слать следующее можно только когда он освободился.
    stalled = None
    for i, n in enumerate(names):
        if not step(f"run {n}"):
            stalled = n
            break
        if not step(f"echo ELFDONE{i:03d}"):
            stalled = n
            break
    if stalled is None:
        step("echo ELFRUN VERDICT SURVIVED")
    time.sleep(1)
    p.kill()
    p.wait()

    text = open(log_path, "rb").read().decode("utf-8", "replace")
    # Считаем ОТВЕТЫ шелла, а не посланные команды: посланное ничего не доказывает, а вот
    # `ELFDONE<N>` печатает сам шелл — значит он этот образ пережил и вернулся.
    done = sum(1 for i in range(len(names)) if f"ELFDONE{i:03d}" in text)
    lost = sum(1 for l in text.splitlines() if "команда не найдена" in l)
    killed = sum(1 for l in text.splitlines() if "ПРОЦЕСС УБИТ" in l)
    print(f"журнал: {log_path}")
    print(f"  образов пережито шеллом: {done} из {len(names)}")
    print(f"  процессов убито ядром (это ПРАВИЛЬНО): {killed}")
    if lost:
        print(f"  ВНИМАНИЕ: {lost} команд потеряно вводом — прогон неполон")
    bad = [m for m in ("[PANIC]", "FATAL TRAP") if m in text]
    if bad:
        print(f"  НАХОДКА: {', '.join(bad)} — виновен {stalled or names[done] if done < len(names) else '?'}")
        return 1
    if stalled is not None:
        print(f"  НАХОДКА: шелл не ответил после {stalled} — зависание")
        return 1
    if done < len(names) or lost:
        print("  прогон НЕПОЛОН — верить ему нельзя")
        return 1
    print("  ядро живо, шелл отвечает на всех — находок нет")
    return 0


if __name__ == "__main__":
    sys.exit(main())
