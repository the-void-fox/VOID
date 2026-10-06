#!/usr/bin/env python3
"""smoke.py — ДЫМОВОЙ ПРОГОН: загрузиться в QEMU и дождаться подъёма сеанса (Веха 223.12).

Использование: smoke.py <образ.img> [каталог-выхода]

## Зачем он, если есть CI

CI с Вехи 214.8 проверяет, что ядро СОБИРАЕТСЯ. Этого мало ровно в том месте, ради которого CI
и заводили: «SMP однажды отнял плавность так, что заметили не сразу». Собирается всё и у
сломанного ядра — оно просто не грузится. Самый тяжёлый класс регрессий (не стартует, падает на
подъёме, умирает композитор) сборкой не ловится вовсе, а ловится глазами владельца, то есть
случайно и поздно.

Дымовой прогон закрывает именно его: машина поднимается, сеанс встаёт, окна появляются. Это не
проверка поведения — это проверка того, что проверять вообще есть что.

## Почему именно serial, а не снимки экрана

Снимок — это картинка, и сравнивать его надо с эталоном: эталон стареет при каждой правке
оформления, а расхождение в один пиксель выглядит как поломка. Для CI это источник ложных
тревог, а красный CI, которому не верят, хуже отсутствующего (так и вышло с 30.09 по 06.10).

Строка в журнале — отметка однозначная: `[wm] сеанс поднят` печатает композитор ПОСЛЕ того, как
поднял окна сеанса. Она либо есть, либо нет.

## Чего здесь НЕТ и почему

Чисел `fps` и вообще любых замеров скорости. На общих runner'ах нет KVM, QEMU эмулирует каждую
инструкцию, и кадр композитора стоит сотни миллисекунд — «медленно» в таком прогоне не значит
ничего. Мерить скорость можно только на машине владельца (`fps`, см. [[void-fps-tool]]).

Стенд берётся из `tools/qemu-machine.sh` — тем же способом, что у `run.sh` и `screenrun.py`.
Своим списком устройств здесь завели бы ТРЕТЬЕ описание машины, и однажды дымовой прогон
проверял бы не ту машину, на которой работают.
"""

import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
MACHINE_SH = os.path.join(HERE, "qemu-machine.sh")

# Отметка подъёма: печатается композитором после того, как сеанс встал.
GOOD = "[wm] сеанс поднят"
# Отметки беды. `[PANIC]` — паника ядра; `ПРОЦЕСС УБИТ` — смертельный отказ страницы у процесса
# (так выглядела смерть композитора в Вехе 223.9). Обе печатаются только в этих случаях, поэтому
# ложной тревоги от них не бывает.
BAD = ("[PANIC]", "ПРОЦЕСС УБИТ")

# Сроки. С KVM сеанс встаёт за десяток секунд; без него QEMU эмулирует каждую инструкцию, и
# разница выходит кратной, а не процентной. Отсюда два разных срока, а не один с запасом: общий
# срок либо не дождался бы эмуляции, либо прощал бы зависание под KVM по десять минут.
DEADLINE_KVM = 90
DEADLINE_TCG = 600


def stand(*args):
    """Кусок стенда из общего описания — по аргументу на строку (как в `screenrun.py`)."""
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не вышло собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    img = sys.argv[1]
    outdir = sys.argv[2] if len(sys.argv) > 2 else "."
    os.makedirs(outdir, exist_ok=True)
    log_path = os.path.join(outdir, "smoke-serial.log")

    # `VOID_QEMU_ACCEL=tcg` — та же переменная, что у `run.sh`: ею проверяют, как прогон ведёт
    # себя БЕЗ ускорителя, не отбирая у машины /dev/kvm. Именно так гость поедет на runner'е.
    accel = os.environ.get("VOID_QEMU_ACCEL", "")
    kvm = accel != "tcg" and os.access("/dev/kvm", os.W_OK)
    deadline = DEADLINE_KVM if kvm else DEADLINE_TCG
    print(f"дымовой прогон: {img}, ускоритель {'kvm' if kvm else 'НЕТ (tcg)'}, срок {deadline} с")

    qemu = [
        "qemu-system-x86_64",
        *stand("machine", img, os.environ.get("VOID_QEMU_MEM", "1280M")),
        # Ускоритель называется ЯВНО в обоих случаях. Без ключа QEMU берёт свой список по
        # умолчанию, и он разный у разных сборок: на этой машине «прогон без KVM» молча шёл с
        # KVM и показывал те же две секунды, то есть не проверял ничего из задуманного.
        "-accel", "kvm" if kvm else "tcg",
        # Пишем во временный слой поверх образа: прогон не должен менять store, иначе второй
        # запуск идёт уже не с того поколения, что первый.
        "-snapshot",
        *stand("net", "user", "", "", "", ""),
        "-display", "none",
        "-serial", "stdio",
    ]
    log = open(log_path, "wb")
    p = subprocess.Popen(qemu, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)

    # Журнал читаем ОТДЕЛЬНЫМ дескриптором, а не трубой: труба заполнилась бы и остановила
    # гостя на середине загрузки, и прогон бы «завис» без всякой поломки в системе.
    seen = ""
    verdict = None
    with open(log_path, "rb") as tail:
        started = time.monotonic()
        while time.monotonic() - started < deadline:
            chunk = tail.read().decode("utf-8", "replace")
            if chunk:
                seen += chunk
                if any(b in seen for b in BAD):
                    verdict = next(b for b in BAD if b in seen)
                    break
                if GOOD in seen:
                    verdict = True
                    break
            if p.poll() is not None:
                # QEMU вышел сам — машина выключилась, не дойдя до сеанса.
                verdict = "QEMU завершился раньше подъёма сеанса"
                break
            time.sleep(0.25)
    p.kill()
    p.wait()

    took = int(time.monotonic() - started)
    if verdict is True:
        print(f"сеанс поднялся за {took} с — порядок")
        return 0
    # Хвост журнала — прямо в вывод CI: иначе отчёт говорит «упало» и не говорит, на чём.
    print(f"ПРОВАЛ: {verdict or f'за {deadline} с отметки «{GOOD}» не было'}")
    print(f"── хвост {log_path}:")
    for line in seen.splitlines()[-40:]:
        print("   " + line)
    return 1


if __name__ == "__main__":
    sys.exit(main())
