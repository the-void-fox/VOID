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
//!   время, 3 — карты нет / негодный запрос, 5 — имя ЗАПРЕЩЕНО политикой (Веха 136).
//!
//! **Веха 136 — резолвер настраивается конфигом поколения** ([[vvsh-config-layout]]), теми же
//! `arg:`-токенами:
//!
//! - `dns=A.B.C.D[,A.B.C.D…]` — до четырёх резолверов по порядку опроса (был один);
//! - `host=имя=A.B.C.D` — своя запись имени: отвечаем сами, не спрашивая сеть;
//! - `block=КОРЕНЬ|CONTENT-ID` — список блокировки («блокировщик рекламы»): объект store с
//!   именами (формат hosts или голые имена), имя из списка закрывает и его поддомены. Читать
//!   его сервису можно, только если конфиг дал право `store:r`.
//!
//! Порядок разбора имени — свои записи, потом список, и лишь потом провод. Список живёт в store
//! объектом, а не файлом сбоку: он тогда неизменяем, адресуется по содержимому, применяется
//! `rebuild`'ом и откатывается вместе с поколением.

#![no_std]
#![no_main]

use void_user as sys;

use core::sync::atomic::{AtomicU16, Ordering};

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
    ST_BLOCKED, ST_EOF, ST_ERR, ST_OK, ST_TIMEOUT,
};

/// Идентификатор наших echo-запросов (ICMP ident) — по нему стек отдаёт нам ответы. Сокету с
/// номером `i` в пуле достаётся `PING_IDENT + i`: одинаковый ident на всех означал бы, что ответ
/// на ЧУЖОЙ запрос примет первый попавшийся сокет.
const PING_IDENT: u16 = 0x1D0;

/// Размер пула сокетов для пинга (Веха 135). Зачем пул — см. `ping`.
///
/// Восемь, а не один, и не сто. Сокет сжигается только на адресе, который НЕ ОТЗЫВАЕТСЯ НА ARP, а
/// такой обязан лежать в нашей же подсети: до всего остального ARP делается к шлюзу, и отвечает
/// шлюз. То есть сжечь пул можно лишь восемью разными несуществующими соседями подряд.
const PING_SOCKETS: usize = 8;

/// Коды отказа пинга (уходят клиенту первым байтом ответа).
const PING_UNRESOLVED: u8 = 1;
const PING_NO_REPLY: u8 = 2;
const PING_EXHAUSTED: u8 = 3;

/// Пул сокетов пинга: `cur` — текущий, всё до него сожжено и из набора удалено.
struct PingPool {
    handles: [SocketHandle; PING_SOCKETS],
    cur: usize,
}

/// Номер очередного echo-запроса. Растёт на каждый пинг (Веха 135).
///
/// Раньше здесь стояла жёсткая единица, а приём брал за ответ ЛЮБОЙ пакет, попавший в сокет:
/// ни отправителя, ни тип, ни номер никто не проверял. Это давало ложь в замере, причём в
/// сторону «всё хорошо». Пинг ушёл, ответ опоздал за бюджет — вернули «нет ответа», но ответ-то
/// пришёл и лёг в буфер приёма. Следующий пинг — хоть бы и на заведомо мёртвый адрес — видел его
/// первым же `can_recv` и рапортовал успех с RTT в считаные микросекунды.
///
/// В SLIRP этого было не увидеть: он отвечает мгновенно и не теряет, так что буфер приёма никогда
/// не бывал непустым к началу следующего пинга. Нашлось на честном сегменте с записью провода
/// (`netlab.py`): в дампе все три запроса шли с `seq 1`.
static PING_SEQ: AtomicU16 = AtomicU16::new(1);

/// Веха 90 — бюджет операции, которой может понадобиться РАЗРЕШИТЬ адрес. Больше секунды не по
/// прихоти: `NeighborCache` в smoltcp глушит ARP-запросы на 1 с после ЛЮБОГО предыдущего (поле
/// `silent_until` общее на весь кэш, а не на адрес). Наш самопинг шлюза при загрузке как раз
/// такой запрос и делает, поэтому следующая операция к НОВОМУ адресу молчит почти всю эту
/// секунду. Бюджет ровно в 1 с давал ложное «нет ответа» — на диагностику этого ушёл целый заход
/// с дампом трафика: в дампе не было даже ARP.
const RESOLVE_MS: u64 = 3000;

/// Бюджет DNS-запроса: дольше пинга, потому что путь длиннее (запрос уходит за пределы машины)
/// и smoltcp сам ретранслирует по своему таймеру.
///
/// Веха 136 — число продиктовано smoltcp, а не вкусом. Измерено на стенде: запрос повторяется
/// через 1 с, потом 2 с (дальше удвоение), а к СЛЕДУЮЩЕМУ серверу стек переходит только по
/// своему сроку в 10 с (`RETRANSMIT_TIMEOUT`, константа вендоренного крейта). Бюджет меньше 10 с
/// означал бы, что второй резолвер не спрашивают НИКОГДА — то есть резерв только на бумаге.
const DNS_MS: u64 = 12_000;

/// Сколько резолверов держим (`DNS_MAX_SERVER_COUNT` в сборке smoltcp — столько же).
const DNS_SERVERS: usize = 4;

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
/// Веха 135.2 — было 8192, и после снятия зажима окна (135.1) именно этот буфер стал потолком
/// скорости: объявляемое окно есть `min(свободно в буфере, max_burst_size × сегмент)`.
///
/// Больше 16 КиБ пока нельзя, и причина не в сети: эти массивы лежат на СТЕКЕ `main`, а стек
/// пользовательского процесса — 64 страницы (256 КиБ, `USER_STACK_PAGES`). 4 × 16 КиБ = 64 КиБ
/// уже заметная его доля. Дальше растить — только вынеся буферы со стека.
const TCP_RX: usize = 16384;
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

// ── Веха 136: политика имён ──────────────────────────────────────────────────────────────────
/// Сколько своих записей «имя → адрес» помещается (мини-hosts из конфига).
const HOSTS_MAX: usize = 16;
/// Потолок длины имени. DNS позволяет 255; столько в наших таблицах не нужно, а память нужна.
const NAME_MAX: usize = 96;
/// Арена имён списка блокировки и число ячеек таблицы (степень двойки — маска вместо деления).
///
/// 192 КиБ вмещают около десяти тысяч имён. Настоящие списки бывают на порядок больше, и
/// **это ограничение честное**: при переполнении сервер печатает, сколько имён взял из скольких,
/// а не делает вид, что взял все. Держать их сжатыми (фильтр Блума) было бы соблазнительно, но
/// его ложные срабатывания — это МОЛЧА заблокированный чужой домен, чего от блокировщика ждать
/// нельзя.
const BLOCK_ARENA: usize = 192 * 1024;
const BLOCK_SLOTS: usize = 16 * 1024;
/// Буфер чтения куска блоба (столько же, сколько кусок у `fetch`).
const BLOB_CHUNK: usize = 16 * 1024;
/// Потолок числа кусков списка (16 КиБ каждый) — 8 МиБ файла.
const BLOB_KIDS: usize = 512;

/// Список блокировки: имена в арене, поиск по открытой адресации.
///
/// Живёт статикой, а не на стеке: стек процесса — 256 КиБ на всё, а таблица заведомо больше.
struct Blocklist {
    names: [u8; BLOCK_ARENA],
    /// Ячейка = смещение имени в арене + 1 (0 — пусто).
    slot: [u32; BLOCK_SLOTS],
    used: usize,
    count: usize,
    /// Сколько имён ВСТРЕТИЛОСЬ в списке (включая не влезшие) — чтобы «взято 10000 из 150000»
    /// было видно, а не подразумевалось.
    seen: usize,
    /// Рабочие буферы чтения из store (тоже статикой, по той же причине).
    chunk: [u8; BLOB_CHUNK],
    kids: [[u8; 32]; BLOB_KIDS],
}

static mut BLOCKLIST: Blocklist = Blocklist {
    names: [0; BLOCK_ARENA],
    slot: [0; BLOCK_SLOTS],
    used: 0,
    count: 0,
    seen: 0,
    chunk: [0; BLOB_CHUNK],
    kids: [[0; 32]; BLOB_KIDS],
};

impl Blocklist {
    /// FNV-1a: короткая, без таблиц и достаточная для рассеивания доменных имён.
    fn hash(name: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in name {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    /// Положить имя (уже в нижнем регистре). `false` — арена или таблица кончились.
    fn insert(&mut self, name: &[u8]) -> bool {
        if name.is_empty() || name.len() > NAME_MAX || self.count * 4 >= BLOCK_SLOTS * 3 {
            return false;
        }
        if self.used + 1 + name.len() > BLOCK_ARENA {
            return false;
        }
        if self.contains(name) {
            return true; // повтор — не ошибка и не место
        }
        let off = self.used;
        self.names[off] = name.len() as u8;
        self.names[off + 1..off + 1 + name.len()].copy_from_slice(name);
        self.used += 1 + name.len();
        let mut i = (Self::hash(name) as usize) & (BLOCK_SLOTS - 1);
        while self.slot[i] != 0 {
            i = (i + 1) & (BLOCK_SLOTS - 1);
        }
        self.slot[i] = off as u32 + 1;
        self.count += 1;
        true
    }

    fn contains(&self, name: &[u8]) -> bool {
        let mut i = (Self::hash(name) as usize) & (BLOCK_SLOTS - 1);
        while self.slot[i] != 0 {
            let off = self.slot[i] as usize - 1;
            let len = self.names[off] as usize;
            if &self.names[off + 1..off + 1 + len] == name {
                return true;
            }
            i = (i + 1) & (BLOCK_SLOTS - 1);
        }
        false
    }

    /// Запрещено ли имя: само или ЛЮБОЙ его родительский домен. Список «example.com» обязан
    /// закрывать и `ads.example.com` — иначе от блокировщика нет толку: рекламные сети раздают
    /// поддомены пачками.
    fn blocks(&self, name: &[u8]) -> bool {
        if self.count == 0 {
            return false;
        }
        let mut start = 0;
        loop {
            if self.contains(&name[start..]) {
                return true;
            }
            match name[start..].iter().position(|&c| c == b'.') {
                // Одна метка («com») в списке — это блокировка всей зоны; такое в списках
                // встречается и делается намеренно, поэтому доходим до конца.
                Some(i) => start += i + 1,
                None => return false,
            }
        }
    }

    /// Разобрать строку списка. Понимает и голое имя, и hosts-формат (`0.0.0.0 ads.example.com`)
    /// — именно в нём раздают готовые списки блокировки.
    fn feed_line(&mut self, line: &[u8]) {
        let line = line.split(|&c| c == b'#').next().unwrap_or(&[]);
        let mut fields = line.split(|c: &u8| c.is_ascii_whitespace()).filter(|s| !s.is_empty());
        let (first, second) = (fields.next(), fields.next());
        let name = match (first, second) {
            // hosts-формат: адрес и имя. Имя — второе поле.
            (Some(_), Some(n)) => n,
            (Some(n), None) => n,
            _ => return,
        };
        // Собственные записи hosts-файлов, которые к блокировке отношения не имеют.
        if matches!(name, b"localhost" | b"localhost.localdomain" | b"broadcasthost" | b"local") {
            return;
        }
        let mut lower = [0u8; NAME_MAX];
        let Some(n) = normalize(name, &mut lower) else { return };
        self.seen += 1;
        self.insert(&lower[..n]);
    }
}

/// Привести имя к нижнему регистру и проверить, что это вообще доменное имя. `None` — не имя
/// (мусорная строка, слишком длинное, посторонние символы).
fn normalize(name: &[u8], out: &mut [u8; NAME_MAX]) -> Option<usize> {
    let name = name.strip_suffix(b".").unwrap_or(name); // «example.com.» — то же имя
    if name.is_empty() || name.len() > NAME_MAX {
        return None;
    }
    for (i, &b) in name.iter().enumerate() {
        let c = b.to_ascii_lowercase();
        if !(c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_') {
            return None;
        }
        out[i] = c;
    }
    Some(name.len())
}

/// Своя запись «имя → адрес»: отвечаем сами, на провод не ходим.
#[derive(Clone, Copy)]
struct Host {
    name: [u8; NAME_MAX],
    len: usize,
    addr: Ipv4Address,
}

/// Настройки сервера: значения по умолчанию + то, что переопределил конфиг системы.
struct Cfg {
    dhcp: bool,
    cidr: Ipv4Cidr,
    gw: Option<Ipv4Address>,
    /// Резолверы по порядку опроса. Веха 136: их может быть несколько — один сервер означает
    /// «резолвер умер = имён нет вовсе».
    dns: [Option<Ipv4Address>; DNS_SERVERS],
    /// Свои записи имён (`arg:host=имя=A.B.C.D`).
    hosts: [Host; HOSTS_MAX],
    nhosts: usize,
    /// Корень store (или content-id) со списком блокировки (`arg:block=…`).
    block: [u8; 96],
    block_len: usize,
}

impl Cfg {
    /// Умолчания под SLIRP QEMU — они же запасной путь, если DHCP молчит.
    fn new() -> Self {
        let mut dns = [None; DNS_SERVERS];
        dns[0] = Some(Ipv4Address::new(10, 0, 2, 3));
        Self {
            dhcp: true,
            cidr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24),
            gw: Some(Ipv4Address::new(10, 0, 2, 2)),
            dns,
            hosts: [Host { name: [0; NAME_MAX], len: 0, addr: Ipv4Address::new(0, 0, 0, 0) };
                HOSTS_MAX],
            nhosts: 0,
            block: [0; 96],
            block_len: 0,
        }
    }

    /// Резолверы, которые надо объявить стеку (в порядке опроса).
    fn dns_list(&self) -> ([IpAddress; DNS_SERVERS], usize) {
        let mut out = [IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)); DNS_SERVERS];
        let mut n = 0;
        for a in self.dns.iter().flatten() {
            out[n] = IpAddress::Ipv4(*a);
            n += 1;
        }
        (out, n)
    }

    /// Найти свою запись имени (`arg:host=…`). Ищется ДО списка блокировки и до сети: это
    /// сознательное «я знаю лучше» владельца машины.
    fn host(&self, name: &[u8]) -> Option<Ipv4Address> {
        self.hosts[..self.nhosts]
            .iter()
            .find(|h| &h.name[..h.len] == name)
            .map(|h| h.addr)
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
                    cfg.dns = [None; DNS_SERVERS];
                    true
                }
                // Веха 136: резолверов может быть несколько через запятую. Один — это
                // «резолвер молчит = имён в системе нет»; на настоящей сети так не живут.
                b"dns" => {
                    let mut list = [None; DNS_SERVERS];
                    let mut n = 0;
                    let mut ok = true;
                    for part in val.split(|&b| b == b',').filter(|s| !s.is_empty()) {
                        match parse_ipv4(part) {
                            Some(a) if n < DNS_SERVERS => {
                                list[n] = Some(a);
                                n += 1;
                            }
                            Some(_) => {
                                sys::write("[net-srv] резолверов больше ".as_bytes());
                                write_dec(DNS_SERVERS);
                                sys::write(" — лишние пропущены\n".as_bytes());
                            }
                            None => ok = false,
                        }
                    }
                    if ok && n > 0 {
                        cfg.dns = list;
                    }
                    ok && n > 0
                }
                // Веха 136 — своя запись имени: `host=имя=A.B.C.D`. Значение содержит второй
                // '=', поэтому режем по нему, а не по первому.
                b"host" => match val.iter().position(|&b| b == b'=') {
                    Some(e) if cfg.nhosts < HOSTS_MAX => {
                        let (n, a) = (&val[..e], &val[e + 1..]);
                        let mut slot = Host {
                            name: [0; NAME_MAX],
                            len: 0,
                            addr: Ipv4Address::new(0, 0, 0, 0),
                        };
                        match (normalize(n, &mut slot.name), parse_ipv4(a)) {
                            (Some(len), Some(addr)) => {
                                slot.len = len;
                                slot.addr = addr;
                                cfg.hosts[cfg.nhosts] = slot;
                                cfg.nhosts += 1;
                                true
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                },
                // Веха 136 — список блокировки: корень store или content-id. Сам список читается
                // позже, когда известно, дали ли нам право на store.
                b"block" if val.len() <= cfg.block.len() => {
                    cfg.block[..val.len()].copy_from_slice(val);
                    cfg.block_len = val.len();
                    true
                }
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
    sys::write(
        "' (жду dhcp=off|on, ip=A.B.C.D/NN, gw=A.B.C.D, dns=A.B.C.D[,A.B.C.D], \
         host=имя=A.B.C.D, block=корень)\n"
            .as_bytes(),
    );
}

#[no_mangle]
pub extern "C" fn _start(dev_cap: usize, _a1: usize) -> ! {
    let mut mac = [0u8; 6];
    if sys::net_mac(dev_cap, &mut mac) != 0 {
        // Карты нет — сервер всё равно поднимается, запросы вернут status=3.
        //
        // Формулировка узкая намеренно (замечание владельца): «карты нет» читалось как заявление
        // обо ВСЕЙ машине, а строкой ниже драйвер в userspace печатал MAC живой карты. Мы знаем
        // ровно одно — ядро нам карты не дало; чужие драйверы к стеку пока не подключены, и это
        // отдельная веха, а не сбой.
        sys::write("[net-srv] ядро не дало карты — стек не поднимаю\n".as_bytes());
        sys::write("[net-srv] (драйверы карт в userspace к стеку ещё не подключены)\n".as_bytes());
        serve_no_device();
    }
    let cfg = Cfg::from_args();

    sys::write("[net-srv] запущен, MAC ".as_bytes());
    write_mac(&mac);
    sys::write(" (smoltcp)\n".as_bytes());

    // Веха 136 — список блокировки. Таблица имён статическая: стек процесса 256 КиБ на всё, а
    // она заведомо больше. `addr_of_mut!` вместо `&mut STATIC` — иначе это ссылка на статик со
    // всеми вытекающими предупреждениями компилятора.
    let blocklist: &'static mut Blocklist = unsafe { &mut *core::ptr::addr_of_mut!(BLOCKLIST) };
    if cfg.block_len > 0 {
        // Право на store у сервиса появляется только если оно ДАНО в конфиге (`store:r`).
        // Отсутствие права — не поломка: список просто не читается, и об этом говорится вслух.
        match sys::cap_named("STORE") {
            Some(store_cap) => {
                let (kept, seen) = load_blocklist(store_cap, &cfg.block[..cfg.block_len], blocklist);
                sys::write("[net-srv] список блокировки: ".as_bytes());
                write_dec(kept);
                if seen > kept {
                    sys::write(" имён из ".as_bytes());
                    write_dec(seen);
                    sys::write(" (остальные не влезли)".as_bytes());
                } else {
                    sys::write(" имён".as_bytes());
                }
                sys::write("\n".as_bytes());
            }
            None => sys::write(
                "[net-srv] список блокировки задан, но права на store нет (нужен store:r)\n"
                    .as_bytes(),
            ),
        }
    }
    if cfg.nhosts > 0 {
        sys::write("[net-srv] своих записей имён: ".as_bytes());
        write_dec(cfg.nhosts);
        sys::write("\n".as_bytes());
    }

    let mut device = sys::net_phy::VoidDevice::new(dev_cap);
    let mut config = Config::new(EthernetAddress(mac).into());
    // Сид для выбора эфемерных портов/ISN и XID у DHCP. Часов ещё нет смысла спрашивать — тики.
    config.random_seed = sys::now() as u64;
    // Интерфейс поднимается БЕЗ адреса: его либо назовёт DHCP, либо поставим статикой ниже.
    let mut iface = Interface::new(config, &mut device, sys::net_phy::now());

    // Сокеты и их буферы — статические (smoltcp собран без alloc): по одному кадру на сторону.
    // ICMP — пул из PING_SOCKETS штук (Веха 135, см. `ping`); буферы объявлены ДО `SocketSet`,
    // как и у TCP: сокеты одалживают их на всё время жизни набора.
    let mut icmp_rx_meta = [[PacketMetadata::EMPTY; 4]; PING_SOCKETS];
    let mut icmp_rx_data = [[0u8; 256]; PING_SOCKETS];
    let mut icmp_tx_meta = [[PacketMetadata::EMPTY; 4]; PING_SOCKETS];
    let mut icmp_tx_data = [[0u8; 256]; PING_SOCKETS];
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

    let mut storage = [SocketStorage::EMPTY; 2 + PING_SOCKETS + MAX_CONN];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let mut icmp_bufs = icmp_rx_meta
        .iter_mut()
        .zip(icmp_rx_data.iter_mut())
        .zip(icmp_tx_meta.iter_mut())
        .zip(icmp_tx_data.iter_mut());
    let icmp_handles: [SocketHandle; PING_SOCKETS] = core::array::from_fn(|_| {
        let (((rm, rd), tm), td) = icmp_bufs.next().expect("буферов ровно PING_SOCKETS");
        sockets.add(icmp::Socket::new(
            icmp::PacketBuffer::new(&mut rm[..], &mut rd[..]),
            icmp::PacketBuffer::new(&mut tm[..], &mut td[..]),
        ))
    });
    let mut pings = PingPool { handles: icmp_handles, cur: 0 };
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

    for (i, h) in pings.handles.iter().enumerate() {
        let s = sockets.get_mut::<icmp::Socket>(*h);
        if s.bind(icmp::Endpoint::Ident(PING_IDENT + i as u16)).is_err() {
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
            // СПАТЬ между попытками, а не крутить процессор (Веха 132.4). Здесь был глухой цикл
            // на все шесть секунд, и в QEMU это ничего не стоило: SLIRP отвечает на DHCP сразу,
            // выход происходил на первом же обороте. На машине, где ответа нет, цикл выбирал свои
            // шесть секунд целиком — а планировщик по кругу отдавал ему половину времени. Это и
            // была «вялость первых секунд после загрузки», которую владелец видел всегда и
            // считал свойством системы.
            //
            // Сколько спать, называет САМ стек: `poll_delay` — срок его ближайшего таймера
            // (ретрансмиссия DHCP-запроса). Проспать его нельзя, спать дольше незачем.
            let nap = match iface.poll_delay(sys::net_phy::now(), &sockets) {
                Some(d) => d.millis().clamp(1, 200),
                None => 50,
            };
            sys::sleep_ns(nap * 1_000_000);
        }
        if !leased {
            sys::write("[net-srv] DHCP: никто не ответил — беру статику\n".as_bytes());
            // Веха 136 — вернуть СПОКОЙНЫЙ период повтора. Ускоренный (2 с) заведён ради
            // загрузки: в бюджет баннера обязаны влезть два DISCOVER, иначе единственный
            // потерянный пакет стоил бы всей загрузки. Дальше он вреден — на сегменте без
            // DHCP-сервера машина вещала бы широковещательный DISCOVER каждые две секунды до
            // самого выключения. Видно это стало только на честном проводе: SLIRP отвечает
            // сразу, и повтора там не бывает вовсе.
            let s = sockets.get_mut::<dhcpv4::Socket>(dh);
            let mut retry = s.get_retry_config();
            retry.discover_timeout = Duration::from_secs(10);
            s.set_retry_config(retry);
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
        match ping(&mut iface, &mut device, &mut sockets, &mut pings, gw) {
            Ok(rtt) => {
                sys::write("ответ за ".as_bytes());
                write_dec(rtt);
                sys::write(" мкс\n".as_bytes());
            }
            Err(PING_NO_REPLY) => sys::write("нет ICMP-ответа\n".as_bytes()),
            Err(PING_EXHAUSTED) => sys::write("сокеты пинга кончились\n".as_bytes()),
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
            sys::recv_net(&mut req, sys::ns_to_ticks(sleep_ms * 1_000_000) as usize)
        }) else {
            continue;
        };
        let body = &req[..m.len.min(req.len())];
        match m.op {
            OP_PING if body.len() >= 4 => {
                let target = Ipv4Address::new(body[0], body[1], body[2], body[3]);
                let mut rep = [0u8; 5];
                match ping(&mut iface, &mut device, &mut sockets, &mut pings, target) {
                    Ok(rtt) => rep[1..5].copy_from_slice(&(rtt as u32).to_le_bytes()),
                    Err(code) => rep[0] = code,
                }
                sys::reply(m.reply_cap, &rep);
            }
            OP_RESOLVE if !body.is_empty() => {
                let mut rep = [0u8; 5];
                match core::str::from_utf8(body) {
                    Ok(name) => {
                        match resolve(
                            &mut iface, &mut device, &mut sockets, dns_handle, &cfg, blocklist,
                            name,
                        ) {
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
        /// Веха 136 — резолверов от роутера обычно ДВА, и раньше второй молча выбрасывался
        /// (`dns_servers.first()`). Берём всех, кого дали.
        dns: [Option<Ipv4Address>; DNS_SERVERS],
        ndns: usize,
    },
}

fn poll_dhcp(sockets: &mut SocketSet, handle: SocketHandle) -> Option<Lease> {
    match sockets.get_mut::<dhcpv4::Socket>(handle).poll()? {
        dhcpv4::Event::Deconfigured => Some(Lease::Lost),
        dhcpv4::Event::Configured(c) => {
            let mut dns = [None; DNS_SERVERS];
            let mut ndns = 0;
            for a in c.dns_servers.iter().take(DNS_SERVERS) {
                dns[ndns] = Some(*a);
                ndns += 1;
            }
            Some(Lease::Got { cidr: c.address, router: c.router, dns, ndns })
        }
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
        Lease::Got { cidr, router, dns, ndns } => {
            set_addr(iface, cidr);
            match router {
                Some(r) => {
                    let _ = iface.routes_mut().add_default_ipv4_route(r);
                }
                None => {
                    iface.routes_mut().remove_default_ipv4_route();
                }
            }
            let mut list = [IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)); DNS_SERVERS];
            for (i, a) in dns.iter().flatten().enumerate() {
                list[i] = IpAddress::Ipv4(*a);
            }
            set_dns(sockets, dns_handle, &list[..ndns]);
            sys::write("[net-srv] DHCP: адрес ".as_bytes());
            write_cidr(cidr);
            if let Some(r) = router {
                sys::write(", шлюз ".as_bytes());
                write_ipv4(r);
            }
            for (i, d) in dns.iter().flatten().enumerate() {
                sys::write(if i == 0 { ", DNS ".as_bytes() } else { ", ".as_bytes() });
                write_ipv4(*d);
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
    let (list, n) = cfg.dns_list();
    set_dns(sockets, dns_handle, &list[..n]);
    sys::write("[net-srv] статика: адрес ".as_bytes());
    write_cidr(cfg.cidr);
    if let Some(gw) = cfg.gw {
        sys::write(", шлюз ".as_bytes());
        write_ipv4(gw);
    }
    for (i, d) in cfg.dns.iter().flatten().enumerate() {
        sys::write(if i == 0 { ", DNS ".as_bytes() } else { ", ".as_bytes() });
        write_ipv4(*d);
    }
    sys::write("\n".as_bytes());
}

fn set_addr(iface: &mut Interface, cidr: Ipv4Cidr) {
    iface.update_ip_addrs(|addrs| {
        addrs.clear();
        let _ = addrs.push(IpCidr::Ipv4(cidr));
    });
}

fn set_dns(sockets: &mut SocketSet, handle: SocketHandle, servers: &[IpAddress]) {
    sockets.get_mut::<dns::Socket>(handle).update_servers(servers);
}

/// Текущий шлюз по умолчанию (для самопинга). Читается из таблицы маршрутов, а не из конфига:
/// после DHCP он мог смениться.
fn default_gateway(iface: &mut Interface) -> Option<Ipv4Address> {
    match iface.routes_mut().get_default_ipv4_route()?.via_router {
        IpAddress::Ipv4(v4) => Some(v4),
    }
}

/// Веха 136 — прочитать список блокировки из store. `spec` — имя корня либо content-id (64
/// hex-символа). Возвращает `(взято, встретилось)`.
///
/// Почему из store, а не из файла: список — это ДАННЫЕ ПОКОЛЕНИЯ. Положенный `fetch`'ем объект
/// неизменяем и адресуется по содержимому, конфиг называет его, `rebuild` применяет, откат
/// поколения возвращает прежний. Файл в posixfs дал бы изменяемое состояние сбоку от системы —
/// ровно то, чего VOID не делает ([[0002-persistent-content-addressed-capability-core]]).
///
/// Формат — какой раздают в интернете: строки `0.0.0.0 имя` (hosts) или голые имена, `#` —
/// комментарий.
fn load_blocklist(store_cap: usize, spec: &[u8], bl: &mut Blocklist) -> (usize, usize) {
    let mut id = [0u8; 32];
    if spec.len() == 64 && spec.iter().all(|b| b.is_ascii_hexdigit()) {
        // Content-id прямо в конфиге: список прибит НАВСЕГДА к своему содержимому. Корень удобнее
        // (его можно переназначить новой загрузкой), id — строже (его нельзя подменить).
        let hex = |c: u8| match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => c - b'A' + 10,
        };
        for (i, pair) in spec.chunks(2).enumerate() {
            id[i] = hex(pair[0]) << 4 | hex(pair[1]);
        }
    } else if sys::obj_get_root(store_cap, spec, &mut id) != 32 {
        sys::write("[net-srv] список блокировки: нет такого корня\n".as_bytes());
        return (0, 0);
    }

    // Объект бывает двух видов: БЛОБ (`fetch` режет всё длиннее 16 КиБ на куски и связывает их
    // узлом — так приезжают настоящие списки) или простой объект, положенный целиком.
    let (n, total) = sys::obj_get_ex(store_cap, &id, &mut bl.chunk);
    if n == 0 || n == usize::MAX {
        sys::write("[net-srv] список блокировки: объект не читается\n".as_bytes());
        return (0, 0);
    }
    let mut lines = Lines::new();
    match sys::http::blob_info(&bl.chunk[..n]) {
        Some((_, nchunks, _)) => {
            if nchunks > BLOB_KIDS {
                sys::write("[net-srv] список длиннее 8 МиБ — беру начало\n".as_bytes());
            }
            let want = nchunks.min(BLOB_KIDS);
            let got = sys::obj_children(store_cap, &id, &mut bl.kids[..want]);
            if got == 0 || got == usize::MAX {
                sys::write("[net-srv] список блокировки: куски не читаются\n".as_bytes());
                return (0, 0);
            }
            for i in 0..got.min(want) {
                let kid = bl.kids[i];
                let len = sys::obj_get(store_cap, &kid, &mut bl.chunk);
                if len == 0 || len == usize::MAX {
                    break;
                }
                // По байту, а не слайсом: буфер куска лежит В ТОМ ЖЕ `bl`, что и таблица имён, и
                // одолжить его целиком нельзя. Байт копируется — заимствование не держится.
                for j in 0..len {
                    lines.push(bl.chunk[j], bl);
                }
            }
        }
        None => {
            if total > bl.chunk.len() {
                sys::write("[net-srv] список не влез целиком — беру начало\n".as_bytes());
            }
            for j in 0..n.min(bl.chunk.len()) {
                lines.push(bl.chunk[j], bl);
            }
        }
    }
    lines.flush(bl); // последняя строка могла остаться без перевода строки
    (bl.count, bl.seen)
}

/// Сборка строк из потока байт: список приезжает кусками по 16 КиБ, и имя не обязано уложиться
/// в границу куска.
struct Lines {
    buf: [u8; NAME_MAX * 2],
    len: usize,
    /// Строка длиннее буфера — выбрасываем её целиком, а не берём огрызок: огрызок имени это
    /// ЧУЖОЕ имя, и заблокировать его было бы хуже, чем пропустить строку.
    overflow: bool,
}

impl Lines {
    fn new() -> Self {
        Self { buf: [0; NAME_MAX * 2], len: 0, overflow: false }
    }

    fn push(&mut self, byte: u8, bl: &mut Blocklist) {
        if byte == b'\n' {
            self.flush(bl);
        } else if self.len < self.buf.len() {
            self.buf[self.len] = byte;
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }

    fn flush(&mut self, bl: &mut Blocklist) {
        if !self.overflow && self.len > 0 {
            let (buf, len) = (self.buf, self.len);
            bl.feed_line(&buf[..len]);
        }
        self.len = 0;
        self.overflow = false;
    }
}


/// Веха 92 — разрешить имя в A-запись через DNS-сокет smoltcp. Возвращает адрес либо код
/// (1 — имя не разрешилось / DNS-сервер неизвестен, 2 — не ответил за [`DNS_MS`],
/// [`ST_BLOCKED`] — запрещено политикой).
///
/// Веха 136 — ПОЛИТИКА идёт до провода, и порядок в ней не случаен:
/// 1. **своя запись** (`arg:host=…`) — прямое «я знаю лучше» владельца машины;
/// 2. **список блокировки** — отказ с отдельным кодом, чтобы «заблокировано» не выглядело
///    поломкой сети;
/// 3. и только потом вопрос резолверу.
///
/// Запрос асинхронный: `start_query` только заводит слот, а сам обмен делает `iface.poll`.
/// Поэтому здесь тот же приём, что в `ping` — крутим стек до дедлайна. Клиент всё это время
/// стоит в `SYS_CALL`, то есть ждёт ровно свой запрос, а не общий цикл сервера.
fn resolve(
    iface: &mut Interface,
    device: &mut sys::net_phy::VoidDevice,
    sockets: &mut SocketSet,
    handle: SocketHandle,
    cfg: &Cfg,
    bl: &Blocklist,
    name: &str,
) -> Result<Ipv4Address, u8> {
    let mut lower = [0u8; NAME_MAX];
    if let Some(n) = normalize(name.as_bytes(), &mut lower) {
        if let Some(addr) = cfg.host(&lower[..n]) {
            return Ok(addr);
        }
        if bl.blocks(&lower[..n]) {
            return Err(ST_BLOCKED);
        }
    }
    let query = {
        let cx = iface.context();
        let s = sockets.get_mut::<dns::Socket>(handle);
        s.start_query(cx, name, DnsQueryType::A).map_err(|_| 1u8)?
    };
    let since = sys::net_phy::now();
    let deadline = since + Duration::from_millis(DNS_MS);
    while sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        wait_step(iface, sockets, since);
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
/// Пинг. Отдельный сокет из пула — не блажь, а обход свойства smoltcp (Веха 135).
///
/// Пакет, для которого не разрешился сосед, smoltcp НЕ ВЫБРАСЫВАЕТ: он остаётся головой очереди
/// передачи сокета навсегда (`dispatch` возвращает ошибку, а `dequeue_with` при ошибке оставляет
/// запись). Публичного способа очистить очередь передачи у сокета нет.
///
/// Что это давало на одном общем сокете. Один пинг по несуществующему адресу СВОЕЙ подсети — и
/// сетевая служба ломалась насовсем: очередь передачи навсегда занята, все последующие пинги
/// (уже по живым адресам) вставали за ним и не уходили на провод вовсе, а машина до перезагрузки
/// молотила ARP-запросами по мёртвому адресу. Замерено: 186 ARP за полторы минуты, три пинга по
/// заведомо живому шлюзу подряд — ни одного ICMP-пакета на проводе.
///
/// Лечение: сокет, в котором пакет застрял, СПИСЫВАЕТСЯ из набора (`SocketSet::remove` — вместе с
/// сокетом уходит и застрявший пакет, и ARP-долбёжка), а пинг переходит на следующий из пула.
/// Пул конечен, и когда он кончится, служба скажет об этом прямо, а не соврёт «нет ответа».
fn ping(
    iface: &mut Interface,
    device: &mut sys::net_phy::VoidDevice,
    sockets: &mut SocketSet,
    pool: &mut PingPool,
    target: Ipv4Address,
) -> Result<usize, u8> {
    let started = sys::now();
    let deadline = sys::net_phy::now() + Duration::from_millis(RESOLVE_MS);

    if pool.cur >= PING_SOCKETS {
        return Err(PING_EXHAUSTED);
    }
    let handle = pool.handles[pool.cur];
    let ident = PING_IDENT + pool.cur as u16;

    // 1) Отправить. `can_send` станет истинным не сразу: smoltcp сперва разрешит адрес по ARP,
    //    а до этого места в очереди нет — поэтому крутим poll до дедлайна.
    let payload = [0u8; 16];
    let seq_no = PING_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut sent = false;
    let since = sys::net_phy::now();
    while !sent && sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        wait_step(iface, sockets, since);
        let s = sockets.get_mut::<icmp::Socket>(handle);
        if s.can_send() {
            let repr = Icmpv4Repr::EchoRequest { ident, seq_no, data: &payload };
            if let Ok(buf) = s.send(repr.buffer_len(), IpAddress::Ipv4(target)) {
                repr.emit(&mut Icmpv4Packet::new_unchecked(buf), &device_checksum(device));
                sent = true;
            }
        }
    }
    if !sent {
        return Err(PING_UNRESOLVED);
    }

    // 2) Ждать ответ ИМЕННО НА ЭТОТ запрос. Всё, что не совпало (опоздавший ответ на прошлый
    //    пинг, ICMP-ошибка, чужой echo), выбрасываем и ждём дальше — иначе засчитаем чужое за
    //    своё и соврём про RTT.
    let checksum = device_checksum(device);
    let mut answered = None;
    let since = sys::net_phy::now();
    while answered.is_none() && sys::net_phy::now() < deadline {
        iface.poll(sys::net_phy::now(), device, sockets);
        wait_step(iface, sockets, since);
        loop {
            let s = sockets.get_mut::<icmp::Socket>(handle);
            if !s.can_recv() {
                break;
            }
            let ours = match s.recv() {
                Ok((bytes, from)) => is_our_reply(bytes, from, target, ident, seq_no, &checksum),
                Err(_) => false,
            };
            if ours {
                answered = Some(sys::now().wrapping_sub(started));
                break;
            }
        }
    }

    // 3) Ушёл ли запрос ВООБЩЕ. Непустая очередь передачи по истечении бюджета означает, что
    //    сосед так и не разрешился и пакет застрял навсегда — сокет придётся списать.
    if sockets.get_mut::<icmp::Socket>(handle).send_queue() > 0 {
        sockets.remove(handle);
        pool.cur += 1;
        sys::write("[net-srv] адрес не отзывается на ARP, сокет пинга списан (осталось ".as_bytes());
        write_dec(PING_SOCKETS - pool.cur);
        sys::write(")\n".as_bytes());
        return Err(PING_UNRESOLVED);
    }

    match answered {
        // Веха 136: тики → микросекунды по ИЗМЕРЕННОЙ таймбазе. С прежней константой RTT на этой
        // машине печатался втрое больше настоящего — цифра выглядела правдоподобной и потому
        // никого не настораживала.
        Some(ticks) => Ok(sys::ticks_to_ns(ticks as u64) as usize / 1000),
        None => Err(PING_NO_REPLY),
    }
}

/// Веха 136 — шаг ожидания внутри блокирующей операции (`ping`, `resolve`): сперва крутим стек,
/// потом СПИМ.
///
/// Зачем не спать сразу: `ping` меряет RTT, и сон в 10 мс превратил бы честные 400 мкс в 10 мс.
/// Зачем вообще спать: без этого ожидание — глухой цикл на весь бюджет (у DNS это 5 секунд), а
/// планировщик по кругу отдаёт такому циклу половину времени машины. Ровно этим была «вялость
/// первых секунд после загрузки» (Веха 132.4) — там же, в этом файле, только в цикле DHCP.
///
/// Плата за сон честная: кадр, пришедший во время сна, замечаем не сразу, а до 10 мс спустя.
/// Поэтому первые [`SPIN_MS`] и не спим — быстрый ответ ловится с прежней точностью.
fn wait_step(iface: &mut Interface, sockets: &SocketSet, since: Instant) {
    const SPIN_MS: u64 = 20;
    const NAP_CAP_MS: u64 = 10;
    let now = sys::net_phy::now();
    if now - since < Duration::from_millis(SPIN_MS) {
        return;
    }
    let nap = match iface.poll_delay(now, sockets) {
        Some(d) => d.millis().clamp(1, NAP_CAP_MS),
        None => NAP_CAP_MS,
    };
    sys::sleep_ns(nap * 1_000_000);
}

/// Ответ ли это на наш запрос: echo-reply от того, кого спрашивали, с нашим ident и номером.
/// `Icmpv4Repr::parse` заодно сверяет контрольную сумму, так что битый пакет за ответ не сойдёт.
fn is_our_reply(
    bytes: &[u8],
    from: IpAddress,
    target: Ipv4Address,
    ident: u16,
    seq_no: u16,
    checksum: &smoltcp::phy::ChecksumCapabilities,
) -> bool {
    if from != IpAddress::Ipv4(target) {
        return false;
    }
    let Ok(packet) = Icmpv4Packet::new_checked(bytes) else {
        return false;
    };
    matches!(
        Icmpv4Repr::parse(&packet, checksum),
        Ok(Icmpv4Repr::EchoReply { ident: got_id, seq_no: got_seq, .. })
            if got_id == ident && got_seq == seq_no
    )
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
