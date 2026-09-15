//! Разбор отчётов HID-клавиатуры — ОБЩИЙ для обоих контроллеров USB (Веха 199).
//!
//! Протокол загрузочной клавиатуры один и тот же, кто бы её ни вёз: xHCI (Веха 50) или EHCI.
//! Поэтому разбор живёт здесь, а не в драйверах: две копии одного словаря разошлись бы на
//! первой же правке раскладки, и различить их снаружи было бы нечем — «на этом ноутбуке
//! клавиша печатает не то» звучит как что угодно, только не как «драйверов два».
//!
//! Наружу отчёт уходит ДВУМЯ путями, и оба нужны: байтом в кольцо консоли (его читает
//! спасательный шелл, который живёт без графики) и СОБЫТИЕМ клавиатуры с модификаторами (его
//! читает композитор, а через него окна).

/// HID Usage → код клавиши VOID (`KeyEvent::sym`) — тот же словарь, которым говорит PS/2
/// ([`crate::arch`]), иначе оболочка не узнает клавишу по USB.
///
/// Веха 196 — без этого USB-клавиатура в ОКОННОМ сеансе не работала вовсе: репорты уходили
/// байтами в консольное кольцо ядра, а его читает спасательный шелл, а не композитор. То
/// есть на машине, где клавиатура только по USB, сеанс поднимался, а набрать в нём было
/// нечего — и увидеть это можно было только на такой машине.
fn hid_to_sym(k: u8) -> u16 {
    match k {
        0x04..=0x1d => (b'a' + (k - 0x04)) as u16, // буквы: НЕсдвинутый символ, как у PS/2
        0x1e..=0x26 => (b'1' + (k - 0x1e)) as u16, // 1..9
        0x27 => b'0' as u16,
        0x28 => 0x101, // Enter
        0x29 => 0x102, // Esc
        0x2a => 0x104, // Backspace
        0x2b => 0x103, // Tab
        0x2c => b' ' as u16,
        0x2d => b'-' as u16,
        0x2e => b'=' as u16,
        0x2f => b'[' as u16,
        0x30 => b']' as u16,
        0x31 => b'\\' as u16,
        0x33 => b';' as u16,
        0x34 => b'\'' as u16,
        0x35 => b'`' as u16,
        0x36 => b',' as u16,
        0x37 => b'.' as u16,
        0x38 => b'/' as u16,
        0x3a..=0x43 => 0x120 + (k - 0x3a) as u16, // F1..F10
        0x44 => 0x12a,                            // F11
        0x45 => 0x12b,                            // F12
        0x49 => 0x106, // Insert
        0x4a => 0x114, // Home
        0x4b => 0x116, // PageUp
        0x4c => 0x105, // Delete
        0x4d => 0x115, // End
        0x4e => 0x117, // PageDown
        0x4f => 0x111, // Right
        0x50 => 0x110, // Left
        0x51 => 0x113, // Down
        0x52 => 0x112, // Up
        _ => 0,
    }
}

/// HID Usage (boot keyboard) → ASCII. Достаточно для vsh: буквы, цифры, пробел, Enter, Backspace,
/// Tab, базовая пунктуация; Shift даёт верхний регистр/символы. Неизвестное — `None`.
fn hid_to_ascii(k: u8, shift: bool) -> Option<u8> {
    let b = match k {
        0x04..=0x1d => {
            let c = b'a' + (k - 0x04);
            return Some(if shift { c - 32 } else { c });
        }
        0x1e..=0x26 => {
            let d = b'1' + (k - 0x1e);
            let sym = [b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'('];
            return Some(if shift { sym[(k - 0x1e) as usize] } else { d });
        }
        0x27 => if shift { b')' } else { b'0' },
        0x28 => b'\r', // Enter
        0x2a => 0x08,  // Backspace
        0x2b => b'\t', // Tab
        0x2c => b' ',  // Space
        0x2d => if shift { b'_' } else { b'-' },
        0x2e => if shift { b'+' } else { b'=' },
        0x2f => if shift { b'{' } else { b'[' },
        0x30 => if shift { b'}' } else { b']' },
        0x31 => if shift { b'|' } else { b'\\' },
        0x33 => if shift { b':' } else { b';' },
        0x34 => if shift { b'"' } else { b'\'' },
        0x36 => if shift { b'<' } else { b',' },
        0x37 => if shift { b'>' } else { b'.' },
        0x38 => if shift { b'?' } else { b'/' },
        _ => return None,
    };
    Some(b)
}

/// Опрос USB-клавиатуры — зовётся из `console_drain` (тик/IRQ) наравне с PS/2 и COM1.

/// Разобрать один восьмибайтный отчёт: `prev` — набор клавиш из прошлого отчёта (по нему видно,
/// что НОВОЕ нажали и что отпустили), `r` — свежий отчёт.
pub fn report(prev: &mut [u8; 6], r: &[u8]) {
    if r.len() < 8 {
        return;
    }
    let hid_mods = r[0];
    let shift = hid_mods & 0x22 != 0; // Left/Right Shift
    // Модификаторы в наших битах: 1 Shift, 2 Ctrl, 4 Alt, 8 Super — те же, что у PS/2.
    let mut mods = 0u8;
    if shift {
        mods |= 1;
    }
    if hid_mods & 0x11 != 0 {
        mods |= 2; // Left/Right Ctrl
    }
    if hid_mods & 0x44 != 0 {
        mods |= 4; // Left/Right Alt
    }
    if hid_mods & 0x88 != 0 {
        mods |= 8; // Left/Right GUI (Super)
    }
    for &k in &r[2..8] {
        // Новое нажатие: код есть в этом репорте, но не было в прошлом.
        if k != 0 && !prev.contains(&k) {
            if let Some(b) = hid_to_ascii(k, shift) {
                crate::arch::usb_key(b); // байт в консольное кольцо (спасательный шелл)
            }
            // Веха 196 — и СОБЫТИЕ клавиатуры: его читает композитор, а через него —
            // окна. Оба пути нужны: консоль живёт без графики, окна — без консоли.
            let sym = hid_to_sym(k);
            if sym != 0 {
                let ch = hid_to_ascii(k, shift).map_or(0, |b| b as u16);
                crate::arch::usb_key_event(sym, mods, true, ch);
            }
        }
    }
    // Отпускания: код БЫЛ в прошлом репорте, а в этом его нет. Без них окно считает
    // клавишу зажатой навсегда — и первый же аккорд оболочки залипает.
    for &k in prev.iter() {
        if k != 0 && !r[2..8].contains(&k) {
            let sym = hid_to_sym(k);
            if sym != 0 {
                crate::arch::usb_key_event(sym, mods, false, 0);
            }
        }
    }
    prev.copy_from_slice(&r[2..8]);
}
