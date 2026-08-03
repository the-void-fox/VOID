//! # ereb-core
//!
//! Сердце терминала: разбор escape-последовательностей и поддержка [`Grid`]
//! ячеек. Графики, PTY и окон здесь нет — только модель данных и её мутации.
//!
//! ## Поток данных
//!
//! 1. Байты из master-PTY скармливаются в [`vte::Parser`].
//! 2. Парсер вызывает методы [`vte::Perform`], реализованные для [`Grid`].
//! 3. [`Grid`] обновляет ячейки, курсор и текущее «перо» (атрибуты SGR).
//!
//! ```
//! use ereb_core::{Grid, Color, NamedColor};
//!
//! let mut grid = Grid::new(10, 5);
//! let mut parser = vte::Parser::new();
//! parser.advance(&mut grid, b"\x1b[31mhello\x1b[0m");
//!
//! assert_eq!(grid.cell_at(0, 0).ch, 'h');
//! assert_eq!(grid.cell_at(0, 0).fg, Color::Named(NamedColor::Red));
//! ```
//!
//! См. `obsidian/03-subsystems/terminal-core.md` за объёмом поддержки и
//! `obsidian/02-architecture/data-model.md` за моделью данных.

// Крейт не трогает ОС: ему нужна только куча. Поэтому `no_std` — и он собирается для VOID,
// где нет ни libc, ни std ([ADR 0005]). На Linux ничего не меняется: `alloc` там тот же.
//
// [ADR 0005]: ../../../obsidian/02-architecture/adr/0005-void-target.md
#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod cell;
mod grid;
mod perform;

pub use cell::{Cell, CellFlags, Color, NamedColor};
pub use grid::{Cursor, CursorStyle, EraseMode, Grid, Modes, MouseEncoding, MouseProtocol};

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};

    use super::*;

    /// Прогоняет байты через свежий грид заданного размера.
    fn run(cols: usize, rows: usize, bytes: &[u8]) -> Grid {
        let mut grid = Grid::new(cols, rows);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, bytes);
        grid
    }

    /// Собирает текст строки `row` без хвостовых пробелов.
    fn line(grid: &Grid, row: usize) -> String {
        (0..grid.cols())
            .map(|col| grid.cell_at(col, row).ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    // --- Cell / Color --------------------------------------------------------

    #[test]
    fn cell_defaults_to_blank() {
        let cell = Cell::default();
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.fg, Color::Default);
        assert_eq!(cell.bg, Color::Default);
        assert!(cell.flags.is_empty());
    }

    #[test]
    fn named_color_from_ansi_covers_16() {
        assert_eq!(NamedColor::from_ansi(0), NamedColor::Black);
        assert_eq!(NamedColor::from_ansi(7), NamedColor::White);
        assert_eq!(NamedColor::from_ansi(8), NamedColor::BrightBlack);
        assert_eq!(NamedColor::from_ansi(15), NamedColor::BrightWhite);
        // вне диапазона насыщается
        assert_eq!(NamedColor::from_ansi(200), NamedColor::BrightWhite);
    }

    // --- Grid: базовые методы ------------------------------------------------

    #[test]
    fn new_grid_is_blank() {
        let grid = Grid::new(4, 3);
        assert_eq!(grid.cols(), 4);
        assert_eq!(grid.rows(), 3);
        assert_eq!(grid.cursor().row, 0);
        assert_eq!(grid.cursor().col, 0);
        for row in 0..3 {
            for col in 0..4 {
                assert_eq!(grid.cell_at(col, row).ch, ' ');
            }
        }
    }

    #[test]
    fn view_cell_shows_scrollback() {
        // 2×2; печатаем 3 строки → верхняя уходит в scrollback.
        let grid = run(2, 2, b"a\r\nb\r\nc");
        assert_eq!(grid.scrollback_len(), 1);
        // scroll=0 — живой экран: row0="b", row1="c".
        assert_eq!(grid.view_cell(0, 0, 0).ch, 'b');
        assert_eq!(grid.view_cell(0, 1, 0).ch, 'c');
        // scroll=1 — сдвиг на строку: row0="a" (из истории), row1="b".
        assert_eq!(grid.view_cell(0, 0, 1).ch, 'a');
        assert_eq!(grid.view_cell(0, 1, 1).ch, 'b');
        // Перебор scroll насыщается к максимуму.
        assert_eq!(grid.view_cell(0, 0, 99).ch, 'a');
    }

    #[test]
    fn move_cursor_clamps_to_bounds() {
        let mut grid = Grid::new(4, 3);
        grid.move_cursor(99, 99);
        assert_eq!(grid.cursor().col, 3);
        assert_eq!(grid.cursor().row, 2);
    }

    #[test]
    fn clear_blanks_everything() {
        let mut grid = run(4, 2, b"ab");
        grid.clear();
        assert_eq!(line(&grid, 0), "");
    }

    // --- print ---------------------------------------------------------------

    #[test]
    fn print_writes_and_advances() {
        let grid = run(10, 2, b"hi");
        assert_eq!(grid.cell_at(0, 0).ch, 'h');
        assert_eq!(grid.cell_at(1, 0).ch, 'i');
        assert_eq!(grid.cursor().col, 2);
    }

    #[test]
    fn print_wraps_at_right_edge() {
        let grid = run(3, 2, b"abcd");
        assert_eq!(line(&grid, 0), "abc");
        assert_eq!(grid.cell_at(0, 1).ch, 'd');
        assert_eq!(grid.cursor().row, 1);
        assert_eq!(grid.cursor().col, 1);
    }

    #[test]
    fn print_handles_utf8() {
        let grid = run(10, 1, "héllo→".as_bytes());
        assert_eq!(grid.cell_at(1, 0).ch, 'é');
        assert_eq!(grid.cell_at(5, 0).ch, '→');
    }

    // --- execute (\n \r \b \t) ----------------------------------------------

    #[test]
    fn carriage_return_and_line_feed() {
        let grid = run(10, 3, b"abc\r\ndef");
        assert_eq!(line(&grid, 0), "abc");
        assert_eq!(line(&grid, 1), "def");
    }

    #[test]
    fn backspace_moves_left() {
        let mut grid = run(10, 1, b"abc");
        assert_eq!(grid.cursor().col, 3);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"\x08");
        assert_eq!(grid.cursor().col, 2);
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let grid = run(20, 1, b"a\tb");
        assert_eq!(grid.cell_at(0, 0).ch, 'a');
        assert_eq!(grid.cell_at(8, 0).ch, 'b');
    }

    #[test]
    fn line_feed_scrolls_at_bottom() {
        // 2 строки: "a", "b", затем \n со скроллом, печать "c".
        let grid = run(10, 2, b"a\r\nb\r\nc");
        assert_eq!(line(&grid, 0), "b");
        assert_eq!(line(&grid, 1), "c");
    }

    // --- CSI: позиционирование и движение курсора ----------------------------

    #[test]
    fn cup_positions_cursor_1based() {
        // CSI 2;3 H → row 1, col 2 (0-based).
        let grid = run(10, 5, b"\x1b[2;3HX");
        assert_eq!(grid.cell_at(2, 1).ch, 'X');
    }

    #[test]
    fn cup_without_params_homes_cursor() {
        let mut grid = run(10, 5, b"\x1b[3;3Hfoo");
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"\x1b[H");
        assert_eq!(grid.cursor().row, 0);
        assert_eq!(grid.cursor().col, 0);
    }

    #[test]
    fn cursor_directional_moves() {
        let mut grid = Grid::new(10, 10);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"\x1b[5;5H"); // (col 4, row 4)
        parser.advance(&mut grid, b"\x1b[2A"); // up 2  -> row 2
        parser.advance(&mut grid, b"\x1b[3C"); // right 3 -> col 7
        parser.advance(&mut grid, b"\x1b[1B"); // down 1 -> row 3
        parser.advance(&mut grid, b"\x1b[4D"); // left 4 -> col 3
        assert_eq!(grid.cursor().row, 3);
        assert_eq!(grid.cursor().col, 3);
    }

    #[test]
    fn cursor_move_defaults_to_one() {
        let mut grid = Grid::new(10, 10);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"\x1b[5;5H\x1b[A"); // up без параметра -> 1
        assert_eq!(grid.cursor().row, 3);
    }

    // --- CSI: очистка (J / K) ------------------------------------------------

    #[test]
    fn erase_in_line_to_end() {
        // напечатать "abcde", вернуть курсор на col 2, стереть до конца.
        let grid = run(10, 1, b"abcde\x1b[1;3H\x1b[K");
        assert_eq!(line(&grid, 0), "ab");
    }

    #[test]
    fn erase_in_line_to_start() {
        let grid = run(10, 1, b"abcde\x1b[1;3H\x1b[1K");
        // ячейки 0..=2 стёрты, 'd','e' остаются на 3,4
        assert_eq!(grid.cell_at(0, 0).ch, ' ');
        assert_eq!(grid.cell_at(2, 0).ch, ' ');
        assert_eq!(grid.cell_at(3, 0).ch, 'd');
    }

    #[test]
    fn erase_in_display_all() {
        let grid = run(10, 3, b"line0\r\nline1\x1b[2J");
        assert_eq!(line(&grid, 0), "");
        assert_eq!(line(&grid, 1), "");
    }

    #[test]
    fn erase_in_display_to_end() {
        let grid = run(10, 3, b"AAAA\r\nBBBB\r\nCCCC\x1b[2;3H\x1b[J");
        assert_eq!(line(&grid, 0), "AAAA");
        assert_eq!(line(&grid, 1), "BB"); // от курсора (col2) до конца стёрто
        assert_eq!(line(&grid, 2), "");
    }

    // --- SGR: атрибуты и базовые цвета ---------------------------------------

    #[test]
    fn sgr_named_colors() {
        let grid = run(10, 1, b"\x1b[31;42mX");
        let cell = grid.cell_at(0, 0);
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
        assert_eq!(cell.bg, Color::Named(NamedColor::Green));
    }

    #[test]
    fn sgr_bright_colors() {
        let grid = run(10, 1, b"\x1b[91;102mX");
        let cell = grid.cell_at(0, 0);
        assert_eq!(cell.fg, Color::Named(NamedColor::BrightRed));
        assert_eq!(cell.bg, Color::Named(NamedColor::BrightGreen));
    }

    #[test]
    fn sgr_bold_underline_reverse() {
        let grid = run(10, 1, b"\x1b[1;4;7mX");
        let flags = grid.cell_at(0, 0).flags;
        assert!(flags.contains(CellFlags::BOLD));
        assert!(flags.contains(CellFlags::UNDERLINE));
        assert!(flags.contains(CellFlags::REVERSE));
    }

    #[test]
    fn sgr_reset_clears_attrs() {
        let grid = run(10, 1, b"\x1b[1;31mA\x1b[0mB");
        let a = grid.cell_at(0, 0);
        let b = grid.cell_at(1, 0);
        assert!(a.flags.contains(CellFlags::BOLD));
        assert_eq!(a.fg, Color::Named(NamedColor::Red));
        assert!(b.flags.is_empty());
        assert_eq!(b.fg, Color::Default);
    }

    #[test]
    fn sgr_attr_off_codes() {
        let grid = run(10, 1, b"\x1b[1;4mA\x1b[22;24mB");
        assert!(grid.cell_at(0, 0).flags.contains(CellFlags::BOLD));
        assert!(grid.cell_at(1, 0).flags.is_empty());
    }

    #[test]
    fn sgr_default_fg_bg() {
        let grid = run(10, 1, b"\x1b[31;42mA\x1b[39;49mB");
        assert_eq!(grid.cell_at(1, 0).fg, Color::Default);
        assert_eq!(grid.cell_at(1, 0).bg, Color::Default);
    }

    // --- SGR: 256-color и truecolor ------------------------------------------

    #[test]
    fn sgr_indexed_256() {
        let grid = run(10, 1, b"\x1b[38;5;202;48;5;17mX");
        let cell = grid.cell_at(0, 0);
        assert_eq!(cell.fg, Color::Indexed(202));
        assert_eq!(cell.bg, Color::Indexed(17));
    }

    #[test]
    fn sgr_truecolor_semicolon() {
        let grid = run(10, 1, b"\x1b[38;2;10;20;30;48;2;200;100;50mX");
        let cell = grid.cell_at(0, 0);
        assert_eq!(cell.fg, Color::Rgb(10, 20, 30));
        assert_eq!(cell.bg, Color::Rgb(200, 100, 50));
    }

    #[test]
    fn sgr_truecolor_colon_subparams() {
        // двоеточная форма 38:2:R:G:B
        let grid = run(10, 1, b"\x1b[38:2:10:20:30mX");
        assert_eq!(grid.cell_at(0, 0).fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn red_hello() {
        // Эталон из terminal-core.md.
        let grid = run(10, 5, b"\x1b[31mhello\x1b[0m");
        assert_eq!(grid.cell_at(0, 0).ch, 'h');
        assert_eq!(grid.cell_at(0, 0).fg, Color::Named(NamedColor::Red));
    }

    // --- v0.1: видимость курсора ---------------------------------------------

    #[test]
    fn hide_and_show_cursor() {
        let grid = run(10, 2, b"\x1b[?25l");
        assert!(!grid.cursor().visible);
        let grid = run(10, 2, b"\x1b[?25l\x1b[?25h");
        assert!(grid.cursor().visible);
    }

    // --- v0.1: save/restore курсора ------------------------------------------

    #[test]
    fn csi_save_restore_cursor() {
        // Курсор в (col2,row1), сохранить, уехать, восстановить.
        let grid = run(10, 5, b"\x1b[2;3H\x1b[s\x1b[5;5H\x1b[u");
        assert_eq!(grid.cursor().row, 1);
        assert_eq!(grid.cursor().col, 2);
    }

    #[test]
    fn esc_decsc_decrc() {
        // ESC 7 сохраняет, ESC 8 восстанавливает; перо тоже.
        let grid = run(10, 5, b"\x1b[3;3H\x1b[31m\x1b7\x1b[9;9H\x1b[0m\x1b8");
        assert_eq!((grid.cursor().row, grid.cursor().col), (2, 2));
        // перо вернулось к красному
        let grid2 = run(10, 1, b"\x1b[31m\x1b7\x1b[0m\x1b8Z");
        assert_eq!(grid2.cell_at(0, 0).fg, Color::Named(NamedColor::Red));
    }

    // --- v0.1: alt-screen ----------------------------------------------------

    #[test]
    fn alt_screen_saves_and_restores_primary() {
        let mut grid = Grid::new(10, 3);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"primary");
        assert!(!grid.is_alt_screen());

        // Войти в alt-screen → пусто.
        parser.advance(&mut grid, b"\x1b[?1049h");
        assert!(grid.is_alt_screen());
        assert_eq!(line(&grid, 0), "");
        parser.advance(&mut grid, b"ALT");
        assert_eq!(line(&grid, 0), "ALT");

        // Выйти → основной экран восстановлен.
        parser.advance(&mut grid, b"\x1b[?1049l");
        assert!(!grid.is_alt_screen());
        assert_eq!(line(&grid, 0), "primary");
    }

    // --- v0.1: scrollback ----------------------------------------------------

    #[test]
    fn scrollback_collects_scrolled_lines() {
        // 2 строки экрана; печатаем 4 строки → 2 уехали в scrollback.
        let grid = run(10, 2, b"a\r\nb\r\nc\r\nd");
        assert_eq!(grid.scrollback_len(), 2);
        assert_eq!(line(&grid, 0), "c");
        assert_eq!(line(&grid, 1), "d");
    }

    #[test]
    fn alt_screen_has_no_scrollback() {
        let grid = run(10, 2, b"\x1b[?1049ha\r\nb\r\nc\r\nd");
        assert_eq!(grid.scrollback_len(), 0);
    }

    // --- v0.1: OSC заголовок -------------------------------------------------

    #[test]
    fn osc_sets_window_title() {
        let grid = run(10, 1, b"\x1b]0;hello title\x07");
        assert_eq!(grid.title(), Some("hello title"));
        let grid = run(10, 1, b"\x1b]2;just title\x1b\\");
        assert_eq!(grid.title(), Some("just title"));
    }

    // --- v0.1: режимы (bracketed paste, мышь) --------------------------------

    #[test]
    fn bracketed_paste_mode() {
        let grid = run(10, 1, b"\x1b[?2004h");
        assert!(grid.modes().bracketed_paste);
        let grid = run(10, 1, b"\x1b[?2004h\x1b[?2004l");
        assert!(!grid.modes().bracketed_paste);
    }

    #[test]
    fn mouse_reporting_modes() {
        let grid = run(10, 1, b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(grid.modes().mouse_protocol, MouseProtocol::ButtonEvent);
        assert_eq!(grid.modes().mouse_encoding, MouseEncoding::Sgr);
        // выключение протокола
        let grid = run(10, 1, b"\x1b[?1000h\x1b[?1000l");
        assert_eq!(grid.modes().mouse_protocol, MouseProtocol::None);
    }

    // --- v0.1: dirty-трекинг -------------------------------------------------

    #[test]
    fn dirty_tracks_changed_rows() {
        let mut grid = Grid::new(10, 3);
        grid.clear_dirty();
        assert!(grid.dirty().iter().all(|&d| !d));

        // печать в строке 0 → грязная только строка 0
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"x");
        assert!(grid.dirty()[0]);
        assert!(!grid.dirty()[1]);

        // скролл пачкает всё
        grid.clear_dirty();
        grid.scroll_up(1);
        assert!(grid.dirty().iter().all(|&d| d));
    }

    // --- позиционирование: CHA (G) / VPA (d) ---------------------------------

    #[test]
    fn cha_sets_absolute_column() {
        // "abcde", затем CSI 3 G → col 2, печать X → "abXde".
        let grid = run(10, 1, b"abcde\x1b[3GX");
        assert_eq!(line(&grid, 0), "abXde");
    }

    #[test]
    fn vpa_sets_absolute_row() {
        let grid = run(10, 5, b"\x1b[3d");
        assert_eq!(grid.cursor().row, 2);
        assert_eq!(grid.cursor().col, 0);
    }

    // --- редактирование строки: ICH (@) / DCH (P) / ECH (X) ------------------

    #[test]
    fn ich_inserts_blanks_and_shifts_right() {
        let grid = run(10, 1, b"abc\x1b[1;1H\x1b[2@");
        assert_eq!(grid.cell_at(0, 0).ch, ' ');
        assert_eq!(grid.cell_at(1, 0).ch, ' ');
        assert_eq!(grid.cell_at(2, 0).ch, 'a');
        assert_eq!(grid.cell_at(4, 0).ch, 'c');
    }

    #[test]
    fn dch_deletes_and_shifts_left() {
        let grid = run(10, 1, b"abcde\x1b[1;1H\x1b[2P");
        assert_eq!(line(&grid, 0), "cde");
    }

    #[test]
    fn ech_erases_without_shift() {
        let grid = run(10, 1, b"abcde\x1b[1;1H\x1b[3X");
        assert_eq!(grid.cell_at(0, 0).ch, ' ');
        assert_eq!(grid.cell_at(2, 0).ch, ' ');
        assert_eq!(grid.cell_at(3, 0).ch, 'd');
    }

    // --- редактирование строк: IL (L) / DL (M) -------------------------------

    #[test]
    fn delete_lines_shifts_up() {
        // 4 строки; на строке 1 удалить одну → L0,L2,L3,пусто.
        let grid = run(10, 4, b"L0\r\nL1\r\nL2\r\nL3\x1b[2;1H\x1b[M");
        assert_eq!(line(&grid, 0), "L0");
        assert_eq!(line(&grid, 1), "L2");
        assert_eq!(line(&grid, 2), "L3");
        assert_eq!(line(&grid, 3), "");
    }

    #[test]
    fn insert_lines_shifts_down() {
        let grid = run(10, 4, b"L0\r\nL1\r\nL2\r\nL3\x1b[2;1H\x1b[L");
        assert_eq!(line(&grid, 0), "L0");
        assert_eq!(line(&grid, 1), "");
        assert_eq!(line(&grid, 2), "L1");
        assert_eq!(line(&grid, 3), "L2");
    }

    #[test]
    fn delete_all_lines_from_top_does_not_panic() {
        // Граничный случай: удаление всех строк от верха (n == высота региона).
        let grid = run(10, 3, b"L0\r\nL1\r\nL2\x1b[1;1H\x1b[9M");
        assert_eq!(line(&grid, 0), "");
        assert_eq!(line(&grid, 2), "");
    }

    // --- область прокрутки (DECSTBM) + IND/RI/NEL ----------------------------

    #[test]
    fn scroll_region_confines_line_feed() {
        // Регион строки 2..3 (0-based 1..=2). \n на нижней границе скроллит
        // только регион; строки 0 и 3 неподвижны.
        let mut grid = Grid::new(10, 4);
        let mut p = vte::Parser::new();
        p.advance(&mut grid, b"\x1b[2;3r");
        p.advance(&mut grid, b"\x1b[1;1HTOP");
        p.advance(&mut grid, b"\x1b[4;1HBOT");
        p.advance(&mut grid, b"\x1b[2;1HA");
        p.advance(&mut grid, b"\x1b[3;1HB\n");
        assert_eq!(line(&grid, 0), "TOP");
        assert_eq!(line(&grid, 1), "B");
        assert_eq!(line(&grid, 2), "");
        assert_eq!(line(&grid, 3), "BOT");
    }

    #[test]
    fn reverse_index_scrolls_down_at_top() {
        let mut grid = run(10, 3, b"L0\r\nL1\r\nL2");
        let mut p = vte::Parser::new();
        p.advance(&mut grid, b"\x1b[1;1H\x1bM"); // домой + RI
        assert_eq!(line(&grid, 0), "");
        assert_eq!(line(&grid, 1), "L0");
        assert_eq!(line(&grid, 2), "L1");
    }

    // --- resize с сохранением содержимого ------------------------------------

    #[test]
    fn resize_grow_preserves_content() {
        let mut grid = run(8, 2, b"hello\r\nworld");
        grid.resize(20, 5);
        assert_eq!(grid.cols(), 20);
        assert_eq!(grid.rows(), 5);
        assert_eq!(line(&grid, 0), "hello");
        assert_eq!(line(&grid, 1), "world");
    }

    #[test]
    fn resize_shrink_keeps_top_left() {
        let mut grid = run(8, 3, b"ABCDEF\r\nGHIJKL");
        grid.resize(4, 2);
        assert_eq!(grid.cols(), 4);
        assert_eq!(grid.rows(), 2);
        assert_eq!(line(&grid, 0), "ABCD");
        assert_eq!(line(&grid, 1), "GHIJ");
    }

    #[test]
    fn resize_clamps_cursor() {
        let mut grid = run(10, 5, b"\x1b[5;9HX");
        assert_eq!((grid.cursor().row, grid.cursor().col), (4, 9));
        grid.resize(4, 2);
        assert_eq!((grid.cursor().row, grid.cursor().col), (1, 3));
    }

    #[test]
    fn alt_exit_restores_full_scroll_region() {
        // На alt-screen ставим узкий регион; после выхода он должен сброситься
        // на весь экран (LF снизу снова скроллит всё).
        let mut grid = Grid::new(10, 3);
        let mut p = vte::Parser::new();
        p.advance(&mut grid, b"A\r\nB\r\nC");
        p.advance(&mut grid, b"\x1b[?1047h\x1b[1;2r\x1b[?1047l");
        p.advance(&mut grid, b"\x1b[3;1H\nD");
        assert_eq!(line(&grid, 0), "B");
        assert_eq!(line(&grid, 1), "C");
        assert_eq!(line(&grid, 2), "D");
    }
}
