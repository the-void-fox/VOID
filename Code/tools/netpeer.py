#!/usr/bin/env python3
r"""Управляемый сосед на проводе VOID: ARP + ICMP + DNS (Веха 136).

Зачем. `netlab.py` соединяет два VOID'а, но на таком сегменте нет НИКОГО, кто отвечал бы на DNS,
а SLIRP отвечает сам — мгновенно, всегда и не теряя. Из-за этого резолвер VOID ни разу не видели
под задержкой, потерей и отказом сервера: то есть под тем единственным, ради чего у DNS вообще
есть повторы и таймауты.

Здесь «остальной мир» — этот скрипт. Он садится на тот же unix-сокет, что и QEMU (`-netdev
stream`), разбирает кадры сам и отвечает ровно так, как ему велено:

    --loss 0.5          выбрасывать половину запросов (детерминированно, см. --seed)
    --delay 300         отвечать через 300 мс
    --rcode servfail    отвечать отказом вместо адреса
    --silent            молчать вовсе (проверка бюджета ожидания)
    --truncate          ставить бит TC (ответ «не влез, спроси по TCP»)

Привилегий не нужно никаких: ни root, ни tap, ни моста, ни порта 53 на хосте.

Использование (сосед слушает, VOID присоединяется):

    python3 Code/tools/netpeer.py --listen /tmp/void-seg.sock --zone a.void=10.0.2.99 &
    VOID_QEMU_NET=join:/tmp/void-seg.sock python3 Code/tools/screenrun.py образ.img сцен.txt вых/

По умолчанию сосед берёт себе ДВА адреса — 10.0.2.2 (шлюз) и 10.0.2.3 (DNS): ровно те, что
`net-srv` берёт статикой, когда DHCP на голом сегменте не отвечает. То есть VOID попадает в
привычный ему мир, только теперь этот мир честный и управляемый.

Каждое решение печатается строкой: что пришло, что ушло и почему — журнал СОСЕДА, а не VOID'а.
Сверять его с журналом VOID и с pcap — три независимых свидетеля.
"""
import argparse
import os
import random
import socket
import struct
import sys
import time

ETH_ARP = 0x0806
ETH_IPV4 = 0x0800
IP_ICMP = 1
IP_UDP = 17
DNS_PORT = 53

# ── провод ──────────────────────────────────────────────────────────────────────────────────
# QEMU (`-netdev stream`) возит по потоковому сокету кадры с 4-байтной длиной впереди (big-endian)
# — тот же формат, что у старого `-netdev socket`. Без этой обёртки поток кадров не разделить:
# TCP-сокет границ сообщений не хранит.


class Wire:
    def __init__(self, sock):
        self.sock = sock
        self.buf = b""

    def recv(self, timeout):
        """Следующий кадр или None, если за timeout секунд ничего не пришло."""
        while True:
            if len(self.buf) >= 4:
                (n,) = struct.unpack("!I", self.buf[:4])
                if len(self.buf) >= 4 + n:
                    frame, self.buf = self.buf[4 : 4 + n], self.buf[4 + n :]
                    return frame
            self.sock.settimeout(timeout)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                raise EOFError("провод закрыт")
            self.buf += chunk

    def send(self, frame):
        self.sock.sendall(struct.pack("!I", len(frame)) + frame)


# ── контрольные суммы ───────────────────────────────────────────────────────────────────────


def csum(data):
    if len(data) % 2:
        data += b"\0"
    s = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return (~s) & 0xFFFF


def ip4(src, dst, proto, payload, ident=0):
    hdr = struct.pack(
        "!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), ident, 0, 64, proto, 0, src, dst
    )
    hdr = hdr[:10] + struct.pack("!H", csum(hdr)) + hdr[12:]
    return hdr + payload


def udp4(src_ip, dst_ip, sport, dport, payload):
    hdr = struct.pack("!HHHH", sport, dport, 8 + len(payload), 0)
    pseudo = struct.pack("!4s4sBBH", src_ip, dst_ip, 0, IP_UDP, 8 + len(payload))
    c = csum(pseudo + hdr + payload) or 0xFFFF  # 0 означает «суммы нет» — подменяем, как в RFC 768
    return struct.pack("!HHHH", sport, dport, 8 + len(payload), c) + payload


def eth(dst_mac, src_mac, ethertype, payload):
    return dst_mac + src_mac + struct.pack("!H", ethertype) + payload


# ── DNS ─────────────────────────────────────────────────────────────────────────────────────


def dns_name(data, off):
    """Развернуть имя (со сжатием) → (имя, смещение за ним)."""
    labels, jumped, end = [], False, off
    for _ in range(128):
        n = data[off]
        if n & 0xC0 == 0xC0:
            ptr = struct.unpack("!H", data[off : off + 2])[0] & 0x3FFF
            if not jumped:
                end = off + 2
            off, jumped = ptr, True
            continue
        off += 1
        if n == 0:
            if not jumped:
                end = off
            return ".".join(labels), end
        labels.append(data[off : off + n].decode("ascii", "replace"))
        off += n
    raise ValueError("имя не кончается")


def dns_encode(name):
    out = b""
    for label in name.split("."):
        if label:
            out += bytes([len(label)]) + label.encode("ascii")
    return out + b"\0"


RCODES = {"noerror": 0, "formerr": 1, "servfail": 2, "nxdomain": 3, "refused": 5}


def dns_reply(query, zone, rcode, truncate, ttl):
    """Собрать ответ на запрос. Возвращает (байты, пояснение для журнала)."""
    ident, flags, qdcount = struct.unpack("!HHH", query[:6])
    if qdcount != 1:
        return None, f"вопросов {qdcount} — не отвечаю"
    name, off = dns_name(query, 12)
    qtype, qclass = struct.unpack("!HH", query[off : off + 4])
    question = query[12:off] + query[off : off + 4]

    answers, ancount = b"", 0
    if rcode == 0 and qtype == 1 and name.lower() in zone:  # A
        for addr in zone[name.lower()]:
            answers += (
                dns_encode(name)
                + struct.pack("!HHIH", 1, 1, ttl, 4)
                + socket.inet_aton(addr)
            )
            ancount += 1
        note = f"A {', '.join(zone[name.lower()])}"
    elif rcode == 0 and qtype != 1:
        rcode, note = 0, f"тип {qtype} не A — пустой ответ"
    elif rcode == 0:
        rcode, note = 3, "нет такого имени (NXDOMAIN)"
    else:
        note = f"rcode {rcode}"
    if truncate:
        answers, ancount, note = b"", 0, note + " + бит TC"

    # QR=1, RD копируем из запроса, RA=1 (рекурсию мы «умеем»), TC по требованию.
    out_flags = 0x8000 | (flags & 0x0100) | 0x0080 | (0x0200 if truncate else 0) | rcode
    header = struct.pack("!HHHHHH", ident, out_flags, 1, ancount, 0, 0)
    return header + question + answers, f"{name} → {note}"


# ── сосед ───────────────────────────────────────────────────────────────────────────────────


class Peer:
    def __init__(self, args):
        self.mac = bytes(int(x, 16) for x in args.mac.split(":"))
        self.ips = [socket.inet_aton(a) for a in args.ip]
        self.zone = {}
        for z in args.zone:
            name, _, addr = z.partition("=")
            self.zone.setdefault(name.lower(), []).append(addr)
        self.args = args
        self.rng = random.Random(args.seed)
        self.queued = []  # отложенные ответы: (когда отправить, кадр, пояснение)
        self.stats = {"arp": 0, "icmp": 0, "dns": 0, "dropped": 0}

    def log(self, *msg):
        print(f"[{time.strftime('%H:%M:%S')}] ", *msg, sep="", flush=True)

    def mine(self, ip):
        return ip in self.ips

    # -- разбор ------------------------------------------------------------------------------
    def handle(self, frame, wire):
        if len(frame) < 14:
            return
        dst, src, ethertype = frame[0:6], frame[6:12], struct.unpack("!H", frame[12:14])[0]
        body = frame[14:]
        if ethertype == ETH_ARP:
            self.arp(dst, src, body, wire)
        elif ethertype == ETH_IPV4:
            self.ipv4(src, body, wire)

    def arp(self, dst, src, arp, wire):
        if len(arp) < 28:
            return
        htype, ptype, hlen, plen, op = struct.unpack("!HHBBH", arp[:8])
        if (htype, ptype, hlen, plen) != (1, ETH_IPV4, 6, 4) or op != 1:
            return
        sha, spa, tpa = arp[8:14], arp[14:18], arp[24:28]
        if not self.mine(tpa):
            return
        self.stats["arp"] += 1
        reply = struct.pack("!HHBBH", 1, ETH_IPV4, 6, 4, 2) + self.mac + tpa + sha + spa
        wire.send(eth(sha, self.mac, ETH_ARP, reply))
        self.log(f"ARP кто такой {socket.inet_ntoa(tpa)}? → это я (спросил {socket.inet_ntoa(spa)})")

    def ipv4(self, src_mac, pkt, wire):
        if len(pkt) < 20:
            return
        ihl = (pkt[0] & 0x0F) * 4
        proto, src, dst = pkt[9], pkt[12:16], pkt[16:20]
        if not self.mine(dst):
            return
        payload = pkt[ihl : struct.unpack("!H", pkt[2:4])[0]]
        if proto == IP_ICMP:
            self.icmp(src_mac, src, dst, payload, wire)
        elif proto == IP_UDP:
            self.udp(src_mac, src, dst, payload, wire)

    def icmp(self, src_mac, src, dst, icmp, wire):
        if len(icmp) < 8 or icmp[0] != 8:
            return
        self.stats["icmp"] += 1
        ident, seq = struct.unpack("!HH", icmp[4:8])
        reply = b"\0\0\0\0" + icmp[4:]
        reply = reply[:2] + struct.pack("!H", csum(reply)) + reply[4:]
        wire.send(eth(src_mac, self.mac, ETH_IPV4, ip4(dst, src, IP_ICMP, reply)))
        self.log(f"ICMP echo от {socket.inet_ntoa(src)} (ident {ident}, seq {seq}) → ответил")

    def udp(self, src_mac, src, dst, udp, wire):
        if len(udp) < 8:
            return
        sport, dport = struct.unpack("!HH", udp[:4])
        if dport != DNS_PORT:
            return
        self.stats["dns"] += 1
        query = udp[8:]
        try:
            name, _ = dns_name(query, 12)
        except (IndexError, ValueError):
            self.log("DNS: запрос не разобрался")
            return
        if self.args.silent:
            self.stats["dropped"] += 1
            self.log(f"DNS {name}: молчу (--silent)")
            return
        if self.args.loss and self.rng.random() < self.args.loss:
            self.stats["dropped"] += 1
            self.log(f"DNS {name}: ВЫБРОШЕН (--loss {self.args.loss})")
            return
        answer, note = dns_reply(
            query, self.zone, RCODES[self.args.rcode], self.args.truncate, self.args.ttl
        )
        if answer is None:
            self.log(f"DNS: {note}")
            return
        frame = eth(
            src_mac,
            self.mac,
            ETH_IPV4,
            ip4(dst, src, IP_UDP, udp4(dst, src, DNS_PORT, sport, answer)),
        )
        if self.args.delay:
            self.queued.append((time.monotonic() + self.args.delay / 1000, frame, note))
            self.log(f"DNS {note} — отвечу через {self.args.delay} мс")
        else:
            wire.send(frame)
            self.log(f"DNS {note}")

    def flush(self, wire):
        """Отправить отложенные ответы, чей срок пришёл. Возвращает срок ближайшего."""
        now, rest, nearest = time.monotonic(), [], None
        for when, frame, note in self.queued:
            if when <= now:
                wire.send(frame)
                self.log(f"DNS {note} — отправлен (с задержкой)")
            else:
                rest.append((when, frame, note))
                nearest = when if nearest is None else min(nearest, when)
        self.queued = rest
        return nearest


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--listen", required=True, help="путь unix-сокета сегмента (VOID идёт join:)")
    p.add_argument("--mac", default="52:54:00:00:00:fe")
    p.add_argument("--ip", action="append", default=None, help="адрес соседа (можно несколько)")
    p.add_argument("--zone", action="append", default=None, metavar="ИМЯ=A.B.C.D")
    p.add_argument("--ttl", type=int, default=60)
    p.add_argument("--loss", type=float, default=0.0, help="доля выброшенных запросов, 0..1")
    p.add_argument("--delay", type=int, default=0, help="задержка ответа, мс")
    p.add_argument("--rcode", choices=sorted(RCODES), default="noerror")
    p.add_argument("--truncate", action="store_true", help="ставить бит TC")
    p.add_argument("--silent", action="store_true", help="принимать запросы и не отвечать")
    p.add_argument("--seed", type=int, default=1, help="сид потерь — прогон повторяем")
    p.add_argument("--seconds", type=float, default=0, help="выйти через N секунд (0 — до Ctrl-C)")
    args = p.parse_args()
    # Умолчания — мир, к которому VOID привык по статике net-srv: шлюз .2 и DNS .3.
    args.ip = args.ip or ["10.0.2.2", "10.0.2.3"]
    args.zone = args.zone or ["void.test=10.0.2.99", "example.com=93.184.216.34"]

    if os.path.exists(args.listen):
        os.unlink(args.listen)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(args.listen)
    srv.listen(1)
    print(f"сосед слушает {args.listen}; адреса {', '.join(args.ip)}", flush=True)
    srv.settimeout(60)
    try:
        conn, _ = srv.accept()
    except socket.timeout:
        sys.exit("никто не присоединился к сегменту за 60 с")
    peer, wire = Peer(args), Wire(conn)
    peer.log("VOID на проводе")
    stop = time.monotonic() + args.seconds if args.seconds else None
    try:
        while stop is None or time.monotonic() < stop:
            nearest = peer.flush(wire)
            timeout = 0.05 if nearest is None else max(0.001, min(0.05, nearest - time.monotonic()))
            try:
                frame = wire.recv(timeout)
            except EOFError:
                peer.log("VOID отсоединился")
                break
            if frame:
                peer.handle(frame, wire)
    except KeyboardInterrupt:
        pass
    finally:
        s = peer.stats
        print(
            f"итого: ARP {s['arp']}, ICMP {s['icmp']}, DNS-запросов {s['dns']} "
            f"(выброшено {s['dropped']})",
            flush=True,
        )
        os.unlink(args.listen)


if __name__ == "__main__":
    main()
