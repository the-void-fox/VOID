#!/usr/bin/env python3
"""Прогон VOID с НАСТОЯЩИМ экраном + снимки кадра (Веха 114).

Путь `-kernel` фреймбуфера не даёт, поэтому грузимся с GRUB-образа; ввод идёт в serial,
а кадр снимается через монитор QEMU (`screendump`) и переводится в PNG без внешних зависимостей.

Зачем. Путь `cargo run` (-kernel, PVH) фреймбуфера НЕ даёт, поэтому графику VOID до сих пор
смотрели только на железе. Оказалось, достаточно грузиться с GRUB-образа (`Code/boot/mkdisk.sh`)
и снимать кадр монитором QEMU: получается настоящий 1280x800, который видно прямо в разработке.
Для графической фазы (ADR 0007) это основной инструмент проверки.

Сборка образа:
    nix-shell -p grub2 mtools util-linux --run \
      "Code/boot/mkdisk.sh Code/target/x86_64-unknown-none/release/void-kernel out.img 700"

Использование: screenrun.py <образ.img> <сценарий.txt> <каталог-выхода>
Строки сценария:
    sleep <сек>        — подождать
    key <строка>       — послать строку в консоль гостя (с переводом строки)
    shot <имя>         — снять кадр в <каталог-выхода>/<имя>.png
"""
import os, socket, struct, subprocess, sys, time, zlib

img, script, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(outdir, exist_ok=True)
# Путь к unix-сокету ограничен 108 байтами — каталог скратчпада длиннее, поэтому сокет
# кладём в /tmp с коротким именем по pid.
sock_path = f"/tmp/void-mon-{os.getpid()}.sock"
if os.path.exists(sock_path):
    os.unlink(sock_path)

qemu = [
    "qemu-system-x86_64", "-machine", "q35", "-m", "512M",
    "-device", "ich9-ahci,id=a",
    "-drive", f"if=none,id=d,file={img},format=raw",
    "-device", "ide-hd,drive=d,bus=a.0", "-boot", "c",
    "-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0,disable-legacy=on",
    "-device", "virtio-rng-pci,disable-legacy=on",
    "-display", "none",
    "-serial", "stdio",
    "-monitor", f"unix:{sock_path},server,nowait",
]
log = open(os.path.join(outdir, "serial.log"), "wb")
p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)

mon = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(50):
    try:
        mon.connect(sock_path)
        break
    except OSError:
        time.sleep(0.2)
else:
    p.kill()
    sys.exit("монитор QEMU не отвечает")


def monitor(cmd):
    mon.sendall((cmd + "\n").encode())
    time.sleep(0.6)
    try:
        mon.recv(65536)
    except OSError:
        pass


def ppm_to_png(src, dst):
    raw = open(src, "rb").read()
    h = raw.split(maxsplit=4)
    w, ht, mx = int(h[1]), int(h[2]), int(h[3])
    off = raw.index(bytes(str(mx), "ascii"), 2) + len(str(mx)) + 1
    px = raw[off:off + w * ht * 3]
    lines = b"".join(b"\x00" + px[y * w * 3:(y + 1) * w * 3] for y in range(ht))

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))

    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, ht, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(lines, 6))
           + chunk(b"IEND", b""))
    open(dst, "wb").write(png)
    os.unlink(src)
    return w, ht


try:
    for line in open(script, encoding="utf-8"):
        line = line.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        cmd, _, arg = line.partition(" ")
        if cmd == "sleep":
            time.sleep(float(arg))
        elif cmd == "key":
            p.stdin.write((arg + "\n").encode())
            p.stdin.flush()
        elif cmd == "shot":
            ppm = os.path.join(outdir, arg + ".ppm")
            monitor(f"screendump {ppm}")
            for _ in range(30):
                if os.path.exists(ppm) and os.path.getsize(ppm) > 1000:
                    break
                time.sleep(0.3)
            w, ht = ppm_to_png(ppm, os.path.join(outdir, arg + ".png"))
            print(f"снимок {arg}.png {w}×{ht}", flush=True)
        else:
            print("непонятная строка сценария:", line, file=sys.stderr)
finally:
    monitor("quit")
    time.sleep(1)
    if p.poll() is None:
        p.kill()
    log.close()
print("готово")
