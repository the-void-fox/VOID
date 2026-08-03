//! Базовые типы ячейки грида: [`Cell`], [`Color`], [`NamedColor`], [`CellFlags`].
//!
//! См. `obsidian/02-architecture/data-model.md` за обоснованием размеров.

use bitflags::bitflags;

bitflags! {
    /// Атрибуты рендеринга одной ячейки.
    ///
    /// Хранится упакованно в `u16`; устанавливается SGR-последовательностями
    /// (`ESC [ ... m`) и читается рендером.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct CellFlags: u16 {
        const BOLD          = 1 << 0;
        const ITALIC        = 1 << 1;
        const UNDERLINE     = 1 << 2;
        const STRIKETHROUGH = 1 << 3;
        const REVERSE       = 1 << 4;
        const HIDDEN        = 1 << 5;
        const BLINK         = 1 << 6;
        const DIM           = 1 << 7;
        /// Символ занимает две ячейки (CJK, emoji).
        const WIDE          = 1 << 8;
        /// Вторая (пустая) ячейка [`WIDE`](Self::WIDE)-символа.
        const WIDE_SPACER   = 1 << 9;
    }
}

/// 16 стандартных ANSI-цветов (8 обычных + 8 ярких).
///
/// Соответствуют SGR-кодам `30..37`/`90..97` (передний план) и
/// `40..47`/`100..107` (фон), а также индексам `0..15` 256-цветной палитры.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl NamedColor {
    /// Превращает индекс `0..=15` ANSI-палитры в именованный цвет.
    ///
    /// Индексы вне диапазона насыщаются до [`BrightWhite`](Self::BrightWhite).
    pub fn from_ansi(index: u8) -> Self {
        use NamedColor::*;
        match index {
            0 => Black,
            1 => Red,
            2 => Green,
            3 => Yellow,
            4 => Blue,
            5 => Magenta,
            6 => Cyan,
            7 => White,
            8 => BrightBlack,
            9 => BrightRed,
            10 => BrightGreen,
            11 => BrightYellow,
            12 => BrightBlue,
            13 => BrightMagenta,
            14 => BrightCyan,
            _ => BrightWhite,
        }
    }
}

/// Цвет переднего плана или фона ячейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    /// Цвет по умолчанию (определяется темой/рендером).
    #[default]
    Default,
    /// Один из 16 именованных ANSI-цветов.
    Named(NamedColor),
    /// Индекс `0..=255` 256-цветной палитры.
    Indexed(u8),
    /// Truecolor — произвольный RGB.
    Rgb(u8, u8, u8),
}

/// Одна ячейка грида: символ плюс его атрибуты рендеринга.
///
/// `Copy`, чтобы грид мог хранить плотный `Vec<Cell>` и дёшево его очищать.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Видимый символ. Пустая ячейка — пробел `' '`.
    pub ch: char,
    /// Цвет переднего плана (текста).
    pub fg: Color,
    /// Цвет фона.
    pub bg: Color,
    /// Атрибуты: bold, underline, reverse и т.д.
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: CellFlags::empty(),
        }
    }
}

impl Cell {
    /// Сбрасывает атрибуты ячейки к значениям по умолчанию, оставляя `ch`.
    ///
    /// Используется «пером» (pen) при обработке `SGR 0` (reset).
    pub fn reset_attrs(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.flags = CellFlags::empty();
    }
}
