//! Реализация [`vte::Perform`] для [`Grid`].
//!
//! `vte::Parser` разбирает поток байт из master-PTY и для каждой осмысленной
//! последовательности вызывает методы этого trait'а; здесь они переводятся в
//! мутации грида.

use alloc::string::String;

use vte::{Params, Perform};

use crate::cell::{CellFlags, Color, NamedColor};
use crate::grid::{EraseMode, Grid};

/// Первый параметр CSI, трактуя отсутствие и `0` как `default`.
///
/// Для движения/позиционирования курсора `0` эквивалентен `1`.
fn first_param(params: &Params, default: u16) -> u16 {
    match params.iter().next().and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

/// Разбирает расширенный цвет SGR (`38`/`48`) в обеих формах разделителей.
///
/// `head` — под-параметры самого `38`/`48`. В двоеточной форме
/// (`38:2:R:G:B`) всё лежит здесь как subparams. В точка-с-запятой форме
/// (`38;2;R;G;B`) `head` содержит только `[38]`, а компоненты дочитываются
/// из последующих параметров `rest`.
fn parse_extended_color<'a, I>(head: &[u16], rest: &mut I) -> Option<Color>
where
    I: Iterator<Item = &'a [u16]>,
{
    // Двоеточная форма складывает компоненты в subparams `head` (после кода
    // `38`/`48`); точка-с-запятой форма — в отдельные параметры `rest`.
    let mut components = head[1..].iter().copied().chain(rest.flatten().copied());
    match components.next()? {
        2 => {
            let r = components.next()?;
            let g = components.next()?;
            let b = components.next()?;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        5 => components.next().map(|n| Color::Indexed(n as u8)),
        _ => None,
    }
}

impl Grid {
    /// CSI `m` — Select Graphic Rendition: цвета и атрибуты текста.
    fn handle_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.reset_pen();
            return;
        }
        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            match param[0] {
                0 => self.reset_pen(),
                1 => self.add_flags(CellFlags::BOLD),
                2 => self.add_flags(CellFlags::DIM),
                3 => self.add_flags(CellFlags::ITALIC),
                4 => self.add_flags(CellFlags::UNDERLINE),
                5 => self.add_flags(CellFlags::BLINK),
                7 => self.add_flags(CellFlags::REVERSE),
                8 => self.add_flags(CellFlags::HIDDEN),
                9 => self.add_flags(CellFlags::STRIKETHROUGH),
                22 => self.remove_flags(CellFlags::BOLD | CellFlags::DIM),
                23 => self.remove_flags(CellFlags::ITALIC),
                24 => self.remove_flags(CellFlags::UNDERLINE),
                25 => self.remove_flags(CellFlags::BLINK),
                27 => self.remove_flags(CellFlags::REVERSE),
                28 => self.remove_flags(CellFlags::HIDDEN),
                29 => self.remove_flags(CellFlags::STRIKETHROUGH),
                c @ 30..=37 => self.set_fg(Color::Named(NamedColor::from_ansi((c - 30) as u8))),
                38 => {
                    if let Some(color) = parse_extended_color(param, &mut iter) {
                        self.set_fg(color);
                    }
                }
                39 => self.set_fg(Color::Default),
                c @ 40..=47 => self.set_bg(Color::Named(NamedColor::from_ansi((c - 40) as u8))),
                48 => {
                    if let Some(color) = parse_extended_color(param, &mut iter) {
                        self.set_bg(color);
                    }
                }
                49 => self.set_bg(Color::Default),
                c @ 90..=97 => self.set_fg(Color::Named(NamedColor::from_ansi((c - 90 + 8) as u8))),
                c @ 100..=107 => {
                    self.set_bg(Color::Named(NamedColor::from_ansi((c - 100 + 8) as u8)))
                }
                _ => {}
            }
        }
    }

    /// CSI `H`/`f` — абсолютное позиционирование курсора (1-based `row;col`).
    fn handle_cup(&mut self, params: &Params) {
        let mut iter = params.iter();
        let row = iter
            .next()
            .and_then(|p| p.first().copied())
            .filter(|&v| v != 0)
            .unwrap_or(1);
        let col = iter
            .next()
            .and_then(|p| p.first().copied())
            .filter(|&v| v != 0)
            .unwrap_or(1);
        self.move_cursor((col - 1) as usize, (row - 1) as usize);
    }
}

impl Perform for Grid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.line_feed(), // LF, VT, FF
            b'\r' => self.carriage_return(),
            b'\t' => self.tab(),
            0x08 => self.backspace(),
            _ => {} // BEL (0x07) и прочее игнорируем на MVP
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Приватные режимы DECSET/DECRST: `CSI ? Pm h/l`.
        let private = intermediates.first() == Some(&b'?');
        match action {
            'm' => self.handle_sgr(params),
            'H' | 'f' => self.handle_cup(params),
            'A' => self.move_up(first_param(params, 1) as usize),
            'B' => self.move_down(first_param(params, 1) as usize),
            'C' => self.move_right(first_param(params, 1) as usize),
            'D' => self.move_left(first_param(params, 1) as usize),
            'G' | '`' => self.move_cursor_col(first_param(params, 1)), // CHA / HPA
            'd' => self.move_cursor_row(first_param(params, 1)),       // VPA
            'J' => self.erase_in_display(EraseMode::from_param(first_param(params, 0))),
            'K' => self.erase_in_line(EraseMode::from_param(first_param(params, 0))),
            '@' => self.insert_chars(first_param(params, 1) as usize), // ICH
            'P' => self.delete_chars(first_param(params, 1) as usize), // DCH
            'X' => self.erase_chars(first_param(params, 1) as usize),  // ECH
            'L' => self.insert_lines(first_param(params, 1) as usize), // IL
            'M' => self.delete_lines(first_param(params, 1) as usize), // DL
            'S' => self.scroll_up(first_param(params, 1) as usize),    // SU
            'T' => self.scroll_down(first_param(params, 1) as usize),  // SD
            'r' if !private => {
                // DECSTBM: top;bottom (0 = по умолчанию).
                let mut iter = params.iter();
                let top = iter.next().and_then(|p| p.first().copied()).unwrap_or(0);
                let bottom = iter.next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.set_scroll_region(top, bottom);
            }
            'h' | 'l' if private => {
                let enabled = action == 'h';
                for param in params.iter() {
                    self.set_mode(param[0], enabled);
                }
            }
            's' if !private => self.save_cursor(),    // SCOSC
            'u' if !private => self.restore_cursor(), // SCORC
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.save_cursor(),    // DECSC
            b'8' => self.restore_cursor(), // DECRC
            b'D' => self.line_feed(),      // IND
            b'M' => self.reverse_index(),  // RI
            b'E' => self.next_line(),      // NEL
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 (icon + title) и OSC 2 (title): `OSC Ps ; текст ST`.
        if let Some(kind) = params.first()
            && (*kind == b"0" || *kind == b"2")
            && let Some(title) = params.get(1)
        {
            self.set_title(String::from_utf8_lossy(title));
        }
    }
}
