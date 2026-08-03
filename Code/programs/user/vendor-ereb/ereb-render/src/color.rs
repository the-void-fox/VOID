//! Цвета пикселей и палитра: перевод [`ereb_core::Color`] в конкретный RGB.


use ereb_core::{Color, NamedColor};

/// Непрозрачный RGB-цвет пикселя.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }
}

/// Порядковый индекс именованного цвета в ANSI-палитре `0..=15`.
fn named_index(color: NamedColor) -> usize {
    use NamedColor::*;
    match color {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
    }
}

/// Цветовая схема: 16 ANSI-цветов плюс дефолтные fg/bg/курсор.
///
/// Полноценные темы из файлов — задача v1.0 (Этап 17); здесь только дефолт.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Цвет текста по умолчанию ([`Color::Default`] на переднем плане).
    pub foreground: Rgb,
    /// Цвет фона по умолчанию ([`Color::Default`] на фоне).
    pub background: Rgb,
    /// Цвет курсора.
    pub cursor: Rgb,
    /// 16 базовых ANSI-цветов (индексы `0..=15`).
    pub ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        // Классическая VGA-подобная палитра на тёмном фоне.
        let ansi = [
            Rgb::new(0, 0, 0),       // 0  black
            Rgb::new(170, 0, 0),     // 1  red
            Rgb::new(0, 170, 0),     // 2  green
            Rgb::new(170, 85, 0),    // 3  yellow
            Rgb::new(0, 0, 170),     // 4  blue
            Rgb::new(170, 0, 170),   // 5  magenta
            Rgb::new(0, 170, 170),   // 6  cyan
            Rgb::new(170, 170, 170), // 7  white
            Rgb::new(85, 85, 85),    // 8  bright black
            Rgb::new(255, 85, 85),   // 9  bright red
            Rgb::new(85, 255, 85),   // 10 bright green
            Rgb::new(255, 255, 85),  // 11 bright yellow
            Rgb::new(85, 85, 255),   // 12 bright blue
            Rgb::new(255, 85, 255),  // 13 bright magenta
            Rgb::new(85, 255, 255),  // 14 bright cyan
            Rgb::new(255, 255, 255), // 15 bright white
        ];
        Palette {
            foreground: Rgb::new(204, 204, 204),
            background: Rgb::new(0, 0, 0),
            cursor: Rgb::new(204, 204, 204),
            ansi,
        }
    }
}

impl Palette {
    /// Разрешает [`Color`] в конкретный RGB. [`Color::Default`] → `default`
    /// (передаётся вызывающим: дефолтный fg или bg в зависимости от роли).
    pub fn resolve(&self, color: Color, default: Rgb) -> Rgb {
        match color {
            Color::Default => default,
            Color::Named(n) => self.ansi[named_index(n)],
            Color::Indexed(i) => self.indexed(i),
            Color::Rgb(r, g, b) => Rgb::new(r, g, b),
        }
    }

    /// Цвет из 256-цветной палитры xterm.
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                // 6×6×6 цветовой куб.
                let i = index - 16;
                let r = i / 36;
                let g = (i % 36) / 6;
                let b = i % 6;
                let comp = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                Rgb::new(comp(r), comp(g), comp(b))
            }
            232..=255 => {
                // 24 градации серого.
                let v = 8 + (index - 232) * 10;
                Rgb::new(v, v, v)
            }
        }
    }
}
