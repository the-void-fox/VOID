#![cfg_attr(not(test), no_std)]

//! Конфиг поколения: грамматика записи и словарь видов (Веха 148.8).
//!
//! Конфиг системы человек пишет на языке `vvsh` ([[0006-vvsh]]), но ЧИТАЕТ его не язык. `rebuild`
//! вычисляет модули и кладёт в store НОРМАЛИЗОВАННЫЙ текст — по строке на запись:
//!
//! ```text
//! service posixfs store:rw
//! shell wm endpoint:posixfs store:rwx mmio:fb power env arg:term
//! desktop anim 360
//! ui accent #4c7dfd
//! bind wm Super+Return spawn-term
//! packages nerd-fonts-fira-mono htop
//! ```
//!
//! Такой текст читается без вычислителя — и в этом смысл разделения: ядру на загрузке нельзя
//! зависеть от языка, а панели нельзя тащить его ради одной строки с цветом.
//!
//! ## Почему разбор общий, и что стоила его нехватка
//!
//! Читателей у одного текста девять — ядро (`init`), тулкит (тема, движение, устройство),
//! композитор (обои, анимация, панель, клавиши), терминал, панель, `pkg`. К Вехе 148 каждый из
//! них носил свой обход строк, и они успели разойтись по трём разным местам:
//!
//! - **граница слова.** `strip_prefix(key)` без проверки следующего знака: `device name X` и
//!   `device nameserver Y` — одно и то же для того, кто спрашивает `name`, и он получал
//!   `"server Y"`. Здесь ключ сравнивается СЛОВОМ целиком;
//! - **хвост.** Значение записи — одна строка языка, и пробел внутри неё законен
//!   (`desktop wallpaper мои обои.png`). Половина копий резала хвост по пробелам и теряла его
//!   конец, половина брала целиком. Здесь есть и [`Entry::tail`] (как есть), и
//!   [`Entry::words`] (по словам) — каждый берёт то, что ему нужно, СОЗНАТЕЛЬНО;
//! - **словарь.** Список видов был записан ТРИЖДЫ: в ядре (какие строки не ему), в `vvsh-core`
//!   (сколько у вида полей) и в справке конфига. Новый вид записи требовал правки ЯДРА — иначе
//!   система на каждой загрузке говорила «неизвестная директива». Теперь словарь один: [`KINDS`].
//!
//! Комментарии и пустые строки в нормализованном тексте не появляются, но пропускаются: тот же
//! разбор читает и конфиг по умолчанию, зашитый в ядро исходником с пояснениями.

use core::str::{FromStr, SplitWhitespace};

// ─── запись ───────────────────────────────────────────────────────────────────

/// Одна строка конфига: вид и всё, что за ним.
///
/// Срезы исходного текста — разбор не выделяет памяти, поэтому годится и там, где кучи нет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Первое слово строки: `service`, `ui`, `bind`, …
    pub kind: &'a str,
    /// Всё после вида, без крайних пробелов.
    pub rest: &'a str,
}

impl<'a> Entry<'a> {
    /// Ключ записи — первое слово хвоста. У `ui accent #4c7dfd` это `accent`.
    ///
    /// Пустая строка, если хвоста нет вовсе: `vvsh-core` такую запись не выпустит, но конфиг
    /// может прийти и не от него (ядро читает свой текст по умолчанию, человек — правит руками).
    pub fn key(&self) -> &'a str {
        self.rest.split_whitespace().next().unwrap_or("")
    }

    /// Значение — всё после ключа, КАК ЕСТЬ.
    ///
    /// Именно так читается имя объекта store, путь и любая строка, которую в конфиге написали
    /// в кавычках: пробел в ней — часть значения, а не разделитель.
    pub fn tail(&self) -> &'a str {
        let r = self.rest;
        match r.find(char::is_whitespace) {
            Some(i) => r[i..].trim(),
            None => "",
        }
    }

    /// Слова хвоста, ВКЛЮЧАЯ ключ. Так читаются записи-списки (`packages a b c`) и записи с
    /// несколькими полями (`bind wm Super+Return spawn-term`).
    pub fn words(&self) -> SplitWhitespace<'a> {
        self.rest.split_whitespace()
    }

    /// Слово хвоста по номеру (`0` — ключ).
    pub fn word(&self, i: usize) -> Option<&'a str> {
        self.words().nth(i)
    }

    /// Значение числом. Из хвоста целиком, а не из первого слова: лишнее слово в числовой
    /// записи — ошибка конфига, и молча взять половину было бы хуже, чем не взять ничего.
    pub fn num<T: FromStr>(&self) -> Option<T> {
        self.tail().parse().ok()
    }
}

// ─── обход ────────────────────────────────────────────────────────────────────

/// Все записи текста, по порядку.
pub fn entries(text: &str) -> impl Iterator<Item = Entry<'_>> {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (kind, rest) = match line.find(char::is_whitespace) {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        Some(Entry { kind, rest })
    })
}

/// Записи одного вида, по порядку.
///
/// Записей вида в тексте может быть сколько угодно — их приносят разные модули конфига, и
/// объявленное ими множество есть их ОБЪЕДИНЕНИЕ (так устроены `service`, `packages`, `bind`).
pub fn of<'a>(text: &'a str, kind: &'a str) -> impl Iterator<Item = Entry<'a>> {
    entries(text).filter(move |e| e.kind == kind)
}

/// Первая запись `<вид> <ключ> …`. Первая, а не последняя: раньше объявленное сильнее — тот же
/// порядок, что у `append` в самом конфиге.
pub fn find<'a>(text: &'a str, kind: &'a str, key: &str) -> Option<Entry<'a>> {
    of(text, kind).find(|e| e.key() == key)
}

/// Значение записи `<вид> <ключ> <значение>` — хвостом целиком.
pub fn get<'a>(text: &'a str, kind: &'a str, key: &str) -> Option<&'a str> {
    find(text, kind, key).map(|e| e.tail()).filter(|v| !v.is_empty())
}

/// Значение числом.
pub fn num<T: FromStr>(text: &str, kind: &str, key: &str) -> Option<T> {
    find(text, kind, key)?.num()
}

/// Выключатель: запись есть и её значение — `on`.
///
/// Именно значением, а не «строка присутствует»: `desktop bar off` обязан ВЫКЛЮЧАТЬ панель, а не
/// молча значить то же, что и `on`.
pub fn on(text: &str, kind: &str, key: &str) -> bool {
    find(text, kind, key).is_some_and(|e| e.tail() == "on")
}

// ─── словарь видов ────────────────────────────────────────────────────────────

/// Вид записи: как её писать и кто её читает.
pub struct Kind {
    /// Первое слово строки.
    pub name: &'static str,
    /// Форма записи в языке конфига — для сообщения об ошибке `rebuild`.
    pub form: &'static str,
    /// Сколько ЗНАЧЕНИЙ у записи в тексте конфига: `ui("accent", "#4c7dfd")` — два. `None` —
    /// список произвольной длины (`service` с правами, `packages` с именами).
    ///
    /// Считается по форме языка, а не по словам строки: одно значение в кавычках может содержать
    /// пробелы и стать в строке несколькими словами.
    pub values: Option<usize>,
    /// Читает ли запись ЯДРО на загрузке. Прочие адресованы программам, и ядро их пропускает
    /// молча — конфиг у системы один, а читателей много.
    pub kernel: bool,
}

/// Словарь конфига целиком. Единственное место, где перечислены виды записей.
///
/// Добавить вид — добавить строку СЮДА: `rebuild` начнёт проверять его форму, а ядро перестанет
/// звать его неизвестной директивой. До Вехи 148.8 новый вид требовал правки ядра.
pub const KINDS: &[Kind] = &[
    Kind {
        name: "service",
        form: "(service имя право…)",
        values: None,
        kernel: true,
    },
    Kind {
        name: "shell",
        form: "(shell имя право…)",
        values: None,
        kernel: true,
    },
    Kind {
        name: "bind",
        form: "(bind режим клавиша действие)",
        values: Some(3),
        kernel: false,
    },
    Kind {
        name: "terminal",
        form: "(terminal ключ значение)",
        values: Some(2),
        kernel: false,
    },
    Kind {
        name: "desktop",
        form: "(desktop ключ значение)",
        values: Some(2),
        kernel: false,
    },
    Kind {
        name: "ui",
        form: "(ui ключ значение)",
        values: Some(2),
        kernel: false,
    },
    // Веха 160 — РАСКЛАДКА ПАНЕЛИ: `(bar группа остров)`, по строке на остров. Группа — `left`,
    // `center` или `right`, порядок строк внутри группы и есть порядок слева направо.
    //
    // Строкой на остров, а не одной строкой со списком, по той же причине, по которой так
    // сделаны `desktop power` и `packages`: список в ОДНОЙ строке пришлось бы разбирать второй
    // грамматикой (свои пробелы, свои кавычки), а строки конфиг уже умеет складывать сам.
    Kind {
        name: "bar",
        form: "(bar группа остров)",
        values: Some(2),
        kernel: false,
    },
    Kind {
        name: "device",
        form: "(device ключ значение)",
        values: Some(2),
        kernel: false,
    },
    Kind {
        name: "packages",
        form: "(packages имя…)",
        values: None,
        kernel: false,
    },
    // Веха 167 — ПРОГРАММА ПО УМОЛЧАНИЮ: `(default роль имя)`. Роль — «чем открывать вот такое»
    // («terminal», «files», «editor»), имя — корень программы в store.
    //
    // Отдельный вид, а не ключ внутри `desktop`, потому что отвечает на другой вопрос. `desktop`
    // говорит, КОМУ композитор отдаёт права; здесь же нет ни прав, ни композитора — есть выбор
    // человека, чем ему открывать. Спрашивать его будет любая программа, а не одна.
    Kind {
        name: "default",
        form: "(default роль имя)",
        values: Some(2),
        kernel: false,
    },
    // Канал один: «откуда система берёт софт» — решение, а не список предпочтений.
    Kind {
        name: "channel",
        form: "(channel url)",
        values: Some(1),
        kernel: false,
    },
];

/// Вид по имени. `None` — такого вида в системе нет (опечатка в конфиге).
pub fn kind(name: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "\
service posixfs store:rw
shell wm endpoint:posixfs store:rwx power env arg:term
desktop wallpaper мои обои.png
desktop anim 360
desktop bar on
ui accent #4c7dfd
ui scale 150
device nameserver 10.0.2.3
device name voidbook
packages htop
packages nerd-fonts-fira-mono ripgrep
bind wm Super+Return spawn-term
";

    #[test]
    fn tail_is_taken_whole() {
        assert_eq!(get(TEXT, "desktop", "wallpaper"), Some("мои обои.png"));
    }

    #[test]
    fn key_matches_by_word_not_by_prefix() {
        // Ровно тот случай, ради которого разбор общий: `nameserver` объявлен РАНЬШЕ, и старый
        // `strip_prefix("name")` отдавал бы на запрос `name` строку «server 10.0.2.3».
        assert_eq!(get(TEXT, "device", "name"), Some("voidbook"));
        assert_eq!(get(TEXT, "device", "nameserver"), Some("10.0.2.3"));
    }

    #[test]
    fn numbers_and_switches() {
        assert_eq!(num::<u64>(TEXT, "desktop", "anim"), Some(360));
        assert_eq!(num::<u32>(TEXT, "ui", "scale"), Some(150));
        assert!(on(TEXT, "desktop", "bar"));
        assert!(!on(TEXT, "desktop", "wallpaper"));
        // Число с мусором в хвосте — не число, а не «первое слово».
        assert_eq!(num::<u64>(TEXT, "desktop", "wallpaper"), None);
    }

    #[test]
    fn lists_are_a_union_of_all_entries() {
        let names: Vec<&str> = of(TEXT, "packages").flat_map(|e| e.words()).collect();
        assert_eq!(names, ["htop", "nerd-fonts-fira-mono", "ripgrep"]);
    }

    #[test]
    fn fields_by_number() {
        let b = of(TEXT, "bind").next().unwrap();
        assert_eq!(b.key(), "wm");
        assert_eq!(b.word(1), Some("Super+Return"));
        assert_eq!(b.word(2), Some("spawn-term"));
        assert_eq!(b.word(3), None);
    }

    #[test]
    fn kernel_reads_only_its_own() {
        assert!(kind("service").unwrap().kernel);
        assert!(!kind("ui").unwrap().kernel);
        assert!(kind("сервис").is_none());
    }

    #[test]
    fn junk_lines_are_skipped() {
        assert_eq!(entries("\n  # комментарий\n\n  ui accent #123456  \n").count(), 1);
        // Строка из одного вида — не запись с ключом, но и не повод падать.
        let e = entries("desktop").next().unwrap();
        assert_eq!((e.key(), e.tail()), ("", ""));
    }
}
