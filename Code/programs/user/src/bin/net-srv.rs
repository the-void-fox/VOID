//! Сетевой сервер VOID — стек в userspace (Веха 34 → **Веха 90: настоящий стек smoltcp**).
//!
//! Микроядерность неизменна: ядро отдаёт только сырые кадры (`net_send`/`net_recv`), протоколы
//! живут здесь. Изменилось ЧТО именно их разбирает. До Вехи 90 это были ~200 строк своих
//! ARP+IPv4+ICMP — ровно столько, сколько нужно для `ping`, и ни байтом больше: ни UDP, ни TCP,
//! ни ретрансмиссий. Дальше по дорожной карте нужен TCP, а его с нуля не пишут (ADR 0008),
//! поэтому стек — **vendored smoltcp** с прибитой версией, а наш код сведён к мосту
//! ([`sys::net_phy`]) и политике.
//!
//! **Веха 92 — адрес больше не зашит.** Сервер поднимается БЕЗ адреса и спрашивает его у сети
//! (DHCPv4): адрес/маска, шлюз и DNS приходят от роутера. Статика осталась запасным путём и
//! настраивается в конфиге системы аргументами (`arg:` в `.vv`, см. [[vvsh-config-layout]]):
//! `dhcp=off`, `ip=A.B.C.D/NN`, `gw=A.B.C.D`, `dns=A.B.C.D`. Значения по умолчанию — под SLIRP
//! QEMU (10.0.2.15/24, шлюз 10.0.2.2, DNS 10.0.2.3), чтобы демо работало и без DHCP.
//!
//! Отдельная UDP-проверка Вехи 90 отсюда УБРАНА: сам DHCP — это UDP туда и обратно, причём с
//! широковещанием и без готового адреса. Если аренда получена, транспорт доказан делом, а не
//! отдельным зондом.
//!
//! Протокол IPC (клиент → сервер), ответ всегда 5 байт `[status(1) | payload(4)]`:
//! - `OP_PING`  — нагрузка 4 байта IPv4; payload = RTT в мкс. status 0 — ok, 1 — адрес не
//!   разрешился, 2 — нет ответа, 3 — карты нет.
//! - `OP_RESOLVE` (Веха 92) — нагрузка = имя (UTF-8, без завершающего NUL); payload = A-запись.
//!   status 0 — ok, 1 — имя не разрешилось (NXDOMAIN/нет DNS), 2 — нет ответа за отведённое
//!   время, 3 — карты нет / негодный запрос.

#![no_std]
#![no_main]

use void_user as sys;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::socket::{dhcpv4, dns, icmp, tcp};
use smoltcp::storage::PacketMetadata;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    DnsQueryType, EthernetAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Address,
    Ipv4Cidr,
};

use sys::net_cli::{
    MAX_CHUNK, OP_PING, OP_RESOLVE, OP_TCP_CLOSE, OP_TCP_CONNECT, OP_TCP_RECV, OP_TCP_SEND, ST_BAD,
    ST_EOF, ST_ERR, ST_OK, ST_TIMEOUT,
};

/// Идентификатор наших echo-запросов (ICMP ident) — по нему стек отдаёт нам ответы.
const PING_IDENT: u16 = 0x1D0;

/// Веха 90 — бюджет операции, которой может понадобиться РАЗРЕШИТЬ адрес. Больше секунды не по
/// прихоти: `NeighborCache` в smoltcp глушит ARP-запросы на 1 с после ЛЮБОГО предыдущего (поле
/// `silent_until` общее на весь кэш, а не на адрес). Наш самопинг шлюза при загрузке как раз
/// такой запрос и делает, поэтому следующая операция к НОВОМУ адресу молчит почти всю эту
/// секунду. Бюджет ровно в 1 с давал ложное «нет ответа» — на диагностику этого ушёл целый заход
/// с дампом трафика: в дампе не было даже ARP.
const RESOLVE_MS: u64 = 3000;

/// Бюджет DNS-запроса: дольше пинга, потому что путь длиннее (запрос уходит за пределы машины)
/// и smoltcp сам ретранслирует по своему таймеру.
const DNS_MS: u64 = 5000;

/// Сколько ждать аренду ПРИ ЗАГРУЗКЕ, прежде чем печатать баннер и падать на статику. Это
/// бюджет БАННЕРА, а не DHCP: сокет остаётся в наборе и продолжает пытаться в рабочем цикле —
/// поздняя аренда просто применится позже (и заменит статику).
const DHCP_BOOT_MS: u64 = 6000;

// ── Веха 93: TCP ────────────────────────────────────────────────────────────────────────────
/// Сколько соединений держим одновременно. Каждое стоит своих буферов (см. ниже), а буферы у нас
/// статические — поэтому число фиксировано и невелико.
const MAX_CONN: usize = 4;
/// Приём должен вмещать заметно больше одного ответа IPC: TCP наливает в буфер по мере прихода
/// сегментов, а клиент вычерпывает кусками по [`MAX_CHUNK`]. Маленький rx = маленькое окно =
/// втрое больше round trip'ов на ту же страницу.
const TCP_RX: usize = 8192;
const TCP_TX: usize = 2048;
/// Бюджет рукопожатия. Отвергнутое соединение видно сразу (RST), а вот молчащий адрес — только
/// по этому сроку.
const CONNECT_MS: u64 = 10_000;
/// Бюджет ожидания данных в `recv`. Истёк — отвечаем `ST_TIMEOUT`, соединение живо, клиент может
/// звать снова. Держать клиента вечно нельзя: он ждёт в `SYS_CALL`, а не в своём цикле.
const RECV_MS: u64 = 10_000;
/// Первый эфемерный порт. Растёт по кругу — свежий порт на каждое соединение обязателен, иначе
/// TIME_WAIT прошлого не даст открыть новое к тому же адресу.
const EPHEMERAL_BASE: u16 = 49152;

/// Отложенный запрос: сервер УЖЕ принял его, но ответить пока нечем. Держит одноразовый
/// reply-cap — клиент всё это время стоит в `SYS_CALL`, а цикл сервера продолжает крутиться.
/// Это и есть то, ради чего Веха 93 переделала сервер: до неё ожидание означало блокирующий
/// цикл внутри обработчика, то есть «один клиент за раз».
#[derive(Clone, Copy)]
struct Pending {
    op: usize,
    reply_cap: usize,
    deadline: Instant,
    /// Для `OP_TCP_RECV` — сколько байт запросил клиент (больше отдавать нельзя: лишнее
    /// обрезал бы IPC, и оно пропало бы из потока молча).
    want: usize,
}

/// Состояние слота соединения. Сам сокет живёт в `SocketSet`, здесь — то, чего smoltcp не знает.
#[derive(Clone, Copy)]
struct Conn {
    used: bool,
    /// Клиент попросил закрыть. Слот освободится, когда закрытие доиграет до `Closed` — но
    /// только по ЭТОМУ флагу: если соединение оборвала другая сторона, слот держим, пока клиент
    /// не закроет его сам. Иначе `recv` после обрыва вернул бы «негодный хэндл» вместо EOF.
    closing: bool,
    /// Байты, принятые от клиента, но ещё не влезшие в передающий буфер сокета.
    hold_len: usize,
    pending: Option<Pending>,
}

impl Conn {
    const EMPTY: Conn = Conn { used: false, closing: false, hold_len: 0, pending: None };
}

/// Настройки сервера: значения по умолчанию + то, что переопределил конфиг системы.
struct Cfg {
    dhcp: bool,
    cidr: Ipv4Cidr,
    gw: Option<Ipv4Address>,
    dns: Option<Ipv4Address>,
}

impl Cfg {
    /// Умолчания под SLIRP QEMU — они же запасной путь, если DHCP молчит.
    fn new() -> Self {
        Self {
            dhcp: true,
            cidr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24),
            gw: Some(Ipv4Address::new(10, 0, 2, 2)),
            dns: Some(Ipv4Address::new(10, 0, 2, 3)),
        }
    }

    /// Разобрать argv (`SYS_ARGS(0)`, записи через NUL; [0] — имя программы).
    fn from_args() -> Self {
        let mut cfg = Cfg::new();
        let mut buf = [0u8; 512];
        let n = sys::args(&mut buf);
        for (i, tok) in buf[..n].split(|&b| b == 0).enumerate() {
            if i == 0 || tok.is_empty() {
                continue; // argv[0] — имя программы
            }
            let Some(eq) = tok.iter().position(|&b| b == b'=') else {
                warn_arg(tok);
                continue;
            };
            let (key, val) = (&tok[..eq], &tok[eq + 1..]);
            let ok = match key {
                b"dhcp" => match val {
                    b"off" | b"no" => {
                        cfg.dhcp = false;
                        true
                    }
                    b"on" | b"yes" => {
                        cfg.dhcp = true;
                        true
                    }
                    _ => false,
                },
                b"ip" => match parse_cidr(val) {
                    Some(c) => {
                        cfg.cidr = c;
                        true
                    }
                    None => false,
                },
                b"gw" if matches!(val, b"none") => {
                    cfg.gw = None;
                    true
                }
                b"gw" => match parse_ipv4(val) {
                    Some(a) => {
                        cfg.gw = Some(a);
                        true
                    }
                    None => false,
                },
                b"dns" if matches!(val, b"none") => {
                    cfg.dns = None;
                    true
                }
                b"dns" => match parse_ipv4(val) {
                    Some(a) => {
                        cfg.dns = Some(a);
                        true
                    }
                    None => false,
                },
                _ => false,
            };
            if !ok {
                warn_arg(tok);
            }
        }
        cfg
    }
}

fn warn_arg(tok: &[u8]) {
    sys::write("[net-srv] не понял аргумент '".as_bytes());
    sys::write(tok);
    sys::write("' (жду dhcp=off|on, ip=A.B.C.D/NN, gw=A.B.C.D, dns=A.B.C.D)\n".as_bytes());
}

#[no_mangle]
pub extern "C" fn _start(dev_cap: usize, _a1: usize) -> ! {
    let mut mac = [0u8; 6];
    if sys::net_mac(dev_cap, &mut mac) != 0 {
        // Карты нет — сервер всё равно поднимается, запросы вернут status=3.
        sys::write("[net-srv] карты нет — сеть недоступна\n".as_bytes());
        serve_no_device();
    }
    let cfg = Cfg::from_args();

    sys::write("[net-srv] запущен, MAC ".as_bytes());
    write_mac(&mac);
    sys::write(" (smoltcp)\n".as_bytes());

    let mut device = sys::net_phy::VoidDevice::new(dev_cap);
    let mut config = Config::new(EthernetAddress(mac).into());
    // Сид для выбора эфемерных портов/ISN и XID у DHCP. Часов ещё нет смысла спрашивать — тики.
    config.random_seed = sys::now() as u64;
    // Интерфейс поднимается БЕЗ адреса: его либо назовёт DHCP, либо поставим статикой ниже.
    let mut iface = Interface::new(config, &mut device, sys::net_phy::now());

    // Сокеты и их буферы — статические (smoltcp собран без alloc): по одному кадру на сторону.
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_data = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_data = [0u8; 1024];
    let icmp_socket = icmp::Socket::new(
        icmp::PacketBuffer::new(&mut rx_meta[..], &mut rx_data[..]),
        icmp::PacketBuffer::new(&mut tx_meta[..], &mut tx_data[..]),
    );
    let mut dhcp_socket = dhcpv4::Socket::new();
    // Умолчание smoltcp — повтор DISCOVER раз в 10 с; это дольше нашего бюджета на баннер, и
    // единственный потерянный пакет стоил бы всей загрузки. Два повтора внутри бюджета честнее.
    let mut retry = dhcp_socket.get_retry_config();
    retry.discover_timeout = Duration::from_secs(2);
    dhcp_socket.set_retry_config(retry);
    // Слоты DNS-запросов: наш протокол синхронный (один запрос — один ответ клиенту), но два
    // слота стоят копейки и оставляют место повтору, пока прошлый освобождается.
    let mut queries: [Option<dns::DnsQuery>; 2] = [None, None];
    let dns_socket = dns::Socket::new(&[], &mut queries[..]);
    // Веха 93 — буферы пула TCP. Объявлены ДО `SocketSet`: сокеты одалживают их на всё время
    // жизни набора, значит буферы обязаны его пережить.
    let mut tcp_rx = [[0u8; TCP_RX]; MAX_CONN];
    let mut tcp_tx = [[0u8; TCP_TX]; MAX_CONN];

    let mut storage = [SocketStorage::EMPTY; 3 + MAX_CONN];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let icmp_handle = sockets.add(icmp_socket);
    let dns_handle = sockets.add(dns_socket);
    let dhcp_handle = cfg.dhcp.then(|| sockets.add(dhcp_socket));
    let mut bufs = tcp_rx.iter_mut().zip(tcp_tx.iter_mut());
    let tcp_handles: [SocketHandle; MAX_CONN] = core::array::from_fn(|_| {
        let (rx, tx) = bufs.next().expect("буферов ровно MAX_CONN");
        sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(&mut rx[..]),
            tcp::SocketBuffer::new(&mut tx[..]),
        ))
    });
    let mut conns = [Conn::EMPTY; MAX_CONN];
    // Хвост отправки: то, что клиент отдал, а передающий буфер сокета пока не принял (окно
    // закрыто, другая сторона не подтверждает). Без него пришлось бы отвечать «отправлено 0» и
    // клиент крутил бы вызовы вхолостую.
    let mut hold = [[0u8; MAX_CHUNK]; MAX_CONN];
    let mut next_port: u16 = EPHEMERAL_BASE;

    {
        let s = sockets.get_mut::<icmp::Socket>(icmp_handle);
        if s.bind(icmp::Endpoint::Ident(PING_IDENT)).is_err() {
            sys::write("[net-srv] не удалось открыть ICMP-сокет\n".as_bytes());
        }
    }

    // Аренда: крутим стек до бюджета, пока DHCP-сокет не объявит конфигурацию.
    let mut leased = false;
    if let Some(dh) = dhcp_handle {
        sys::write("[net-srv] DHCP: спрашиваю адрес у сети\n".as_bytes());
        let deadline = sys::net_phy::now() + Duration::from_millis(DHCP_BOOT_MS);
        while sys::net_phy::now() < deadline && !leased {
            iface.poll(sys::net_phy::now(), &mut device, &mut sockets);
            if let Some(ev) = poll_dhcp(&mut sockets, dh) {
                apply_dhcp(&mut iface, &mut sockets, dns_handle, ev, &mut leased);
            }
        }
        if !leased {
            sys::write("[net-srv] DHCP: никто не ответил — беру статику\n".as_bytes());
        }
    }
    if !leased {
        apply_static(&mut iface, &mut sockets, dns_handle, &cfg);
    }

    // Самопроверка на каждой загрузке: пингуем шлюз (в QEMU это SLIRP — не выходя за его пределы).
    if let Some(gw) = default_gateway(&mut iface) {
        sys::write("[net-srv] самопинг шлюза ".as_bytes());
        write_ipv4(gw);
        sys::write(": ".as_bytes());
        match ping(&mut iface, &mut device, &mut sockets, icmp_handle, gw) {
            Ok(rtt) => {
                sys::write("ответ за ".as_bytes());
                write_dec(rtt);
                sys::write(" мкс\n".as_bytes());
            }
            Err(2) => sys::write("нет ICMP-ответа\n".as_bytes()),
            Err(_) => sys::write("адрес не разрешился\n".as_bytes()),
        }
    } else {
        sys::write("[net-srv] шлюза нет — доступна только локальная сеть\n".as_bytes());
    }

    // Серверный цикл (Веха 90 — РЕАКТОР): стек прокачивается постоянно, а IPC разбирается между
    // тиками неблокирующим приёмом. До этого сервер стоял в блокирующем `recv`, и стек тикал
    // только внутри `ping` — для ICMP сходило, для TCP (Веха 93) не сойдёт: ретрансмиссии и
    // таймеры требуют, чтобы `poll` звался всегда, а не когда кто-то попросил.
    let mut req = [0u8; MAX_CHUNK + 8];
    loop {
        let busy = iface.poll(sys::net_phy::now(), &mut device, &mut sockets);
        // Веха 92: аренда не вечна. Роутер может продлить её с ДРУГИМ адресом или отобрать —
        // сокет скажет об этом здесь, и интерфейс переедет на ходу.
        if let Some(dh) = dhcp_handle {
            if let Some(ev) = poll_dhcp(&mut sockets, dh) {
                apply_dhcp(&mut iface, &mut sockets, dns_handle, ev, &mut leased);
            }
        }
        // Веха 93: раздать ответы тем, чьи сокеты дошли до нужного состояния, и прибрать
        // доигравшие закрытия. Это делается ДО сна — иначе клиент ждал бы лишний круг.
        complete_pending(&mut sockets, &tcp_handles, &mut conns, &mut hold);
        // Веха 91 — СОН ВМЕСТО ОПРОСА. Спим в самом `SYS_RECV` (режим с дедлайном): пока никто
        // не зовёт и стеку нечего делать, процесс не занимает процессор вовсе — он заблокирован
        // ядром, а не крутит цикл. Момент пробуждения называет САМ стек: `poll_at` — это время
        // ближайшего таймера (ретрансмиссия, ARP-повтор, продление аренды); проспать его нельзя.
        //
        // Веха 91 (часть 2): сон снимают ТРИ события - запрос клиента, приход кадра (прерывание
        // карты) или названный стеком срок. Прежний потолок в 20 мс был нужен лишь потому, что
        // кадр нас не будил и его надо было успевать заметить опросом; теперь его нет.
        const IDLE_CAP_MS: u64 = 1000;
        let sleep_ms = if busy != smoltcp::iface::PollResult::None {
            0 // стек что-то сделал — возможно, есть ещё работа; проверим сразу
        } else {
            match iface.poll_delay(sys::net_phy::now(), &sockets) {
                Some(d) => d.millis().min(IDLE_CAP_MS),
                None => IDLE_CAP_MS,
            }
        };
        // Веха 93: проспать чужой срок нельзя — иначе клиент, которому вышло время, узна́ет об
        // этом с опозданием на целый круг сна.
        let sleep_ms = sleep_ms.min(pending_delay_ms(&conns, sys::net_phy::now()));
        let Some(m) = (if sleep_ms == 0 {
            sys::try_recv(&mut req)
        } else {
            sys::recv_net(&mut req, (sleep_ms as usize) * (1_000_000 / sys::TICK_NS))
        }) else {
            continue;
        };
        let body = &req[..m.len.min(req.len())];
        match m.op {
            OP_PING if body.len() >= 4 => {
                let target = Ipv4Address::new(body[0], body[1], body[2], body[3]);
                let mut rep = [0u8; 5];
                match ping(&mut iface, &mut device, &mut sockets, icmp_handle, target) {
                    Ok(rtt) => rep[1..5].copy_from_slice(&(rtt as u32).to_le_bytes()),
                    Err(code) => rep[0] = code,
                }
                sys::reply(m.reply_cap, &rep);
            }
            OP_RESOLVE if !body.is_empty() => {
                let mut rep = [0u8; 5];
                match core::str::from_utf8(body) {
                    Ok(name) => {
                        match resolve(&mut iface, &mut device, &mut sockets, dns_handle, name) {
                            Ok(addr) => rep[1..5].copy_from_slice(&addr.octets()),
                            Err(code) => rep[0] = code,
                        }
                    }
                    Err(_) => rep[0] = ST_BAD,
                }
                sys::reply(m.reply_cap, &rep);
            }
            OP_TCP_CONNECT if body.len() >= 6 => op_connect(
                &mut iface, &mut sockets, &tcp_handles, &mut conns, &mut next_port, body,
                m.reply_cap,
            ),
            OP_TCP_SEND if body.len() >= 2 => {
                op_send(&mut sockets, &tcp_handles, &mut conns, &mut hold, body, m.reply_cap)
            }
            OP_TCP_RECV if body.len() >= 3 => {
                op_recv(&mut sockets, &tcp_handles, &mut conns, body, m.reply_cap)
            }
            OP_TCP_CLOSE if !body.is_empty() => {
                op_close(&mut sockets, &tcp_handles, &mut conns, body, m.reply_cap)
            }
            _ => {
                sys::reply(m.reply_cap, &[ST_BAD]);
            }
        }
    }
}

// ── Веха 93 — TCP: обработчики запросов и раздача отложенных ответов ────────────────────────
//
// Общее правило: обработчик либо отвечает СРАЗУ, либо кладёт запрос в `conns[i].pending` и
// молчит. Молчание безопасно — reply-cap одноразовый и живёт в c-space сервера, пока `SYS_REPLY`
// его не исполнит; клиент всё это время стоит в `SYS_CALL`. Именно это и делает сервер
// многоклиентским без единой нити: ждёт КЛИЕНТ, а не цикл сервера.

/// Ответить одним байтом статуса.
fn reply_status(reply_cap: usize, st: u8) {
    sys::reply(reply_cap, &[st]);
}

/// Проверить хэндл клиента: он же индекс слота. `None` — врёт или слот свободен.
fn slot(conns: &[Conn; MAX_CONN], h: u8) -> Option<usize> {
    let i = h as usize;
    (i < MAX_CONN && conns[i].used).then_some(i)
}

/// `OP_TCP_CONNECT`: `[ip(4) | port(2 LE)]` → отложенный `[status | хэндл]`.
fn op_connect(
    iface: &mut Interface,
    sockets: &mut SocketSet,
    handles: &[SocketHandle; MAX_CONN],
    conns: &mut [Conn; MAX_CONN],
    next_port: &mut u16,
    req: &[u8],
    reply_cap: usize,
) {
    let ip = Ipv4Address::new(req[0], req[1], req[2], req[3]);
    let port = u16::from_le_bytes([req[4], req[5]]);
    let Some(i) = conns.iter().position(|c| !c.used) else {
        return reply_status(reply_cap, ST_ERR); // все слоты заняты
    };
    // Свежий локальный порт на каждое соединение обязателен: TIME_WAIT прошлого не дал бы
    // открыть новое к тому же адресу и порту.
    let local = *next_port;
    *next_port = if local >= 65000 { EPHEMERAL_BASE } else { local + 1 };

    let cx = iface.context();
    let s = sockets.get_mut::<tcp::Socket>(handles[i]);
    if s.connect(cx, (IpAddress::Ipv4(ip), port), local).is_err() {
        return reply_status(reply_cap, ST_ERR);
    }
    conns[i] = Conn {
        used: true,
        closing: false,
        hold_len: 0,
        pending: Some(Pending {
            op: OP_TCP_CONNECT,
            reply_cap,
            deadline: sys::net_phy::now() + Duration::from_millis(CONNECT_MS),
            want: 0,
        }),
    };
}

/// `OP_TCP_SEND`: `[хэндл | данные…]` → `[status | принято(2 LE)]`, возможно отложенный.
fn op_send(
    sockets: &mut SocketSet,
    handles: &[SocketHandle; MAX_CONN],
    conns: &mut [Conn; MAX_CONN],
    hold: &mut [[u8; MAX_CHUNK]; MAX_CONN],
    req: &[u8],
    reply_cap: usize,
) {
    let Some(i) = slot(conns, req[0]) else {
        return reply_status(reply_cap, ST_BAD);
    };
    if conns[i].pending.is_some() {
        return reply_status(reply_cap, ST_BAD); // на слоте уже висит запрос
    }
    let data = &req[1..];
    let n = data.len().min(MAX_CHUNK);
    hold[i][..n].copy_from_slice(&data[..n]);
    conns[i].hold_len = n;

    let s = sockets.get_mut::<tcp::Socket>(handles[i]);
    if !s.may_send() {
        conns[i].hold_len = 0;
        return reply_status(reply_cap, ST_EOF); // другая сторона уже не примет
    }
    match push_hold(s, &hold[i], &mut conns[i].hold_len) {
        0 => {
            // Окно закрыто — ждём, пока другая сторона подтвердит принятое. Клиенту вернуть
            // «отправлено 0» было бы приглашением крутить вызовы вхолостую.
            conns[i].pending = Some(Pending {
                op: OP_TCP_SEND,
                reply_cap,
                deadline: sys::net_phy::now() + Duration::from_millis(RECV_MS),
                want: n,
            });
        }
        sent => {
            sys::reply(reply_cap, &[ST_OK, sent as u8, (sent >> 8) as u8]);
        }
    }
}

/// Отдать сокету столько хвоста, сколько влезет; вернуть, сколько ушло, и подвинуть остаток.
fn push_hold(s: &mut tcp::Socket, buf: &[u8; MAX_CHUNK], hold_len: &mut usize) -> usize {
    if *hold_len == 0 {
        return 0;
    }
    match s.send_slice(&buf[..*hold_len]) {
        Ok(sent) => {
            *hold_len -= sent;
            sent
        }
        Err(_) => 0,
    }
}

/// `OP_TCP_RECV`: `[хэндл | сколько(2 LE)]` → `[status | данные…]`, возможно отложенный.
fn op_recv(
    sockets: &mut SocketSet,
    handles: &[SocketHandle; MAX_CONN],
    conns: &mut [Conn; MAX_CONN],
    req: &[u8],
    reply_cap: usize,
) {
    let Some(i) = slot(conns, req[0]) else {
        return reply_status(reply_cap, ST_BAD);
    };
    if conns[i].pending.is_some() {
        return reply_status(reply_cap, ST_BAD);
    }
    let want = (u16::from_le_bytes([req[1], req[2]]) as usize).min(MAX_CHUNK);
    if want == 0 {
        return reply_status(reply_cap, ST_BAD);
    }
    let s = sockets.get_mut::<tcp::Socket>(handles[i]);
    match take_recv(s, want, reply_cap) {
        Some(()) => {}
        None => {
            conns[i].pending = Some(Pending {
                op: OP_TCP_RECV,
                reply_cap,
                deadline: sys::net_phy::now() + Duration::from_millis(RECV_MS),
                want,
            });
        }
    }
}

/// Попробовать ответить на `recv` прямо сейчас. `None` — данных пока нет, но соединение живо.
///
/// Порядок проверок важен: сперва ДАННЫЕ, потом закрытие. После FIN в буфере может лежать
/// непрочитанный хвост, и отдать EOF раньше него значило бы потерять байты.
fn take_recv(s: &mut tcp::Socket, want: usize, reply_cap: usize) -> Option<()> {
    if s.can_recv() {
        let mut rep = [0u8; MAX_CHUNK + 1];
        rep[0] = ST_OK;
        let n = s.recv_slice(&mut rep[1..1 + want]).unwrap_or(0);
        sys::reply(reply_cap, &rep[..1 + n]);
        return Some(());
    }
    if !s.may_recv() {
        reply_status(reply_cap, ST_EOF);
        return Some(());
    }
    None
}

/// `OP_TCP_CLOSE`: `[хэндл]` → `[status]`. Закрытие аккуратное (FIN); слот освободит
/// [`complete_pending`], когда состояние дойдёт до `Closed`.
fn op_close(
    sockets: &mut SocketSet,
    handles: &[SocketHandle; MAX_CONN],
    conns: &mut [Conn; MAX_CONN],
    req: &[u8],
    reply_cap: usize,
) {
    let Some(i) = slot(conns, req[0]) else {
        return reply_status(reply_cap, ST_BAD);
    };
    // Если на слоте висел чужой отложенный запрос — закрыть его отказом, иначе тот клиент
    // остался бы в `SYS_CALL` навсегда.
    if let Some(p) = conns[i].pending.take() {
        reply_status(p.reply_cap, ST_EOF);
    }
    conns[i].closing = true;
    conns[i].hold_len = 0;
    sockets.get_mut::<tcp::Socket>(handles[i]).close();
    reply_status(reply_cap, ST_OK);
}

/// Раздать ответы отложенным запросам, чьи сокеты дошли до нужного состояния, и прибрать
/// слоты доигравших закрытий. Зовётся каждый круг реактора.
fn complete_pending(
    sockets: &mut SocketSet,
    handles: &[SocketHandle; MAX_CONN],
    conns: &mut [Conn; MAX_CONN],
    hold: &mut [[u8; MAX_CHUNK]; MAX_CONN],
) {
    let now = sys::net_phy::now();
    for i in 0..MAX_CONN {
        if !conns[i].used {
            continue;
        }
        let s = sockets.get_mut::<tcp::Socket>(handles[i]);
        let state = s.state();

        if let Some(p) = conns[i].pending {
            let done = match p.op {
                OP_TCP_CONNECT => match state {
                    tcp::State::Established => {
                        sys::reply(p.reply_cap, &[ST_OK, i as u8]);
                        true
                    }
                    // `connect` сразу переводит сокет в SynSent, поэтому Closed здесь — это
                    // отказ (RST) или сброс по нашему же таймауту, а не «ещё не начали».
                    tcp::State::Closed => {
                        conns[i] = Conn::EMPTY;
                        reply_status(p.reply_cap, ST_ERR);
                        true
                    }
                    _ if now > p.deadline => {
                        s.abort();
                        conns[i] = Conn::EMPTY;
                        reply_status(p.reply_cap, ST_TIMEOUT);
                        true
                    }
                    _ => false,
                },
                OP_TCP_SEND => match push_hold(s, &hold[i], &mut conns[i].hold_len) {
                    0 if !s.may_send() => {
                        reply_status(p.reply_cap, ST_EOF);
                        true
                    }
                    0 if now > p.deadline => {
                        reply_status(p.reply_cap, ST_TIMEOUT);
                        true
                    }
                    0 => false,
                    sent => {
                        sys::reply(p.reply_cap, &[ST_OK, sent as u8, (sent >> 8) as u8]);
                        true
                    }
                },
                OP_TCP_RECV => match take_recv(s, p.want, p.reply_cap) {
                    Some(()) => true,
                    None if now > p.deadline => {
                        reply_status(p.reply_cap, ST_TIMEOUT);
                        true
                    }
                    None => false,
                },
                _ => true, // такого быть не может; не держать клиента
            };
            if done {
                conns[i].pending = None;
            }
        }

        // Слот отпускаем ТОЛЬКО после закрытия по просьбе клиента (см. `Conn::closing`).
        if conns[i].closing && conns[i].pending.is_none() && state == tcp::State::Closed {
            conns[i] = Conn::EMPTY;
        }
    }
}

/// Сколько миллисекунд можно спать, не проспав ближайший отложенный срок.
fn pending_delay_ms(conns: &[Conn; MAX_CONN], now: Instant) -> u64 {
    let mut best = u64::MAX;
    for c in conns {
        if let Some(p) = c.pending {
            let left = if p.deadline > now { (p.deadline - now).millis() } else { 0 };
            best = best.min(left);
        }
    }
    best
}

/// Аренда DHCP в виде, который переживает снятие заимствования с сокета. `Event` держит
/// `&mut` на сам сокет, а применять конфигурацию надо к интерфейсу и DNS-сокету — то есть
/// снова к `SocketSet`. Поэтому нужное копируется здесь и заимствование отпускается.
enum Lease {
    Lost,
    Got {
        cidr: Ipv4Cidr,
        router: Option<Ipv4Address>,
        dns: Option<Ipv4Address>,
    },
}

fn poll_dhcp(sockets: &mut SocketSet, handle: SocketHandle) -> Option<Lease> {
    match sockets.get_mut::<dhcpv4::Socket>(handle).poll()? {
        dhcpv4::Event::Deconfigured => Some(Lease::Lost),
        dhcpv4::Event::Configured(c) => Some(Lease::Got {
            cidr: c.address,
            router: c.router,
            dns: c.dns_servers.first().copied(),
        }),
    }
}

/// Применить событие DHCP; `have_addr` — держим ли мы сейчас адрес от DHCP.
///
/// Флаг нужен не для красоты: только что созданный сокет держит «конфигурация изменилась»
/// взведённым, поэтому ПЕРВЫЙ же `poll` отдаёт `Deconfigured` — состояние «адреса ещё нет».
/// Без флага загрузка честной системы начиналась бы со строки «аренда истекла», которой не
/// было. Сообщать надо о потере, а не о том, что мы ещё не начинали.
fn apply_dhcp(
    iface: &mut Interface,
    sockets: &mut SocketSet,
    dns_handle: SocketHandle,
    lease: Lease,
    have_addr: &mut bool,
) {
    match lease {
        Lease::Lost => {
            if *have_addr {
                sys::write("[net-srv] DHCP: аренда истекла — адреса нет\n".as_bytes());
                iface.update_ip_addrs(|a| a.clear());
                iface.routes_mut().remove_default_ipv4_route();
                *have_addr = false;
            }
        }
        Lease::Got { cidr, router, dns } => {
            set_addr(iface, cidr);
            match router {
                Some(r) => {
                    let _ = iface.routes_mut().add_default_ipv4_route(r);
                }
                None => {
                    iface.routes_mut().remove_default_ipv4_route();
                }
            }
            set_dns(sockets, dns_handle, dns);
            sys::write("[net-srv] DHCP: адрес ".as_bytes());
            write_cidr(cidr);
            if let Some(r) = router {
                sys::write(", шлюз ".as_bytes());
                write_ipv4(r);
            }
            if let Some(d) = dns {
                sys::write(", DNS ".as_bytes());
                write_ipv4(d);
            }
            sys::write("\n".as_bytes());
            *have_addr = true;
        }
    }
}

/// Запасной путь: адрес/шлюз/DNS из конфига системы (или зашитых умолчаний).
fn apply_static(iface: &mut Interface, sockets: &mut SocketSet, dns_handle: SocketHandle, cfg: &Cfg) {
    set_addr(iface, cfg.cidr);
    if let Some(gw) = cfg.gw {
        let _ = iface.routes_mut().add_default_ipv4_route(gw);
    }
    set_dns(sockets, dns_handle, cfg.dns);
    sys::write("[net-srv] статика: адрес ".as_bytes());
    write_cidr(cfg.cidr);
    if let Some(gw) = cfg.gw {
        sys::write(", шлюз ".as_bytes());
        write_ipv4(gw);
    }
    if let Some(d) = cfg.dns {
        sys::write(", DNS ".as_bytes());
        write_ipv4(d);
    }
    sys::write("\n".as_bytes());
}

fn set_addr(iface: &mut Interface, cidr: Ipv4Cidr) {
    iface.update_ip_addrs(|addrs| {
        addrs.clear();
        let _ = addrs.push(IpCidr::Ipv4(cidr));
    });
}

fn set_dns(sockets: &mut SocketSet, handle: SocketHandle, server: Option<Ipv4Address>) {
    let s = sockets.get_mut::<dns::Socket>(handle);
    match server {
        Some(a) => s.update_servers(&[IpAddress::Ipv4(a)]),
        None => s.update_servers(&[]),
    }
}

/// Текущий шлюз по умолчанию (для самопинга). Читается из таблицы маршрутов, а не из конфига:
/// после DHCP он мог смениться.
fn default_gateway(iface: &mut Interface) -> Option<Ipv4Address> {
    match iface.routes_mut().get_default_ipv4_route()?.via_router {
        IpAddress::Ipv4(v4) => Some(v4),
    }
}

/// Веха 92 — разрешить имя в A-запись через DNS-сокет smoltcp. Возвращает адрес либо код
/// (1 — имя не разрешилось / DNS-сервер неизвестен, 2 — не ответил за [`DNS_MS`]).
///
/// Запрос асинхронный: `start_query` только заводит слот, а сам обмен делает `iface.poll`.
/// Поэтому здесь тот же приём, что в `ping` — крутим стек до дедлайна. Клиент всё это время
/// стоит в `SYS_CALL`, то есть ждёт ровно свой запрос, а не общий цикл сервера.
fn resolve(
    iface: &mut Interface,
    device: &mut sys::net_phy::VoidDevice,
    sockets: &mut SocketSet,
    handle: SocketHandle,
    name: &str,
) -> Result<Ipv4Address, u8> {
    let query = {
        let cx = iface.context();
        let s = sockets.get_mut::<dns::Socket>(handle);
        s.start_query(cx, name, DnsQueryType::A).map_err(|_| 1u8)?
    };
    let deadline = sys::net_phy::now() + Duration::from_millis(DNS_MS);
    while sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        match sockets.get_mut::<dns::Socket>(handle).get_query_result(query) {
            // Слот уже освобождён самим `get_query_result` — второй раз его трогать нельзя.
            Ok(addrs) => {
                return addrs
                    .iter()
                    .find_map(|a| match a {
                        IpAddress::Ipv4(v4) => Some(*v4),
                    })
                    .ok_or(1)
            }
            Err(dns::GetQueryResultError::Failed) => return Err(1),
            Err(dns::GetQueryResultError::Pending) => {}
        }
    }
    sockets.get_mut::<dns::Socket>(handle).cancel_query(query);
    Err(2)
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
    handle: SocketHandle,
    target: Ipv4Address,
) -> Result<usize, u8> {
    let started = sys::now();
    let deadline = sys::net_phy::now() + Duration::from_millis(RESOLVE_MS);

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
    let mut req = [0u8; 256];
    loop {
        let m = sys::recv(&mut req);
        sys::reply(m.reply_cap, &[3u8, 0, 0, 0, 0]);
    }
}

/// Разобрать «A.B.C.D» (ровно четыре октета).
fn parse_ipv4(s: &[u8]) -> Option<Ipv4Address> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    let mut val: u32 = 0;
    let mut digits = 0;
    for &b in s {
        if b == b'.' {
            if digits == 0 || idx >= 3 {
                return None;
            }
            octets[idx] = val as u8;
            idx += 1;
            val = 0;
            digits = 0;
        } else if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as u32;
            if val > 255 {
                return None;
            }
            digits += 1;
        } else {
            return None;
        }
    }
    if idx != 3 || digits == 0 {
        return None;
    }
    octets[3] = val as u8;
    Some(Ipv4Address::from(octets))
}

/// Разобрать «A.B.C.D/NN» (без `/NN` — маска /24, как в домашних сетях).
fn parse_cidr(s: &[u8]) -> Option<Ipv4Cidr> {
    let (addr, prefix) = match s.iter().position(|&b| b == b'/') {
        Some(i) => {
            let mut n: u32 = 0;
            if s[i + 1..].is_empty() {
                return None;
            }
            for &b in &s[i + 1..] {
                if !b.is_ascii_digit() {
                    return None;
                }
                n = n * 10 + (b - b'0') as u32;
                if n > 32 {
                    return None;
                }
            }
            (&s[..i], n as u8)
        }
        None => (s, 24),
    };
    Some(Ipv4Cidr::new(parse_ipv4(addr)?, prefix))
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

fn write_ipv4(a: Ipv4Address) {
    for (i, o) in a.octets().iter().enumerate() {
        if i > 0 {
            sys::write(b".");
        }
        write_dec(*o as usize);
    }
}

fn write_cidr(c: Ipv4Cidr) {
    write_ipv4(c.address());
    sys::write(b"/");
    write_dec(c.prefix_len() as usize);
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
