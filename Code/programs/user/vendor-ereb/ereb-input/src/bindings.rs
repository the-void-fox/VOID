//! [`Bindings`] — таблица соответствий «режим + модификаторы + клавиша → действие».
//!
//! Структура, не логика переходов: префиксные режимы крутит `ereb-mux`
//! (Этап 13). Здесь — хранение и поиск, плюс дефолтная схема. Парсинг из KDL —
//! Этап 14.

use alloc::collections::BTreeMap;

use crate::action::{Action, Mode};
use crate::event::ModMask;
use crate::keysym::Keysym;

/// Ключ биндинга: keysym нормализован (Shift-регистр букв убран).
type BindKey = (Mode, ModMask, u32);

/// Таблица биндингов.
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    map: BTreeMap<BindKey, Action>,
}

impl Bindings {
    /// Пустая таблица.
    pub fn new() -> Self {
        Bindings::default()
    }

    /// Дефолтная схема: терминал-копипаст в духе Wayland-эмуляторов.
    pub fn with_defaults() -> Self {
        let mut bindings = Bindings::new();
        let cs = ModMask::CTRL | ModMask::SHIFT;
        bindings.bind(Mode::Normal, cs, Keysym::from_char('c'), Action::Copy);
        bindings.bind(Mode::Normal, cs, Keysym::from_char('v'), Action::Paste);
        bindings
    }

    /// Привязывает действие к комбинации.
    pub fn bind(&mut self, mode: Mode, mods: ModMask, keysym: Keysym, action: Action) {
        self.map.insert((mode, mods, keysym.normalized().0), action);
    }

    /// Ищет действие для комбинации в заданном режиме.
    pub fn get(&self, mode: Mode, mods: ModMask, keysym: Keysym) -> Option<&Action> {
        self.map.get(&(mode, mods, keysym.normalized().0))
    }

    /// Число биндингов.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Пуста ли таблица.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
