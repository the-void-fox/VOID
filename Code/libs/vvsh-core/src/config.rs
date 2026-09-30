//! Нормализатор: вычисленное значение конфига → текст для init'а VOID (`kernel/src/init.rs`).
//!
//! Верхняя форма — `(#system запись…)`, запись — `(kind имя право…)`. Печатаем построчно
//! `kind имя право право` — ровно формат, который сегодня генерирует `nix/system.nix` и читает
//! `apply()`. Это выход этапа «сборки» (`rebuild`): его коммитят поколением, ядро грузит на буте.
//!
//! **Читателей у одного текста несколько.** `service`/`shell` — ядру, `terminal`/`bind` —
//! терминалу (`bin/term` читает поколение из store сам), `desktop` — композитору, `ui` —
//! тулкиту оболочки (Веха 144), `packages`/`channel` — `pkg` (Вехи 112–113).
//! Разделять их на два объекта не за что: конфигурация системы — ОДНА вещь, у неё одна история
//! поколений и один откат; кто какие строки берёт — дело читателя, а не хранилища.

use alloc::string::String;
use core::fmt::Write;

use crate::value::{EvalError, Value};

/// Значение `(#system …)` → нормализованный текст конфига (строки `service …` / `shell …`).
pub fn normalize_config(v: &Value) -> Result<String, EvalError> {
    let items = match v {
        Value::List(items) if head_is(items, "#system") => items,
        _ => return Err(EvalError::new("верхняя форма конфига должна быть (system …)")),
    };
    let mut out = String::new();
    for e in &items[1..] {
        let entry = match e {
            Value::List(x) => x,
            _ => return Err(EvalError::new("запись конфига — список")),
        };
        if entry.len() < 2 {
            return Err(EvalError::new("запись: (kind имя право…)"));
        }
        let kind = match &entry[0] {
            Value::Sym(s) => &**s,
            _ => return Err(EvalError::new("kind записи — символ")),
        };
        // Число полей записи проверяется ЗДЕСЬ: смысл этапа сборки в том, чтобы опечатка стала
        // ошибкой `rebuild`, а не молчаливо пропущенной строкой в уже загруженной системе.
        //
        // Веха 148.8 — сколько полей у вида, знает СЛОВАРЬ (`void_conf::KINDS`), а не эта
        // функция: тот же словарь читает ядро («чья это строка»), и разъехаться им теперь не на
        // чем. Вид, которого в словаре нет, здесь не отвергается: конфиг вправе нести строки для
        // программ, о которых ядро не знает вовсе, — а вот ядро о таком скажет в журнал.
        if let Some(k) = void_conf::kind(kind) {
            if let Some(want) = k.values {
                if entry.len() != want + 1 {
                    return Err(EvalError::new(alloc::format!("{}: {}", kind, k.form)));
                }
            }
        }
        let name = match &entry[1] {
            Value::Str(s) => &**s,
            _ => return Err(EvalError::new("имя записи — строка")),
        };
        let _ = write!(out, "{} {}", kind, name);
        for cap in &entry[2..] {
            match cap {
                Value::Str(s) => {
                    let _ = write!(out, " {}", &**s);
                }
                _ => return Err(EvalError::new("право записи — строка")),
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn head_is(items: &[Value], sym: &str) -> bool {
    matches!(items.first(), Some(Value::Sym(s)) if &**s == sym)
}

/// Веха 220.1 — ПРОВЕРИТЬ КОНФИГ ЦЕЛИКОМ: права, устройства, ссылки на сервисы.
///
/// ## Почему это отдельная проверка, а не «ядро разберётся»
///
/// Ядро разбиралось — молча. Непонятный токен, устройство с опечаткой, endpoint несуществующего
/// сервера — всё это давало строку `(пропуск)` в журнале загрузки, которую на машине без
/// COM-порта не читает никто. Снаружи это выглядит как работающая система, у которой почему-то
/// не работает одна вещь. Владелец назвал этот класс прямо: «молчаливые ошибки», и попросил
/// давать по рукам за любое несоответствие — как это делает Rust, а не как JavaScript.
///
/// Проверяется то, что МОЖНО проверить по тексту:
///
/// - **вид токена**: `store:rw`, `dev:net:rw`, `endpoint:имя`, `mmio:устройство`, `power`, `dma`,
///   `netdev`, `sysview`, `hwprobe`, `env`, `arg:…` — и ничего больше;
/// - **буквы прав**: `store:q` до этой вехи означало НОЛЬ прав, и тоже молча;
/// - **имя устройства**: `mmio:wifii` — всегда опечатка, на любой машине;
/// - **ссылка на сервер**: `endpoint:hda` без строки `service hda` в этом же конфиге.
///
/// Чего здесь НЕТ и почему. Есть ли в этой машине названное устройство — не проверяем: один
/// конфиг ездит по разным машинам, и `mmio:wifi` на машине без карты совершенно верен. Есть ли в
/// store названная программа — тоже не здесь: это вопрос к store, а не к тексту (проверяет
/// `rebuild`).
///
/// Возвращается список претензий, по одной на строку; пустой список — конфиг чист.
pub fn check_config(norm: &str) -> alloc::vec::Vec<alloc::string::String> {
    let mut беды = alloc::vec::Vec::new();
    // Первым проходом — кто вообще объявлен: на них ссылаются `endpoint:`.
    let объявлены: alloc::vec::Vec<&str> = norm
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let kind = w.next()?;
            void_conf::spawns_program(kind).then(|| w.next()).flatten()
        })
        .collect();
    for line in norm.lines() {
        let mut w = line.split_whitespace();
        let Some(kind) = w.next() else { continue };
        if !void_conf::spawns_program(kind) {
            continue;
        }
        let Some(имя) = w.next() else { continue };
        for tok in w {
            if let Err(почему) = void_conf::check_cap_token(tok) {
                беды.push(alloc::format!("{} {}: `{}` — {}", kind, имя, tok, почему));
                continue;
            }
            // Ссылку на сервер проверяем здесь: по одному токену не видно, есть ли такой.
            let база = tok.strip_suffix('!').unwrap_or(tok);
            if let Some(rest) = база.strip_prefix("endpoint:") {
                let сервер = rest.split(':').next().unwrap_or(rest);
                if !объявлены.contains(&сервер) {
                    беды.push(alloc::format!(
                        "{} {}: `{}` — такого сервера в конфиге нет",
                        kind, имя, tok
                    ));
                }
            }
        }
    }
    беды
}

/// Веха 220.1 — ЧТО НОВЫЙ КОНФИГ ТЕРЯЕТ по сравнению со старым.
///
/// Этот класс кусал владельца дважды и оба раза одинаково: пересборка конфига отняла у системы
/// сеть, и узналось это после перезагрузки, когда отвалился даже кабель. Потеря сама по себе не
/// ошибка — убрать сервис можно и нарочно, — но она обязана быть СКАЗАНА. Молча система меняется
/// только в одну сторону: становится беднее, а человек об этом узнаёт позже всех.
///
/// Сравниваются программы (`service`/`shell`) по имени и их права по токенам. Возвращается список
/// потерь; пустой — новый конфиг умеет всё, что умел старый.
pub fn lost_entries(old: &str, new: &str) -> alloc::vec::Vec<alloc::string::String> {
    let программы = |t: &str| -> alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<alloc::string::String>)> {
        t.lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let kind = w.next()?;
                if !void_conf::spawns_program(kind) {
                    return None;
                }
                let имя = w.next()?;
                Some((
                    alloc::format!("{} {}", kind, имя),
                    w.map(alloc::string::String::from).collect(),
                ))
            })
            .collect()
    };
    let (было, стало) = (программы(old), программы(new));
    let mut потери = alloc::vec::Vec::new();
    for (имя, права) in &было {
        match стало.iter().find(|(n, _)| n == имя) {
            None => потери.push(alloc::format!("{} — программы больше нет", имя)),
            Some((_, новые)) => {
                for p in права {
                    // Аргументы не права: их меняют осознанно и часто (`arg:ssid=…`), и
                    // предупреждать о каждой правке значило бы приучить не читать.
                    if !p.starts_with("arg:") && !новые.contains(p) {
                        потери.push(alloc::format!("{} — нет права `{}`", имя, p));
                    }
                }
            }
        }
    }
    потери
}

/// Веха 219.1 — КОРНИ STORE, НАЗВАННЫЕ КОНФИГОМ.
///
/// Секретов в конфиге нет и быть не должно: пароль сети — это объект store, а в строке сервиса
/// стоит только имя его корня со знаком `@`:
///
/// ```text
/// service wifi mmio:wifi dma store:r arg:ssid=ДОМ arg:key=@wifi/upc
/// ```
///
/// Знак нужен не для красоты. `rebuild` обязан проверить, что названный корень существует, —
/// иначе конфиг, скопированный на другую машину, соберётся молча, а беда вылезет при подключении.
/// А гадать «похоже ли значение на имя корня» нельзя: в тех же аргументах ходят пути, имена
/// программ и адреса, и однажды проверка заругалась бы на верный конфиг. Со знаком гадать не о
/// чем: сказано «корень» — ищем корень.
///
/// Возвращаются имена в том порядке, в каком стоят в конфиге; пустое имя (`=@` без имени) тоже
/// возвращается — это ошибка конфига, и молчать о ней хуже, чем назвать.
pub fn store_refs(norm: &str) -> alloc::vec::Vec<&str> {
    let mut out = alloc::vec::Vec::new();
    for tok in norm.split_whitespace() {
        if let Some((_, root)) = tok.split_once("=@") {
            out.push(root);
        }
    }
    out
}
