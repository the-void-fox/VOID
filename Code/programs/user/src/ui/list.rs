//! Фильтрованный список: набранное, отбор, выбор и прокрутка (Веха 148.3).
//!
//! ## Почему он здесь, а не в каждой программе
//!
//! Правило фазы — «виджеты не писать впрок». Здесь оно выполнено с запасом: потребителей **уже
//! два**, и написан этот список дважды — в строке запуска ([[launcher]]) и во вьювере корней
//! ([[store-viewer]]). По сотне строк каждый, и одинаковых до мелочей: `hits`, `sel`, `top`,
//! `scroll_to_sel`, «какая строка под курсором», Up/Down/PgUp/PgDn, колесо со ступенькой в три
//! строки, отбор подстрокой без учёта регистра.
//!
//! Третий и четвёртый на подходе и будут тем же списком: файловый менеджер, диспетчер задач,
//! настройки.
//!
//! ## Что здесь есть и чего здесь нет
//!
//! Здесь **навигация**: где мы в списке и что под курсором. Здесь НЕТ отбора — предикат
//! принадлежит программе, и по-разному: вьювер ищет подстроку в имени корня, строка запуска
//! отбирает в два яруса (сперва ярлыки, а если среди них пусто — все программы store). Список
//! говорит «набранное изменилось» ([`Hit::Query`]), а чем на это ответить, решает программа.
//!
//! Рисования здесь тоже нет: строку рисует [`super::Ui::entry`], и как она выглядит — дело
//! программы. Список только считает, ГДЕ она.
//!
//! ## «Горячая» строка — ВИДИМАЯ, а не элемент
//!
//! [`List::hot`] хранит номер строки НА ЭКРАНЕ, а не индекс элемента. Разница видна при
//! прокрутке: список уехал, под курсором оказался другой элемент, а подсвечена та же строка —
//! и кадра это не стоит. Ошибиться тут легко, а заметно это только по отставанию подсветки от
//! руки (Веха 148.2, жалоба владельца).

use alloc::string::String;
use alloc::vec::Vec;

use void_user::win::sym;

use super::Rect;

/// Что случилось от клавиши.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// Клавиша не наша — программа разбирается сама (Enter, Escape, свои аккорды).
    None,
    /// Сменился выбор или прокрутка: перерисовать список.
    Moved,
    /// Изменилось НАБРАННОЕ — программе надо пересобрать [`List::hits`] своим отбором.
    Query,
}

/// Состояние списка. Поля открыты: программа и так владеет своим отбором, а прятать от неё
/// `hits` значило бы заводить второй способ узнать, что нашлось.
#[derive(Default)]
pub struct List {
    /// Набранное в поле поиска.
    pub query: String,
    /// Индексы подходящих — их считает ПРОГРАММА своим отбором.
    pub hits: Vec<usize>,
    /// Выбранный (индекс в [`List::hits`]).
    pub sel: usize,
    /// Первая видимая строка.
    pub top: usize,
    /// Видимая строка под курсором. `None` — курсор не над строкой.
    pub hot: Option<usize>,

    // ── измерено кадром, спрошено событием ────────────────────────────────────────────────
    //
    // Раскладку знает только рисование (ей нужны тема и шрифт), а решать «изменилось ли что-то»
    // приходится в обработчике события, где их нет.
    area: Rect,
    row_h: i32,
    rows: usize,
}

impl List {
    /// Сказать списку, где он нарисован. Зовётся из кадра, до первой строки.
    pub fn measure(&mut self, area: Rect, row_h: i32, rows: usize) {
        self.area = area;
        self.row_h = row_h;
        self.rows = rows;
    }

    /// Сколько строк видно.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Прямоугольник строки `k` (видимой), нарисованной в этом кадре.
    pub fn row_rect(&self, k: usize) -> Rect {
        Rect::new(self.area.x, self.area.y + self.row_h * k as i32, self.area.w, self.row_h)
    }

    /// Какая ВИДИМАЯ строка под точкой. `None` — точка не над строкой или там пусто.
    pub fn row_at(&self, p: Option<(i32, i32)>) -> Option<usize> {
        let (x, y) = p?;
        if self.row_h <= 0 || !self.area.contains(x, y) {
            return None;
        }
        let k = ((y - self.area.y) / self.row_h) as usize;
        (k < self.rows && self.top + k < self.hits.len()).then_some(k)
    }

    /// Элемент под выбором.
    pub fn current(&self) -> Option<usize> {
        self.hits.get(self.sel).copied()
    }

    /// Держать выбранное в видимой части.
    pub fn scroll_to_sel(&mut self) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.rows > 0 && self.sel >= self.top + self.rows {
            self.top = self.sel + 1 - self.rows;
        }
    }

    /// Отбор пересобран программой — поставить выбор в начало.
    pub fn refiltered(&mut self) {
        self.sel = 0;
        self.top = 0;
    }

    /// Движение мыши. `true` — подсветка переехала на другую строку, и кадр за это стоит платить.
    ///
    /// Внутри одной строки на экране не меняется НИЧЕГО, и кадр там — чистый убыток: ровно из-за
    /// него подсветка и отставала от курсора (Веха 148.2).
    pub fn motion(&mut self, p: Option<(i32, i32)>) -> bool {
        let hot = self.row_at(p);
        let changed = hot != self.hot;
        self.hot = hot;
        changed
    }

    /// Колесо. `true` — список правда сдвинулся; упёрся в край — кадра это не стоит.
    ///
    /// Крутится СПИСОК, а не выбор: выбранное остаётся на месте, пока человек смотрит, что рядом.
    /// Так ведут себя списки везде, где их крутят мышью.
    pub fn wheel(&mut self, delta: i8) -> bool {
        const STEP: usize = 3;
        let max = self.hits.len().saturating_sub(self.rows);
        let was = self.top;
        self.top = if delta > 0 {
            self.top.saturating_sub(STEP)
        } else {
            (self.top + STEP).min(max)
        };
        self.top != was
    }

    /// Клавиша списка. Enter, Escape и свои аккорды сюда не входят — они значат разное у разных
    /// программ (у вьювера Escape очищает поиск, у строки запуска закрывает окно).
    pub fn key(&mut self, sym_code: u16, ch: u16) -> Hit {
        let last = self.hits.len().saturating_sub(1);
        let rows = self.rows.max(1);
        match sym_code {
            sym::UP => self.sel = self.sel.saturating_sub(1),
            sym::DOWN => self.sel = (self.sel + 1).min(last),
            sym::PAGE_UP => self.sel = self.sel.saturating_sub(rows),
            sym::PAGE_DOWN => self.sel = (self.sel + rows).min(last),
            sym::HOME => self.sel = 0,
            sym::END => self.sel = last,
            sym::BACKSPACE => {
                if self.query.pop().is_none() {
                    return Hit::None;
                }
                return Hit::Query;
            }
            _ => {
                // Печатающая клавиша — и только она: управляющие символы в строку поиска
                // попадать не должны, иначе Tab и Ctrl-что-нибудь молча «набирались» бы
                // невидимыми знаками.
                return match char::from_u32(ch as u32) {
                    Some(c) if !c.is_control() => {
                        self.query.push(c);
                        Hit::Query
                    }
                    _ => Hit::None,
                };
            }
        }
        self.scroll_to_sel();
        Hit::Moved
    }
}
