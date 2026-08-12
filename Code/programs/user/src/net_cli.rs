//! Протокол сетевого сервера и клиентская обёртка над ним (Веха 93).
//!
//! Одно место, где записан формат сообщений к `net-srv`: **и сервер, и клиенты берут константы
//! отсюда**. До Вехи 93 номера операций жили числовыми литералами в трёх файлах — при добавлении
//! TCP такое разъехалось бы молча.
//!
//! Право пользоваться сетью — это **cap на эндпоинт `net-srv`** (`endpoint:net-srv` в конфиге
//! системы, [[declarative-init]]). Отдельных прав на «сокет» нет: у кого есть эндпоинт, тот
//! может открывать соединения. Соответственно и хэндлы соединений общие для всех клиентов
//! сервера — владения соединением пока нет (записано в [[known-gaps]]).
//!
//! Формат ответа единый: **`[status(1) | payload…]`**, где status:
//! `0` — ок, `1` — ошибка (адрес не разрешился / соединение отвергнуто), `2` — не дождались,
//! `3` — негодный запрос или сети нет, `4` — соединение закрыто другой стороной (EOF),
//! `5` — имя ЗАПРЕЩЕНО политикой резолвера (Веха 136).

use crate as sys;

pub const OP_PING: usize = 0;
pub const OP_RESOLVE: usize = 1;
pub const OP_TCP_CONNECT: usize = 2;
pub const OP_TCP_SEND: usize = 3;
pub const OP_TCP_RECV: usize = 4;
pub const OP_TCP_CLOSE: usize = 5;

pub const ST_OK: u8 = 0;
pub const ST_ERR: u8 = 1;
pub const ST_TIMEOUT: u8 = 2;
pub const ST_BAD: u8 = 3;
pub const ST_EOF: u8 = 4;
/// Веха 136 — имя запрещено политикой резолвера (список блокировки). Отдельный код, а не «не
/// разрешилось»: пользователь обязан различать «такого имени нет» и «мы САМИ его не пустили»,
/// иначе блокировщик неотличим от поломки сети.
pub const ST_BLOCKED: u8 = 5;

/// Потолок полезной нагрузки одного сообщения (в обе стороны). Ограничение НЕ ядра — IPC копирует
/// сколько попросят, — а буферов сервера: он обязан не забирать из сокета больше, чем сможет
/// отдать в ответе, иначе разница пропала бы молча. Клиент шлёт и принимает кусками по столько.
pub const MAX_CHUNK: usize = 1024;

/// Открыть TCP-соединение. Возвращает хэндл или код ошибки (`ST_*`).
/// Вызов БЛОКИРУЕТСЯ до завершения рукопожатия: сервер держит reply-cap и отвечает, когда
/// соединение установилось, — поэтому его собственный цикл в это время не стоит.
pub fn tcp_connect(ep: usize, ip: [u8; 4], port: u16) -> Result<u8, u8> {
    let mut req = [0u8; 6];
    req[..4].copy_from_slice(&ip);
    req[4..6].copy_from_slice(&port.to_le_bytes());
    let mut rep = [0u8; 2];
    let n = sys::call(ep, OP_TCP_CONNECT, &req, &mut rep);
    // Ответ-ОШИБКА — один байт статуса, ответ-успех — два. Требовать двух всегда значило бы
    // подменять честную причину отказа на «негодный запрос» (так и было в первом заходе).
    match (n, rep[0]) {
        (0, _) | (usize::MAX, _) => Err(ST_BAD),
        (_, ST_OK) if n >= 2 => Ok(rep[1]),
        (_, ST_OK) => Err(ST_BAD), // успех без хэндла — сервер соврал
        (_, st) => Err(st),
    }
}

/// Отправить байты. Возвращает, сколько ПРИНЯЛ сервер (может быть меньше запрошенного — как
/// `write(2)`), либо код ошибки. Вызывающий обязан слать остаток сам.
pub fn tcp_send(ep: usize, h: u8, data: &[u8]) -> Result<usize, u8> {
    let n = data.len().min(MAX_CHUNK);
    let mut req = [0u8; MAX_CHUNK + 1];
    req[0] = h;
    req[1..1 + n].copy_from_slice(&data[..n]);
    let mut rep = [0u8; 3];
    let got = sys::call(ep, OP_TCP_SEND, &req[..1 + n], &mut rep);
    match (got, rep[0]) {
        (0, _) | (usize::MAX, _) => Err(ST_BAD),
        (_, ST_OK) if got >= 3 => Ok(u16::from_le_bytes([rep[1], rep[2]]) as usize),
        (_, ST_OK) => Err(ST_BAD),
        (_, st) => Err(st),
    }
}

/// Принять байты в `buf` (не больше [`MAX_CHUNK`]). `Ok(n)` — принято `n`; `Err(ST_EOF)` —
/// другая сторона закрыла соединение; `Err(ST_TIMEOUT)` — за отведённое сервером время данных
/// не пришло (соединение при этом живо, можно звать снова).
pub fn tcp_recv(ep: usize, h: u8, buf: &mut [u8]) -> Result<usize, u8> {
    let want = buf.len().min(MAX_CHUNK);
    let req = [h, (want & 0xff) as u8, (want >> 8) as u8];
    let mut rep = [0u8; MAX_CHUNK + 1];
    let n = sys::call(ep, OP_TCP_RECV, &req, &mut rep);
    if n == usize::MAX || n < 1 {
        return Err(ST_BAD);
    }
    if rep[0] != ST_OK {
        return Err(rep[0]);
    }
    let got = (n - 1).min(buf.len());
    buf[..got].copy_from_slice(&rep[1..1 + got]);
    Ok(got)
}

/// Закрыть соединение аккуратно (FIN, а не сброс). Слот освобождается сервером сам, когда
/// закрытие доиграет до конца.
pub fn tcp_close(ep: usize, h: u8) -> bool {
    let mut rep = [0u8; 1];
    sys::call(ep, OP_TCP_CLOSE, &[h], &mut rep) >= 1 && rep[0] == ST_OK
}
