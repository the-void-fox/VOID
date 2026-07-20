//! Сетевой сервер (Веха 34) — стек ARP + IPv4 + ICMP в userspace (микроядерность:
//! ядро отдаёт лишь сырые кадры, протоколы — здесь). Держит cap на сетевое устройство
//! (`a0`), общается с картой через `net_send`/`net_recv`/`net_mac`.
//!
//! Наш адрес — 10.0.2.15 (гость сети QEMU SLIRP), шлюз/DNS — 10.0.2.2/10.0.2.3;
//! эти адреса SLIRP обслуживает: отвечает на ARP и ICMP echo, не выходя за пределы QEMU
//! (демо самодостаточно — внешняя сеть не нужна).
//!
//! Протокол IPC (клиент → сервер): op = OP_PING, нагрузка = 4 байта целевого IPv4.
//! Ответ 5 байт: `[status(1) | rtt_us(u32 LE)]`; status 0 — ok, 1 — ARP не разрешился,
//! 2 — нет ICMP-ответа, 3 — карты нет.
//!
//! На старте сервер сам пингует шлюз и печатает результат — автоматическое доказательство
//! на каждой загрузке (как демо у других серверов).

#![no_std]
#![no_main]

use void_user as sys;

const OP_PING: usize = 0;

/// Наш IPv4 (SLIRP-гость) и таймауты.
const OUR_IP: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];

const ETH_ARP: u16 = 0x0806;
const ETH_IPV4: u16 = 0x0800;
const IP_PROTO_ICMP: u8 = 1;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;
const BROADCAST: [u8; 6] = [0xff; 6];

/// Бюджет ожидания в тиках (rdtime/rdtsc): ~1 с. TICK_NS: riscv 100, x86 1.
const TIMEOUT_TICKS: usize = 1_000_000_000 / sys::TICK_NS;

#[no_mangle]
pub extern "C" fn _start(dev_cap: usize, _a1: usize) -> ! {
    let mut mac = [0u8; 6];
    if sys::net_mac(dev_cap, &mut mac) != 0 {
        // Карты нет — сервер всё равно поднимается, но ping'и вернут status=3.
        sys::write("[net-srv] карты нет — сеть недоступна\n".as_bytes());
    } else {
        sys::write("[net-srv] запущен, MAC ".as_bytes());
        write_mac(&mac);
        sys::write(b" IP 10.0.2.15\n");

        // Самопроверка: пингуем шлюз 10.0.2.2 (SLIRP отвечает, не выходя из QEMU).
        sys::write("[net-srv] самопинг шлюза 10.0.2.2: ".as_bytes());
        match ping(dev_cap, &mac, GATEWAY) {
            Ok(rtt) => {
                sys::write("ответ за ".as_bytes());
                write_dec(rtt);
                sys::write(" мкс\n".as_bytes());
            }
            Err(2) => sys::write("нет ICMP-ответа\n".as_bytes()),
            Err(_) => sys::write("ARP не разрешился\n".as_bytes()),
        }
    }

    // Серверный цикл: обслуживаем OP_PING по IPC.
    let mut req = [0u8; 64];
    loop {
        let m = sys::recv(&mut req);
        let mut rep = [0u8; 5];
        if m.op == OP_PING && m.len >= 4 {
            let target = [req[0], req[1], req[2], req[3]];
            match ping(dev_cap, &mac, target) {
                Ok(rtt) => {
                    rep[0] = 0;
                    rep[1..5].copy_from_slice(&(rtt as u32).to_le_bytes());
                }
                Err(code) => rep[0] = code,
            }
        } else {
            rep[0] = 3;
        }
        sys::reply(m.reply_cap, &rep);
    }
}

/// Пропинговать `target`: ARP разрешает MAC следующего узла, затем ICMP echo с замером RTT.
/// Возвращает RTT в мкс либо код ошибки (1 — ARP, 2 — ICMP).
fn ping(dev: usize, our_mac: &[u8; 6], target: [u8; 4]) -> Result<usize, u8> {
    // Следующий узел: цель, если она в нашей подсети /24, иначе шлюз.
    let next_hop = if target[0] == OUR_IP[0] && target[1] == OUR_IP[1] && target[2] == OUR_IP[2] {
        target
    } else {
        GATEWAY
    };
    let dst_mac = arp_resolve(dev, our_mac, next_hop).ok_or(1u8)?;
    icmp_echo(dev, our_mac, dst_mac, target).ok_or(2u8)
}

/// Разрешить MAC для `ip` через ARP: шлём запрос, ждём ответ (попутно отвечая на чужие
/// ARP-запросы к нам). `None` — таймаут.
fn arp_resolve(dev: usize, our_mac: &[u8; 6], ip: [u8; 4]) -> Option<[u8; 6]> {
    let mut frame = [0u8; 42];
    build_eth(&mut frame, &BROADCAST, our_mac, ETH_ARP);
    let a = &mut frame[14..42];
    a[0..2].copy_from_slice(&1u16.to_be_bytes()); // htype ethernet
    a[2..4].copy_from_slice(&ETH_IPV4.to_be_bytes()); // ptype IPv4
    a[4] = 6; // hlen
    a[5] = 4; // plen
    a[6..8].copy_from_slice(&ARP_REQUEST.to_be_bytes());
    a[8..14].copy_from_slice(our_mac);
    a[14..18].copy_from_slice(&OUR_IP);
    a[18..24].copy_from_slice(&[0u8; 6]); // target mac неизвестен
    a[24..28].copy_from_slice(&ip);
    sys::net_send(dev, &frame);

    let start = sys::now();
    let mut buf = [0u8; 2048];
    while sys::now().wrapping_sub(start) < TIMEOUT_TICKS {
        let n = sys::net_recv(dev, &mut buf);
        if n == 0 || n == usize::MAX {
            continue;
        }
        if n < 42 || ethertype(&buf) != ETH_ARP {
            continue;
        }
        let a = &buf[14..42];
        let op = u16::from_be_bytes([a[6], a[7]]);
        if op == ARP_REPLY && a[14..18] == ip {
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&a[8..14]);
            return Some(mac);
        }
        // Кто-то спрашивает наш MAC — ответить (иначе к нам не достучатся).
        if op == ARP_REQUEST && a[24..28] == OUR_IP {
            answer_arp(dev, our_mac, a);
        }
    }
    None
}

/// Ответить на ARP-запрос к нашему IP.
fn answer_arp(dev: usize, our_mac: &[u8; 6], req: &[u8]) {
    let mut sender_mac = [0u8; 6];
    sender_mac.copy_from_slice(&req[8..14]);
    let mut frame = [0u8; 42];
    build_eth(&mut frame, &sender_mac, our_mac, ETH_ARP);
    let a = &mut frame[14..42];
    a[0..2].copy_from_slice(&1u16.to_be_bytes());
    a[2..4].copy_from_slice(&ETH_IPV4.to_be_bytes());
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&ARP_REPLY.to_be_bytes());
    a[8..14].copy_from_slice(our_mac);
    a[14..18].copy_from_slice(&OUR_IP);
    a[18..24].copy_from_slice(&sender_mac);
    a[24..28].copy_from_slice(&req[14..18]); // спрашивавший IP
    sys::net_send(dev, &frame);
}

/// Отправить ICMP echo request и дождаться reply от `target`. Возвращает RTT в мкс.
fn icmp_echo(dev: usize, our_mac: &[u8; 6], dst_mac: [u8; 6], target: [u8; 4]) -> Option<usize> {
    const PAYLOAD: usize = 32;
    let mut frame = [0u8; 14 + 20 + 8 + PAYLOAD];
    build_eth(&mut frame, &dst_mac, our_mac, ETH_IPV4);

    // IPv4-заголовок.
    let total_ip = 20 + 8 + PAYLOAD;
    {
        let ip = &mut frame[14..34];
        ip[0] = 0x45; // версия 4, IHL 5
        ip[2..4].copy_from_slice(&(total_ip as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&0x1234u16.to_be_bytes()); // id
        ip[8] = 64; // TTL
        ip[9] = IP_PROTO_ICMP;
        ip[12..16].copy_from_slice(&OUR_IP);
        ip[16..20].copy_from_slice(&target);
        let csum = checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&csum.to_be_bytes());
    }
    // ICMP echo request.
    {
        let icmp = &mut frame[34..34 + 8 + PAYLOAD];
        icmp[0] = ICMP_ECHO_REQUEST;
        icmp[4..6].copy_from_slice(&0xbeefu16.to_be_bytes()); // id
        icmp[6..8].copy_from_slice(&1u16.to_be_bytes()); // seq
        for (i, b) in icmp[8..].iter_mut().enumerate() {
            *b = i as u8;
        }
        let csum = checksum(icmp);
        icmp[2..4].copy_from_slice(&csum.to_be_bytes());
    }

    let start = sys::now();
    sys::net_send(dev, &frame);

    let mut buf = [0u8; 2048];
    while sys::now().wrapping_sub(start) < TIMEOUT_TICKS {
        let n = sys::net_recv(dev, &mut buf);
        if n == 0 || n == usize::MAX {
            continue;
        }
        if n < 14 + 20 + 8 || ethertype(&buf) != ETH_IPV4 {
            // Попутный ARP к нам — ответить.
            if n >= 42 && ethertype(&buf) == ETH_ARP {
                let a = &buf[14..42];
                if u16::from_be_bytes([a[6], a[7]]) == ARP_REQUEST && a[24..28] == OUR_IP {
                    answer_arp(dev, our_mac, a);
                }
            }
            continue;
        }
        let ip = &buf[14..];
        let ihl = ((ip[0] & 0x0f) as usize) * 4;
        if ip[9] != IP_PROTO_ICMP || ip[12..16] != target {
            continue;
        }
        let icmp = &ip[ihl..];
        if icmp[0] == ICMP_ECHO_REPLY {
            let elapsed = sys::now().wrapping_sub(start);
            return Some(elapsed * sys::TICK_NS / 1000);
        }
    }
    None
}

/// Заполнить Ethernet-заголовок (14 байт): dst, src, ethertype.
fn build_eth(frame: &mut [u8], dst: &[u8; 6], src: &[u8; 6], etype: u16) {
    frame[0..6].copy_from_slice(dst);
    frame[6..12].copy_from_slice(src);
    frame[12..14].copy_from_slice(&etype.to_be_bytes());
}

fn ethertype(frame: &[u8]) -> u16 {
    u16::from_be_bytes([frame[12], frame[13]])
}

/// Интернет-контрольная сумма (16-битная, дополнение до единицы).
fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Напечатать MAC как xx:xx:xx:xx:xx:xx.
fn write_mac(mac: &[u8; 6]) {
    let hex = b"0123456789abcdef";
    let mut out = [0u8; 17];
    for i in 0..6 {
        out[i * 3] = hex[(mac[i] >> 4) as usize];
        out[i * 3 + 1] = hex[(mac[i] & 0xf) as usize];
        if i < 5 {
            out[i * 3 + 2] = b':';
        }
    }
    sys::write(&out);
}

/// Напечатать число десятично (форматтера в no_std-бинаре нет).
fn write_dec(mut v: usize) {
    let mut nb = [0u8; 20];
    let mut n = 0;
    if v == 0 {
        sys::write(b"0");
        return;
    }
    while v > 0 {
        nb[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    let mut out = [0u8; 20];
    for i in 0..n {
        out[i] = nb[n - 1 - i];
    }
    sys::write(&out[..n]);
}
