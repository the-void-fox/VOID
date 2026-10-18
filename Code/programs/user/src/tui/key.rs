//! Байты ввода → клавиши.
//!
//! Разбор жил ДВУМЯ копиями: своя у редактора (`ved::Input`) и своя у строки шелла
//! (`vvsh::read_line`). Копии уже разошлись — одна понимает `Home`/`End` в обеих формах
//! (`ESC[H` и `ESC[1~`), другая только в одной, — и это ровно тот случай, против которого
//! заведено правило «две меры расходятся молча». Здесь — одна мера.
//!
//! Чего тут НЕТ: модификаторов у стрелок (`ESC[1;5C` — Ctrl+→). Они разбираются и отбрасываются:
//! ни одна наша программа их пока не спрашивает, а делать вид, что мы их понимаем, — обещание,
//! которого никто не держит. Появится спрос — появится поле в [`Key`].

/// Клавиша.
///
/// `Ctrl(буква)` отдаётся КАК ЕСТЬ, без толкования: «^S — сохранить» решает программа, а не
/// разборщик. Редактор и шелл понимают одни и те же аккорды по-разному, и это нормально.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Tab,
    Esc,
    Back,
    Del,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PgUp,
    PgDn,
}

/// Разборщик потока байт. Держит незаконченное: CSI-последовательность и хвост UTF-8 могут
/// приехать разорванными между двумя чтениями — хост отдаёт столько байт, сколько накопилось,
/// и на границы не смотрит.
#[derive(Default)]
pub struct Keys {
    /// 0 — обычный текст, 1 — видели ESC, 2 — внутри `ESC [ …`.
    esc: u8,
    /// Первый числовой параметр CSI (`ESC[3~` — Delete).
    param: usize,
    /// Видели `;` — дальше модификаторы, и на клавишу они не влияют.
    tail: bool,
    utf: [u8; 4],
    ulen: usize,
    uneed: usize,
}

impl Keys {
    /// Скормить байт. `Some` — клавиша собралась.
    pub fn feed(&mut self, b: u8) -> Option<Key> {
        if self.uneed > 0 {
            if b & 0xc0 == 0x80 && self.ulen < 4 {
                self.utf[self.ulen] = b;
                self.ulen += 1;
                if self.ulen == self.uneed {
                    let ch = core::str::from_utf8(&self.utf[..self.ulen])
                        .ok()
                        .and_then(|s| s.chars().next());
                    self.uneed = 0;
                    self.ulen = 0;
                    return ch.map(Key::Char);
                }
                return None;
            }
            // Рваная последовательность: бросаем накопленное и разбираем этот байт заново.
            self.uneed = 0;
            self.ulen = 0;
        }
        match self.esc {
            1 => {
                match b {
                    b'[' | b'O' => {
                        self.param = 0;
                        self.tail = false;
                        self.esc = 2;
                        None
                    }
                    // ESC, за которым не последовательность, — это сам Esc. Решаем по ПЕРВОМУ
                    // же байту, а не по таймауту: таймаут требует часов в разборщике, а ввод у
                    // нас приезжает кусками, и «одинокий ESC» от «ESC + буква» так не отличить
                    // надёжнее. Зато Esc срабатывает мгновенно, а не через полсекунды.
                    _ => {
                        self.esc = 0;
                        let k = self.plain(b);
                        // Esc отдаём первым, а приехавший следом знак потеряем — но ESC+знак
                        // шлёт только Alt, которого наши программы не слушают.
                        let _ = k;
                        Some(Key::Esc)
                    }
                }
            }
            2 => {
                if b == b';' {
                    self.tail = true;
                    return None;
                }
                if b.is_ascii_digit() {
                    if !self.tail {
                        self.param = self.param * 10 + (b - b'0') as usize;
                    }
                    return None;
                }
                self.esc = 0;
                match b {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    b'~' => match self.param {
                        1 | 7 => Some(Key::Home),
                        3 => Some(Key::Del),
                        4 | 8 => Some(Key::End),
                        5 => Some(Key::PgUp),
                        6 => Some(Key::PgDn),
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => self.plain(b),
        }
    }

    fn plain(&mut self, b: u8) -> Option<Key> {
        match b {
            0x1b => {
                self.esc = 1;
                None
            }
            b'\r' | b'\n' => Some(Key::Enter),
            b'\t' => Some(Key::Tab),
            0x7f | 0x08 => Some(Key::Back),
            // Управляющие: 0x01..0x1a — это Ctrl+A..Ctrl+Z. Enter (0x0d), Tab (0x09) и
            // Backspace разобраны выше — у них своё имя, и отдавать их как `Ctrl('m')` значило
            // бы заставлять каждую программу помнить эту мелочь.
            0x01..=0x1a => Some(Key::Ctrl((b - 1 + b'a') as char)),
            0x20..=0x7e => Some(Key::Char(b as char)),
            // Начало многобайтного UTF-8.
            0xc0..=0xdf | 0xe0..=0xef | 0xf0..=0xf7 => {
                self.uneed = match b {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                self.utf[0] = b;
                self.ulen = 1;
                None
            }
            _ => None,
        }
    }
}
