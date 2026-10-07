#!/usr/bin/env python3
"""rvscreen.py — ЭКРАН RISCV: загрузить ядро riscv с virtio-gpu и снять кадр (Веха 224.1).

Использование: rvscreen.py <ядро-elf> <каталог-выхода> [секунд-ждать]

## Зачем отдельный инструмент, а не `screenrun.py`

У `screenrun.py` половина работы — ВВОД: клавиатура и мышь через QMP. На `virt` их подать
некуда: PS/2 там нет, а virtio-input (`virtio-keyboard-device`) VOID пока не умеет. Половина
инструмента на этой машине не заработала бы, а вторая половина — это тридцать строк.

Поэтому здесь ровно то, что на riscv возможно: загрузиться, подождать и снять кадр. Когда
появится virtio-input, этот файл исчезнет, а riscv уедет в `screenrun.py` целиком.

## Что это доказывает

До Вехи 224 на riscv не было экрана вовсе: QEMU `virt` не даёт ни VBE, ни тега от загрузчика, и
весь оконный мир VOID жил только на x86. virtio-gpu закрыла эту дыру тем же драйвером, что и
семантику показа на x86, — и снимок отсюда и есть доказательство, что композитор, панель и окна
работают на второй архитектуре.

Машина берётся из `tools/qemu-machine.sh` (`machine-riscv`) — тем же способом, что у `run.sh`,
`screenrun.py` и `smoke.py`: четвёртого описания стенда в проекте быть не должно.
"""

import json
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
MACHINE_SH = os.path.join(HERE, "qemu-machine.sh")
# Сокет QMP — в /tmp коротким именем. У UNIX-сокета путь не длиннее 108 байт, а каталоги
# прогонов бывают глубокими: первая же попытка положить его рядом с журналом кончилась
# «UNIX socket path is too long», и это не та ошибка, на которую хочется тратить заход дважды.
QMP_PATH = "/tmp/void-rvscreen-qmp.sock"


def stand(*args):
    """Кусок стенда из общего описания — по аргументу на строку."""
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не вышло собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


def qmp_call(f, cmd, **args):
    f.write(json.dumps({"execute": cmd, "arguments": args} if args else {"execute": cmd}) + "\n")
    f.flush()
    for _ in range(30):
        line = f.readline()
        if not line:
            return None
        msg = json.loads(line)
        if "return" in msg or "error" in msg:
            return msg
    return None


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    kernel, outdir = sys.argv[1], sys.argv[2]
    wait = float(sys.argv[3]) if len(sys.argv) > 3 else 90.0
    os.makedirs(outdir, exist_ok=True)
    if os.path.exists(QMP_PATH):
        os.unlink(QMP_PATH)

    # Диск нужен: store — это и есть система, без него загрузка кончается отказами. Разрежённый,
    # место занимает только записанное.
    disk = os.path.join(outdir, "rv-disk.img")
    if not os.path.exists(disk):
        subprocess.run(["truncate", "-s", "512M", disk], check=True)

    os.environ["VOID_QEMU_GPU"] = "1"  # без неё машина без экрана, и снимать будет нечего
    qemu = [
        "qemu-system-riscv64",
        *stand("machine-riscv", disk, os.environ.get("VOID_QEMU_MEM", "1024M")),
        "-display", "none",
        "-serial", "stdio",
        "-qmp", f"unix:{QMP_PATH},server,nowait",
        "-kernel", kernel,
    ]
    with open(os.path.join(outdir, "qemu-cmd.txt"), "w") as f:
        f.write(" ".join(qemu) + "\n")
    log_path = os.path.join(outdir, "serial.log")
    log = open(log_path, "wb")
    p = subprocess.Popen(qemu, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    for _ in range(60):
        try:
            sock.connect(QMP_PATH)
            break
        except OSError:
            time.sleep(0.3)
    else:
        p.kill()
        sys.exit("QMP не отвечает — QEMU не поднялся, смотри " + log_path)
    q = sock.makefile("rw", encoding="utf-8", newline="\n")
    q.readline()  # приветствие
    qmp_call(q, "qmp_capabilities")

    print(f"riscv + virtio-gpu: жду {wait:.0f} с подъёма сеанса…")
    time.sleep(wait)
    ppm = os.path.abspath(os.path.join(outdir, "screen.ppm"))
    res = qmp_call(q, "screendump", filename=ppm)
    time.sleep(1.0)
    p.kill()
    p.wait()
    if not res or "error" in res:
        sys.exit(f"снимок не вышел: {res}")
    print(f"снимок {ppm} ({os.path.getsize(ppm)} Б)")
    # PPM — это то, что отдаёт QEMU; в PNG его переводит кто угодно (`magick`), и тащить сюда
    # зависимость ради одного вызова незачем.
    print("в PNG:  magick", ppm, ppm.replace(".ppm", ".png"))
    # Отметку подъёма печатаем сами: иначе «снимок снят» ничего не говорит о том, что на нём.
    text = open(log_path, "rb").read().decode("utf-8", "replace")
    for mark in ("[gpu] virtio-gpu:", "[wm] жесты:"):
        line = next((l for l in text.splitlines() if mark in l), None)
        print(("  " + line.strip()) if line else f"  НЕТ отметки «{mark}»")
    return 0


if __name__ == "__main__":
    sys.exit(main())
