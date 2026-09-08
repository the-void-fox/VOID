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
    type <строка>      — набрать строку НА КЛАВИАТУРЕ (PS/2): единственный ввод, доходящий
                         до шелла в ОКНЕ (mode = "wm"), куда serial не идёт вовсе
    mouse <dx> <dy>    — подвинуть мышь (относительное событие)
    click <кнопка>     — нажать и отпустить (left / right / middle)
    btn <кнопка> <down|up> — держать/отпустить (для перетаскивания и снимков «нажато»)
    wheel up|down      — колесо мыши (в QEMU это кнопки wheel-up/wheel-down)
    hotkey <аккорд>    — аккорд клавиатуры PS/2, например: hotkey Super+Return
    hold <клавиша> <down|up> — ЗАДЕРЖАТЬ клавишу (Super+колесо, перетаскивание с Super)
    shot <имя>         — снять кадр в <каталог-выхода>/<имя>.png

Окружение (сеть — Веха 135, см. блок ниже): VOID_QEMU_ACCEL, VOID_QEMU_MEM, VOID_QEMU_CPU,
VOID_QEMU_NIC, VOID_QEMU_NET, VOID_QEMU_PCAP.

Сама машина (чипсет, диск, память, энтропия, сеть) описана в `tools/qemu-machine.sh` — общим
файлом с `run.sh`, чтобы прогон со снимками шёл на ТОМ ЖЕ стенде, что и обычный запуск.
"""
import json, os, socket, struct, subprocess, sys, time, zlib

HERE = os.path.dirname(os.path.abspath(__file__))
# Описание СТЕНДА — общее с `run.sh`. До этого машину описывали два места, и совпадали они лишь
# по памяти человека: «замер на том же стенде, на котором работаем» не проверялось ничем, а
# отличие стенда в глаза не бросается — гость поднимается, снимки выходят, числа получаются.
MACHINE_SH = os.path.join(HERE, "qemu-machine.sh")

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
# Пусто — «возьми умолчание стенда». Своё умолчание здесь стояло (512 МиБ) и расходилось с тем,
# с которым система работает (1280 МиБ): найденное на одной машине могло не воспроизвестись на
# другой, а выглядело это как «у меня не повторяется».
mem = os.environ.get("VOID_QEMU_MEM", "")
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
#
# Сами ключи QEMU собирает общее описание стенда — те же самые и для `run.sh`. Здесь остаётся
# только прочитать, чего от нас хотят; пустое значение означает «умолчание стенда».
nic = os.environ.get("VOID_QEMU_NIC", "")
net = os.environ.get("VOID_QEMU_NET", "")
pcap = os.environ.get("VOID_QEMU_PCAP", "")
delay_us = os.environ.get("VOID_QEMU_DELAY_US", "")

# Умолчание QEMU для первой карты — 52:54:00:12:34:56, ОДИНАКОВОЕ у всех машин. Пока машина одна,
# это незаметно; на общем сегменте два одинаковых MAC ломают всё сразу (см. netlab.py).
mac = os.environ.get("VOID_QEMU_MAC", "")


def stand(*args):
    """Кусок стенда из общего описания (`tools/qemu-machine.sh`) — по аргументу на строку.

    Через общий файл, а не своим списком: устройства машины должны быть ОДНИ И ТЕ ЖЕ у прогона
    со снимками и у обычного запуска, иначе замер сделан не на той машине, на которой работают.
    Отдельный процесс здесь ничего не стоит — прогон и так идёт секундами.
    """
    r = subprocess.run(["bash", MACHINE_SH, *args], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(r.stderr.strip() or f"стенд: не вышло собрать '{' '.join(args)}'")
    return r.stdout.splitlines()


qemu = [
    "qemu-system-x86_64",
    *stand("machine", img, mem),
    *(["-accel", accel] if accel else []),
    *(["-cpu", cpu] if cpu else []),
    # VOID_QEMU_SNAPSHOT=1 — писать не в образ, а во временный слой поверх него. Нужно, когда
    # один образ гонят СРАЗУ НЕСКОЛЬКО машин (netlab.py): иначе вторая падает на «Failed to get
    # write lock» — QEMU честно не даёт двум писать в один диск. Побочно это делает прогон
    # повторяемым: store каждый раз стартует с одного и того же поколения.
    *(["-snapshot"] if os.environ.get("VOID_QEMU_SNAPSHOT") else []),
    *stand("net", net, nic, mac, pcap, delay_us),
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
    # Веха 178 — `Delete` стала настоящей клавишей системы (в корзину), и называть её надо явно:
    # без записи здесь она уехала бы в `last.lower()` и совпала бы случайно, а не по уговору.
    "Delete": "delete", "Backspace": "backspace", "Insert": "insert",
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


# Печатные знаки, у которых имя qcode не совпадает с самим знаком. Верхний регистр и знаки из
# `SHIFTED` набираются с зажатым Shift — то есть ровно так, как их набирает человек.
PRINTABLE = {
    " ": "spc", "/": "slash", ".": "dot", "-": "minus", ",": "comma", ";": "semicolon",
    "'": "apostrophe", "=": "equal", "[": "bracket_left", "]": "bracket_right",
    "\\": "backslash", "`": "grave_accent", "\n": "ret", "\t": "tab",
}
SHIFTED = {"_": "minus", ":": "semicolon", '"': "apostrophe", "(": "9", ")": "0", "+": "equal",
           "?": "slash", "*": "8", "!": "1", "~": "grave_accent", "{": "bracket_left",
           "}": "bracket_right", "|": "backslash", "<": "comma", ">": "dot"}


def typewrite(s):
    """Строка НАСТОЯЩЕЙ клавиатурой (PS/2), а не в serial.

    Нужно оконному режиму: там ввод идёт через композитор к окну, и серийная консоль до шелла
    в окне не доходит вовсе. Пауза между знаками та же, что у [`serial`], и по той же причине.
    """
    for ch in s:
        if ch.isalpha() and ch.isupper() or ch in SHIFTED:
            hotkey("Shift+" + (SHIFTED[ch] if ch in SHIFTED else ch.lower()))
        elif ch.isalnum():
            hotkey(ch.lower())
        elif ch in PRINTABLE:
            hotkey(PRINTABLE[ch])
        else:
            print(f"нечем набрать знак {ch!r}", file=sys.stderr)
        time.sleep(0.04)


def serial(data):
    """Байты в консоль гостя — ПО ОДНОМУ, с паузой.

    Веха 138: писать строку одним куском нельзя. У 16550 приёмный FIFO на 16 байт, а хост под
    KVM успевает налить туда всю команду прежде, чем гость разберёт первый байт: остальное
    молча теряется. Ровно так `rebuild` доезжал до шелла как `reb` — команда не находилась, а
    сценарий выглядел исполненным. Пауза в 20 мс на знак стоит четверть секунды на команду и
    снимает весь класс.
    """
    for b in data:
        p.stdin.write(bytes([b]))
        p.stdin.flush()
        time.sleep(0.02)


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
            serial(arg.encode() + b"\n")
        elif cmd == "raw":
            serial(unescape(arg))
        elif cmd == "type":
            typewrite(arg)
        elif cmd == "mouse":
            # Двигаем ШАГАМИ: в пакете PS/2 смещение — девять знаковых бит, и всё, что больше,
            # мышь просто не умеет сказать. Один вызов с `dx = -3000` доезжал до гостя как
            # «-255», то есть сценарий целился в пилюлю панели, а попадал куда придётся — и
            # выглядело это как «клик не сработал», а не как обрезанное число.
            dx, dy = (int(v) for v in arg.split())
            step = 120
            while dx or dy:
                sx = max(-step, min(step, dx))
                sy = max(-step, min(step, dy))
                call("input-send-event", events=[rel("x", sx), rel("y", sy)])
                dx -= sx
                dy -= sy
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
        elif cmd == "hold":
            # Веха 142 — клавишу можно ЗАДЕРЖАТЬ: без этого не проверить ни Super+колесо, ни
            # перетаскивание окон с Super, потому что `hotkey` отпускает клавишу сразу.
            k, _, state = arg.partition(" ")
            key = QCODE.get(k.strip(), k.strip().lower())
            call("input-send-event", events=[{"type": "key", "data": {
                "down": state.strip() == "down", "key": {"type": "qcode", "data": key}}}])
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
