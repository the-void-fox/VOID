//! Где взять файл шрифта (Веха 144; переехало из `bin/term.rs`).
//!
//! Шрифт в VOID не лежит в бинаре: он приходит **пакетом** — `packages("nerd-fonts-fira-mono")`
//! кладёт его в профиль, а имя файла называет конфиг (`terminal("font", …)` терминалу,
//! `ui("font", …)` тулкиту). Значит его может и не быть, и обе программы обязаны это пережить.
//!
//! Код был написан для терминала, а когда за тем же самым пришла панель, копировать его было
//! нельзя: расхождение «терминал шрифт нашёл, панель нет» человек не смог бы объяснить себе
//! ничем. Поэтому поиск живёт здесь, а `term` и тулкит зовут одно и то же.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use void_user as sys;

/// Где в пакете лежат шрифты. Раскладка внутри разная — у `nerd-fonts-fira-mono` это
/// `share/fonts/opentype/NerdFonts/FiraMono/…otf`, у других `share/fonts/truetype/…ttf`, — зато
/// глубина ограничена: пакет чужой, и бродить по нему без предела не стоит.
const ROOT: &str = "share/fonts";
const DEPTH: usize = 4;
/// Потолок числа найденных файлов: список нужен для поиска по имени и для подсказки человеку.
const MAX: usize = 256;

/// Найти файл шрифта: абсолютный путь читается как есть, имя — ищется в пакетах профиля.
pub fn find(name: &str) -> Option<Vec<u8>> {
    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    if name.starts_with('/') {
        return read_whole(ep, name.as_bytes());
    }
    let scap = super::conf::store_cap()?;
    for path in files(ep, scap) {
        if path.rsplit('/').next() == Some(name) {
            let bytes = read_whole(ep, path.as_bytes())?;
            say(&format!("шрифт {} ({} Б)", path, bytes.len()));
            return Some(bytes);
        }
    }
    None
}

/// Сказать вслух, какие файлы шрифтов вообще есть в установленных пакетах.
///
/// «Не найден» — половина ответа. Вторая половина: а что там ЕСТЬ. Без неё человек остаётся
/// гадать между опечаткой в имени, не тем пакетом и не той раскладкой внутри пакета.
pub fn list() {
    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    let Some(scap) = super::conf::store_cap() else {
        say("  профиль не прочитать: нет права на store");
        return;
    };
    let files = files(ep, scap);
    if files.is_empty() {
        say("  в пакетах профиля нет ни одного файла шрифта — поставьте пакет со шрифтом");
        return;
    }
    for f in files.iter().take(24) {
        say(&format!("  есть: {}", f));
    }
    if files.len() > 24 {
        say(&format!("  …и ещё {}", files.len() - 24));
    }
}

/// Все файлы шрифтов, какие видны в пакетах профиля.
fn files(ep: usize, scap: usize) -> Vec<String> {
    let mut out = Vec::new();
    for item in super::profile::path_items(scap) {
        if !item.top {
            continue;
        }
        walk(ep, &format!("/nix/store/{}/{}", item.base, ROOT), DEPTH, &mut out);
    }
    out
}

/// Обойти каталог вглубь, складывая пути ФАЙЛОВ. Каталоги posixfs отдаёт с хвостовым '/'.
fn walk(ep: usize, dir: &str, depth: usize, out: &mut Vec<String>) {
    if depth == 0 || out.len() >= MAX {
        return;
    }
    let mut buf = alloc::vec![0u8; 8 * 1024];
    let n = sys::posix::readdir(ep, dir.as_bytes(), &mut buf);
    if n == 0 || n > buf.len() {
        return;
    }
    let Ok(text) = core::str::from_utf8(&buf[..n]) else { return };
    for e in text.lines().filter(|e| !e.is_empty()) {
        match e.strip_suffix('/') {
            Some(sub) => walk(ep, &format!("{}/{}", dir, sub), depth - 1, out),
            None => {
                if out.len() < MAX {
                    out.push(format!("{}/{}", dir, e));
                }
            }
        }
    }
}

/// Прочитать файл целиком через файловый сервер. `None` — файла нет или это каталог.
fn read_whole(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    use sys::posix as px;
    match px::stat(ep, path) {
        Some((is_dir, _)) if !is_dir => {}
        _ => return None,
    }
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    // Кусок побольше строчного: шрифт — мегабайты, а каждый вызов это IPC.
    let mut chunk = alloc::vec![0u8; 16 * 1024];
    loop {
        let n = px::read(ep, fd, &mut chunk);
        if n == 0 || n == usize::MAX {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    px::close(ep, fd);
    (!out.is_empty()).then_some(out)
}

/// В графическом режиме консоль ядра уезжает в serial — это единственный способ клиента
/// пожаловаться на себя: экран в этот момент рисует композитор, а не он.
fn say(s: &str) {
    sys::write_console(s.as_bytes());
    sys::write_console(b"\n");
}
