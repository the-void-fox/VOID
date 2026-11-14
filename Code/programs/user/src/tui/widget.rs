//! Виджеты: рамка, текст, список, полоса хода, строка состояния, поле ввода.
//!
//! **Правило то же, что у оконного тулкита ([[void-ui]]): экран собирают ИСКЛЮЧИТЕЛЬНО из того,
//! что лежит здесь.** Не хватает виджета — дописать сюда, а не рисовать знаки на месте. Довод не
//! про красоту: своя рамка в каждой программе — это своя мера отступа, своя ширина заголовка и
//! свой цвет выделения, и расходятся они молча, пока кто-нибудь не поставит два окна рядом.
//!
//! Знаки псевдографики (`─│┌┐└┘`, `█░`) есть во ВСТРОЕННОМ шрифте (CP866), то есть рамка видна
//! и в консоли ядра, и в `term` без настоящего шрифта. Это не везение: таблица перевода
//! Unicode→CP866 (`void_user::glyph`) их содержит нарочно.

use super::buf::Buffer;
use super::layout::Rect;
use super::style::{Color, Style, REVERSE};

/// Рамка с заголовком. Возвращает ВНУТРЕННОСТЬ — то, во что рисовать дальше.
///
/// Заголовок пустой — рамка просто рамка. Слишком узко для рамки (меньше двух ячеек в любую
/// сторону) — рисуем что влезает и отдаём пустой прямоугольник: на узком окне лучше потерять
/// рамку, чем содержимое.
pub fn block(buf: &mut Buffer, r: Rect, title: &str, style: Style) -> Rect {
    if r.w < 2 || r.h < 2 {
        return r;
    }
    let (x0, y0, x1, y1) = (r.x, r.y, r.right() - 1, r.bottom() - 1);
    for x in x0 + 1..x1 {
        buf.set(x, y0, '─', style);
        buf.set(x, y1, '─', style);
    }
    for y in y0 + 1..y1 {
        buf.set(x0, y, '│', style);
        buf.set(x1, y, '│', style);
    }
    buf.set(x0, y0, '┌', style);
    buf.set(x1, y0, '┐', style);
    buf.set(x0, y1, '└', style);
    buf.set(x1, y1, '┘', style);
    if !title.is_empty() && r.w > 4 {
        // Заголовок с пробелами по бокам — чтобы он не слипался с углом рамки.
        let room = r.w - 4;
        let n = buf.put_clip(x0 + 2, y0, title, room, style);
        buf.set(x0 + 1, y0, ' ', style);
        buf.set(x0 + 2 + n, y0, ' ', style);
    }
    r.shrink(1, 1)
}

/// Текст с переносом ПО СЛОВАМ. Возвращает, сколько строк занято.
///
/// По словам, а не по ячейкам: разорванное посреди слова читается как ошибка вывода. Слово
/// длиннее строки всё же рвётся — иначе оно не поместится никогда.
pub fn text(buf: &mut Buffer, r: Rect, s: &str, style: Style) -> u16 {
    let mut y = r.y;
    for para in s.split('\n') {
        if y >= r.bottom() {
            break;
        }
        let mut x = r.x;
        for word in para.split(' ') {
            let len = word.chars().count() as u16;
            if x > r.x && x + len > r.right() {
                y += 1;
                x = r.x;
                if y >= r.bottom() {
                    break;
                }
            }
            if len > r.w {
                // Слово шире строки: пишем сколько влезет и продолжаем со следующей.
                let mut rest = word;
                while !rest.is_empty() && y < r.bottom() {
                    let take = (r.right() - x) as usize;
                    let end = rest.char_indices().nth(take).map_or(rest.len(), |(i, _)| i);
                    buf.put(x, y, &rest[..end], r.right() - x, style);
                    rest = &rest[end..];
                    if !rest.is_empty() {
                        y += 1;
                        x = r.x;
                    } else {
                        x += (end as u16).min(r.right() - x);
                    }
                }
            } else {
                x += buf.put(x, y, word, r.right() - x, style);
            }
            if x < r.right() {
                buf.set(x, y, ' ', style);
                x += 1;
            }
        }
        y += 1;
    }
    y.saturating_sub(r.y)
}

/// Список строк с выделением и прокруткой.
///
/// `top` — состояние программы (первая видимая строка), и правим мы его здесь: прокрутка должна
/// следовать за выделением сама, иначе каждая программа заводит свою арифметику «не уехало ли
/// выделение за край» — и ошибается в ней по-своему.
pub fn list(
    buf: &mut Buffer,
    r: Rect,
    items: &[&str],
    sel: Option<usize>,
    top: &mut usize,
    style: Style,
) {
    if r.is_empty() {
        return;
    }
    let rows = r.h as usize;
    if let Some(s) = sel {
        if s < *top {
            *top = s;
        } else if s >= *top + rows {
            *top = s + 1 - rows;
        }
    }
    // Список укоротился (отфильтровали, удалили) — прокрутка не должна висеть за его концом.
    if *top + rows > items.len() {
        *top = items.len().saturating_sub(rows);
    }
    for (i, item) in items.iter().skip(*top).take(rows).enumerate() {
        let y = r.y + i as u16;
        let picked = sel == Some(*top + i);
        let st = if picked { style.with(REVERSE) } else { style };
        // Выделенная строка красится ЦЕЛИКОМ, до правого края: полоса в пол-ширины выглядит как
        // недорисованная, а не как выбор.
        if picked {
            buf.fill(Rect::new(r.x, y, r.w, 1), ' ', st);
        }
        buf.put_clip(r.x, y, item, r.w, st);
    }
}

/// Полоса хода: `доля` от 0.0 до 1.0.
///
/// Рисуется блоками `█` по заполненному и `░` по пустому — и то и другое есть во встроенном
/// шрифте. Цветом фона полосу не делаем: в терминале фон это чужая тема, и «синяя полоса» на
/// синей теме исчезает.
pub fn gauge(buf: &mut Buffer, r: Rect, share: f32, style: Style) {
    if r.is_empty() {
        return;
    }
    let full = (share.clamp(0.0, 1.0) * r.w as f32 + 0.5) as u16;
    for x in 0..r.w {
        buf.set(r.x + x, r.y, if x < full { '█' } else { '░' }, style);
    }
}

/// Строка состояния: слева одно, справа другое, между ними фон.
///
/// Правое прижато к краю, и если места мало — его режет ПЕРВЫМ: слева обычно имя файла или
/// режим (без него непонятно, где ты), справа — числа, которые можно пережить.
pub fn status(buf: &mut Buffer, r: Rect, left: &str, right: &str, style: Style) {
    if r.is_empty() {
        return;
    }
    buf.fill(Rect::new(r.x, r.y, r.w, 1), ' ', style);
    let rlen = (right.chars().count() as u16).min(r.w);
    let room_left = r.w.saturating_sub(rlen + 1);
    buf.put_clip(r.x, r.y, left, room_left, style);
    if rlen > 0 {
        buf.put(r.right() - rlen, r.y, right, rlen, style);
    }
}

/// Поле ввода: текст и курсор на нём. Возвращает ЯЧЕЙКУ КУРСОРА — её отдают
/// [`super::Screen::cursor`], чтобы настоящий курсор терминала стоял там, где человек печатает.
///
/// `cur` — позиция курсора В ЗНАКАХ (не в байтах): в русском тексте байт это полсимвола, и
/// считать курсор в байтах значит рано или поздно встать в середину буквы.
pub fn field(buf: &mut Buffer, r: Rect, s: &str, cur: usize, style: Style) -> Option<(u16, u16)> {
    if r.is_empty() {
        return None;
    }
    buf.fill(Rect::new(r.x, r.y, r.w, 1), ' ', style);
    // Длинная строка едет влево вместе с курсором: иначе курсор уходит за край и человек
    // печатает вслепую.
    let len = s.chars().count();
    let cur = cur.min(len);
    let off = cur.saturating_sub(r.w.saturating_sub(1) as usize);
    let shown: alloc::string::String = s.chars().skip(off).take(r.w as usize).collect();
    buf.put(r.x, r.y, &shown, r.w, style);
    Some((r.x + (cur - off) as u16, r.y))
}

/// Подпись «клавиша — что делает», как в нижней строке редакторов.
///
/// Отдельным виджетом, потому что это единственное место, где программа ОБЪЯСНЯЕТ СЕБЯ. Сделать
/// его одинаковым во всех программах дешевле, чем потом объяснять человеку, что `^S` в одной
/// значит то же, что `Ctrl-S` в другой.
pub fn hints(buf: &mut Buffer, r: Rect, pairs: &[(&str, &str)], style: Style) {
    let mut x = r.x;
    for (key, what) in pairs {
        if x >= r.right() {
            break;
        }
        x += buf.put(x, r.y, key, r.right() - x, style.with(REVERSE));
        if x < r.right() {
            buf.set(x, r.y, ' ', style);
            x += 1;
        }
        x += buf.put_clip(x, r.y, what, r.right().saturating_sub(x), style);
        if x + 2 < r.right() {
            buf.put(x, r.y, "  ", 2, style);
            x += 2;
        }
    }
}

/// Цвет для подписи «всё хорошо / внимание / беда». Одна на все программы: разнобой в том,
/// каким цветом система говорит об ошибке, — худшее, что можно сделать с цветом.
pub fn tone(ok: bool) -> Style {
    if ok {
        Style::fg(Color::GREEN)
    } else {
        Style::fg(Color::RED)
    }
}
