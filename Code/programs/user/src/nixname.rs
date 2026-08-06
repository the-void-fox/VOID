//! Правила ИМЕНОВАНИЯ nix: как из `<хэш>-<имя>` достать имя пакета, версию и выход и как
//! сравнить две версии (Веха 111).
//!
//! Отдельным файлом по двум причинам. Первая — правила здесь **чужие**: их задаёт nix, и наша
//! свобода тут ровно нулевая. `hello-wayland-0-unstable-2026-04-14` разбирается на имя
//! `hello-wayland` и версию `0-unstable-2026-04-14` не потому, что нам так удобно, а потому что
//! так делает `builtins.parseDrvName`; любая самодеятельность разошлась бы с nixpkgs молча — и
//! человек получил бы не тот пакет, не узнав об этом. Вторая — ровно поэтому код обязан быть
//! проверяем на хосте, рядом с настоящим nix и настоящим индексом канала, а `no_main`-бинарь
//! `pkg` тестами не покрыть. Файл подключается и туда, и туда (`#[path]` / `include!`), так что
//! проверяется ИМЕННО ТОТ код, который поедет на устройство.
//!
//! Ни `alloc`, ни syscall-ов здесь нет намеренно: это разбор строк и ничего больше.

/// Длина хэша пути store в символах.
pub const HASH_LEN: usize = 32;

/// Разбить имя на имя пакета и версию ПО ПРАВИЛУ NIX (`DrvName::DrvName`): имя кончается на
/// первом дефисе, за которым идёт не буква. Дефис в самом конце строки не считается.
pub fn split_pname(name: &str) -> (&str, &str) {
    let b = name.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'-' && i + 1 < b.len() && !b[i + 1].is_ascii_alphabetic() {
            return (&name[..i], &name[i + 1..]);
        }
    }
    (name, "")
}

/// Отделить от версии хвост-выход: `2.42-67-bin` → (`2.42-67`, `bin`).
///
/// Признак, а не список. Список выходов невозможен в принципе: их имена задаёт сам пакет, и
/// кроме привычных `bin`/`dev`/`man` в канале живут `locate` у findutils, `xxd` у vim,
/// `getent` у glibc, `terminfo` у ncurses. Проверено: с белым списком `findutils-4.11.0-locate`
/// выглядел как версия СТАРШЕ, чем `findutils-4.11.0`, и выигрывал выбор.
///
/// Признак такой: хвост из одних строчных букв после части, кончающейся цифрой. Он ошибается на
/// суффиксах версии вроде `20250622-git`, но ошибается БЕЗОБИДНО: выход не отбрасывает
/// кандидата, а лишь опускает его ниже — и только если есть с чем сравнивать.
pub fn split_out(version: &str) -> (&str, &str) {
    let Some(i) = version.rfind('-') else { return (version, "") };
    let (head, tail) = (&version[..i], &version[i + 1..]);
    if head.is_empty() || tail.is_empty() {
        return (version, "");
    }
    if !head.as_bytes()[head.len() - 1].is_ascii_digit() {
        return (version, "");
    }
    if !tail.bytes().all(|b| b.is_ascii_lowercase()) {
        return (version, "");
    }
    (head, tail)
}

/// (имя пакета, версия, выход) для базы пути `<хэш>-<имя>`.
pub fn parts(base: &str) -> (&str, &str, &str) {
    let name = base.get(HASH_LEN + 1..).unwrap_or("");
    let (p, v) = split_pname(name);
    let (v, o) = split_out(v);
    (p, v, o)
}

/// Компоненты версии по правилу nix: разделители `.` и `-`, а внутри — граница между цифрами и
/// не-цифрами (`2.12.3rc1` → `2`, `12`, `3`, `rc`, `1`).
pub struct VerParts<'a>(pub &'a str);

impl<'a> Iterator for VerParts<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        let b = self.0.as_bytes();
        let mut i = 0;
        while i < b.len() && (b[i] == b'.' || b[i] == b'-') {
            i += 1;
        }
        if i >= b.len() {
            self.0 = "";
            return None;
        }
        let start = i;
        let digit = b[i].is_ascii_digit();
        while i < b.len() && b[i] != b'.' && b[i] != b'-' && b[i].is_ascii_digit() == digit {
            i += 1;
        }
        let s = &self.0[start..i];
        self.0 = &self.0[i..];
        Some(s)
    }
}

/// Сравнить компоненты версий (nix, `componentsLT`): числа — как числа, число старше строки,
/// `pre` младше вообще всего (иначе `1.0pre2` оказалось бы старше `1.0`).
///
/// Ширина `i32` — не небрежность и не «хватит на всё»: nix читает компоненту в `int`, и то, что
/// в него не влезло, становится У НЕГО обычной строкой. Даты-версии вроде `20260705030346` и
/// `3212590527` в канале есть, они переполняют `int` — и сравниваются как строки. Возьми мы
/// `u64`, разошлись бы с nixpkgs ровно на них (замер: 50 расхождений на 81 019 пар).
pub fn cmp_comp(a: &str, b: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering::*;
    let (na, nb) = (a.parse::<i32>().ok(), b.parse::<i32>().ok());
    if let (Some(x), Some(y)) = (na, nb) {
        return x.cmp(&y);
    }
    if a == b {
        return Equal;
    }
    if a == "pre" {
        return Less;
    }
    if b == "pre" {
        return Greater;
    }
    if na.is_some() {
        return Greater;
    }
    if nb.is_some() {
        return Less;
    }
    a.cmp(b)
}

/// Насколько выход годится как «сам пакет»: меньше — лучше.
///
/// Порядок не выдуман, а взят у nixpkgs: `outputsToInstall` в `mkDerivation` — это
/// `[ (if hasOutput "bin" then "bin" else head outputs) ] ++ optional (hasOutput "man") "man"`.
/// То есть **есть выход `bin` — ставится он**, а иначе первый, и первый почти всегда `out`
/// (у него нет суффикса в имени пути). Отсюда ровно три ступени ниже.
///
/// Проверено на канале: `jq`, `curl`, `xz`, `zstd`, `glibc` ставятся выходом `bin`, а
/// `hello`, `git`, `coreutils` — выходом `out`; правило «без суффикса лучше всего» дало бы
/// первым четырём пакет БЕЗ `bin/`, то есть без программы, за которой человек и пришёл.
/// `man` мы не берём: читать его пока нечем.
fn out_rank(out: &str) -> u8 {
    match out {
        "bin" => 0,
        "" => 1,
        _ => 2,
    }
}

/// Пометки предвыпуска в версии. Выпуск всегда предпочтительнее предвыпуска — иначе `pkg install
/// python3` в канале, где рядом лежат `3.14.6` и `3.15.0rc1`, отдал бы кандидат в релизы.
///
/// Это ПРЕДПОЧТЕНИЕ, а не отбор: пакеты, у которых других версий не бывает (`…-0-unstable-2026-…`
/// — обычное дело в nixpkgs), остаются на месте, просто уступают дорогу выпуску, если он есть.
const PRERELEASE: &[&str] = &["rc", "alpha", "beta", "pre", "unstable", "snapshot"];

/// Есть ли в версии пометка предвыпуска.
fn is_prerelease(version: &str) -> bool {
    VerParts(version).any(|c| PRERELEASE.contains(&c))
}

/// Кто из двух путей предпочтительнее как «этот пакет»: сперва ВЫПУСК против предвыпуска, потом
/// СТАРШАЯ версия, потом годность выхода ([`out_rank`]). Равные — равны, и разрешает их порядок
/// индекса: сортировка устойчива, а индекс отсортирован по хэшу, значит выбор воспроизводим.
///
/// Версия важнее выхода намеренно: иначе `foo-1.0-bin` заслонил бы `foo-2.0`.
///
/// Правило ничего не знает про архитектуру и знать не может: имя пути её не содержит. Отбор по
/// архитектуре — отдельный шаг и делается раньше (см. `pkg`, якорь libc).
pub fn cmp_pref(a: &str, b: &str) -> core::cmp::Ordering {
    let (_, va, oa) = parts(a);
    let (_, vb, ob) = parts(b);
    is_prerelease(va)
        .cmp(&is_prerelease(vb)) // false < true: выпуск вперёд
        .then_with(|| cmp_version(vb, va)) // старшая версия — вперёд
        .then(out_rank(oa).cmp(&out_rank(ob)))
}

/// Сравнить версии покомпонентно; кончившаяся версия добирается пустыми компонентами.
pub fn cmp_version(a: &str, b: &str) -> core::cmp::Ordering {
    use core::cmp::Ordering::*;
    let (mut ia, mut ib) = (VerParts(a), VerParts(b));
    loop {
        let (x, y) = (ia.next(), ib.next());
        if x.is_none() && y.is_none() {
            return Equal;
        }
        let c = cmp_comp(x.unwrap_or(""), y.unwrap_or(""));
        if c != Equal {
            return c;
        }
    }
}
