#!/usr/bin/env python3
r"""Прогон VOID с НАСТОЯЩИМ экраном: снимки кадра, клавиатура и мышь (Вехи 114–115).

Зачем. Путь `cargo run` (-kernel, PVH) фреймбуфера НЕ даёт, поэтому графику VOID до сих пор
смотрели только на железе. Оказалось, достаточно грузиться с GRUB-образа (`Code/boot/mkdisk.sh`)
и снимать кадр из QEMU: получается настоящий 1280x800, который видно прямо в разработке.
Для графической фазы (ADR 0007/0016) это основной инструмент проверки.

Управление идёт по **QMP**, а не по человеческому монитору: HMP-команды `sendkey`/`mouse_move`
в QEMU 11 до гостя не доходят (проверено — 8042 не получает ни байта), а `input-send-event`
доходит. Текст в консоль по-прежнему шлём в serial: это ввод гостя, а не событие устройства.

Сборка образа:
    nix-shell -p grub2 mtools util-linux --run \\
      "Code/boot/mkdisk.sh Code/target/x86_64-unknown-none/release/void-kernel out.img 700"

Использование: screenrun.py <образ.img> <сценарий.txt> <каталог-выхода>
Строки сценария:
    sleep <сек>        — подождать
    key <строка>       — послать строку в консоль гостя (serial, с переводом строки)
    raw <байты>        — то же БЕЗ перевода строки; \e = Esc (для CSI: raw \e[5;2~)
    mouse <dx> <dy>    — подвинуть мышь (относительное событие)
    click <кнопка>     — нажать и отпустить (left / right / middle)
    btn <кнопка> <down|up> — держать/отпустить (для перетаскивания и снимков «нажато»)
    hotkey <аккорд>    — аккорд клавиатуры PS/2, например: hotkey Super+Return
    shot <имя>         — снять кадр в <каталог-выхода>/<имя>.png
"""
import json, os, socket, struct, subprocess, sys, time, zlib

img, script, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(outdir, exist_ok=True)
# Путь unix-сокета ограничен 108 байтами, а каталоги сборки длиннее — держим его в /tmp.
qmp_path = f"/tmp/void-qmp-{os.getpid()}.sock"
if os.path.exists(qmp_path):
    os.unlink(qmp_path)

qemu = [
    "qemu-system-x86_64", "-machine", "q35", "-m", "512M",
    "-device", "ich9-ahci,id=a",
    "-drive", f"if=none,id=d,file={img},format=raw",
    "-device", "ide-hd,drive=d,bus=a.0", "-boot", "c",
    "-netdev", "user,id=net0", "-device", "virtio-net-pci,netdev=net0,disable-legacy=on",
    "-device", "virtio-rng-pci,disable-legacy=on",
    "-display", "none",
    "-serial", "stdio",
    "-qmp", f"unix:{qmp_path},server,nowait",
]
log = open(os.path.join(outdir, "serial.log"), "wb")
p = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
for _ in range(50):
    try:
        sock.connect(qmp_path)
        break
    except OSError:
        time.sleep(0.2)
else:
    p.kill()
    sys.exit("QMP не отвечает")
qmp = sock.makefile("rw", encoding="utf-8", newline="\n")
qmp.readline()  # приветствие


def call(execute, **args):
    """Одна команда QMP. События (они приходят без `return`) пропускаем."""
    qmp.write(json.dumps({"execute": execute, "arguments": args} if args
                         else {"execute": execute}) + "\n")
    qmp.flush()
    for _ in range(20):
        line = qmp.readline()
        if not line:
            return None
        reply = json.loads(line)
        if "event" not in reply:
            return reply
    return None


call("qmp_capabilities")


def rel(axis, value):
    return {"type": "rel", "data": {"axis": axis, "value": value}}


# Имена модификаторов и особых клавиш → qcode QEMU. Печатные буквы совпадают сами с собой.
QCODE = {
    "Super": "meta_l", "Mod": "meta_l", "Shift": "shift", "Ctrl": "ctrl", "Alt": "alt",
    "Return": "ret", "Enter": "ret", "Tab": "tab", "Escape": "esc", "Space": "spc",
    "Left": "left", "Right": "right", "Up": "up", "Down": "down",
    "PageUp": "pgup", "PageDown": "pgdn", "Home": "home", "End": "end",
    # Знаки препинания у QEMU называются словами, а не символами: `[` это `bracket_left`.
    "BracketLeft": "bracket_left", "BracketRight": "bracket_right",
    "Equal": "equal", "Minus": "minus",
}


def hotkey(combo):
    """Аккорд как настоящая клавиатура: модификаторы зажимаются и отпускаются вокруг клавиши."""
    parts = combo.split("+")
    mods = [QCODE[p] for p in parts[:-1]]
    last = parts[-1]
    key = QCODE.get(last, last.lower())
    ev = lambda k, d: {"type": "key", "data": {"down": d, "key": {"type": "qcode", "data": k}}}
    events = [ev(m, True) for m in mods] + [ev(key, True), ev(key, False)]
    events += [ev(m, False) for m in reversed(mods)]
    call("input-send-event", events=events)


def unescape(s):
    r"""Строка сценария → байты: `\e` — Esc, `\xNN` — любой байт (Веха 120: аккорды Ctrl
    редактора приходят по serial одним управляющим байтом, `\x13` = ^S)."""
    out = bytearray()
    i = 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            if s[i + 1] == "e":
                out.append(0x1B)
                i += 2
                continue
            if s[i + 1] == "x" and i + 3 < len(s):
                out.append(int(s[i + 2:i + 4], 16))
                i += 4
                continue
        out += s[i].encode()
        i += 1
    return bytes(out)


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
        elif cmd == "raw":
            p.stdin.write(unescape(arg))
            p.stdin.flush()
        elif cmd == "mouse":
            dx, dy = arg.split()
            call("input-send-event", events=[rel("x", int(dx)), rel("y", int(dy))])
        elif cmd == "click":
            btn = arg.strip() or "left"
            call("input-send-event", events=[{"type": "btn", "data": {"down": True, "button": btn}}])
            time.sleep(0.3)
            call("input-send-event", events=[{"type": "btn", "data": {"down": False, "button": btn}}])
        elif cmd == "btn":
            btn, _, state = arg.partition(" ")
            call("input-send-event",
                 events=[{"type": "btn",
                          "data": {"down": state.strip() == "down", "button": btn}}])
        elif cmd == "hotkey":
            hotkey(arg.strip())
        elif cmd == "shot":
            ppm = os.path.join(outdir, arg + ".ppm")
            call("screendump", filename=ppm)
            for _ in range(30):
                if os.path.exists(ppm) and os.path.getsize(ppm) > 1000:
                    break
                time.sleep(0.3)
            w, ht = ppm_to_png(ppm, os.path.join(outdir, arg + ".png"))
            print(f"снимок {arg}.png {w}x{ht}", flush=True)
        else:
            print("непонятная строка сценария:", line, file=sys.stderr)
finally:
    try:
        call("quit")
    except (BrokenPipeError, OSError):
        pass  # гость уже выключился сам (`poweroff` в сценарии) — это норма
    time.sleep(1)
    if p.poll() is None:
        p.kill()
    log.close()
    if os.path.exists(qmp_path):
        os.unlink(qmp_path)
print("готово")
