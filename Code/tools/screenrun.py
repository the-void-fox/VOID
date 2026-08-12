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
    wheel up|down      — колесо мыши (в QEMU это кнопки wheel-up/wheel-down)
    hotkey <аккорд>    — аккорд клавиатуры PS/2, например: hotkey Super+Return
    shot <имя>         — снять кадр в <каталог-выхода>/<имя>.png

Окружение (сеть — Веха 135, см. блок ниже): VOID_QEMU_ACCEL, VOID_QEMU_MEM, VOID_QEMU_CPU,
VOID_QEMU_NIC, VOID_QEMU_NET, VOID_QEMU_PCAP.
"""
import json, os, socket, struct, subprocess, sys, time, zlib

img, script, outdir = sys.argv[1], sys.argv[2], sys.argv[3]
os.makedirs(outdir, exist_ok=True)
# Путь unix-сокета ограничен 108 байтами, а каталоги сборки длиннее — держим его в /tmp.
qmp_path = f"/tmp/void-qmp-{os.getpid()}.sock"
if os.path.exists(qmp_path):
    os.unlink(qmp_path)

# Ускоритель и память — через окружение (Веха 126.4). По умолчанию TCG и 512 МиБ, как было:
# снимки экрана от скорости не зависят. Но всё, где важна СКОРОСТЬ или где баг ловится только
# на реальном темпе (гонки, переполнение кучи, анимации), требует `VOID_QEMU_ACCEL=kvm` —
# см. notes/void-qemu-run.md.
accel = os.environ.get("VOID_QEMU_ACCEL", "")
mem = os.environ.get("VOID_QEMU_MEM", "512M")
# Модель процессора. По умолчанию QEMU даёт `qemu64` — там нет ни SMEP, ни половины того, что
# есть на любом живом железе. Всё, что зависит от возможностей процессора, обязано проверяться с
# `VOID_QEMU_CPU=host` (под KVM) или `max`, иначе код просто не исполнится и «проверка» соврёт.
cpu = os.environ.get("VOID_QEMU_CPU", "")

# ─── сетевой стенд (Веха 135) ────────────────────────────────────────────────────────────────
#
# Раньше здесь было намертво вбито `-netdev user` — SLIRP. Для сетевой фазы это плохой стенд, и
# вот чем: SLIRP не провод, а НАТ в процессе QEMU. Он отвечает мгновенно и всегда, сам
# придумывает ответы на ARP и DHCP, не пускает широковещание дальше себя и не показывает, что
# именно мы положили на провод. Ровно тот класс лжи, на котором мы уже обожглись (см.
# notes/known-gaps.md и «QEMU прячет ошибки»): вялость старта из-за глухого цикла DHCP была
# невидима именно потому, что SLIRP отвечал в тот же миг.
#
#   VOID_QEMU_NIC   virtio | e1000 | none      — какую карту показать гостю
#   VOID_QEMU_NET   user | seg:<путь> | join:<путь> | tap:<имя> | none
#   VOID_QEMU_PCAP  <файл.pcap>                — записать ВСЁ, что прошло через карту
#
# `seg:` и `join:` — сегмент L2 между двумя VOID'ами: первый слушает unix-сокет, второй
# подключается, и дальше это честный кусок провода (кадр в кадр, без чужого стека посередине).
# Привилегий не требует. `tap:<имя>` — выход в настоящую сеть машины; tap должен быть заведён
# заранее и принадлежать пользователю (см. notes/void-qemu-run.md).
nic = os.environ.get("VOID_QEMU_NIC", "virtio")
net = os.environ.get("VOID_QEMU_NET", "user")
pcap = os.environ.get("VOID_QEMU_PCAP", "")
delay_us = os.environ.get("VOID_QEMU_DELAY_US", "")

NIC_DEV = {
    "virtio": "virtio-net-pci,netdev=net0,disable-legacy=on",
    "e1000": "e1000,netdev=net0",
}
# Умолчание QEMU для первой карты — 52:54:00:12:34:56, ОДИНАКОВОЕ у всех машин. Пока машина одна,
# это незаметно; на общем сегменте два одинаковых MAC ломают всё сразу (см. netlab.py).
mac = os.environ.get("VOID_QEMU_MAC", "")


def netdev_args():
    """Строки `-netdev`/`-device`/`-object` под выбранный стенд."""
    if net == "none" or nic == "none":
        # `-nic none` обязателен: без КАКИХ-ЛИБО сетевых ключей QEMU молча добавляет карту сам
        # (SLIRP по умолчанию). Проверено — «none» давал гостю сеть и адрес по DHCP, то есть
        # ровно то, что просили выключить.
        return ["-nic", "none"]
    kind, _, arg = net.partition(":")
    if kind == "user":
        backend = "user,id=net0"
    elif kind == "seg":
        backend = f"stream,id=net0,server=on,addr.type=unix,addr.path={arg}"
    elif kind == "join":
        backend = f"stream,id=net0,server=off,addr.type=unix,addr.path={arg}"
    elif kind == "tap":
        # script=no/downscript=no: поднимать tap — дело хозяина стенда, не QEMU.
        backend = f"tap,id=net0,ifname={arg},script=no,downscript=no"
    else:
        sys.exit(f"VOID_QEMU_NET: не понимаю '{net}'")
    if nic not in NIC_DEV:
        sys.exit(f"VOID_QEMU_NIC: не понимаю '{nic}' (есть: {', '.join(NIC_DEV)}, none)")
    dev = NIC_DEV[nic] + (f",mac={mac}" if mac else "")
    args = ["-netdev", backend, "-device", dev]
    if pcap:
        args += ["-object", f"filter-dump,id=dump0,netdev=net0,file={pcap}"]
    # Задержка канала. Локально RTT ~0.3 мс, и на таком проводе НЕ ВИДНО всего, что зависит от
    # произведения «полоса × задержка»: окна, ретрансмиссий, размера порции. Настоящая сеть — это
    # десятки миллисекунд, и там ошибки в окне стоят порядков скорости.
    if delay_us:
        args += ["-object", f"filter-buffer,id=lag0,netdev=net0,interval={delay_us}"]
    return args


qemu = [
    "qemu-system-x86_64", "-machine", "q35", "-m", mem,
    *(["-accel", accel] if accel else []),
    *(["-cpu", cpu] if cpu else []),
    "-device", "ich9-ahci,id=a",
    "-drive", f"if=none,id=d,file={img},format=raw",
    "-device", "ide-hd,drive=d,bus=a.0", "-boot", "c",
    # VOID_QEMU_SNAPSHOT=1 — писать не в образ, а во временный слой поверх него. Нужно, когда
    # один образ гонят СРАЗУ НЕСКОЛЬКО машин (netlab.py): иначе вторая падает на «Failed to get
    # write lock» — QEMU честно не даёт двум писать в один диск. Побочно это делает прогон
    # повторяемым: store каждый раз стартует с одного и того же поколения.
    *(["-snapshot"] if os.environ.get("VOID_QEMU_SNAPSHOT") else []),
    *netdev_args(),
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
        elif cmd == "wheel":
            # Колесо у QEMU — это кнопки `wheel-up`/`wheel-down` (Веха 123.1).
            btn = "wheel-up" if arg.strip() in ("up", "вверх") else "wheel-down"
            call("input-send-event", events=[{"type": "btn", "data": {"down": True, "button": btn}}])
            call("input-send-event", events=[{"type": "btn", "data": {"down": False, "button": btn}}])
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
