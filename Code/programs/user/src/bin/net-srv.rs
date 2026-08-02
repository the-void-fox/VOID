//! Сетевой сервер VOID — стек в userspace (Веха 34 → **Веха 90: настоящий стек smoltcp**).
//!
//! Микроядерность неизменна: ядро отдаёт только сырые кадры (`net_send`/`net_recv`), протоколы
//! живут здесь. Изменилось ЧТО именно их разбирает. До Вехи 90 это были ~200 строк своих
//! ARP+IPv4+ICMP — ровно столько, сколько нужно для `ping`, и ни байтом больше: ни UDP, ни TCP,
//! ни ретрансмиссий. Дальше по дорожной карте нужен TCP, а его с нуля не пишут (ADR 0008),
//! поэтому стек — **vendored smoltcp** с прибитой версией, а наш код сведён к мосту
//! ([`sys::net_phy`]) и политике.
//!
//! Адреса пока статические (SLIRP QEMU): мы 10.0.2.15/24, шлюз 10.0.2.2, DNS 10.0.2.3.
//! DHCP придёт Вехой 92, TCP — Вехой 93.
//!
//! Протокол IPC (клиент → сервер) не менялся — vvsh и `ping` работают как раньше:
//! op = OP_PING, нагрузка = 4 байта IPv4; ответ 5 байт `[status(1) | rtt_us(u32 LE)]`,
//! status 0 — ok, 1 — адрес не разрешился, 2 — нет ответа, 3 — карты нет.

#![no_std]
#![no_main]

use void_user as sys;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::socket::{icmp, udp};
use smoltcp::storage::PacketMetadata;
use smoltcp::wire::{
    EthernetAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, IpEndpoint, Ipv4Address,
};

const OP_PING: usize = 0;

const OUR_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
/// DNS-сервер SLIRP. Веха 90 использует его как мишень UDP-проверки: он отвечает, не выходя за
/// пределы QEMU, поэтому демо самодостаточно (полноценный DNS-резолвер — Веха 92).
const DNS: Ipv4Address = Ipv4Address::new(10, 0, 2, 3);

/// Идентификатор наших echo-запросов (ICMP ident) — по нему стек отдаёт нам ответы.
const PING_IDENT: u16 = 0x1D0;

/// Веха 90 — бюджет операции, которой может понадобиться РАЗРЕШИТЬ адрес. Больше секунды не по
/// прихоти: `NeighborCache` в smoltcp глушит ARP-запросы на 1 с после ЛЮБОГО предыдущего (поле
/// `silent_until` общее на весь кэш, а не на адрес). Наш самопинг шлюза при загрузке как раз
/// такой запрос и делает, поэтому следующая операция к НОВОМУ адресу молчит почти всю эту
/// секунду. Бюджет ровно в 1 с давал ложное «нет ответа» — на диагностику этого ушёл целый заход
/// с дампом трафика: в дампе не было даже ARP.
const RESOLVE_MS: i64 = 3000;

#[no_mangle]
pub extern "C" fn _start(dev_cap: usize, _a1: usize) -> ! {
    let mut mac = [0u8; 6];
    if sys::net_mac(dev_cap, &mut mac) != 0 {
        // Карты нет — сервер всё равно поднимается, ping'и вернут status=3.
        sys::write("[net-srv] карты нет — сеть недоступна\n".as_bytes());
        serve_no_device();
    }

    sys::write("[net-srv] запущен, MAC ".as_bytes());
    write_mac(&mac);
    sys::write(b" IP 10.0.2.15 (smoltcp)\n");

    let mut device = sys::net_phy::VoidDevice::new(dev_cap);
    let mut config = Config::new(EthernetAddress(mac).into());
    // Сид для выбора эфемерных портов/ISN. Часов ещё нет смысла спрашивать — берём тики.
    config.random_seed = sys::now() as u64;
    let mut iface = Interface::new(config, &mut device, sys::net_phy::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(OUR_IP), 24));
    });
    let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);

    // Сокеты и их буферы — статические (smoltcp собран без alloc): по одному кадру на сторону.
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_data = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_data = [0u8; 1024];
    let icmp_socket = icmp::Socket::new(
        icmp::PacketBuffer::new(&mut rx_meta[..], &mut rx_data[..]),
        icmp::PacketBuffer::new(&mut tx_meta[..], &mut tx_data[..]),
    );
    let mut urx_meta = [udp::PacketMetadata::EMPTY; 4];
    let mut urx_data = [0u8; 1024];
    let mut utx_meta = [udp::PacketMetadata::EMPTY; 4];
    let mut utx_data = [0u8; 1024];
    let udp_socket = udp::Socket::new(
        udp::PacketBuffer::new(&mut urx_meta[..], &mut urx_data[..]),
        udp::PacketBuffer::new(&mut utx_meta[..], &mut utx_data[..]),
    );
    let mut storage = [SocketStorage::EMPTY; 4];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let icmp_handle = sockets.add(icmp_socket);
    let udp_handle = sockets.add(udp_socket);

    {
        let s = sockets.get_mut::<icmp::Socket>(icmp_handle);
        if s.bind(icmp::Endpoint::Ident(PING_IDENT)).is_err() {
            sys::write("[net-srv] не удалось открыть ICMP-сокет\n".as_bytes());
        }
    }

    // Самопроверка на каждой загрузке: пингуем шлюз (SLIRP отвечает, не выходя из QEMU).
    sys::write("[net-srv] самопинг шлюза 10.0.2.2: ".as_bytes());
    match ping(&mut iface, &mut device, &mut sockets, icmp_handle, GATEWAY) {
        Ok(rtt) => {
            sys::write("ответ за ".as_bytes());
            write_dec(rtt);
            sys::write(" мкс\n".as_bytes());
        }
        Err(2) => sys::write("нет ICMP-ответа\n".as_bytes()),
        Err(_) => sys::write("адрес не разрешился\n".as_bytes()),
    }

    // Веха 90 — проверка UDP: ICMP доказывает, что стек живой, но не что работает транспорт.
    // Шлём настоящий DNS-запрос A-записи и ждём ответ — это полный путь UDP туда и обратно.
    sys::write("[net-srv] UDP-проверка (DNS-запрос к 10.0.2.3): ".as_bytes());
    match udp_probe(&mut iface, &mut device, &mut sockets, udp_handle) {
        Ok(n) => {
            sys::write("ответ ".as_bytes());
            write_dec(n);
            sys::write(" байт — UDP работает\n".as_bytes());
        }
        Err(e) => {
            sys::write(e.as_bytes());
            sys::write("\n".as_bytes());
        }
    }

    // Серверный цикл (Веха 90 — РЕАКТОР): стек прокачивается постоянно, а IPC разбирается между
    // тиками неблокирующим приёмом. До этого сервер стоял в блокирующем `recv`, и стек тикал
    // только внутри `ping` — для ICMP сходило, для TCP (Веха 93) не сойдёт: ретрансмиссии и
    // таймеры требуют, чтобы `poll` звался всегда, а не когда кто-то попросил.
    // Опрос временный: ждать СРАЗУ кадра и сообщения научит Веха 91 (пока это жжёт CPU).
    let mut req = [0u8; 64];
    loop {
        let busy = iface.poll(sys::net_phy::now(), &mut device, &mut sockets);
        // Веха 91 — СОН ВМЕСТО ОПРОСА. Спим в самом `SYS_RECV` (режим с дедлайном): пока никто
        // не зовёт и стеку нечего делать, процесс не занимает процессор вовсе — он заблокирован
        // ядром, а не крутит цикл. Момент пробуждения называет САМ стек: `poll_at` — это время
        // ближайшего таймера (ретрансмиссия, ARP-повтор); проспать его нельзя, иначе TCP встанет.
        //
        // Потолок сна временный: у сетевой карты в ядре ещё нет прерывания приёма, поэтому
        // входящий кадр сам нас не разбудит и его надо успеть заметить. Как только RX-IRQ
        // появится, потолок уходит и остаётся чистое пробуждение по событию.
        const IDLE_CAP_MS: u64 = 20;
        let sleep_ms = if busy != smoltcp::iface::PollResult::None {
            0 // стек что-то сделал — возможно, есть ещё работа; проверим сразу
        } else {
            match iface.poll_delay(sys::net_phy::now(), &sockets) {
                Some(d) => d.millis().min(IDLE_CAP_MS),
                None => IDLE_CAP_MS,
            }
        };
        let Some(m) = (if sleep_ms == 0 {
            sys::try_recv(&mut req)
        } else {
            sys::recv_timeout(&mut req, (sleep_ms as usize) * (1_000_000 / sys::TICK_NS))
        }) else {
            continue;
        };
        let mut rep = [0u8; 5];
        if m.op == OP_PING && m.len >= 4 {
            let target = Ipv4Address::new(req[0], req[1], req[2], req[3]);
            match ping(&mut iface, &mut device, &mut sockets, icmp_handle, target) {
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

/// Один echo-запрос с замером RTT. Возвращает микросекунды либо код ошибки
/// (1 — не удалось отправить, например ARP не разрешился за отведённое время; 2 — нет ответа).
///
/// Стек прокачивается `iface.poll` в цикле: это же разбирает ARP, отвечает на чужие запросы к
/// нам и вообще двигает всю машинерию — своей обработки протоколов у нас больше нет.
fn ping(
    iface: &mut Interface,
    device: &mut sys::net_phy::VoidDevice,
    sockets: &mut SocketSet,
    handle: smoltcp::iface::SocketHandle,
    target: Ipv4Address,
) -> Result<usize, u8> {
    let started = sys::now();
    let deadline = sys::net_phy::now() + smoltcp::time::Duration::from_millis(RESOLVE_MS as u64);

    // 1) Отправить. `can_send` станет истинным не сразу: smoltcp сперва разрешит адрес по ARP,
    //    а до этого места в очереди нет — поэтому крутим poll до дедлайна.
    let payload = [0u8; 16];
    let mut sent = false;
    while !sent && sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        let s = sockets.get_mut::<icmp::Socket>(handle);
        if s.can_send() {
            let repr = Icmpv4Repr::EchoRequest { ident: PING_IDENT, seq_no: 1, data: &payload };
            if let Ok(buf) = s.send(repr.buffer_len(), IpAddress::Ipv4(target)) {
                repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &device_checksum(device));
                sent = true;
            }
        }
    }
    if !sent {
        return Err(1);
    }

    // 2) Ждать ответ.
    while sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        let s = sockets.get_mut::<icmp::Socket>(handle);
        if s.can_recv() {
            let _ = s.recv();
            let ticks = sys::now().wrapping_sub(started);
            return Ok(ticks * sys::TICK_NS / 1000);
        }
    }
    Err(2)
}

/// Веха 90 — доказать UDP: минимальный DNS-запрос A-записи `example.com` к серверу SLIRP и
/// ожидание ответа. Возвращает длину ответа либо причину. Полноценного разбора DNS тут нет и не
/// нужно — проверяется ТРАНСПОРТ (запрос ушёл, ответ пришёл на наш порт); резолвер — Веха 92.
fn udp_probe(
    iface: &mut Interface,
    device: &mut sys::net_phy::VoidDevice,
    sockets: &mut SocketSet,
    handle: smoltcp::iface::SocketHandle,
) -> Result<usize, &'static str> {
    // Заголовок DNS: id=0x7601, RD=1, 1 вопрос. Дальше QNAME (7"example" 3"com" 0), QTYPE=A,
    // QCLASS=IN. Собран вручную — ради одной проверки тащить парсер незачем.
    const QUERY: &[u8] = &[
        0x76, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
        0x00, 0x01, 0x00, 0x01,
    ];
    {
        let s = sockets.get_mut::<udp::Socket>(handle);
        if s.bind(49152).is_err() {
            return Err("не удалось занять порт");
        }
    }
    let deadline = sys::net_phy::now() + smoltcp::time::Duration::from_millis(RESOLVE_MS as u64);
    let target = IpEndpoint::new(IpAddress::Ipv4(DNS), 53);
    let mut sent = false;
    while sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        let s = sockets.get_mut::<udp::Socket>(handle);
        if !sent && s.can_send() {
            match s.send_slice(QUERY, target) {
                Ok(()) => sent = true,
                Err(_) => return Err("сокет отказал в отправке")
            }
        }
        if sent && s.can_recv() {
            let n = s.recv().map(|(data, _)| data.len()).unwrap_or(0);
            s.close();
            return Ok(n);
        }
    }
    sockets.get_mut::<udp::Socket>(handle).close();
    if sent { Err("нет ответа на DNS-запрос") } else { Err("не удалось отправить") }
}

/// Возможности устройства по контрольным суммам — нужны `Icmpv4Repr::emit`.
fn device_checksum(
    device: &sys::net_phy::VoidDevice,
) -> smoltcp::phy::ChecksumCapabilities {
    use smoltcp::phy::Device;
    device.capabilities().checksum
}

/// Карты нет: обслуживаем IPC, честно отвечая status=3 (система от этого не встаёт).
fn serve_no_device() -> ! {
    let mut req = [0u8; 64];
    loop {
        let m = sys::recv(&mut req);
        sys::reply(m.reply_cap, &[3u8, 0, 0, 0, 0]);
    }
}

fn write_mac(mac: &[u8; 6]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 17];
    for (i, b) in mac.iter().enumerate() {
        out[i * 3] = HEX[(b >> 4) as usize];
        out[i * 3 + 1] = HEX[(b & 0xf) as usize];
        if i < 5 {
            out[i * 3 + 2] = b':';
        }
    }
    sys::write(&out);
}

fn write_dec(mut v: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    sys::write(&buf[i..]);
}
