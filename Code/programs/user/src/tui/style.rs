//! Цвет и начертание ячейки — и то, как они становятся байтами SGR.
//!
//! Палитра здесь НЕ цветовая схема системы ([[void-ui]] и `ui::Theme` — про пиксели), а шестнадцать
//! цветов терминала плюс «как у терминала по умолчанию». Так сделано потому, что цвета ячеек
//! выбирает не программа, а тот, кто настроил свой терминал: сказать «фон 0x1e1e1e» в гриде
//! нельзя — можно сказать «обратный» или «восьмой из палитры».

use alloc::string::String;
use core::fmt::Write;

/// Цвет ячейки.
///
/// `Default` — не «чёрный» и не «белый», а «не трогай»: терминал подставит своё, и текст
/// останется читаемым на любой теме. Это умолчание всех стилей не случайно: программа, которая
/// красит всё подряд, на чужой теме выглядит сломанной.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Default,
    /// Шестнадцать цветов терминала: 0..=7 — обычные, 8..=15 — яркие.
    Ansi(u8),
    /// Палитра xterm на 256 цветов (`38;5;N`). Понимает её и наш грид, и любой чужой терминал.
    Idx(u8),
}

impl Color {
    pub const BLACK: Color = Color::Ansi(0);
    pub const RED: Color = Color::Ansi(1);
    pub const GREEN: Color = Color::Ansi(2);
    pub const YELLOW: Color = Color::Ansi(3);
    pub const BLUE: Color = Color::Ansi(4);
    pub const MAGENTA: Color = Color::Ansi(5);
    pub const CYAN: Color = Color::Ansi(6);
    pub const WHITE: Color = Color::Ansi(7);
    /// Серый — им подписывают второстепенное (как `th.muted` в оконном тулките).
    pub const MUTED: Color = Color::Ansi(8);
}

/// Начертание: биты, а не отдельные поля, — стиль копируется в каждую ячейку экрана, и восемь
/// лишних байт на ячейку при 80×25 это шестнадцать килобайт на пустом месте.
pub const BOLD: u8 = 1 << 0;
pub const DIM: u8 = 1 << 1;
pub const ITALIC: u8 = 1 << 2;
pub const UNDERLINE: u8 = 1 << 3;
/// Обратный — им выделяют строку списка и курсор. На любой теме он читается, потому что берёт
/// цвета самого терминала, а не наши.
pub const REVERSE: u8 = 1 << 4;

/// Стиль ячейки: цвет пера, цвет фона, начертание.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Style {
    /// Стиль по умолчанию — «как у терминала».
    pub const PLAIN: Style = Style { fg: Color::Default, bg: Color::Default, flags: 0 };

    pub const fn fg(c: Color) -> Style {
        Style { fg: c, bg: Color::Default, flags: 0 }
    }

    pub const fn bg(c: Color) -> Style {
        Style { fg: Color::Default, bg: c, flags: 0 }
    }

    pub const fn with(mut self, flags: u8) -> Style {
        self.flags |= flags;
        self
    }

    pub const fn on(mut self, c: Color) -> Style {
        self.bg = c;
        self
    }

    /// Приглушённый — второстепенные подписи.
    pub const MUTED: Style = Style::fg(Color::MUTED);
    /// Выделенное — строка под курсором в списке, активная вкладка.
    pub const SELECTED: Style = Style::PLAIN.with(REVERSE);
}

/// Дописать в `out` переход к стилю `s`.
///
/// Переход записывается ЦЕЛИКОМ, со сброса (`\x1b[0m`), а не разницей от предыдущего. Разница
/// экономила бы байты и стоила бы правды: чтобы её посчитать, надо знать состояние пера
/// терминала, а его меняет кто угодно — чужая программа в том же гриде, прерванная отрисовка,
/// изменение размера. Один сброс на смену стиля — несколько байт; расхождение с настоящим
/// состоянием — цветной мусор на экране, который ничем не лечится.
pub fn sgr(out: &mut String, s: Style) {
    out.push_str("\x1b[0");
    if s.flags & BOLD != 0 {
        out.push_str(";1");
    }
    if s.flags & DIM != 0 {
        out.push_str(";2");
    }
    if s.flags & ITALIC != 0 {
        out.push_str(";3");
    }
    if s.flags & UNDERLINE != 0 {
        out.push_str(";4");
    }
    if s.flags & REVERSE != 0 {
        out.push_str(";7");
    }
    colour(out, s.fg, false);
    colour(out, s.bg, true);
    out.push('m');
}

fn colour(out: &mut String, c: Color, back: bool) {
    let base = if back { 40 } else { 30 };
    match c {
        Color::Default => {}
        // Яркие восемь — отдельный диапазон (90/100), а не «жирный плюс цвет»: жирность это
        // начертание, и подменять ею цвет значило бы, что подпись серым нельзя сделать жирной.
        Color::Ansi(n) if n < 8 => {
            let _ = write!(out, ";{}", base + n as u16);
        }
        Color::Ansi(n) => {
            let _ = write!(out, ";{}", base + 60 + (n & 7) as u16);
        }
        Color::Idx(n) => {
            let _ = write!(out, ";{};5;{}", base + 8, n);
        }
    }
}
