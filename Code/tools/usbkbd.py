#!/usr/bin/env python3
"""usbkbd.py — доходит ли ввод с USB-КЛАВИАТУРЫ до шелла (Веха 226.10).

Использование: usbkbd.py <ядро-elf|образ.img> <каталог-выхода>
Окружение: UT_TRACE=1 — положить рядом трассу QEMU по HID и переносам xHCI.

## Зачем отдельный стенд, когда есть `screenrun`

`screenrun` поднимает ОКОННЫЙ сеанс и снимает кадры. Для вопроса «доходит ли буква» это лишнее
и, главное, хрупкое: оболочка за сто с лишним вех менялась — аккорды, поколения, раскладка, — и
на старом ядре сценарий просто не выполняется. Здесь ничего этого нет: машина без видео
(`-vga none`), шелл говорит в serial, буквы набираются на USB-клавиатуре через QMP, проверка —
эхо в журнале. Такой опыт одинаково работает и на сегодняшнем ядре, и на ядре столетней
давности, поэтому им можно БИСЕКТИТЬ.

## Что он измерил в первый же день

Что поломка USB-клавиатуры (`known-gaps`, «события есть, а мы их не видим») **зависит от
режима, а не от версии**: без фреймбуфера вовсе — все четыре буквы доходят; в оконном сеансе —
доходят; и только полноэкранный `term` на фреймбуфере теряет всё, кроме первой.
"""

import json
import os
import socket
import subprocess
import sys
import time

img, out = sys.argv[1], sys.argv[2]
os.makedirs(out, exist_ok=True)
log = os.path.join(out, "serial.log")
sock = "/tmp/ut%d.sock" % os.getpid()

media = (["-kernel", img] if not img.endswith(".img")
         else ["-drive", f"if=none,id=d,file={img},format=raw",
               "-device", "ich9-ahci,id=a", "-device", "ide-hd,drive=d,bus=a.0", "-boot", "c"])
q = ["qemu-system-x86_64", "-machine", "q35,i8042=off", "-m", "1280M",
     "-accel", "kvm", "-cpu", "host", *media,
     "-device", "qemu-xhci,id=x", "-device", "usb-kbd,bus=x.0",
     "-device", "virtio-rng-pci,disable-legacy=on",
     "-vga", "none", "-display", "none",
     *( ["-trace", f"enable=hid_kbd_*,file={os.path.join(out,'t.log')}", "-trace", f"enable=usb_xhci_xfer_*,file={os.path.join(out,'t.log')}"] if os.environ.get("UT_TRACE") else [] ),
     "-serial", f"file:{log}", "-qmp", f"unix:{sock},server,nowait", "-no-reboot"]
p = subprocess.Popen(q, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)


def txt():
    try:
        return open(log, "rb").read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""


# Ждём ЛЮБОЕ приглашение шелла: за сто вех их писали по-разному, поэтому ищем набор признаков.
t = time.monotonic()
while time.monotonic() - t < 120:
    b = txt()
    if any(m in b for m in ("шелл VOID", "vsh", "vvsh", "> ")):
        break
    time.sleep(0.5)
time.sleep(3)  # дать оболочке дописать приглашение

c = socket.socket(socket.AF_UNIX)
c.connect(sock)
c.recv(4096)
c.sendall(b'{"execute":"qmp_capabilities"}\n')
c.recv(4096)
mark = txt()
for ch in "abcd":
    for down in (True, False):
        c.sendall(json.dumps({"execute": "input-send-event", "arguments": {"events": [
            {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": ch}}}]}}
        ).encode() + b"\n")
        c.recv(4096)
        time.sleep(0.25)
    time.sleep(1.2)
time.sleep(3)
p.kill()
p.wait()
os.unlink(sock)

new = txt()[len(mark):]
got = "".join(ch for ch in "abcd" if ch in new)
print(f"журнал: {log}")
print(f"набрано abcd → дошло: {got or '(ничего)'}  [{len(got)}/4]")
sys.exit(0 if len(got) == 4 else 1)
