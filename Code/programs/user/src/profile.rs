//! Профиль пакетов: поколения и их содержимое (Вехи 107, 109).
//!
//! Формат родился в `pkg` ([[pkg-closure]]), а с Вехи 109 его читает ещё и шелл — ему нужен
//! **PATH**: голое слово должно находиться не только среди программ системы, но и среди
//! установленного. Поэтому разбор переехал сюда, модулем по пути (нужен `alloc`, а объявить его
//! в библиотеке значило бы потребовать глобальный аллокатор от всех программ).
//!
//! Раскладка:
//!
//! - `pkg/profile/<профиль>/gen<N>` — **узел** store: значение перечисляет пути, исходящие ссылки
//!   держат их содержимое (значит, GC их не тронет);
//! - `pkg/profile/<профиль>/current` — корень-указатель, чьё значение = имя активного поколения.
//!
//! Значение поколения — текст: заголовок с версией формата, дальше строки `top|dep <путь>
//! <размер>`. Порядок строк совпадает с порядком исходящих ссылок узла.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;

/// Имя профиля. Профиль пока один; множественность — это ровно другой префикс корня, поэтому
/// имя вынесено в константу, а не размазано по коду.
pub const PROFILE: &str = "default";

/// Заголовок значения поколения — версия формата, чтобы разбор мог отличить своё от чужого,
/// а не гадать по первой строке.
pub const GEN_MAGIC: &str = "void-profile 1";

/// Одна запись поколения.
#[derive(Clone)]
pub struct Item {
    /// Пакет верхнего уровня (его назвал человек), а не затянутая зависимость.
    pub top: bool,
    /// Имя пути store (`<хэш>-<имя>`).
    pub base: String,
    /// Размер NAR — то, что обещал кэш.
    pub size: usize,
}

pub fn gen_root(n: u32) -> String {
    format!("pkg/profile/{}/gen{}", PROFILE, n)
}

pub fn current_root() -> String {
    format!("pkg/profile/{}/current", PROFILE)
}

/// Content-id именованного корня, если он есть.
pub fn root_id(scap: usize, name: &str) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    (sys::obj_get_root(scap, name.as_bytes(), &mut id) == 32).then_some(id)
}

/// Номер активного поколения профиля. `None` — профиля ещё нет.
pub fn current(scap: usize) -> Option<u32> {
    let id = root_id(scap, &current_root())?;
    let mut buf = [0u8; 64];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    let s = core::str::from_utf8(&buf[..n]).ok()?;
    let num = s.trim().strip_prefix("gen")?;
    num.parse::<u32>().ok()
}

/// Сделать поколение активным: значение корня-указателя — ИМЯ поколения, а не его адрес.
/// Так же устроен `system/current` ([[declarative-init]]): указатель на имя переживает то, что
/// содержимое поколения переехало, и читается человеком в `roots`.
pub fn set_current(scap: usize, n: u32) -> Result<(), String> {
    let name = format!("gen{}", n);
    let mut id = [0u8; 32];
    if sys::obj_put(scap, name.as_bytes(), &mut id) != 0 {
        return Err(String::from("имя поколения не влезло в store"));
    }
    if sys::obj_set_root(scap, current_root().as_bytes(), &id) != 0 {
        return Err(String::from("указатель профиля не переключился"));
    }
    Ok(())
}

/// Разобрать значение поколения.
pub fn parse(text: &str) -> Result<Vec<Item>, String> {
    let mut lines = text.lines();
    if lines.next() != Some(GEN_MAGIC) {
        return Err(String::from("поколение профиля незнакомого формата"));
    }
    let mut out = Vec::new();
    for line in lines {
        let mut it = line.split_whitespace();
        let (Some(kind), Some(base)) = (it.next(), it.next()) else { continue };
        let size = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        out.push(Item { top: kind == "top", base: base.to_string(), size });
    }
    Ok(out)
}

/// Прочитать поколение по номеру.
pub fn read(scap: usize, n: u32) -> Result<Vec<Item>, String> {
    let id = root_id(scap, &gen_root(n)).ok_or_else(|| format!("нет поколения gen{}", n))?;
    // Значение поколения — текст в несколько килобайт; читаем с запасом и по факту.
    let mut buf = alloc::vec![0u8; 256 * 1024];
    let got = sys::obj_get(scap, &id, &mut buf);
    if got == 0 || got > buf.len() {
        return Err(String::from("поколение профиля не читается"));
    }
    buf.truncate(got);
    let text = core::str::from_utf8(&buf).map_err(|_| "поколение профиля не UTF-8")?;
    parse(text)
}

/// Содержимое активного поколения (пусто, если профиля ещё нет).
pub fn active(scap: usize) -> Vec<Item> {
    match current(scap) {
        Some(n) => read(scap, n).unwrap_or_default(),
        None => Vec::new(),
    }
}
