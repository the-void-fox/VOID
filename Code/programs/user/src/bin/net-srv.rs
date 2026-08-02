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
use smoltcp::socket::icmp;
use smoltcp::storage::PacketMetadata;
use smoltcp::wire::{
    EthernetAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address,
};

const OP_PING: usize = 0;

const OUR_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

/// Идентификатор наших echo-запросов (ICMP ident) — по нему стек отдаёт нам ответы.
const PING_IDENT: u16 = 0x1D0;

/// Бюджет ожидания ответа, мс.
const TIMEOUT_MS: i64 = 1000;

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
    let mut storage = [SocketStorage::EMPTY; 4];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let icmp_handle = sockets.add(icmp_socket);

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

    // Серверный цикл: обслуживаем OP_PING по IPC.
    let mut req = [0u8; 64];
    loop {
        let m = sys::recv(&mut req);
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
    let deadline = sys::net_phy::now() + smoltcp::time::Duration::from_millis(TIMEOUT_MS as u64);

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
