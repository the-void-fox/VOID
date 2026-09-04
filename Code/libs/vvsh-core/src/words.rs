//! Разбор строки КОМАНДЫ на слова: пробелы разделяют, кавычки склеивают (Веха 161).
//!
//! Командный режим шелла (`ls /etc`, а не `\ls("/etc")`) резал строку по пробелам и больше НЕ
//! ДЕЛАЛ НИЧЕГО: кавычки уезжали в аргумент буквально. `ls "/etc"` спрашивал у персоналии путь
//! `/"/etc"`, получал «нет такого» — и молчал, а `cat "/etc/system/bar.vv"` отвечал «файл не
//! найден», жалуясь на файл, хотя виновата была строка. Стоило это дня расследования «конфиг не
//! переживает перезагрузку»: `/etc` был на месте всегда, спрашивали не его.
//!
//! Правила ровно те, которых человек ждёт от шелла, и ни одного сверх:
//! - слова разделяют пробелы и табуляции, подряд идущие разделители — один;
//! - `"…"` склеивает: внутри пробел — обычный знак, а сами кавычки в слово НЕ попадают;
//! - внутри кавычек `\"` — кавычка, `\\` — обратная косая, прочий `\` — сам по себе;
//! - `""` даёт ПУСТОЕ слово (иначе `echo ""` было бы просто `echo`), а пробелы пустых слов не
//!   дают;
//! - незакрытая кавычка — ошибка ВСЛУХ, а не «дочитаем до конца строки»: молчаливая догадка
//!   здесь и есть то, из-за чего такие вещи не находятся.
//!
//! Вне кавычек `\` остаётся обычным знаком: ведущий `\` в строке REPL значит «дальше выражение»
//! (ADR 0013), и второго смысла у него быть не должно.
//!
//! Живёт в крейте, а не в бинаре, по той же причине, что и reader: синтаксис шелла проверяется
//! `cargo test` на хосте, без QEMU.

use alloc::string::String;
use alloc::vec::Vec;

/// Единственная ошибка разбора — текстом, готовым к печати.
pub const UNCLOSED: &str = "незакрытая кавычка";

/// Строка команды → слова. Ошибка — только незакрытая кавычка.
pub fn split_words(src: &str) -> Result<Vec<String>, &'static str> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    // Слово «началось» — не то же, что «непусто»: `""` обязано дать пустой аргумент.
    let mut started = false;
    let mut quoted = false;
    let mut it = src.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' if !quoted => {
                if started {
                    out.push(core::mem::take(&mut cur));
                    started = false;
                }
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => match it.next() {
                Some(n @ ('"' | '\\')) => cur.push(n),
                // Прочее экранирование не выдумываем: `\n` внутри кавычек — это два знака, и
                // притворяться переводом строки он не должен.
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err(UNCLOSED);
    }
    if started {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn w(s: &str) -> Vec<String> {
        split_words(s).expect("разбор")
    }

    #[test]
    fn bare_words_unchanged() {
        assert_eq!(w("ls /etc"), vec!["ls", "/etc"]);
        assert_eq!(w("  ping\t10.0.2.2  "), vec!["ping", "10.0.2.2"]);
        assert_eq!(w(""), Vec::<String>::new());
    }

    #[test]
    fn quotes_are_stripped() {
        // Тот самый случай: до Вехи 161 сюда уезжал путь вместе с кавычками.
        assert_eq!(w("ls \"/etc\""), vec!["ls", "/etc"]);
        assert_eq!(w("cat \"/etc/system/bar.vv\""), vec!["cat", "/etc/system/bar.vv"]);
    }

    #[test]
    fn quotes_glue_spaces() {
        assert_eq!(w("cat \"/etc/мой файл.vv\""), vec!["cat", "/etc/мой файл.vv"]);
        // Кавычка склеивает и с соседями: `a"b c"d` — одно слово.
        assert_eq!(w("a\"b c\"d"), vec!["ab cd"]);
    }

    #[test]
    fn empty_quoted_word_survives() {
        assert_eq!(w("echo \"\""), vec!["echo", ""]);
        assert_eq!(w("echo \"\" x"), vec!["echo", "", "x"]);
    }

    #[test]
    fn escapes_inside_quotes() {
        assert_eq!(w("echo \"он сказал \\\"да\\\"\""), vec!["echo", "он сказал \"да\""]);
        assert_eq!(w("echo \"c:\\\\путь\""), vec!["echo", "c:\\путь"]);
        // Неизвестное экранирование остаётся собой — обеими буквами.
        assert_eq!(w("echo \"a\\nb\""), vec!["echo", "a\\nb"]);
    }

    #[test]
    fn backslash_outside_quotes_is_literal() {
        // Вне кавычек `\` не экранирует: ведущий `\` строки — признак выражения (ADR 0013).
        assert_eq!(w("echo a\\b"), vec!["echo", "a\\b"]);
    }

    #[test]
    fn unclosed_quote_is_loud() {
        assert_eq!(split_words("cat \"/etc"), Err(UNCLOSED));
        assert_eq!(split_words("echo \"a\\\""), Err(UNCLOSED));
    }
}
