//! Кодирование [`KeyEvent`] в байты для PTY (xterm-совместимо).
//!
//! Покрывает практичный минимум: печатный текст, `Ctrl+<буква>` → control-байт,
//! `Alt+X` → ESC-префикс, спец-клавиши (Enter/Tab/Backspace/Esc), курсорные и
//! навигационные клавиши с модификаторами по схеме `CSI 1 ; mod <final>`, F1–F12.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::event::{KeyEvent, ModMask};
use crate::keysym::Keysym;

/// Параметр-модификатор в xterm-последовательностях: `1 + Σ`.
fn mod_param(mods: ModMask) -> u8 {
    1 + (mods.contains(ModMask::SHIFT) as u8)
        + (mods.contains(ModMask::ALT) as u8) * 2
        + (mods.contains(ModMask::CTRL) as u8) * 4
        + (mods.contains(ModMask::LOGO) as u8) * 8
}

/// Префикс ESC (0x1b) при зажатом Alt — «meta sends escape».
fn alt_prefix(key: &KeyEvent, mut bytes: Vec<u8>) -> Vec<u8> {
    if key.mods.contains(ModMask::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// `CSI` для курсорной клавиши с финальным байтом `final_byte` (`A`/`B`/`C`/`D`/`H`/`F`).
fn cursor_seq(key: &KeyEvent, final_byte: u8) -> Vec<u8> {
    let m = mod_param(key.mods);
    if m == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
    }
}

/// `CSI num ~` навигационная клавиша (PageUp, Insert, …) с модификаторами.
fn tilde_seq(key: &KeyEvent, num: u8) -> Vec<u8> {
    let m = mod_param(key.mods);
    if m == 1 {
        format!("\x1b[{num}~").into_bytes()
    } else {
        format!("\x1b[{num};{m}~").into_bytes()
    }
}

/// Функциональные клавиши F1–F4 (SS3 `\eO{P..S}`; с модификаторами — `CSI 1;m {P..S}`).
fn ss3_seq(key: &KeyEvent, final_byte: u8) -> Vec<u8> {
    let m = mod_param(key.mods);
    if m == 1 {
        vec![0x1b, b'O', final_byte]
    } else {
        format!("\x1b[1;{m}{}", final_byte as char).into_bytes()
    }
}

/// Кодирует спец-клавиши; `None` — клавиша не «специальная».
fn encode_special(key: &KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.keysym {
        Keysym::RETURN | Keysym::KP_ENTER => alt_prefix(key, vec![b'\r']),
        Keysym::BACKSPACE => alt_prefix(key, vec![0x7f]),
        Keysym::ESCAPE => alt_prefix(key, vec![0x1b]),
        Keysym::TAB => {
            if key.mods.contains(ModMask::SHIFT) {
                b"\x1b[Z".to_vec()
            } else {
                alt_prefix(key, vec![b'\t'])
            }
        }
        Keysym::ISO_LEFT_TAB => b"\x1b[Z".to_vec(),

        Keysym::UP => cursor_seq(key, b'A'),
        Keysym::DOWN => cursor_seq(key, b'B'),
        Keysym::RIGHT => cursor_seq(key, b'C'),
        Keysym::LEFT => cursor_seq(key, b'D'),
        Keysym::HOME => cursor_seq(key, b'H'),
        Keysym::END => cursor_seq(key, b'F'),

        Keysym::INSERT => tilde_seq(key, 2),
        Keysym::DELETE => tilde_seq(key, 3),
        Keysym::PAGE_UP => tilde_seq(key, 5),
        Keysym::PAGE_DOWN => tilde_seq(key, 6),

        Keysym::F1 => ss3_seq(key, b'P'),
        Keysym::F2 => ss3_seq(key, b'Q'),
        Keysym::F3 => ss3_seq(key, b'R'),
        Keysym::F4 => ss3_seq(key, b'S'),
        Keysym::F5 => tilde_seq(key, 15),
        Keysym::F6 => tilde_seq(key, 17),
        Keysym::F7 => tilde_seq(key, 18),
        Keysym::F8 => tilde_seq(key, 19),
        Keysym::F9 => tilde_seq(key, 20),
        Keysym::F10 => tilde_seq(key, 21),
        Keysym::F11 => tilde_seq(key, 23),
        Keysym::F12 => tilde_seq(key, 24),

        _ => return None,
    };
    Some(bytes)
}

/// Кодирует клавишу в байты для PTY, либо `None`, если посылать нечего
/// (например, нажатие чистого модификатора).
pub fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    if let Some(bytes) = encode_special(key) {
        return Some(bytes);
    }

    // Ctrl+<буква> → control-байт (с возможным ESC-префиксом от Alt).
    if key.mods.contains(ModMask::CTRL)
        && let Some(byte) = key.keysym.control_byte()
    {
        return Some(alt_prefix(key, vec![byte]));
    }

    // Печатный текст. Логотип/Ctrl без отдельной обработки текст не шлют.
    if !key.text.is_empty() && !key.mods.contains(ModMask::CTRL) {
        return Some(alt_prefix(key, key.text.as_bytes().to_vec()));
    }

    None
}
