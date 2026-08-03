//! # ereb-input
//!
//! Чистая логика преобразования **событие клавиатуры → действие**. Не открывает
//! устройства и не знает про Wayland/DRM — только про [`KeyEvent`], биндинги и
//! кодирование в PTY (см. `obsidian/03-subsystems/input.md`).
//!
//! ## Конвейер
//!
//! ```text
//! [scancode + mods] ──(xkbcommon, фича)──▶ KeyEvent ──translate──▶ Action
//! ```
//!
//! Декодирование scancode → [`KeyEvent`] делает [`Keyboard`] за фичей
//! `xkbcommon` (C-зависимость). Всё остальное — чистый Rust:
//!
//! ```
//! use ereb_input::{translate, Bindings, KeyEvent, Keysym, ModMask, Mode, Action};
//!
//! let bindings = Bindings::with_defaults();
//!
//! // Обычная клавиша уходит в PTY.
//! let a = KeyEvent::with_text(Keysym::from_char('a'), "a", ModMask::empty());
//! assert_eq!(translate(&a, &bindings, Mode::Normal), Action::SendToPty(b"a".to_vec()));
//!
//! // Ctrl+Shift+C — перехватывается биндингом.
//! let copy = KeyEvent::new(Keysym::from_char('C'), ModMask::CTRL | ModMask::SHIFT);
//! assert_eq!(translate(&copy, &bindings, Mode::Normal), Action::Copy);
//! ```

// Крейт — чистая логика клавиш, ОС ему не нужна: `no_std`, чтобы собираться для VOID
// ([ADR 0005]). Фича `xkbcommon` тянет C-библиотеку и, значит, std — но она нужна только
// бэкендам Linux.
//
// [ADR 0005]: ../../../obsidian/02-architecture/adr/0005-void-target.md
#![cfg_attr(not(feature = "xkbcommon"), no_std)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod action;
mod bindings;
mod encode;
mod event;
mod keysym;

#[cfg(feature = "xkbcommon")]
mod xkb;

pub use action::{Action, Mode};
pub use bindings::Bindings;
pub use encode::encode_key;
pub use event::{KeyDirection, KeyEvent, ModMask};
pub use keysym::Keysym;

#[cfg(feature = "xkbcommon")]
pub use xkb::Keyboard;

/// Превращает событие клавиши в действие: биндинг, если есть, иначе — отправка
/// закодированных байт в PTY (или [`Action::Ignore`], если посылать нечего).
pub fn translate(key: &KeyEvent, bindings: &Bindings, mode: Mode) -> Action {
    if let Some(action) = bindings.get(mode, key.mods, key.keysym) {
        return action.clone();
    }
    match encode_key(key) {
        Some(bytes) => Action::SendToPty(bytes),
        None => Action::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(c: char, mods: ModMask) -> KeyEvent {
        KeyEvent::with_text(Keysym::from_char(c), c.to_string(), mods)
    }

    // --- биндинги ------------------------------------------------------------

    #[test]
    fn defaults_have_copy_paste() {
        let b = Bindings::with_defaults();
        let cs = ModMask::CTRL | ModMask::SHIFT;
        assert_eq!(
            b.get(Mode::Normal, cs, Keysym::from_char('c')),
            Some(&Action::Copy)
        );
        assert_eq!(
            b.get(Mode::Normal, cs, Keysym::from_char('v')),
            Some(&Action::Paste)
        );
    }

    #[test]
    fn binding_lookup_is_shift_normalized() {
        // Shift делает keysym 'C' (0x43); биндинг хранится как 'c' и всё равно матчится.
        let b = Bindings::with_defaults();
        let cs = ModMask::CTRL | ModMask::SHIFT;
        assert_eq!(b.get(Mode::Normal, cs, Keysym(0x43)), Some(&Action::Copy));
    }

    #[test]
    fn binding_respects_mode() {
        let b = Bindings::with_defaults();
        let cs = ModMask::CTRL | ModMask::SHIFT;
        // В режиме Pane тех же биндингов нет.
        assert_eq!(b.get(Mode::Pane, cs, Keysym::from_char('c')), None);
    }

    // --- кодирование в PTY ---------------------------------------------------

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            encode_key(&text('a', ModMask::empty())),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(encode_key(&text('a', ModMask::ALT)), Some(vec![0x1b, b'a']));
    }

    #[test]
    fn ctrl_letter_is_control_byte() {
        // Ctrl+A → 0x01, Ctrl+C → 0x03 (даже без текста в событии).
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::from_char('a'), ModMask::CTRL)),
            Some(vec![0x01])
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::from_char('c'), ModMask::CTRL)),
            Some(vec![0x03])
        );
    }

    #[test]
    fn special_keys_encode() {
        let m = ModMask::empty();
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::RETURN, m)),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::BACKSPACE, m)),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::ESCAPE, m)),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::TAB, m)),
            Some(vec![b'\t'])
        );
    }

    #[test]
    fn shift_tab_is_back_tab() {
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::TAB, ModMask::SHIFT)),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn cursor_keys_plain_and_modified() {
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::UP, ModMask::empty())),
            Some(b"\x1b[A".to_vec())
        );
        // Shift+Up → CSI 1;2A
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::UP, ModMask::SHIFT)),
            Some(b"\x1b[1;2A".to_vec())
        );
        // Ctrl+Right → CSI 1;5C
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::RIGHT, ModMask::CTRL)),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn function_and_nav_keys() {
        let m = ModMask::empty();
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::F1, m)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::F5, m)),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::PAGE_UP, m)),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode_key(&KeyEvent::new(Keysym::DELETE, m)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    // --- translate -----------------------------------------------------------

    #[test]
    fn translate_routes_binding_vs_pty() {
        let b = Bindings::with_defaults();
        // Ctrl+Shift+C → действие.
        let copy = KeyEvent::new(Keysym::from_char('C'), ModMask::CTRL | ModMask::SHIFT);
        assert_eq!(translate(&copy, &b, Mode::Normal), Action::Copy);
        // Просто 'a' → в PTY.
        assert_eq!(
            translate(&text('a', ModMask::empty()), &b, Mode::Normal),
            Action::SendToPty(b"a".to_vec())
        );
        // Ctrl+C (без Shift) не забиндено → control-байт в PTY.
        let ctrl_c = KeyEvent::new(Keysym::from_char('c'), ModMask::CTRL);
        assert_eq!(
            translate(&ctrl_c, &b, Mode::Normal),
            Action::SendToPty(vec![0x03])
        );
    }

    #[test]
    fn modifier_only_press_is_ignored() {
        // Нет текста, не спец-клавиша, не Ctrl+буква → нечего слать.
        let only_shift = KeyEvent::new(Keysym(0xffe1), ModMask::SHIFT); // Shift_L
        assert_eq!(
            translate(&only_shift, &Bindings::new(), Mode::Normal),
            Action::Ignore
        );
    }
}
