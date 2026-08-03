//! [`Keysym`] — символический код клавиши (значения X11 keysym, как у xkbcommon).
//!
//! Держим собственный newtype, чтобы чистая логика (биндинги, кодирование в PTY)
//! не зависела от C-библиотеки xkbcommon. Декодер за фичей `xkbcommon`
//! ([`crate::xkb`]) просто прокидывает сюда сырой `u32`.

/// Символический код клавиши (X11 keysym).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Keysym(pub u32);

#[allow(missing_docs)]
impl Keysym {
    // Управляющие/редактирование.
    pub const BACKSPACE: Keysym = Keysym(0xff08);
    pub const TAB: Keysym = Keysym(0xff09);
    pub const RETURN: Keysym = Keysym(0xff0d);
    pub const ESCAPE: Keysym = Keysym(0xff1b);
    pub const DELETE: Keysym = Keysym(0xffff);
    pub const ISO_LEFT_TAB: Keysym = Keysym(0xfe20);
    pub const KP_ENTER: Keysym = Keysym(0xff8d);
    pub const SPACE: Keysym = Keysym(0x0020);

    // Навигация.
    pub const HOME: Keysym = Keysym(0xff50);
    pub const LEFT: Keysym = Keysym(0xff51);
    pub const UP: Keysym = Keysym(0xff52);
    pub const RIGHT: Keysym = Keysym(0xff53);
    pub const DOWN: Keysym = Keysym(0xff54);
    pub const PAGE_UP: Keysym = Keysym(0xff55);
    pub const PAGE_DOWN: Keysym = Keysym(0xff56);
    pub const END: Keysym = Keysym(0xff57);
    pub const INSERT: Keysym = Keysym(0xff63);

    // Функциональные.
    pub const F1: Keysym = Keysym(0xffbe);
    pub const F2: Keysym = Keysym(0xffbf);
    pub const F3: Keysym = Keysym(0xffc0);
    pub const F4: Keysym = Keysym(0xffc1);
    pub const F5: Keysym = Keysym(0xffc2);
    pub const F6: Keysym = Keysym(0xffc3);
    pub const F7: Keysym = Keysym(0xffc4);
    pub const F8: Keysym = Keysym(0xffc5);
    pub const F9: Keysym = Keysym(0xffc6);
    pub const F10: Keysym = Keysym(0xffc7);
    pub const F11: Keysym = Keysym(0xffc8);
    pub const F12: Keysym = Keysym(0xffc9);
}

impl Keysym {
    /// Создаёт keysym для ASCII-буквы (для биндингов в коде/тестах).
    pub const fn from_char(c: char) -> Keysym {
        Keysym(c as u32)
    }

    /// Сырое значение.
    pub fn raw(self) -> u32 {
        self.0
    }

    /// Если keysym — латинская буква `A..Z`, возвращает её в нижнем регистре.
    /// Используется для нормализации биндингов (Shift не меняет «клавишу»).
    pub fn normalized(self) -> Keysym {
        match self.0 {
            0x41..=0x5a => Keysym(self.0 + 0x20),
            _ => self,
        }
    }

    /// Управляющий байт для `Ctrl+<буква>` (`Ctrl+A` → `0x01`), если применимо.
    pub fn control_byte(self) -> Option<u8> {
        match self.normalized().0 {
            v @ 0x61..=0x7a => Some((v - 0x60) as u8), // a..z → 1..26
            _ => None,
        }
    }
}
