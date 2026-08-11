#!/usr/bin/env python3
"""netlog.py — слушать журнал VOID, приходящий СЫРЫМИ Ethernet-кадрами (Веха 134).

Зачем. Проверка на X54C стоит перезагрузки и фотографии экрана: COM-порта у ноутбука нет, USB для
VOID не блочное устройство, а видеть надо весь журнал ядра. Как только заработала передача кадров,
VOID вещает журнал прямо в провод — а здесь мы его читаем.

Ни IP, ни DHCP не нужно: кадр широковещательный, со своим EtherType (0x88B5 — из диапазона, который
IEEE отвёл под опытное и местное применение). Достаточно, чтобы кабель был воткнут, а интерфейс
поднят.

Использование (нужен root: сырые сокеты иначе не открыть):

    sudo ip link set enp3s0 up          # интерфейс должен быть ПОДНЯТ, адрес не нужен
    sudo Code/tools/netlog.py enp3s0

Имя интерфейса подскажет `ip -br link`. Выход — Ctrl-C.
"""
import socket
import sys

ETHERTYPE = 0x88B5
ETH_HLEN = 14


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    iface = sys.argv[1]

    try:
        sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETHERTYPE))
        sock.bind((iface, 0))
    except PermissionError:
        print("нужен root: sudo Code/tools/netlog.py <интерфейс>", file=sys.stderr)
        return 1
    except OSError as e:
        print(f"не открыть {iface}: {e}", file=sys.stderr)
        return 1

    print(f"слушаю {iface}, EtherType {ETHERTYPE:#06x} — жду журнал VOID (Ctrl-C выход)",
          file=sys.stderr)

    # Отправитель может повторить уже посланное (например, после переворота кольца журнала).
    # Дубликаты подряд глушим: они говорят не о системе, а о нашем способе доставки.
    last = None
    while True:
        frame = sock.recv(2048)
        payload = frame[ETH_HLEN:]
        if not payload or payload == last:
            continue
        last = payload
        sys.stdout.write(payload.decode("utf-8", "replace"))
        sys.stdout.flush()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
