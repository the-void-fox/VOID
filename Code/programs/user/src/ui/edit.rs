//! Веха 167 — **правимая строка**: текст, курсор и выделение (`Shift+стрелки`, `Ctrl+A`).
//!
//! ## Почему в тулките, а не в каждой программе
//!
//! Владелец спросил ровно это: «не уверен, куда лучше встраивать — в тулкит? просто во все
//! текстовые элементы? как-то неправильно звучит». Звучит правильно, и вот почему.
//!
//! Полей ввода в системе уже трое (строка запуска, адрес файлового менеджера, его же
//! переименование), и у каждого одни и те же вопросы: где курсор, что выделено, что делает
//! `Shift+Home`, куда вставляется буфер. Три ответа на них разъедутся — не «может быть», а
//! наверняка: они уже разъезжались у damage, у попаданий клика и у палитры ([[void-ui]]).
//!
//! А вот **чего здесь нет — виджета**. `Edit` не рисует и не знает про экран: это состояние и
//! правила его изменения. Рисует [`super::Ui::edit_field`], и это разделение то же самое, что у
//! движения ([`super::anim::Motion`]): состояние принадлежит программе, виджету оно приезжает
//! готовым.
//!
//! ## Границы
//!
//! Одна СТРОКА, не абзац: перевода строки здесь нет и не будет — многострочная правка это
//! редактор (`ved`), а не поле ввода. Индексы БАЙТОВЫЕ и всегда на границе символа: строки у нас
//! UTF-8, и «курсор в символах» пришлось бы пересчитывать при каждом обращении к тексту.

use alloc::string::String;

/// Что случилось с правимой строкой от клавиши.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Клавиша не наша — пусть смотрит программа.
    None,
    /// Двинулся курсор или выделение: перерисовать, но текст тот же.
    Moved,
    /// Текст изменился.
    Edited,
    /// `Enter` — программа решает, что это значит.
    Done,
    /// `Escape` — то же самое, но наоборот.
    Cancel,
}

/// Правимая строка: текст, курсор и якорь выделения.
#[derive(Clone, Default)]
pub struct Edit {
    pub text: String,
    /// Курсор — байтовый индекс на границе символа.
    caret: usize,
    /// Якорь выделения. Равен курсору — выделения нет.
    anchor: usize,
}

impl Edit {
    /// Завести правку с текстом, курсором в конце и ВЫДЕЛЕННЫМ ВСЕМ.
    ///
    /// Выделено всё намеренно: правку открывают, чтобы заменить написанное (путь, имя файла), и
    /// первая же буква должна стирать старое. Кому нужно дописывать — жмёт `End` или щёлкает.
    pub fn all(text: String) -> Edit {
        let n = text.len();
        Edit { text, caret: n, anchor: 0 }
    }

    /// Правка с курсором в конце и без выделения.
    pub fn tail(text: String) -> Edit {
        let n = text.len();
        Edit { text, caret: n, anchor: n }
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Границы выделения `(от, до)`; `None` — не выделено ничего.
    pub fn selection(&self) -> Option<(usize, usize)> {
        (self.caret != self.anchor)
            .then(|| (self.caret.min(self.anchor), self.caret.max(self.anchor)))
    }

    /// Выделенный кусок (пусто — выделения нет). Он же уезжает в буфер обмена.
    pub fn selected(&self) -> &str {
        match self.selection() {
            Some((a, b)) => &self.text[a..b],
            None => "",
        }
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    /// Поставить курсор по БАЙТОВОМУ индексу (щелчок мышью), сняв выделение.
    pub fn put_caret(&mut self, at: usize) {
        let at = at.min(self.text.len());
        self.caret = at;
        self.anchor = at;
    }

    /// Убрать выделенное. `true` — что-то стёрли.
    fn drop_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else { return false };
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = a;
        true
    }

    pub fn insert(&mut self, c: char) {
        self.drop_selection();
        self.text.insert(self.caret, c);
        self.caret += c.len_utf8();
        self.anchor = self.caret;
    }

    /// Вставить кусок (буфер обмена). Переводы строк выкидываются: строка — одна.
    pub fn insert_str(&mut self, s: &str) {
        self.drop_selection();
        for c in s.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.caret, c);
            self.caret += c.len_utf8();
        }
        self.anchor = self.caret;
    }

    /// Индекс предыдущей/следующей границы символа.
    fn prev(&self, i: usize) -> usize {
        self.text[..i].chars().next_back().map_or(0, |c| i - c.len_utf8())
    }
    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    /// Клавиша. `mods` — биты [`super::super::win::modk`]; нам важны `SHIFT` и `CTRL`.
    ///
    /// Выделение живёт по одному правилу, и оно тут единственное: **`Shift` держит якорь, всё
    /// остальное его двигает за курсором**. Из него само собой следует и «стрелка снимает
    /// выделение», и «`Shift+Home` тянет до начала», и что после вставки выделения нет.
    pub fn key(&mut self, sym: u16, ch: u16, mods: u8) -> Hit {
        use void_user::win::{modk, sym as k};
        let shift = mods & modk::SHIFT != 0;
        let ctrl = mods & modk::CTRL != 0;
        // Аккорды с Ctrl программа разбирает сама (копирование, вставка): у правки на них своё
        // мнение только про `Ctrl+A`.
        if ctrl {
            let letter = match (sym, ch) {
                (c, _) if (0x41..=0x5a).contains(&c) => (c as u8) | 0x20,
                (c, _) if (0x61..=0x7a).contains(&c) => c as u8,
                (_, c) if (0x01..=0x1a).contains(&c) => (c as u8) + 0x60,
                _ => 0,
            };
            if letter == b'a' {
                self.select_all();
                return Hit::Moved;
            }
            return Hit::None;
        }
        let moved = |e: &mut Edit, to: usize| {
            e.caret = to;
            if !shift {
                e.anchor = to;
            }
            Hit::Moved
        };
        match sym {
            k::LEFT => {
                // Без Shift стрелка при выделении встаёт на его КРАЙ, а не сдвигается на знак:
                // так ведёт себя всё, к чему человек привык, и так выделение снимается там, где
                // он его видит.
                match (shift, self.selection()) {
                    (false, Some((a, _))) => moved(self, a),
                    _ => {
                        let to = self.prev(self.caret);
                        moved(self, to)
                    }
                }
            }
            k::RIGHT => match (shift, self.selection()) {
                (false, Some((_, b))) => moved(self, b),
                _ => {
                    let to = self.next(self.caret);
                    moved(self, to)
                }
            },
            k::HOME => moved(self, 0),
            k::END => {
                let n = self.text.len();
                moved(self, n)
            }
            k::BACKSPACE => {
                if !self.drop_selection() {
                    if self.caret == 0 {
                        return Hit::None;
                    }
                    let to = self.prev(self.caret);
                    self.text.replace_range(to..self.caret, "");
                    self.caret = to;
                    self.anchor = to;
                }
                Hit::Edited
            }
            k::DELETE => {
                if !self.drop_selection() {
                    if self.caret >= self.text.len() {
                        return Hit::None;
                    }
                    let to = self.next(self.caret);
                    self.text.replace_range(self.caret..to, "");
                }
                Hit::Edited
            }
            k::RETURN => Hit::Done,
            k::ESCAPE => Hit::Cancel,
            _ => match char::from_u32(ch as u32).filter(|c| !c.is_control()) {
                Some(c) => {
                    self.insert(c);
                    Hit::Edited
                }
                None => Hit::None,
            },
        }
    }
}
