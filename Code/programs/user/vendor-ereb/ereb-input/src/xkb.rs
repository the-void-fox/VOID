//! Декодер scancode → [`KeyEvent`] на базе xkbcommon (за фичей `xkbcommon`).
//!
//! Держит `Context`/`Keymap`/`State` на сессию. Источник событий (Wayland/DRM)
//! кормит сюда evdev-коды и обновления модификаторов; на выходе — готовый
//! бэкенд-независимый [`KeyEvent`].

use xkbcommon::xkb;

use crate::event::{KeyDirection, KeyEvent, ModMask};
use crate::keysym::Keysym;

/// Смещение между evdev-кодами (как шлёт Wayland/ядро) и X11-кодами xkb.
const EVDEV_OFFSET: u32 = 8;

/// Состояние клавиатуры: раскладка + текущие модификаторы.
pub struct Keyboard {
    _context: xkb::Context,
    _keymap: xkb::Keymap,
    state: xkb::State,
}

impl Keyboard {
    /// Раскладка из системных имён (`XKB_DEFAULT_*` при пустом `layout`).
    /// Удобно для DRM-бэкенда и ручного теста.
    pub fn from_names(layout: &str) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        let state = xkb::State::new(&keymap);
        Some(Keyboard {
            _context: context,
            _keymap: keymap,
            state,
        })
    }

    /// Раскладка из строки keymap (Wayland шлёт её через fd, формат TEXT_V1).
    pub fn from_keymap_string(keymap: String) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            keymap,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        let state = xkb::State::new(&keymap);
        Some(Keyboard {
            _context: context,
            _keymap: keymap,
            state,
        })
    }

    /// Обновляет модификаторы/раскладку из Wayland-события `modifiers`.
    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.state
            .update_mask(depressed, latched, locked, 0, 0, group);
    }

    /// Декодирует evdev-код в [`KeyEvent`], обновляя состояние xkb.
    ///
    /// Текст и keysym берутся для нажатия (`Down`); на отпускание они тоже
    /// считаются, но обычно вызывающий шлёт в PTY только `Down`.
    pub fn key(&mut self, evdev_code: u32, direction: KeyDirection, repeat: bool) -> KeyEvent {
        let keycode = xkb::Keycode::new(evdev_code + EVDEV_OFFSET);
        let xkb_dir = match direction {
            KeyDirection::Down => xkb::KeyDirection::Down,
            KeyDirection::Up => xkb::KeyDirection::Up,
        };

        let keysym = Keysym(self.state.key_get_one_sym(keycode).raw());
        let text = self.state.key_get_utf8(keycode);
        let mods = self.current_mods();

        self.state.update_key(keycode, xkb_dir);

        KeyEvent {
            keysym,
            text,
            mods,
            repeat,
        }
    }

    /// Текущая маска модификаторов из состояния xkb.
    fn current_mods(&self) -> ModMask {
        let mut mods = ModMask::empty();
        let active = |name| {
            self.state
                .mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
        };
        if active(xkb::MOD_NAME_CTRL) {
            mods |= ModMask::CTRL;
        }
        if active(xkb::MOD_NAME_ALT) {
            mods |= ModMask::ALT;
        }
        if active(xkb::MOD_NAME_SHIFT) {
            mods |= ModMask::SHIFT;
        }
        if active(xkb::MOD_NAME_LOGO) {
            mods |= ModMask::LOGO;
        }
        mods
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// evdev-код клавиши `A`.
    const KEY_A: u32 = 30;

    #[test]
    fn decodes_letter_with_us_layout() {
        // Требует доступной базы xkeyboard-config. Если её нет (голое окружение),
        // тихо пропускаем — компиляцию это всё равно проверяет.
        let Some(mut kbd) = Keyboard::from_names("us") else {
            eprintln!("skip: нет xkb-раскладки (xkeyboard-config недоступен)");
            return;
        };
        let event = kbd.key(KEY_A, KeyDirection::Down, false);
        assert_eq!(event.keysym, Keysym::from_char('a'));
        assert_eq!(event.text, "a");
        assert!(event.mods.is_empty());
    }
}
