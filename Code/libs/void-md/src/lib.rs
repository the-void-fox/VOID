//! void-md — разметка руководства системы (Веха 223.1).
//!
//! ## Зачем своя, а не чужой разбор markdown
//!
//! Потому что нужен не markdown, а ровно его обрезок: заголовки, абзацы, списки, блоки кода и
//! таблицы. Полный разбор умеет ссылки, картинки, вложенные цитаты и html — всё это в системе без
//! браузера показывать нечем, а тянуть ради четырёх видов блока чужой крейт в оффлайн-сборку
//! дороже, чем написать двести строк.
//!
//! ## Почему разбор и укладка РАЗДЕЛЕНЫ
//!
//! Читателей у одного текста двое: терминал (моноширинный, знает ширину в знаках) и окно
//! (пропорциональный шрифт, знает ширину в пикселях и умеет рисовать заголовок крупнее). Общий у
//! них разбор, а укладка у каждого своя — попытка сделать одну на двоих кончилась бы тем, что
//! окно рисует текст, разложенный под чужую ширину.
//!
//! Здесь: [`parse`] — общий разбор, [`render_text`] — укладка для терминала. Окно берёт блоки и
//! раскладывает их само.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Кусок документа. Ровно то, что мы умеем показать, и ничего сверх.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Заголовок: уровень (1 — `#`, 2 — `##`, …) и текст.
    Heading(u8, String),
    /// Абзац: строки исходника уже склеены в одну — переносить их дело укладки.
    Para(String),
    /// Пункты списка, каждый одной строкой (вложенности нет: в руководстве она не понадобилась).
    List(Vec<String>),
    /// Блок кода: строки как есть, ничего не переносим и не склеиваем.
    Code(Vec<String>),
    /// Таблица. `head` — был ли под первым рядом разделитель `|---|`, то есть шапка ли это.
    ///
    /// Помнить это приходится потому, что показывают таблицу по-разному. Колонками шапка нужна;
    /// а в узком окне таблица разворачивается парами «ключ — объяснение», и там строка
    /// «токен | что даёт» превращается в пару без смысла: подписи к колонкам, которых нет.
    Table { head: bool, rows: Vec<Vec<String>> },
}

/// Разобрать текст руководства в блоки.
///
/// Разметка намеренно скупая, и это описание её целиком:
///
/// | что | как пишется |
/// |---|---|
/// | заголовок | `## Текст` в начале строки |
/// | абзац | подряд идущие строки, пустая кончает |
/// | список | строка начинается с `- `; продолжение — с отступа |
/// | код | между строками из трёх обратных кавычек |
/// | таблица | строки, начинающиеся с `\|`; строка-разделитель пропускается |
pub fn parse(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut строки = src.lines().peekable();
    while let Some(l) = строки.next() {
        let t = l.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        // ── блок кода ──
        if t.trim_start().starts_with("```") {
            let mut code = Vec::new();
            for l in строки.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                code.push(String::from(l));
            }
            out.push(Block::Code(code));
            continue;
        }
        // ── заголовок ──
        if let Some(rest) = t.strip_prefix('#') {
            let mut level = 1u8;
            let mut rest = rest;
            while let Some(r) = rest.strip_prefix('#') {
                level += 1;
                rest = r;
            }
            out.push(Block::Heading(level, String::from(rest.trim())));
            continue;
        }
        // ── таблица ──
        if t.trim_start().starts_with('|') {
            let mut rows = Vec::new();
            let mut line = t;
            let mut head = false;
            loop {
                // Разделитель `|---|---|` — не данные, а черта под шапкой. Встретив его первым
                // делом, запоминаем: значит ряд над ним был шапкой.
                let cells = row_cells(line);
                if cells.iter().all(|c| sep(c)) {
                    head = rows.len() == 1;
                } else {
                    rows.push(cells);
                }
                match строки.peek() {
                    Some(n) if n.trim_start().starts_with('|') => line = строки.next().unwrap(),
                    _ => break,
                }
            }
            out.push(Block::Table { head, rows: rows });
            continue;
        }
        // ── список ──
        if item(t).is_some() {
            let mut items = Vec::new();
            let mut current = String::from(item(t).unwrap());
            loop {
                match строки.peek() {
                    // Продолжение пункта: непустая строка с отступом и не новый пункт.
                    Some(n)
                        if !n.trim().is_empty()
                            && n.starts_with(' ')
                            && item(n.trim_start()).is_none() =>
                    {
                        current.push(' ');
                        current.push_str(строки.next().unwrap().trim());
                    }
                    Some(n) if item(n.trim_start()).is_some() => {
                        items.push(core::mem::take(&mut current));
                        current = String::from(item(строки.next().unwrap().trim_start()).unwrap());
                    }
                    _ => break,
                }
            }
            items.push(current);
            out.push(Block::List(items));
            continue;
        }
        // ── абзац ──
        let mut para = String::from(t.trim());
        while let Some(n) = строки.peek() {
            let n = n.trim_end();
            if n.trim().is_empty()
                || n.trim_start().starts_with('#')
                || n.trim_start().starts_with('|')
                || n.trim_start().starts_with("```")
                || item(n.trim_start()).is_some()
            {
                break;
            }
            para.push(' ');
            para.push_str(строки.next().unwrap().trim());
        }
        out.push(Block::Para(para));
    }
    out
}

/// Текст пункта списка без его знака. `None` — строка не пункт.
fn item(s: &str) -> Option<&str> {
    s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")).map(str::trim)
}

/// Ячейки строки таблицы: режем по `|`, крайние пустышки отбрасываем.
fn row_cells(s: &str) -> Vec<String> {
    let s = s.trim();
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = s.strip_suffix('|').unwrap_or(s);
    s.split('|').map(|c| String::from(c.trim())).collect()
}

/// Ячейка-разделитель шапки: только дефисы и двоеточия (`---`, `:--`, `--:`).
fn sep(c: &str) -> bool {
    !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
}

// ─── укладка для терминала ──────────────────────────────────────────────────────────────────

/// Разметка в текст под ширину `width` знаков.
///
/// Что делается с разметкой и почему:
///
/// - **заголовок подчёркивается**, а решётки уходят. В моноширинном окне заголовок иначе не
///   выделить: шрифт один, цвета у `doc` нет;
/// - **таблица выравнивается по столбцам** — ради неё всё и затевалось: `| a | b |` в терминале
///   нечитаемо, а колонки читаются;
/// - **код отступает на два пробела**, тройные кавычки уходят: рамка из знаков занимает две
///   строки и не говорит ничего, чего не сказал бы отступ;
/// - **`код` и `**жирный**`** теряют свои знаки: подчеркнуть их нечем, а знаки мешают читать.
pub fn render_text(blocks: &[Block], width: usize) -> String {
    let w = width.max(20);
    let mut out = String::new();
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match b {
            Block::Heading(level, t) => {
                let t = strip_marks(t);
                out.push_str(&t);
                out.push('\n');
                // Первый уровень — двойной чертой, прочие одинарной: иерархию видно и так.
                let rule = if *level <= 1 { '=' } else { '-' };
                for _ in 0..t.chars().count().min(w) {
                    out.push(rule);
                }
                out.push('\n');
            }
            Block::Para(t) => {
                wrap(&strip_marks(t), w, "", &mut out);
            }
            Block::List(items) => {
                for p in items {
                    let measure = out.len();
                    wrap(&strip_marks(p), w.saturating_sub(2), "  ", &mut out);
                    // Знак пункта ставим поверх отступа первой строки — так продолжение
                    // выравнивается под текстом, а не под знаком.
                    unsafe { out.as_bytes_mut()[measure] = b'-' };
                }
            }
            Block::Code(строки) => {
                for s in строки {
                    out.push_str("  ");
                    out.push_str(s);
                    out.push('\n');
                }
            }
            Block::Table { rows, .. } => table(rows, w, &mut out),
        }
    }
    out
}

/// Снять знаки разметки, которые в моноширинном тексте показать нечем.
fn strip_marks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '`' => {}
            '*' if it.peek() == Some(&'*') => {
                it.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// Перенести абзац по словам. `indent` ставится перед КАЖДОЙ строкой.
fn wrap(s: &str, w: usize, indent: &str, out: &mut String) {
    let mut len = 0usize;
    let mut first_one = true;
    for word in s.split_whitespace() {
        let n = word.chars().count();
        if !first_one && len + 1 + n > w {
            out.push('\n');
            len = 0;
            first_one = true;
        }
        if first_one {
            out.push_str(indent);
            first_one = false;
        } else {
            out.push(' ');
            len += 1;
        }
        out.push_str(word);
        len += n;
    }
    out.push('\n');
}

/// Таблица колонками. Ширины считаются по самой длинной ячейке, но вся таблица обязана влезть в
/// `w`: если не влезает, лишнее срезается — обрезанная колонка читается, уехавшая нет.
fn table(rows: &[Vec<String>], w: usize, out: &mut String) {
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let clean: Vec<Vec<String>> =
        rows.iter().map(|r| r.iter().map(|c| strip_marks(c)).collect()).collect();
    let mut widths = alloc::vec![0usize; cols];
    for r in &clean {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    // Два пробела между колонками.
    let gap = 2;
    let total: usize = widths.iter().sum::<usize>() + gap * (cols - 1);
    if total > w {
        // Режем САМУЮ ШИРОКУЮ колонку, пока не влезет: обычно это колонка описания, и потерять
        // хвост описания легче, чем съехавшую разметку.
        let mut extra = total - w;
        while extra > 0 {
            let (i, _) = widths.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
            let slice = extra.min(widths[i].saturating_sub(4));
            if slice == 0 {
                break;
            }
            widths[i] -= slice;
            extra -= slice;
        }
    }
    for r in &clean {
        let mut line = String::new();
        for i in 0..cols {
            if i > 0 {
                line.push_str("  ");
            }
            let is_empty = String::new();
            let c = r.get(i).unwrap_or(&is_empty);
            let measure = c.chars().count();
            if measure > widths[i] {
                line.extend(c.chars().take(widths[i].saturating_sub(1)));
                line.push('…');
            } else {
                line.push_str(c);
                // Хвост последней колонки не добиваем: пробелы в конце строки никому не нужны.
                if i + 1 < cols {
                    for _ in measure..widths[i] {
                        line.push(' ');
                    }
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn заголовки_и_уровни() {
        let b = parse("# Раз\n\n## Два\n\n### Три\n");
        assert_eq!(
            b,
            vec![
                Block::Heading(1, "Раз".into()),
                Block::Heading(2, "Два".into()),
                Block::Heading(3, "Три".into()),
            ]
        );
    }

    /// Абзац склеивается из подряд идущих строк: в исходнике он перенесён под ширину файла, а
    /// показать его надо под ширину ОКНА, которая другая.
    #[test]
    fn абзац_склеивается_пустая_строка_кончает() {
        let b = parse("первая строка\nвторая строка\n\nдругой абзац\n");
        assert_eq!(
            b,
            vec![
                Block::Para("первая строка вторая строка".into()),
                Block::Para("другой абзац".into()),
            ]
        );
    }

    #[test]
    fn код_не_трогаем() {
        let b = parse("```\n  ls /etc\n\n  cat файл\n```\n");
        assert_eq!(b, vec![Block::Code(vec!["  ls /etc".into(), "".into(), "  cat файл".into()])]);
    }

    /// Продолжение пункта с отступом принадлежит пункту, а не следующему. Иначе список
    /// разваливается на абзацы ровно там, где пункт длинный.
    #[test]
    fn список_с_продолжением() {
        let b = parse("- первый пункт\n  его продолжение\n- второй\n");
        assert_eq!(
            b,
            vec![Block::List(vec!["первый пункт его продолжение".into(), "второй".into()])]
        );
    }

    #[test]
    fn таблица_без_разделителя() {
        let b = parse("| что | где |\n|---|---|\n| ключ | store |\n");
        assert_eq!(
            b,
            vec![Block::Table {
                head: true,
                rows: vec![
                    vec!["что".into(), "где".into()],
                    vec!["ключ".into(), "store".into()],
                ],
            }]
        );
    }

    /// Веха 223.1 — ради этого всё и затевалось: `| a | b |` в терминале нечитаемо.
    #[test]
    fn таблица_выравнивается_колонками() {
        let t = render_text(&parse("| что | где |\n|---|---|\n| очень длинный ключ | s |\n"), 60);
        let строки: Vec<&str> = t.lines().collect();
        assert_eq!(строки[0], "что                 где");
        assert_eq!(строки[1], "очень длинный ключ  s");
    }

    /// Таблица шире окна не уезжает за край: лишнее срезается с самой широкой колонки.
    #[test]
    fn широкая_таблица_режется_по_ширине() {
        let long_one = "| имя | описание очень длинное которое никуда не влезет совсем |";
        let t = render_text(&parse(long_one), 30);
        for l in t.lines() {
            assert!(l.chars().count() <= 30, "строка шире окна: {:?}", l);
        }
    }

    /// Шапку надо ОТЛИЧАТЬ: в узком окне таблица разворачивается парами, и подпись к колонке
    /// превращается там в пару без смысла.
    #[test]
    fn шапка_таблицы_помнится() {
        let with_head = parse("| что | где |\n|---|---|\n| а | б |\n");
        assert!(matches!(with_head[0], Block::Table { head: true, .. }));
        let without = parse("| а | б |\n| в | г |\n");
        assert!(matches!(without[0], Block::Table { head: false, .. }));
    }

    #[test]
    fn знаки_разметки_уходят() {
        let t = render_text(&parse("это `код` и **жирный** текст"), 60);
        assert_eq!(t.trim(), "это код и жирный текст");
    }

    #[test]
    fn заголовок_подчёркивается() {
        let t = render_text(&parse("## Права"), 60);
        assert_eq!(t.lines().collect::<Vec<_>>(), vec!["Права", "-----"]);
    }

    /// Абзац переносится ПО СЛОВАМ и не вылезает за ширину — её знает только показывающий.
    #[test]
    fn абзац_переносится_по_ширине() {
        let src = "одно два три четыре пять шесть семь восемь девять десять";
        for w in [20usize, 30, 40] {
            let t = render_text(&parse(src), w);
            for l in t.lines() {
                assert!(l.chars().count() <= w, "ширина {}: {:?}", w, l);
            }
            // И ни одного слова не потеряли.
            assert_eq!(t.split_whitespace().count(), src.split_whitespace().count());
        }
    }

    #[test]
    fn пункты_списка_со_знаком() {
        let t = render_text(&parse("- раз\n- два"), 40);
        assert_eq!(t.lines().collect::<Vec<_>>(), vec!["- раз", "- два"]);
    }
}
