//! `httpsc` — HTTPS-клиент VOID (Веха 95): TLS 1.2/1.3 поверх нашего TCP, тело — потоком в store.
//!
//! Запуск: `httpsc URL КОРЕНЬ`. Печатает итог и кладёт тело под указанный корень store тем же
//! деревом кусков, что и HTTP Вехи 94 ([`sys::http`]) — чтобы скачанное по http и по https было
//! неотличимо для всего, что дальше с ним работает.
//!
//! **Почему отдельная программа, а не команда шелла.** TLS — это 105 вендоренных крейтов чужого
//! кода. Запускать их с полномочиями `vvsh` (весь store на запись, posixfs, сеть) значит отдать
//! им всё, чем владеет шелл. Здесь же процесс получает ровно два права: эндпоинт сети и store.
//! Это ровно тот довод, ради которого VOID и построен на capability, — грех было им не
//! воспользоваться в первом же случае, когда чужого кода стало много.
//!
//! **Криптография чисто на Rust** ([[0008-network-stack]]): провайдер `rustls-rustcrypto` вместо
//! `ring`/`aws-lc-rs` — в тех C и ассемблер, а у нас нет ни libc, ни заготовок под VOID.
//!
//! **`no_std`-режим rustls** даёт «небуферизованный» API: библиотека не знает про сокеты и просит
//! у нас байты, а мы возим их через IPC к `net-srv`. Для нас это удобнее буферизованного —
//! сокета с `Read`/`Write` у нас всё равно нет.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, UnbufferedStatus};
use rustls::{ClientConfig, RootCertStore};

use void_user as sys;
use void_user::net_cli;

// Куча: TLS держит корневые сертификаты, разобранную цепочку и буферы записей. 8 МиБ с запасом —
// арена ленивая (`SYS_MAP`), неиспользованные страницы не стоят ничего.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 8 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Буфер исходящих TLS-байт. Одна запись TLS ≤ 16 КиБ плюс заголовки; берём с запасом, чтобы
/// `EncodeError::InsufficientSize` не случался на обычном рукопожатии.
const OUT_CAP: usize = 24 * 1024;
/// Потолок роста буфера входящих. Рукопожатие с длинной цепочкой сертификатов бывает объёмным.
const IN_CAP: usize = 64 * 1024;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let mut argbuf = [0u8; 512];
    let n = sys::args(&mut argbuf);
    let mut it = argbuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1);
    let (Some(url), Some(root)) = (it.next(), it.next()) else {
        sys::write("httpsc: нужно два аргумента — URL и имя корня store\n".as_bytes());
        sys::exit(2);
    };
    // `-q` — молчать при успехе (Веха 107). Нужно тому, кто зовёт нас в цикле: `pkg` тянет
    // замыкание из десятка путей, и отчёт о каждой загрузке тонул бы посреди отчёта о пакете.
    // Об ошибке говорим ВСЕГДА: молчать о ней — совсем другое дело.
    let quiet = it.next() == Some(b"-q");

    match run(url, root) {
        Ok((bytes, chunks, id)) => {
            if !quiet {
                sys::write("скачано ".as_bytes());
                write_dec(bytes);
                sys::write(" байт, кусков ".as_bytes());
                write_dec(chunks);
                sys::write(", корень ".as_bytes());
                sys::write(root);
                sys::write(b"\n");
                write_hex(&id);
                sys::write(b"\n");
            }
            sys::exit(0);
        }
        Err(e) => {
            sys::write("httpsc: ".as_bytes());
            sys::write(e.as_bytes());
            sys::write(b"\n");
            sys::exit(1);
        }
    }
}

/// Время для проверки сроков сертификатов — из часов системы (Веха 86).
///
/// Это не формальность: без верного времени проверка цепочки либо отвергает всё живое, либо
/// принимает давно отозванное. Часы у нас идут от RTC прошивки, а `SYS_TIME` отдаёт UTC.
#[derive(Debug)]
struct SystemTime;

impl TimeProvider for SystemTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(core::time::Duration::from_secs(
            sys::time_ns() / 1_000_000_000,
        )))
    }
}

fn run(url: &[u8], root: &[u8]) -> Result<(usize, usize, [u8; 32]), &'static str> {
    // Без сильного источника случайности ключи сеанса предсказуемы, а TLS превращается в театр.
    // Ядро знает, что у него есть; спрашиваем прямо и отказываемся, а не «работаем как-нибудь».
    if !sys::random_is_strong() {
        return Err("нет криптографического источника случайности — TLS отказано");
    }

    let (host, port, path) = parse_url(url)?;
    let net_ep = sys::start_cap(2);
    if net_ep == sys::NO_CAP {
        return Err("сети нет (net.vv = #f?)");
    }
    let store_cap = sys::start_cap(1);

    let host_str = core::str::from_utf8(host).map_err(|_| "имя хоста не UTF-8")?;
    let ip = match parse_ipv4(host) {
        Some(ip) => ip,
        None => resolve(net_ep, host_str)?,
    };

    let mut conn = new_connection(host_str)?;
    let h = net_cli::tcp_connect(net_ep, ip, port).map_err(|_| "не удалось соединиться")?;
    let r = session(&mut conn, net_ep, store_cap, h, host, path, root);
    net_cli::tcp_close(net_ep, h);
    r
}

/// Собрать клиентскую конфигурацию: корни `webpki-roots`, провайдер RustCrypto, наши часы.
fn new_connection(host: &str) -> Result<UnbufferedClientConnection, &'static str> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let cfg = ClientConfig::builder_with_details(
        Arc::new(rustls_rustcrypto::provider()),
        Arc::new(SystemTime),
    )
    .with_safe_default_protocol_versions()
    .map_err(|_| "не удалось выбрать версии протокола")?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let name = ServerName::try_from(host)
        .map_err(|_| "негодное имя хоста для TLS")?
        .to_owned();
    UnbufferedClientConnection::new(Arc::new(cfg), name).map_err(|_| "не удалось начать сеанс TLS")
}

/// Провести рукопожатие, отправить GET и сложить тело в store.
///
/// Небуферизованный API rustls — это конечный автомат: он говорит, что ему нужно (закодировать
/// запись, передать её, дождаться данных), а транспорт наш. Отсюда цикл ниже.
fn session(
    conn: &mut UnbufferedClientConnection,
    net_ep: usize,
    store_cap: usize,
    h: u8,
    host: &[u8],
    path: &[u8],
    root: &[u8],
) -> Result<(usize, usize, [u8; 32]), &'static str> {
    let mut outgoing = vec![0u8; OUT_CAP];
    let mut incoming: Vec<u8> = Vec::with_capacity(8192);
    let mut request_sent = false;
    let mut body = Body::new(store_cap, root);
    let mut http = HttpParse::new();

    loop {
        let UnbufferedStatus { discard, state } = conn.process_tls_records(&mut incoming);
        let state = state.map_err(tls_error_text)?;

        match state {
            ConnectionState::EncodeTlsData(mut s) => {
                let n = match s.encode(&mut outgoing) {
                    Ok(n) => n,
                    Err(EncodeError::InsufficientSize(_)) => return Err("запись TLS не влезла"),
                    Err(_) => return Err("не удалось закодировать запись TLS"),
                };
                send_all(net_ep, h, &outgoing[..n])?;
            }
            ConnectionState::TransmitTlsData(s) => {
                // Закодированное уже отправлено в ветке выше — здесь только подтверждаем.
                s.done();
            }
            ConnectionState::BlockedHandshake => {
                recv_more(net_ep, h, &mut incoming)?;
            }
            ConnectionState::WriteTraffic(mut s) => {
                if !request_sent {
                    let req = build_request(host, path);
                    let n = s
                        .encrypt(&req, &mut outgoing)
                        .map_err(|_| "не удалось зашифровать запрос")?;
                    send_all(net_ep, h, &outgoing[..n])?;
                    request_sent = true;
                } else {
                    // Запрос ушёл, ответа пока нет — ждём данных от сервера.
                    recv_more(net_ep, h, &mut incoming)?;
                }
            }
            ConnectionState::ReadTraffic(mut s) => {
                while let Some(rec) = s.next_record() {
                    let rec = rec.map_err(|_| "битая запись TLS")?;
                    http.feed(rec.payload, &mut body)?;
                }
            }
            ConnectionState::PeerClosed | ConnectionState::Closed => break,
            _ => return Err("неожиданное состояние TLS"),
        }

        incoming.drain(..discard);
        if http.done() {
            break;
        }
    }

    if !http.saw_status {
        return Err("сервер закрыл соединение без ответа");
    }
    if http.status != 200 {
        return Err(http_status_text(http.status));
    }
    let id = body.finish()?;
    Ok((body.total, body.n, id))
}

/// Дочитать ещё немного зашифрованных байт в буфер входящих.
fn recv_more(net_ep: usize, h: u8, incoming: &mut Vec<u8>) -> Result<(), &'static str> {
    if incoming.len() >= IN_CAP {
        return Err("буфер входящих TLS переполнен");
    }
    let mut buf = [0u8; net_cli::MAX_CHUNK];
    match net_cli::tcp_recv(net_ep, h, &mut buf) {
        Ok(0) | Err(net_cli::ST_EOF) => Err("соединение закрыто до конца обмена"),
        Ok(n) => {
            incoming.extend_from_slice(&buf[..n]);
            Ok(())
        }
        Err(net_cli::ST_TIMEOUT) => Err("сервер молчит"),
        Err(_) => Err("ошибка приёма"),
    }
}

fn send_all(net_ep: usize, h: u8, mut data: &[u8]) -> Result<(), &'static str> {
    while !data.is_empty() {
        match net_cli::tcp_send(net_ep, h, data) {
            Ok(0) => return Err("сервер не принимает данные"),
            Ok(n) => data = &data[n..],
            Err(_) => return Err("не удалось отправить"),
        }
    }
    Ok(())
}

/// Назвать причину отказа TLS своим именем.
///
/// Сваливать всё в «ошибка протокола» нельзя: по такому сообщению нельзя отличить чужую
/// неисправность (сервер с просроченным сертификатом) от своей (сбитые часы), а это ровно те
/// два случая, которые пользователь должен различать. Отдельно назван случай часов: без RTC
/// или после долгого простоя система легко решит, что все сертификаты мира просрочены.
fn tls_error_text(e: rustls::Error) -> &'static str {
    use rustls::CertificateError as C;
    use rustls::Error as E;
    match e {
        E::InvalidCertificate(c) => match c {
            C::Expired | C::ExpiredContext { .. } => {
                "сертификат просрочен (или часы системы отстают — проверьте `date`)"
            }
            C::NotValidYet | C::NotValidYetContext { .. } => {
                "сертификат ещё не действителен (или часы системы спешат — проверьте `date`)"
            }
            C::NotValidForName | C::NotValidForNameContext { .. } => {
                "сертификат выдан на другое имя"
            }
            C::UnknownIssuer => "цепочка не ведёт к доверенному корню",
            C::BadSignature => "подпись сертификата не сходится",
            C::Revoked => "сертификат отозван",
            _ => "сертификат отвергнут проверкой",
        },
        E::InvalidMessage(_) => "сервер прислал негодную запись TLS",
        E::PeerIncompatible(_) => "сервер не поддерживает наши версии/шифры TLS",
        E::PeerMisbehaved(_) => "сервер нарушил протокол TLS",
        E::AlertReceived(_) => "сервер прервал сеанс предупреждением TLS",
        E::NoCertificatesPresented => "сервер не предъявил сертификат",
        E::DecryptError => "не удалось расшифровать запись",
        _ => "ошибка TLS",
    }
}

fn build_request(host: &[u8], path: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(128 + host.len() + path.len());
    r.extend_from_slice(b"GET ");
    r.extend_from_slice(path);
    r.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    r.extend_from_slice(host);
    r.extend_from_slice(b"\r\nUser-Agent: VOID/1\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    r
}

// ── разбор HTTP поверх расшифрованного потока ───────────────────────────────────────────────
//
// Записи TLS приходят кусками произвольной длины, поэтому разбор обязан быть потоковым: заголовки
// могут разорваться посреди строки, а тело — начаться в середине записи.

struct HttpParse {
    line: Vec<u8>,
    saw_status: bool,
    status: u16,
    in_body: bool,
    chunked: bool,
    content_len: Option<usize>,
    got: usize,
    /// Для chunked: сколько байт текущего куска осталось; `None` — ждём строку с размером.
    chunk_left: Option<usize>,
    finished: bool,
}

impl HttpParse {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            saw_status: false,
            status: 0,
            in_body: false,
            chunked: false,
            content_len: None,
            got: 0,
            chunk_left: None,
            finished: false,
        }
    }

    fn done(&self) -> bool {
        self.finished
    }

    fn feed(&mut self, mut data: &[u8], body: &mut Body) -> Result<(), &'static str> {
        while !data.is_empty() && !self.finished {
            if !self.in_body {
                // Заголовки — построчно.
                let Some(nl) = data.iter().position(|&b| b == b'\n') else {
                    self.line.extend_from_slice(data);
                    return Ok(());
                };
                self.line.extend_from_slice(&data[..nl]);
                data = &data[nl + 1..];
                let line = trim_cr(&self.line);
                if !self.saw_status {
                    self.status = parse_status(line)?;
                    self.saw_status = true;
                } else if line.is_empty() {
                    self.in_body = true;
                    if !self.chunked && self.content_len == Some(0) {
                        self.finished = true;
                    }
                } else if let Some(c) = line.iter().position(|&b| b == b':') {
                    let (name, value) = (&line[..c], trim(&line[c + 1..]));
                    if eq_ci(name, b"content-length") {
                        self.content_len = Some(parse_dec(value)?);
                    } else if eq_ci(name, b"transfer-encoding") && contains_ci(value, b"chunked") {
                        self.chunked = true;
                    }
                }
                self.line.clear();
                continue;
            }

            if self.chunked {
                match self.chunk_left {
                    None => {
                        let Some(nl) = data.iter().position(|&b| b == b'\n') else {
                            self.line.extend_from_slice(data);
                            return Ok(());
                        };
                        self.line.extend_from_slice(&data[..nl]);
                        data = &data[nl + 1..];
                        let line = trim(trim_cr(&self.line));
                        if !line.is_empty() {
                            let size = parse_hex(line)?;
                            if size == 0 {
                                self.finished = true;
                            } else {
                                self.chunk_left = Some(size);
                            }
                        }
                        self.line.clear();
                    }
                    Some(left) => {
                        let take = left.min(data.len());
                        body.push(&data[..take])?;
                        data = &data[take..];
                        self.chunk_left = if left == take { None } else { Some(left - take) };
                    }
                }
                continue;
            }

            // Обычное тело: по Content-Length либо до закрытия соединения.
            let take = match self.content_len {
                Some(total) => (total - self.got).min(data.len()),
                None => data.len(),
            };
            body.push(&data[..take])?;
            self.got += take;
            data = &data[take..];
            if self.content_len == Some(self.got) {
                self.finished = true;
            }
        }
        Ok(())
    }
}

/// Накопитель тела в store — тот же формат блоба, что у Вехи 94 ([`sys::http`]).
struct Body {
    store_cap: usize,
    root: Vec<u8>,
    chunk: Vec<u8>,
    kids: Vec<[u8; 32]>,
    total: usize,
    n: usize,
}

impl Body {
    fn new(store_cap: usize, root: &[u8]) -> Self {
        Self {
            store_cap,
            root: root.to_vec(),
            chunk: Vec::with_capacity(sys::http::CHUNK),
            kids: Vec::new(),
            total: 0,
            n: 0,
        }
    }

    fn push(&mut self, mut data: &[u8]) -> Result<(), &'static str> {
        while !data.is_empty() {
            let room = sys::http::CHUNK - self.chunk.len();
            let take = room.min(data.len());
            self.chunk.extend_from_slice(&data[..take]);
            self.total += take;
            data = &data[take..];
            if self.chunk.len() == sys::http::CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), &'static str> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let mut id = [0u8; 32];
        if sys::obj_put(self.store_cap, &self.chunk, &mut id) != 0 {
            return Err("store не принял кусок");
        }
        self.kids.push(id);
        self.n += 1;
        self.chunk.clear();
        Ok(())
    }

    fn finish(&mut self) -> Result<[u8; 32], &'static str> {
        self.flush()?;
        let m = sys::http::blob_manifest(self.total, self.n);
        let mut id = [0u8; 32];
        if sys::obj_put_node(self.store_cap, &m, &self.kids, &mut id) != 0 {
            return Err("store не принял узел");
        }
        if sys::obj_set_root(self.store_cap, &self.root, &id) != 0 {
            return Err("не удалось привязать корень");
        }
        Ok(id)
    }
}

// ── мелочи разбора ──────────────────────────────────────────────────────────────────────────

fn parse_url(url: &[u8]) -> Result<(&[u8], u16, &[u8]), &'static str> {
    let rest = strip_ci(url, b"https://").ok_or("нужен адрес вида https://хост/путь")?;
    let slash = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(slash);
    let (host, port) = match authority.iter().position(|&b| b == b':') {
        Some(i) => (&authority[..i], parse_dec(&authority[i + 1..])? as u16),
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err("пустое имя хоста");
    }
    Ok((host, port, if path.is_empty() { b"/" } else { path }))
}

fn strip_ci<'a>(s: &'a [u8], p: &[u8]) -> Option<&'a [u8]> {
    (s.len() >= p.len() && s[..p.len()].iter().zip(p).all(|(a, b)| a.eq_ignore_ascii_case(b)))
        .then(|| &s[p.len()..])
}

fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| eq_ci(w, needle))
}

fn trim(s: &[u8]) -> &[u8] {
    let a = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let b = s.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(a, |i| i + 1);
    &s[a..b]
}

fn trim_cr(s: &[u8]) -> &[u8] {
    match s.last() {
        Some(b'\r') => &s[..s.len() - 1],
        _ => s,
    }
}

fn parse_status(line: &[u8]) -> Result<u16, &'static str> {
    let rest = strip_ci(line, b"HTTP/").ok_or("это не HTTP-ответ")?;
    let sp = rest.iter().position(|&b| b == b' ').ok_or("нет кода состояния")?;
    let code = trim(&rest[sp..]);
    let d: &[u8] = &code[..code.len().min(3)];
    if d.len() != 3 || !d.iter().all(|b| b.is_ascii_digit()) {
        return Err("негодный код состояния");
    }
    Ok((d[0] - b'0') as u16 * 100 + (d[1] - b'0') as u16 * 10 + (d[2] - b'0') as u16)
}

fn http_status_text(status: u16) -> &'static str {
    match status {
        301 | 302 | 303 | 307 | 308 => "сервер отвечает редиректом (для https он пока не сделан)",
        404 => "не найдено (404)",
        400..=499 => "сервер отказал (4xx)",
        500..=599 => "ошибка на сервере (5xx)",
        _ => "неожиданный код ответа",
    }
}

fn parse_dec(s: &[u8]) -> Result<usize, &'static str> {
    if s.is_empty() {
        return Err("пустое число");
    }
    let mut v = 0usize;
    for &b in s {
        if !b.is_ascii_digit() {
            return Err("не число");
        }
        v = v * 10 + (b - b'0') as usize;
    }
    Ok(v)
}

fn parse_hex(s: &[u8]) -> Result<usize, &'static str> {
    let s = match s.iter().position(|&b| b == b';') {
        Some(i) => &s[..i],
        None => s,
    };
    if s.is_empty() {
        return Err("пустой размер куска");
    }
    let mut v = 0usize;
    for &b in s {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err("размер куска не hex"),
        };
        v = v * 16 + d as usize;
    }
    Ok(v)
}

fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut o = [0u8; 4];
    let (mut idx, mut val, mut digits) = (0usize, 0u32, 0);
    for &b in s {
        if b == b'.' {
            if digits == 0 || idx >= 3 {
                return None;
            }
            o[idx] = val as u8;
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
    (idx == 3 && digits > 0).then(|| {
        o[3] = val as u8;
        o
    })
}

fn resolve(net_ep: usize, name: &str) -> Result<[u8; 4], &'static str> {
    let mut rep = [0u8; 5];
    let n = sys::call(net_ep, net_cli::OP_RESOLVE, name.as_bytes(), &mut rep);
    if n < 5 || rep[0] != net_cli::ST_OK {
        return Err("имя не разрешилось");
    }
    Ok([rep[1], rep[2], rep[3], rep[4]])
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

fn write_hex(id: &[u8; 32]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 64];
    for (i, b) in id.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xf) as usize];
    }
    sys::write(&out);
}
