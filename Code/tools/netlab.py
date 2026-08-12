#!/usr/bin/env python3
r"""Сетевой стенд: два VOID'а на одном проводе (Веха 135).

Зачем это, если есть screenrun.py. До сих пор сеть VOID проверялась только против SLIRP
(`-netdev user`) — а SLIRP не провод, а НАТ внутри QEMU. Он отвечает мгновенно и всегда, сам
сочиняет ответы на ARP и DHCP, широковещание за свои пределы не пускает и молча правит то, что
ему не нравится. Проверка «работает в SLIRP» не значит почти ничего: вялость старта из-за глухого
цикла DHCP (Веха 134) жила незамеченной ровно потому, что SLIRP успевал ответить до первого
оборота цикла.

Здесь провод настоящий: `-netdev stream` через unix-сокет соединяет две QEMU кадр в кадр, без
чужого стека посередине. Никаких привилегий не нужно — ни root, ни tap, ни моста. На таком
сегменте видно всё, чего SLIRP не показывал: наш ARP против нашего ARP, широковещание, гонки
двух стеков, а позже — WireGuard между двумя VOID'ами.

Обе стороны пишут pcap (`-object filter-dump`), так что «кто что реально положил на провод»
читается wireshark'ом, а не додумывается по журналу.

Использование:
    netlab.py <образ.img> <сценарий-A.txt> <сценарий-B.txt> <каталог-выхода>

Выход в <каталог-выхода>/{a,b}/: serial.log, снимки экрана и wire.pcap каждой стороны.
Сценарии — те же, что у screenrun.py. Сторона A стартует первой (она слушает сокет).
"""
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SCREENRUN = os.path.join(HERE, "screenrun.py")

if len(sys.argv) != 5:
    sys.exit(__doc__.strip().splitlines()[-4])

img, script_a, script_b, outdir = sys.argv[1:5]
os.makedirs(outdir, exist_ok=True)
# Путь unix-сокета ограничен 108 байтами, а каталоги сборки длиннее — держим сегмент в /tmp
# (ровно та же ловушка, что у QMP-сокета в screenrun.py; здесь она стоила «сторона A не открыла
# сегмент», потому что QEMU про слишком длинный путь молчит).
seg = f"/tmp/void-seg-{os.getpid()}.sock"
# Сокет остаётся от прошлого прогона и мешает слушателю — снимаем его сами.
if os.path.exists(seg):
    os.unlink(seg)


def side(name, script, net, mac, image):
    """Запустить одну сторону провода отдельным screenrun.py."""
    d = os.path.join(outdir, name)
    os.makedirs(d, exist_ok=True)
    env = dict(os.environ)
    env["VOID_QEMU_MAC"] = mac
    # Образ один на обе стороны, а писать в него одновременно QEMU не даёт — гоним поверх
    # временного слоя. Заодно прогон становится повторяемым.
    env["VOID_QEMU_SNAPSHOT"] = "1"
    env["VOID_QEMU_NET"] = net
    env["VOID_QEMU_PCAP"] = os.path.join(d, "wire.pcap")
    env.setdefault("VOID_QEMU_NIC", "virtio")
    return subprocess.Popen([sys.executable, SCREENRUN, image, script, d], env=env)


# Сторона B может идти со СВОЕГО образа (VOID_LAB_IMG_B). Это единственный способ дать двум
# машинам разные АДРЕСА: адрес живёт в конфиге поколения внутри store, а store лежит в образе.
# Без этого обе стороны берут одинаковую статику, когда DHCP на голом сегменте не отвечает, —
# для проверки L2 и широковещания годится, для разговора между машинами нет.
img_b = os.environ.get("VOID_LAB_IMG_B", img)

a = side("a", script_a, f"seg:{seg}", "52:54:00:00:00:0a", img)
# Клиент обязан прийти ПОСЛЕ слушателя: `stream` без сервера соединения не ждёт и молча остаётся
# без провода (проверено — QEMU об этом даже не ругается). Ждём появления сокета, а не «на глаз».
for _ in range(100):
    if os.path.exists(seg):
        break
    if a.poll() is not None:
        sys.exit("сторона A умерла, не открыв сегмент")
    time.sleep(0.1)
else:
    a.kill()
    sys.exit("сторона A не открыла сегмент за 10 с")
b = side("b", script_b, f"join:{seg}", "52:54:00:00:00:0b", img_b)

rc_a, rc_b = a.wait(), b.wait()
if os.path.exists(seg):
    os.unlink(seg)
for name in ("a", "b"):
    p = os.path.join(outdir, name, "wire.pcap")
    size = os.path.getsize(p) if os.path.exists(p) else 0
    # Заголовок pcap — 24 байта. Ровно 24 значит «на проводе не было НИ ОДНОГО кадра»: это
    # отдельный диагноз, а не «мало трафика», и путать его с молчанием стека нельзя.
    print(f"{name}: {p} — {'кадров нет' if size <= 24 else f'{size} Б'}")
print(f"готово (A={rc_a}, B={rc_b})")
sys.exit(rc_a or rc_b)
