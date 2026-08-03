//! Каноничное событие клавиатуры [`KeyEvent`] и модификаторы [`ModMask`].

use alloc::string::String;

use bitflags::bitflags;

use crate::keysym::Keysym;

bitflags! {
    /// Активные модификаторы.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct ModMask: u8 {
        const CTRL  = 1 << 0;
        const ALT   = 1 << 1;
        const SHIFT = 1 << 2;
        /// Клавиша Super/Win/Logo.
        const LOGO  = 1 << 3;
    }
}

/// Направление события клавиши.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyDirection {
    Down,
    Up,
}

/// Нормализованное событие нажатия клавиши.
///
/// Источник (Wayland/DRM/тест) заполняет его через декодер; дальше работает уже
/// бэкенд-независимая логика биндингов и кодирования в PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// Символический код клавиши.
    pub keysym: Keysym,
    /// UTF-8 текст, который порождает клавиша (может быть пустым).
    pub text: String,
    /// Активные модификаторы.
    pub mods: ModMask,
    /// Сработала ли автоповтором.
    pub repeat: bool,
}

impl KeyEvent {
    /// Событие без текста (спец-клавиша) с заданными keysym и модификаторами.
    pub fn new(keysym: Keysym, mods: ModMask) -> Self {
        KeyEvent {
            keysym,
            text: String::new(),
            mods,
            repeat: false,
        }
    }

    /// Событие с текстом (печатный символ).
    pub fn with_text(keysym: Keysym, text: impl Into<String>, mods: ModMask) -> Self {
        KeyEvent {
            keysym,
            text: text.into(),
            mods,
            repeat: false,
        }
    }
}
