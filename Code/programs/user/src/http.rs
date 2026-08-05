//! HTTP-клиент (Веха 94): GET поверх TCP Вехи 93, тело — **потоком прямо в объектный store**.
//!
//! Почему потоком, а не «скачал → положил». Пакет из бинарного кэша nixpkgs — это мегабайты, и
//! ни куча процесса (4 МиБ у vvsh), ни куча ядра их не держат; у POSIX-персоналии файл к тому же
//! ограничен 128 КиБ. Поэтому тело режется на куски по [`CHUNK`], каждый кусок кладётся в store
//! обычным `obj_put`, а в конце **узел** ([`sys::obj_put_node`]) связывает их в целое. Отсюда
//! три следствия, каждое из которых нужен нам дальше ([[0009-ondevice-packages]]):
//!
//! - в памяти одновременно живёт **один кусок**, а не весь файл;
//! - **дедуп даром**: одинаковый кусок в двух загрузках — один объект в сторе;
//! - **content-id узла — это Merkle-корень** над всем содержимым (узел хэширует список детей, а
//!   каждый ребёнок — свои байты). Одинаковые байты дают одинаковый корень, и считает его
//!   САМО устройство.
//!
//! Своего аллокатора у библиотеки нет (её линкуют программы без кучи), поэтому рабочие буферы
//! даёт вызывающий — [`Sink`].

use crate as sys;
use crate::net_cli;

/// Размер куска тела. Компромисс: мельче — больше объектов и накладных расходов store, крупнее —
/// больше памяти на кусок и грубее дедуп.
pub const CHUNK: usize = 16 * 1024;

/// Максимум переходов по `Location`, чтобы кольцо редиректов не крутилось вечно.
const MAX_REDIRECTS: usize = 5;

/// Сколько раз подряд терпим «данных пока нет» от сервера, прежде чем сдаться.
const MAX_STALLS: usize = 3;

/// Заголовок манифеста блоба. Версия в имени — чтобы будущий формат не спутали с этим.
use void_tree::blob::MAGIC;

/// Рабочие буферы, которые даёт вызывающий: у него есть куча, у библиотеки — нет.
pub struct Sink<'a> {
    /// Накопитель куска, ровно [`CHUNK`] байт.
    pub chunk: &'a mut [u8],
    /// Место под content-id кусков. Его длина и задаёт потолок размера файла.
    pub kids: &'a mut [[u8; 32]],
}

/// Чем кончилась загрузка.
pub struct Fetched {
    pub status: u16,
    pub bytes: usize,
    pub chunks: usize,
    /// content-id узла — Merkle-корень над содержимым.
    pub id: [u8; 32],
}

/// Разобранный адрес. Буферы фиксированные: `Location` может увести на другой хост, и держать
/// ссылки в чужой буфер через редирект было бы неудобно и опасно.
struct Url {
    host: [u8; 128],
    host_len: usize,
    port: u16,
    path: [u8; 512],
    path_len: usize,
}

impl Url {
    fn host(&self) -> &[u8] {
        &self.host[..self.host_len]
    }
    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }

    /// Разобрать `http://host[:port][/path]`. `https://` честно отвергается — TLS будет Вехой 95.
    fn parse(url: &[u8]) -> Result<Url, &'static str> {
        let rest = if let Some(r) = strip(url, b"http://") {
            r
        } else if strip(url, b"https://").is_some() {
            return Err("https пока нет (TLS — Веха 95)");
        } else {
            return Err("нужен адрес вида http://хост/путь");
        };
        let slash = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(slash);
        let (hostb, port) = match authority.iter().position(|&b| b == b':') {
            Some(i) => {
                let mut p: u32 = 0;
                if authority[i + 1..].is_empty() {
                    return Err("порт пуст");
                }
                for &b in &authority[i + 1..] {
                    if !b.is_ascii_digit() {
                        return Err("порт не число");
                    }
                    p = p * 10 + (b - b'0') as u32;
                    if p > 65535 {
                        return Err("порт вне диапазона");
                    }
                }
                (&authority[..i], p as u16)
            }
            None => (authority, 80),
        };
        let mut u = Url { host: [0; 128], host_len: 0, port, path: [0; 512], path_len: 0 };
        if hostb.is_empty() || hostb.len() > u.host.len() {
            return Err("негодное имя хоста");
        }
        u.host[..hostb.len()].copy_from_slice(hostb);
        u.host_len = hostb.len();
        let path = if path.is_empty() { b"/".as_slice() } else { path };
        if path.len() > u.path.len() {
            return Err("слишком длинный путь");
        }
        u.path[..path.len()].copy_from_slice(path);
        u.path_len = path.len();
        Ok(u)
    }

    /// Применить `Location`: абсолютный адрес заменяет всё, начинающийся с `/` — только путь.
    fn redirect(&mut self, loc: &[u8]) -> Result<(), &'static str> {
        if strip(loc, b"http://").is_some() || strip(loc, b"https://").is_some() {
            *self = Url::parse(loc)?;
            return Ok(());
        }
        if !loc.starts_with(b"/") || loc.len() > self.path.len() {
            return Err("относительный Location не поддержан");
        }
        self.path[..loc.len()].copy_from_slice(loc);
        self.path_len = loc.len();
        Ok(())
    }
}

fn strip<'a>(s: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    // Схема регистронезависима, а хосты в дикой природе пишут как попало.
    (s.len() >= prefix.len()
        && s[..prefix.len()].iter().zip(prefix).all(|(a, b)| a.eq_ignore_ascii_case(b)))
    .then(|| &s[prefix.len()..])
}

fn eq_ascii_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
    &s[start..end]
}

/// Читатель поверх соединения: `tcp_recv` отдаёт кусками произвольной длины, а разбор
/// заголовков идёт по СТРОКАМ и должен уметь оставить хвост — он уже часть тела.
struct Reader {
    ep: usize,
    h: u8,
    buf: [u8; net_cli::MAX_CHUNK],
    len: usize,
    pos: usize,
    eof: bool,
    stalls: usize,
}

impl Reader {
    fn new(ep: usize, h: u8) -> Reader {
        Reader { ep, h, buf: [0; net_cli::MAX_CHUNK], len: 0, pos: 0, eof: false, stalls: 0 }
    }

    /// Подтянуть данные, если буфер вычерпан. `false` — поток кончился.
    fn fill(&mut self) -> bool {
        while self.pos == self.len {
            if self.eof {
                return false;
            }
            match net_cli::tcp_recv(self.ep, self.h, &mut self.buf) {
                Ok(0) => {
                    self.eof = true;
                    return false;
                }
                Ok(n) => {
                    self.len = n;
                    self.pos = 0;
                    self.stalls = 0;
                }
                Err(net_cli::ST_EOF) => {
                    self.eof = true;
                    return false;
                }
                Err(net_cli::ST_TIMEOUT) => {
                    // Сервер молчит, но соединение живо — дать ему ещё шанс, но не бесконечно.
                    self.stalls += 1;
                    if self.stalls >= MAX_STALLS {
                        self.eof = true;
                        return false;
                    }
                }
                Err(_) => {
                    self.eof = true;
                    return false;
                }
            }
        }
        true
    }

    fn byte(&mut self) -> Option<u8> {
        self.fill().then(|| {
            let b = self.buf[self.pos];
            self.pos += 1;
            b
        })
    }

    /// Строка до CRLF (сам CRLF съедается и в результат не попадает).
    fn line(&mut self, out: &mut [u8]) -> Option<usize> {
        let mut n = 0;
        loop {
            match self.byte()? {
                b'\n' => {
                    if n > 0 && out[n - 1] == b'\r' {
                        n -= 1;
                    }
                    return Some(n);
                }
                b if n < out.len() => {
                    out[n] = b;
                    n += 1;
                }
                _ => return None, // строка длиннее буфера — не гадаем, отказываем
            }
        }
    }

    /// Скопировать до `dst.len()` байт тела. 0 — поток кончился.
    fn read(&mut self, dst: &mut [u8]) -> usize {
        if !self.fill() {
            return 0;
        }
        let n = (self.len - self.pos).min(dst.len());
        dst[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

/// Накопитель тела: режет поток на куски и кладёт их в store по мере наполнения.
///
/// Инвариант, на котором держится простота манифеста: кусок сбрасывается ТОЛЬКО заполненным,
/// кроме последнего. Значит длины кусков восстанавливаются из общей длины и не нужны в
/// манифесте — а раз так, у манифеста нет и потолка по числу кусков.
struct Blob<'a, 'b> {
    store_cap: usize,
    sink: &'a mut Sink<'b>,
    fill: usize,
    n: usize,
    total: usize,
}

impl Blob<'_, '_> {
    fn push(&mut self, mut data: &[u8]) -> Result<(), &'static str> {
        while !data.is_empty() {
            let room = CHUNK - self.fill;
            let n = room.min(data.len());
            self.sink.chunk[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            self.total += n;
            data = &data[n..];
            if self.fill == CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), &'static str> {
        if self.fill == 0 {
            return Ok(());
        }
        if self.n >= self.sink.kids.len() {
            return Err("файл длиннее, чем вмещает список кусков");
        }
        let mut id = [0u8; 32];
        if sys::obj_put(self.store_cap, &self.sink.chunk[..self.fill], &mut id) != 0 {
            return Err("store не принял кусок (нет права WRITE?)");
        }
        self.sink.kids[self.n] = id;
        self.n += 1;
        self.fill = 0;
        Ok(())
    }

    /// Закрыть блоб: дописать хвост и связать куски узлом-манифестом.
    fn finish(&mut self, root: &[u8]) -> Result<[u8; 32], &'static str> {
        self.flush()?;
        let m = blob_manifest(self.total, self.n);
        let mut id = [0u8; 32];
        if sys::obj_put_node(self.store_cap, &m, &self.sink.kids[..self.n], &mut id) != 0 {
            return Err("store не принял узел");
        }
        if !root.is_empty() && sys::obj_set_root(self.store_cap, root, &id) != 0 {
            return Err("не удалось привязать корень");
        }
        Ok(id)
    }
}

/// Скачать `url` и положить тело в store под корень `root` (пустой — не привязывать).
///
/// Следует за `Location` до [`MAX_REDIRECTS`] раз. Тело понимает и по `Content-Length`, и
/// `Transfer-Encoding: chunked`, и «до закрытия соединения» (HTTP/1.0-стиль).
pub fn get(
    net_ep: usize,
    store_cap: usize,
    url: &[u8],
    root: &[u8],
    sink: &mut Sink,
) -> Result<Fetched, &'static str> {
    if sink.chunk.len() < CHUNK {
        return Err("буфер куска меньше CHUNK");
    }
    let mut u = Url::parse(url)?;
    for _ in 0..MAX_REDIRECTS {
        match once(net_ep, store_cap, &u, root, sink)? {
            Step::Done(f) => return Ok(f),
            Step::Redirect(loc, len) => u.redirect(&loc[..len])?,
        }
    }
    Err("слишком много редиректов")
}

enum Step {
    Done(Fetched),
    Redirect([u8; 512], usize),
}

fn once(
    net_ep: usize,
    store_cap: usize,
    u: &Url,
    root: &[u8],
    sink: &mut Sink,
) -> Result<Step, &'static str> {
    // Адрес: цифровой пропускаем как есть, имя — через DNS (Веха 92).
    let ip = match parse_ipv4(u.host()) {
        Some(ip) => ip,
        None => {
            let name = core::str::from_utf8(u.host()).map_err(|_| "имя хоста не UTF-8")?;
            resolve(net_ep, name)?
        }
    };
    let h = net_cli::tcp_connect(net_ep, ip, u.port).map_err(|st| match st {
        net_cli::ST_TIMEOUT => "хост не ответил",
        _ => "не удалось соединиться",
    })?;
    let r = request(net_ep, store_cap, h, u, root, sink);
    net_cli::tcp_close(net_ep, h);
    r
}

fn request(
    net_ep: usize,
    store_cap: usize,
    h: u8,
    u: &Url,
    root: &[u8],
    sink: &mut Sink,
) -> Result<Step, &'static str> {
    // `Connection: close` — сознательно: keep-alive потребовал бы состояния соединения между
    // запросами, а нам нужен один GET. HTTP/1.1 берём ради `Host` и chunked.
    let mut req = [0u8; 1024];
    let mut p = 0;
    for part in [
        b"GET ".as_slice(), u.path(), b" HTTP/1.1\r\nHost: ", u.host(),
        b"\r\nUser-Agent: VOID/1\r\nAccept: */*\r\nConnection: close\r\n\r\n",
    ] {
        if p + part.len() > req.len() {
            return Err("запрос не влез в буфер");
        }
        req[p..p + part.len()].copy_from_slice(part);
        p += part.len();
    }
    let mut sent = 0;
    while sent < p {
        match net_cli::tcp_send(net_ep, h, &req[sent..p]) {
            Ok(0) => return Err("сервер не принимает запрос"),
            Ok(n) => sent += n,
            Err(_) => return Err("не удалось отправить запрос"),
        }
    }

    let mut rd = Reader::new(net_ep, h);
    let mut line = [0u8; 512];

    // Строка состояния: `HTTP/1.x NNN текст`.
    let n = rd.line(&mut line).ok_or("сервер закрыл соединение без ответа")?;
    let status = parse_status(&line[..n])?;

    // Заголовки до пустой строки.
    let mut content_len: Option<usize> = None;
    let mut chunked = false;
    let mut location = [0u8; 512];
    let mut location_len = 0usize;
    loop {
        let n = rd.line(&mut line).ok_or("заголовки оборвались")?;
        if n == 0 {
            break;
        }
        let Some(colon) = line[..n].iter().position(|&b| b == b':') else { continue };
        let (name, value) = (&line[..colon], trim(&line[colon + 1..n]));
        if eq_ascii_ci(name, b"content-length") {
            let mut v = 0usize;
            for &b in value {
                if !b.is_ascii_digit() {
                    return Err("негодный Content-Length");
                }
                v = v * 10 + (b - b'0') as usize;
            }
            content_len = Some(v);
        } else if eq_ascii_ci(name, b"transfer-encoding") && contains_ci(value, b"chunked") {
            chunked = true;
        } else if eq_ascii_ci(name, b"location") && value.len() <= location.len() {
            location[..value.len()].copy_from_slice(value);
            location_len = value.len();
        }
    }

    if matches!(status, 301 | 302 | 303 | 307 | 308) && location_len > 0 {
        return Ok(Step::Redirect(location, location_len));
    }
    if status != 200 {
        return Err(status_text(status));
    }

    let mut blob = Blob { store_cap, sink, fill: 0, n: 0, total: 0 };
    let mut pipe = [0u8; net_cli::MAX_CHUNK];
    if chunked {
        // Кусочная кодировка: `размер-в-hex CRLF данные CRLF`, конец — нулевой размер.
        loop {
            let n = rd.line(&mut line).ok_or("оборван размер куска")?;
            let size = parse_hex(trim(&line[..n]))?;
            if size == 0 {
                break;
            }
            let mut left = size;
            while left > 0 {
                let want = left.min(pipe.len());
                let got = rd.read(&mut pipe[..want]);
                if got == 0 {
                    return Err("тело оборвалось внутри куска");
                }
                blob.push(&pipe[..got])?;
                left -= got;
            }
            rd.line(&mut line).ok_or("нет CRLF после куска")?;
        }
    } else if let Some(total) = content_len {
        let mut left = total;
        while left > 0 {
            let want = left.min(pipe.len());
            let got = rd.read(&mut pipe[..want]);
            if got == 0 {
                return Err("тело короче объявленного Content-Length");
            }
            blob.push(&pipe[..got])?;
            left -= got;
        }
    } else {
        // Ни длины, ни кусков — читаем до закрытия соединения (так делал HTTP/1.0).
        loop {
            let got = rd.read(&mut pipe);
            if got == 0 {
                break;
            }
            blob.push(&pipe[..got])?;
        }
    }

    let id = blob.finish(root)?;
    Ok(Step::Done(Fetched { status, bytes: blob.total, chunks: blob.n, id }))
}

fn parse_status(line: &[u8]) -> Result<u16, &'static str> {
    let rest = strip(line, b"HTTP/").ok_or("это не HTTP-ответ")?;
    let sp = rest.iter().position(|&b| b == b' ').ok_or("нет кода состояния")?;
    let code = trim(&rest[sp..]);
    let digits: &[u8] = &code[..code.len().min(3)];
    if digits.len() != 3 || !digits.iter().all(|b| b.is_ascii_digit()) {
        return Err("негодный код состояния");
    }
    Ok((digits[0] - b'0') as u16 * 100 + (digits[1] - b'0') as u16 * 10 + (digits[2] - b'0') as u16)
}

fn status_text(status: u16) -> &'static str {
    match status {
        400..=403 => "сервер отказал (4xx)",
        404 => "не найдено (404)",
        405..=499 => "сервер отказал (4xx)",
        500..=599 => "ошибка на сервере (5xx)",
        _ => "неожиданный код ответа",
    }
}

fn parse_hex(s: &[u8]) -> Result<usize, &'static str> {
    // После размера может стоять `;расширение` — по стандарту его надо игнорировать.
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

fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| eq_ascii_ci(w, needle))
}

fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut o = [0u8; 4];
    let mut idx = 0;
    let mut val: u32 = 0;
    let mut digits = 0;
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

// ── чтение блоба обратно ────────────────────────────────────────────────────────────────────

/// Собрать манифест блоба: magic, общая длина, число кусков, размер куска.
///
/// Публичный, потому что тем же форматом пишет HTTPS-клиент (Веха 95, `bin/httpsc`): скачанное
/// по http и по https обязано быть неотличимо для всего, что дальше с ним работает. Сама
/// раскладка с Вехи 108.3 живёт в `void_tree::blob` — её читают ещё posixfs и ядро.
pub fn blob_manifest(total: usize, chunks: usize) -> [u8; MAGIC.len() + 16] {
    void_tree::blob::manifest(total, chunks, CHUNK)
}

/// Разобрать манифест блоба: (общая длина, число кусков, размер куска). `None` — это не блоб.
///
/// Размер куска берётся из манифеста, а не из константы: блобы, лежащие в сторе, должны
/// читаться и после того, как [`CHUNK`] в коде поменяется. (Блобы Вехи 94 первого вида несли
/// вместо этого поля длину ПЕРВОГО куска — для полного куска это одно и то же.)
pub fn blob_info(manifest: &[u8]) -> Option<(usize, usize, usize)> {
    void_tree::blob::info(manifest, CHUNK)
}
