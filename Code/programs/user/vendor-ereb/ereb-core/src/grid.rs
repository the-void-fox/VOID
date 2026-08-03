//! [`Grid`] — двумерная сетка ячеек плюс курсор, «перо» (pen), режимы терминала,
//! alt-screen, scrollback и отслеживание изменившихся строк (dirty).
//!
//! Координаты везде нулевые: `col` — столбец (0 слева), `row` — строка
//! (0 сверху). Внешние ANSI-последовательности используют 1-based координаты;
//! их перевод в 0-based выполняется в [`crate::perform`].

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::cell::{Cell, CellFlags, Color};

/// Максимум строк scrollback (ушедших за верх основного экрана).
const SCROLLBACK_LIMIT: usize = 10_000;

/// Внешний вид курсора. Заполняется DECSCUSR; на MVP только хранится.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorStyle {
    /// Залитый прямоугольник.
    #[default]
    Block,
    /// Подчёркивание.
    Underline,
    /// Вертикальная черта.
    Bar,
}

/// Положение и состояние курсора грида.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// Строка (0 — верхняя).
    pub row: usize,
    /// Столбец (0 — левый).
    pub col: usize,
    /// Стиль отрисовки.
    pub style: CursorStyle,
    /// Видим ли курсор (`ESC [ ?25 h/l`).
    pub visible: bool,
    /// Мигает ли курсор.
    pub blinking: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            row: 0,
            col: 0,
            style: CursorStyle::Block,
            visible: true,
            blinking: true,
        }
    }
}

/// Режим стирания для [`Grid::erase_in_display`] / [`Grid::erase_in_line`].
///
/// Соответствует параметру CSI `J` и `K`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EraseMode {
    /// `0` — от курсора (включительно) до конца.
    ToEnd,
    /// `1` — от начала до курсора (включительно).
    ToStart,
    /// `2` — всё целиком.
    All,
}

impl EraseMode {
    /// Разбирает числовой параметр CSI в режим стирания.
    ///
    /// Значения `2` и `3` (для `J` — включая scrollback) трактуются как
    /// [`All`](Self::All).
    pub fn from_param(param: u16) -> Self {
        match param {
            1 => EraseMode::ToStart,
            2 | 3 => EraseMode::All,
            _ => EraseMode::ToEnd,
        }
    }
}

/// Протокол отчётов мыши (DECSET 9/1000/1002/1003). Хранится для бэкенда.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseProtocol {
    /// Мышь не репортится — выделение обрабатывает сам эмулятор.
    #[default]
    None,
    /// `?9` — X10: только нажатия.
    X10,
    /// `?1000` — нажатия и отпускания.
    Normal,
    /// `?1002` — плюс движение с зажатой кнопкой.
    ButtonEvent,
    /// `?1003` — любое движение.
    AnyEvent,
}

/// Кодировка координат мыши (DECSET 1006).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Классическая `CSI M` (координаты байтами).
    #[default]
    Default,
    /// `?1006` — SGR (`CSI < … M/m`), без лимита 223.
    Sgr,
}

/// Режимы терминала (private DECSET), которые мы отслеживаем для бэкенда.
///
/// Видимость курсора живёт в [`Cursor::visible`], активность alt-screen — в
/// [`Grid::is_alt_screen`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Modes {
    /// `?2004` — bracketed paste: вставку оборачивать в `ESC[200~`/`ESC[201~`.
    pub bracketed_paste: bool,
    /// Протокол отчётов мыши.
    pub mouse_protocol: MouseProtocol,
    /// Кодировка координат мыши.
    pub mouse_encoding: MouseEncoding,
}

/// Сохранённое состояние курсора (DECSC/DECRC, CSI s/u).
#[derive(Clone, Copy, Debug)]
struct SavedCursor {
    cursor: Cursor,
    pen: Cell,
}

/// Сохранённое состояние основного экрана, пока активен alt-screen.
struct Primary {
    cells: Vec<Cell>,
    cursor: Cursor,
}

/// Сетка терминала фиксированного размера `cols × rows`.
///
/// Ячейки хранятся плотно в row-major порядке: `cells[row * cols + col]`.
/// `Grid` сам реализует [`vte::Perform`] (см. [`crate::perform`]), поэтому его
/// можно скармливать прямо в `vte::Parser`.
pub struct Grid {
    cells: Vec<Cell>,
    cols: usize,
    rows: usize,
    cursor: Cursor,
    /// Текущие атрибуты SGR, применяемые к печатаемым символам.
    pen: Cell,
    /// Сохранённый курсор (DECSC/CSI s).
    saved_cursor: Option<SavedCursor>,
    /// Строки, ушедшие за верх основного экрана.
    scrollback: VecDeque<Vec<Cell>>,
    /// `Some`, пока активен alt-screen: хранит основной экран для восстановления.
    alt: Option<Primary>,
    /// Режимы терминала.
    modes: Modes,
    /// Заголовок окна (OSC 0/2).
    title: Option<String>,
    /// Верхняя граница области прокрутки (включительно, DECSTBM).
    scroll_top: usize,
    /// Нижняя граница области прокрутки (включительно, DECSTBM).
    scroll_bottom: usize,
    /// Изменившиеся с последней отрисовки строки (по индексу).
    dirty: Vec<bool>,
}

/// Перекладывает плоский буфер ячеек из размера `old` в `new`, привязывая
/// содержимое к верхнему-левому углу (без reflow). Не влезшее отбрасывается,
/// недостающее заполняется пробелами.
fn reshape(
    src: &[Cell],
    old_cols: usize,
    old_rows: usize,
    new_cols: usize,
    new_rows: usize,
) -> Vec<Cell> {
    let mut dst = vec![Cell::default(); new_cols * new_rows];
    let rows = old_rows.min(new_rows);
    let cols = old_cols.min(new_cols);
    for r in 0..rows {
        let s = r * old_cols;
        let d = r * new_cols;
        dst[d..d + cols].copy_from_slice(&src[s..s + cols]);
    }
    dst
}

impl Grid {
    /// Создаёт пустой грид размером `cols × rows`, заполненный пробелами.
    ///
    /// # Panics
    /// Если `cols` или `rows` равны нулю.
    pub fn new(cols: usize, rows: usize) -> Self {
        assert!(cols > 0 && rows > 0, "grid must be non-empty");
        Grid {
            cells: vec![Cell::default(); cols * rows],
            cols,
            rows,
            cursor: Cursor::default(),
            pen: Cell::default(),
            saved_cursor: None,
            scrollback: VecDeque::new(),
            alt: None,
            modes: Modes::default(),
            title: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            // Свежий грид целиком требует первой отрисовки.
            dirty: vec![true; rows],
        }
    }

    /// Меняет размер грида до `new_cols × new_rows`, сохраняя содержимое в
    /// перекрывающейся области (привязка к верхнему-левому углу).
    ///
    /// Курсор, сохранённый курсор и (если активен alt-screen) запомненный
    /// основной экран насыщаются к новым границам. Область прокрутки
    /// сбрасывается на весь экран, scrollback не трогается. На MVP без reflow
    /// (перенос длинных строк при сужении — позже).
    ///
    /// # Panics
    /// Если `new_cols` или `new_rows` равны нулю.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        assert!(new_cols > 0 && new_rows > 0, "grid must be non-empty");
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        self.cells = reshape(&self.cells, self.cols, self.rows, new_cols, new_rows);
        if let Some(primary) = self.alt.as_mut() {
            primary.cells = reshape(&primary.cells, self.cols, self.rows, new_cols, new_rows);
            primary.cursor.col = primary.cursor.col.min(new_cols - 1);
            primary.cursor.row = primary.cursor.row.min(new_rows - 1);
        }
        if let Some(saved) = self.saved_cursor.as_mut() {
            saved.cursor.col = saved.cursor.col.min(new_cols - 1);
            saved.cursor.row = saved.cursor.row.min(new_rows - 1);
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.cursor.col = self.cursor.col.min(new_cols - 1);
        self.cursor.row = self.cursor.row.min(new_rows - 1);
        self.scroll_top = 0;
        self.scroll_bottom = new_rows - 1;
        self.dirty = vec![true; new_rows];
    }

    /// Число столбцов.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Число строк.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Текущее состояние курсора.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Текущее «перо» — атрибуты, применяемые к новым символам.
    pub fn pen(&self) -> Cell {
        self.pen
    }

    /// Текущие режимы терминала (для бэкенда: мышь, bracketed paste).
    pub fn modes(&self) -> Modes {
        self.modes
    }

    /// Заголовок окна, заданный через OSC 0/2.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Активен ли alt-screen (`?1049`/`?47`/`?1047`).
    pub fn is_alt_screen(&self) -> bool {
        self.alt.is_some()
    }

    /// Число строк в scrollback.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Флаги «строка изменилась с последней отрисовки» (длина = [`rows`](Self::rows)).
    pub fn dirty(&self) -> &[bool] {
        &self.dirty
    }

    /// Сбрасывает отметки dirty (после того как кадр отрисован).
    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
    }

    #[inline]
    fn index(&self, col: usize, row: usize) -> usize {
        row * self.cols + col
    }

    #[inline]
    fn mark_dirty(&mut self, row: usize) {
        if let Some(flag) = self.dirty.get_mut(row) {
            *flag = true;
        }
    }

    #[inline]
    fn mark_all_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Ссылка на ячейку в позиции `(col, row)`.
    ///
    /// # Panics
    /// Если координаты выходят за пределы грида.
    pub fn cell_at(&self, col: usize, row: usize) -> &Cell {
        assert!(col < self.cols && row < self.rows, "cell out of bounds");
        &self.cells[self.index(col, row)]
    }

    /// Ячейка вьюпорта, сдвинутого вверх на `scroll` строк в scrollback.
    ///
    /// `scroll == 0` — живой экран (эквивалент [`cell_at`](Self::cell_at)).
    /// `scroll` насыщается к [`scrollback_len`](Self::scrollback_len): вьюпорт
    /// показывает последние `rows` строк объединённого буфера
    /// «scrollback ++ экран», сдвинутого на `scroll` строк вверх. Возвращает
    /// ячейку по значению (история и экран — разные хранилища).
    ///
    /// # Panics
    /// Если `col`/`row` выходят за пределы грида.
    pub fn view_cell(&self, col: usize, row: usize, scroll: usize) -> Cell {
        assert!(col < self.cols && row < self.rows, "cell out of bounds");
        let scroll = scroll.min(self.scrollback.len());
        let line = (self.scrollback.len() - scroll) + row;
        if line < self.scrollback.len() {
            // Строка из истории (могла быть сохранена при другой ширине).
            self.scrollback[line].get(col).copied().unwrap_or_default()
        } else {
            let r = line - self.scrollback.len();
            self.cells[r * self.cols + col]
        }
    }

    /// Изменяемая ссылка на ячейку в позиции `(col, row)`.
    ///
    /// Помечает строку как dirty. # Panics при выходе за пределы.
    pub fn cell_at_mut(&mut self, col: usize, row: usize) -> &mut Cell {
        assert!(col < self.cols && row < self.rows, "cell out of bounds");
        self.mark_dirty(row);
        let i = self.index(col, row);
        &mut self.cells[i]
    }

    // --- мутация пера (SGR) --------------------------------------------------

    /// Сбрасывает перо к атрибутам по умолчанию (`SGR 0`).
    pub fn reset_pen(&mut self) {
        self.pen.reset_attrs();
    }

    /// Добавляет атрибуты к перу.
    pub fn add_flags(&mut self, flags: CellFlags) {
        self.pen.flags.insert(flags);
    }

    /// Убирает атрибуты из пера.
    pub fn remove_flags(&mut self, flags: CellFlags) {
        self.pen.flags.remove(flags);
    }

    /// Устанавливает цвет переднего плана пера.
    pub fn set_fg(&mut self, color: Color) {
        self.pen.fg = color;
    }

    /// Устанавливает цвет фона пера.
    pub fn set_bg(&mut self, color: Color) {
        self.pen.bg = color;
    }

    // --- печать и движение курсора ------------------------------------------

    /// Печатает символ в позиции курсора текущим пером и двигает курсор вправо.
    ///
    /// При достижении правого края выполняется перенос на следующую строку
    /// (с автоскроллом, если нужно). Это «жадный» перенос — достаточно для MVP.
    pub fn put_char(&mut self, ch: char) {
        self.mark_dirty(self.cursor.row);
        let i = self.index(self.cursor.col, self.cursor.row);
        self.cells[i] = Cell { ch, ..self.pen };
        self.advance_cursor();
    }

    /// Двигает курсор на одну позицию вправо с переносом строки на краю.
    pub fn advance_cursor(&mut self) {
        if self.cursor.col + 1 < self.cols {
            self.cursor.col += 1;
        } else {
            self.cursor.col = 0;
            self.line_feed();
        }
    }

    /// Устанавливает курсор в `(col, row)`, насыщая координаты к границам грида.
    pub fn move_cursor(&mut self, col: usize, row: usize) {
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.row = row.min(self.rows - 1);
    }

    /// `CSI Ps G` / `` CSI Ps ` `` (CHA/HPA) — абсолютный столбец (1-based).
    pub fn move_cursor_col(&mut self, col_1based: u16) {
        let col = col_1based.max(1) as usize - 1;
        self.cursor.col = col.min(self.cols - 1);
    }

    /// `CSI Ps d` (VPA) — абсолютная строка (1-based).
    pub fn move_cursor_row(&mut self, row_1based: u16) {
        let row = row_1based.max(1) as usize - 1;
        self.cursor.row = row.min(self.rows - 1);
    }

    /// Сдвигает курсор вверх на `n` строк (не пересекая верхний край).
    pub fn move_up(&mut self, n: usize) {
        self.cursor.row = self.cursor.row.saturating_sub(n);
    }

    /// Сдвигает курсор вниз на `n` строк (не пересекая нижний край).
    pub fn move_down(&mut self, n: usize) {
        self.cursor.row = (self.cursor.row + n).min(self.rows - 1);
    }

    /// Сдвигает курсор вправо на `n` столбцов (не пересекая правый край).
    pub fn move_right(&mut self, n: usize) {
        self.cursor.col = (self.cursor.col + n).min(self.cols - 1);
    }

    /// Сдвигает курсор влево на `n` столбцов (не пересекая левый край).
    pub fn move_left(&mut self, n: usize) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    /// `\r` — возврат каретки: курсор в начало текущей строки.
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    /// `\n` — перевод строки: курсор на строку ниже, со скроллом на нижней
    /// границе области прокрутки.
    pub fn line_feed(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// `ESC M` (RI) — обратный индекс: на строку вверх, со скроллом вниз на
    /// верхней границе области прокрутки.
    pub fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    /// `ESC E` (NEL) — новая строка: возврат каретки плюс перевод строки.
    pub fn next_line(&mut self) {
        self.carriage_return();
        self.line_feed();
    }

    /// `\b` — забой: курсор на одну позицию влево (без переноса).
    pub fn backspace(&mut self) {
        self.move_left(1);
    }

    /// `\t` — табуляция: к следующей позиции, кратной 8 (насыщается к краю).
    pub fn tab(&mut self) {
        let next = (self.cursor.col / 8 + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
    }

    // --- сохранение/восстановление курсора (DECSC/DECRC, CSI s/u) ------------

    /// Сохраняет позицию курсора и перо.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
        });
    }

    /// Восстанавливает ранее сохранённые курсор и перо (если были).
    pub fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved_cursor {
            self.cursor = saved.cursor;
            self.pen = saved.pen;
            // На случай, если грид с тех пор уменьшился.
            self.cursor.col = self.cursor.col.min(self.cols - 1);
            self.cursor.row = self.cursor.row.min(self.rows - 1);
        }
    }

    // --- режимы и заголовок --------------------------------------------------

    /// Устанавливает/сбрасывает приватный режим DECSET (`?N h/l`).
    pub fn set_mode(&mut self, mode: u16, enabled: bool) {
        match mode {
            25 => self.cursor.visible = enabled,
            2004 => self.modes.bracketed_paste = enabled,
            47 | 1047 | 1049 => {
                if enabled {
                    self.enter_alt_screen();
                } else {
                    self.exit_alt_screen();
                }
            }
            9 => self.set_mouse_protocol(MouseProtocol::X10, enabled),
            1000 => self.set_mouse_protocol(MouseProtocol::Normal, enabled),
            1002 => self.set_mouse_protocol(MouseProtocol::ButtonEvent, enabled),
            1003 => self.set_mouse_protocol(MouseProtocol::AnyEvent, enabled),
            1006 => {
                self.modes.mouse_encoding = if enabled {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::Default
                };
            }
            _ => {}
        }
    }

    fn set_mouse_protocol(&mut self, protocol: MouseProtocol, enabled: bool) {
        self.modes.mouse_protocol = if enabled {
            protocol
        } else {
            MouseProtocol::None
        };
    }

    /// Устанавливает заголовок окна (OSC 0/2).
    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = Some(title.into());
    }

    /// `CSI t ; b r` (DECSTBM) — область прокрутки строками `[top, bottom]`.
    ///
    /// Аргументы 1-based; `0` означает «по умолчанию» (вся высота). Некорректный
    /// диапазон (`top >= bottom`) сбрасывает регион на весь экран. По стандарту
    /// курсор уезжает в домашнюю позицию.
    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let top = if top == 0 {
            0
        } else {
            (top as usize - 1).min(self.rows - 1)
        };
        let bottom = if bottom == 0 {
            self.rows - 1
        } else {
            (bottom as usize - 1).min(self.rows - 1)
        };
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.reset_scroll_region();
        }
        self.move_cursor(0, 0);
    }

    /// Сбрасывает область прокрутки на весь экран.
    fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
    }

    // --- alt-screen ----------------------------------------------------------

    /// Переключается на alt-screen: сохраняет основной экран и курсор, очищает.
    pub fn enter_alt_screen(&mut self) {
        if self.alt.is_some() {
            return;
        }
        self.save_cursor();
        let primary_cells = core::mem::replace(
            &mut self.cells,
            vec![Cell::default(); self.cols * self.rows],
        );
        self.alt = Some(Primary {
            cells: primary_cells,
            cursor: self.cursor,
        });
        self.cursor = Cursor {
            visible: self.cursor.visible,
            ..Cursor::default()
        };
        self.reset_scroll_region();
        self.mark_all_dirty();
    }

    /// Возвращается на основной экран, восстанавливая его содержимое и курсор.
    pub fn exit_alt_screen(&mut self) {
        if let Some(primary) = self.alt.take() {
            self.cells = primary.cells;
            self.cursor = primary.cursor;
            self.restore_cursor();
            self.reset_scroll_region();
            self.mark_all_dirty();
        }
    }

    // --- скролл и очистка ----------------------------------------------------

    /// Сдвигает строки `[top, bottom]` вверх на `n`; снизу диапазона — пустые.
    ///
    /// Чистая механика без scrollback: используется и областью прокрутки, и
    /// удалением строк (DL).
    fn shift_region_up(&mut self, top: usize, bottom: usize, n: usize) {
        let height = bottom - top + 1;
        let n = n.min(height);
        if n == 0 {
            return;
        }
        let move_rows = height - n;
        for i in 0..move_rows {
            let src = (top + i + n) * self.cols;
            let dst = (top + i) * self.cols;
            self.cells.copy_within(src..src + self.cols, dst);
        }
        for i in move_rows..height {
            let start = (top + i) * self.cols;
            for cell in &mut self.cells[start..start + self.cols] {
                *cell = Cell::default();
            }
        }
    }

    /// Сдвигает строки `[top, bottom]` вниз на `n`; сверху диапазона — пустые.
    fn shift_region_down(&mut self, top: usize, bottom: usize, n: usize) {
        let height = bottom - top + 1;
        let n = n.min(height);
        if n == 0 {
            return;
        }
        let move_rows = height - n;
        // С конца, чтобы не затирать ещё не скопированные строки.
        for i in (0..move_rows).rev() {
            let src = (top + i) * self.cols;
            let dst = (top + i + n) * self.cols;
            self.cells.copy_within(src..src + self.cols, dst);
        }
        for i in 0..n {
            let start = (top + i) * self.cols;
            for cell in &mut self.cells[start..start + self.cols] {
                *cell = Cell::default();
            }
        }
    }

    /// Прокручивает область прокрутки вверх на `n` строк; снизу — пустые.
    ///
    /// Если область — весь экран и мы не на alt-screen, ушедшие за верх строки
    /// уходят в scrollback (с лимитом [`SCROLLBACK_LIMIT`]); иначе отбрасываются.
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let n = n.min(bottom - top + 1);
        if n == 0 {
            return;
        }

        if top == 0 && bottom == self.rows - 1 && self.alt.is_none() {
            for row in 0..n {
                let start = row * self.cols;
                let line = self.cells[start..start + self.cols].to_vec();
                self.scrollback.push_back(line);
                if self.scrollback.len() > SCROLLBACK_LIMIT {
                    self.scrollback.pop_front();
                }
            }
        }

        self.shift_region_up(top, bottom, n);
        self.mark_all_dirty();
    }

    /// Прокручивает область прокрутки вниз на `n` строк; сверху — пустые.
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        self.shift_region_down(top, bottom, n);
        self.mark_all_dirty();
    }

    /// `CSI Ps L` (IL) — вставить `n` пустых строк начиная с текущей, сдвинув
    /// нижележащие до нижней границы области вниз. Курсор — к левому краю.
    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        self.shift_region_down(self.cursor.row, self.scroll_bottom, n);
        self.cursor.col = 0;
        self.mark_all_dirty();
    }

    /// `CSI Ps M` (DL) — удалить `n` строк начиная с текущей, подтянув
    /// нижележащие до нижней границы области вверх. Курсор — к левому краю.
    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bottom {
            return;
        }
        self.shift_region_up(self.cursor.row, self.scroll_bottom, n);
        self.cursor.col = 0;
        self.mark_all_dirty();
    }

    /// `CSI Ps @` (ICH) — вставить `n` пробелов под курсором, сдвинув хвост
    /// строки вправо (выехавшие за правый край символы теряются).
    pub fn insert_chars(&mut self, n: usize) {
        let cols = self.cols;
        let col = self.cursor.col;
        let n = n.min(cols - col);
        if n == 0 {
            return;
        }
        let row_start = self.cursor.row * cols;
        let line = &mut self.cells[row_start..row_start + cols];
        line.copy_within(col..cols - n, col + n);
        for cell in &mut line[col..col + n] {
            *cell = Cell::default();
        }
        self.mark_dirty(self.cursor.row);
    }

    /// `CSI Ps P` (DCH) — удалить `n` символов под курсором, подтянув хвост
    /// строки влево (справа дополняется пробелами).
    pub fn delete_chars(&mut self, n: usize) {
        let cols = self.cols;
        let col = self.cursor.col;
        let n = n.min(cols - col);
        if n == 0 {
            return;
        }
        let row_start = self.cursor.row * cols;
        let line = &mut self.cells[row_start..row_start + cols];
        line.copy_within(col + n.., col);
        for cell in &mut line[cols - n..cols] {
            *cell = Cell::default();
        }
        self.mark_dirty(self.cursor.row);
    }

    /// `CSI Ps X` (ECH) — стереть `n` символов от курсора (без сдвига строки).
    pub fn erase_chars(&mut self, n: usize) {
        let cols = self.cols;
        let row_start = self.cursor.row * cols;
        let col = self.cursor.col;
        let end = (col + n).min(cols);
        for cell in &mut self.cells[row_start + col..row_start + end] {
            *cell = Cell::default();
        }
        self.mark_dirty(self.cursor.row);
    }

    /// Полностью очищает грид (все ячейки → пробел). Курсор не трогается.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
        self.mark_all_dirty();
    }

    /// CSI `J` — стирание в пределах экрана относительно курсора.
    pub fn erase_in_display(&mut self, mode: EraseMode) {
        let cursor = self.index(self.cursor.col, self.cursor.row);
        let range = match mode {
            EraseMode::ToEnd => cursor..self.cells.len(),
            EraseMode::ToStart => 0..cursor + 1,
            EraseMode::All => 0..self.cells.len(),
        };
        for cell in &mut self.cells[range] {
            *cell = Cell::default();
        }
        match mode {
            EraseMode::ToEnd => {
                for row in self.cursor.row..self.rows {
                    self.mark_dirty(row);
                }
            }
            EraseMode::ToStart => {
                for row in 0..=self.cursor.row {
                    self.mark_dirty(row);
                }
            }
            EraseMode::All => self.mark_all_dirty(),
        }
    }

    /// CSI `K` — стирание в пределах текущей строки относительно курсора.
    pub fn erase_in_line(&mut self, mode: EraseMode) {
        let row_start = self.cursor.row * self.cols;
        let col = self.cursor.col;
        let range = match mode {
            EraseMode::ToEnd => row_start + col..row_start + self.cols,
            EraseMode::ToStart => row_start..row_start + col + 1,
            EraseMode::All => row_start..row_start + self.cols,
        };
        for cell in &mut self.cells[range] {
            *cell = Cell::default();
        }
        self.mark_dirty(self.cursor.row);
    }
}
