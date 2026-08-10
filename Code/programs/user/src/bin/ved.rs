//! `ved` — экранный редактор текста (Веха 120).
//!
//! ```text
//! ved /etc/system/terminal.vv     открыть файл (нет такого — создастся при сохранении)
//! ```
//!
//! Зачем он появился раньше оконной оболочки, хотя оболочка интереснее. До этой вехи изменить
//! файл в VOID было нечем: `echo текст > файл` из `vsh` пишет файл ЦЕЛИКОМ и одной строкой, то
//! есть поправить два символа в конфиге поколения означало набрать заново все две тысячи байт.
//! Это упиралось прямо в текущую работу — раскладка окон, клавиши, список программ живут в
//! `/etc/system/*.vv`, и каждая проба означала пересборку образа на большой машине.
//!
//! И второе, важнее. Веха 119.1 научила систему поднимать спасательный `vsh`, когда шелл
//! поколения не прожил и пяти секунд. Но спасательный шелл без редактора спасает наполовину:
//! человек видит, ЧТО сломано (`klog`), и не может это починить — только пересеять конфиг
//! целиком (`init-config`), потеряв всё своё. Поэтому редактор — программа, а не команда
//! терминала, и рисует он ОДНИМИ escape-кодами: те же байты понимает и консоль ядра (куда
//! падает спасательный шелл), и грид `term`, и `term` внутри окна.
//!
//! ## Управление
//!
//! Стрелки/Home/End/PageUp/PageDown работают, но продублированы аккордами Ctrl (`^B`/`^F`/`^P`/
//! `^N`/`^A`/`^E`). Это не дань традиции readline: на ноутбуке владельца у встроенной клавиатуры
//! мертвы ← и →, а редактор, в котором нельзя сдвинуть курсор, бесполезен ровно на той машине,
//! где он нужнее всего.
//!
//! `^S` — сохранить, `^Q` — выйти (при несохранённых правках спрашивает второй раз), `^K` —
//! удалить строку, `^L` — перерисовать и перемерить экран.
//!
//! ## Границы, названные вслух
//!
//! Файл целиком лежит в памяти и пишется целиком же — файловая персоналия и так держит файл
//! одним объектом store (потолок — 128 КиБ, `DATA_MAX` в `bin/posixfs`). Не UTF-8 редактор не
//! открывает: молча испортить чужие байты хуже, чем отказать.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use alloc::{format, string::ToString};

use void_user as sys;
use void_user::{posix as px, stdio};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок файла — тот же, что у файловой персоналии (`DATA_MAX` в `bin/posixfs`). Проверяем
/// его САМИ при сохранении: узнать о переполнении из обрезанного файла — узнать слишком поздно.
const FILE_MAX: usize = 128 * 1024;

/// Tab вставляет пробелы: конфиги поколения набраны пробелами, а показ настоящих табуляций
/// требовал бы отдельной арифметики колонок ради символа, которого мы сами не пишем.
const TAB_WIDTH: usize = 2;

/// Размер экрана, когда его не назвал никто — ни хост, ни ядро (консоль в serial). 80×25 есть у
/// любого терминала: это не догадка, а безопасное подмножество.
const FALLBACK: (u16, u16) = (80, 25);

/// «1 строка», «2 строки», «5 строк» — правило русского счёта. Мелочь, но строка состояния
/// висит перед глазами постоянно, и «2 строк» в ней мозолит глаз.
fn lines_word(n: usize) -> &'static str {
    let (t, h) = (n % 10, n % 100);
    if t == 1 && h != 11 {
        "строка"
    } else if (2..=4).contains(&t) && !(12..=14).contains(&h) {
        "строки"
    } else {
        "строк"
    }
}

// ── ввод ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Key {
    Ch(char),
    Enter,
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
    Save,
    Quit,
    CutLine,
    Refresh,
}

/// Разбор потока байт в клавиши: CSI-последовательности терминала плюс сборка многобайтных
/// символов UTF-8.
#[derive(Default)]
struct Input {
    /// 0 — обычный текст, 1 — видели ESC, 2 — внутри `ESC [ …`.
    esc: u8,
    /// Первый числовой параметр CSI (`ESC[3~` — Delete).
    param: usize,
    /// Видели `;` — дальше идут модификаторы xterm (`ESC[1;5C`), и на клавишу они не влияют:
    /// параметр после точки с запятой не должен затирать первый.
    tail: bool,
    utf: [u8; 4],
    ulen: usize,
    uneed: usize,
}

impl Input {
    fn feed(&mut self, b: u8) -> Option<Key> {
        // Хвост многобайтного символа. Он может приехать РАЗОРВАННЫМ между двумя чтениями —
        // хост отдаёт столько байт, сколько накопилось, и на границу символа не смотрит.
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
                    return ch.map(Key::Ch);
                }
                return None;
            }
            // Рваная последовательность: бросаем накопленное и разбираем этот байт заново.
            self.uneed = 0;
            self.ulen = 0;
        }
        match self.esc {
            1 => {
                if b == b'[' {
                    self.param = 0;
                    self.tail = false;
                    self.esc = 2;
                } else {
                    self.esc = 0;
                }
                None
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
            _ => match b {
                0x1b => {
                    self.esc = 1;
                    None
                }
                b'\r' | b'\n' => Some(Key::Enter),
                0x7f | 0x08 => Some(Key::Back),
                0x01 => Some(Key::Home),    // ^A
                0x02 => Some(Key::Left),    // ^B
                0x05 => Some(Key::End),     // ^E
                0x06 => Some(Key::Right),   // ^F
                0x0b => Some(Key::CutLine), // ^K
                0x0c => Some(Key::Refresh), // ^L
                0x0e => Some(Key::Down),    // ^N
                0x10 => Some(Key::Up),      // ^P
                0x11 => Some(Key::Quit),    // ^Q
                0x13 => Some(Key::Save),    // ^S
                b'\t' => Some(Key::Ch('\t')),
                c if (0x20..0x7f).contains(&c) => Some(Key::Ch(c as char)),
                c if c >= 0xc0 => {
                    self.uneed = if c >= 0xf0 {
                        4
                    } else if c >= 0xe0 {
                        3
                    } else {
                        2
                    };
                    self.utf[0] = c;
                    self.ulen = 1;
                    None
                }
                _ => None,
            },
        }
    }
}

// ── файл ────────────────────────────────────────────────────────────────────

/// Прочитать файл целиком. `None` — открыть не удалось (нет файла, либо он больше потолка
/// персоналии — тогда она сама скажет об этом в журнал).
fn read_file(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = px::read(ep, fd, &mut buf);
        if n == 0 || n == usize::MAX || out.len() > FILE_MAX {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    px::close(ep, fd);
    Some(out)
}

// ── редактор ────────────────────────────────────────────────────────────────

struct Ed {
    ep: usize,
    path: String,
    lines: Vec<String>,
    /// Курсор в СИМВОЛАХ, не в байтах: строки конфига наполовину кириллица, и байтовый курсор
    /// вставал бы в середину буквы.
    cx: usize,
    cy: usize,
    top: usize,
    left: usize,
    cols: usize,
    rows: usize,
    changed: bool,
    /// Сообщение в строке состояния — на один кадр (результат сохранения, предупреждение).
    msg: String,
    /// `^Q` при несохранённых правках только предупреждает; выходит второй подряд.
    quit_armed: bool,
    /// Перерисовать весь экран: строки сдвинулись, одной строкой не обойтись.
    full: bool,
    done: bool,
}

impl Ed {
    fn text_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// Ширина текста: последняя клетка строки не трогается. Запись в правый нижний угол на
    /// многих терминалах (и в консоли ядра) прокручивает экран — кадр уезжал бы на строку вверх.
    fn width(&self) -> usize {
        self.cols.saturating_sub(1).max(1)
    }

    fn line_len(&self, i: usize) -> usize {
        self.lines.get(i).map_or(0, |l| l.chars().count())
    }

    /// Байтовое смещение символа №`ci` — стык между «курсор в символах» и `String` в байтах.
    fn byte_at(line: &str, ci: usize) -> usize {
        line.char_indices().nth(ci).map_or(line.len(), |(b, _)| b)
    }

    fn measure(&mut self) {
        // Порядок опроса — это порядок осведомлённости: свой хост знает размер ОКНА (панель
        // терминала), ядро — размер своей консоли (спасательный шелл), и только если молчат оба,
        // берём умолчание.
        let (c, r) = stdio::win_size().or_else(sys::console_size).unwrap_or(FALLBACK);
        self.cols = (c as usize).clamp(20, 400);
        self.rows = (r as usize).clamp(4, 200);
    }

    // ── правки ──

    fn insert(&mut self, ch: char) {
        let at = Self::byte_at(&self.lines[self.cy], self.cx);
        self.lines[self.cy].insert(at, ch);
        self.cx += 1;
        self.changed = true;
    }

    fn newline(&mut self) {
        let at = Self::byte_at(&self.lines[self.cy], self.cx);
        let rest = self.lines[self.cy].split_off(at);
        self.lines.insert(self.cy + 1, rest);
        self.cy += 1;
        self.cx = 0;
        self.changed = true;
        self.full = true;
    }

    fn backspace(&mut self) {
        if self.cx > 0 {
            let at = Self::byte_at(&self.lines[self.cy], self.cx - 1);
            self.lines[self.cy].remove(at);
            self.cx -= 1;
            self.changed = true;
        } else if self.cy > 0 {
            let cur = self.lines.remove(self.cy);
            self.cy -= 1;
            self.cx = self.line_len(self.cy);
            self.lines[self.cy].push_str(&cur);
            self.changed = true;
            self.full = true;
        }
    }

    fn delete(&mut self) {
        if self.cx < self.line_len(self.cy) {
            let at = Self::byte_at(&self.lines[self.cy], self.cx);
            self.lines[self.cy].remove(at);
            self.changed = true;
        } else if self.cy + 1 < self.lines.len() {
            let next = self.lines.remove(self.cy + 1);
            self.lines[self.cy].push_str(&next);
            self.changed = true;
            self.full = true;
        }
    }

    fn cut_line(&mut self) {
        self.lines.remove(self.cy);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cy = self.cy.min(self.lines.len() - 1);
        self.cx = 0;
        self.changed = true;
        self.full = true;
    }

    /// Записать файл. Пишем ЦЕЛИКОМ (`echo_to` = open+O_TRUNC, write, close): у персоналии файл
    /// и так один объект store, и частичная запись не была бы дешевле — зато была бы опаснее.
    fn save(&mut self) {
        let mut text = self.lines.join("\n");
        // Хвостовой перевод строки — как у всех текстовых файлов системы; пустой файл остаётся
        // пустым (перевод строки в нём был бы правкой, которой человек не делал).
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if text.len() > FILE_MAX {
            self.msg = format!("НЕ сохранено: {} Б больше потолка {} Б", text.len(), FILE_MAX);
            return;
        }
        if px::echo_to(self.ep, self.path.as_bytes(), text.as_bytes()) {
            self.changed = false;
            self.msg = format!(
                "сохранено: {} Б, {} {}",
                text.len(),
                self.lines.len(),
                lines_word(self.lines.len())
            );
        } else {
            // Веха 101 научила запись отвечать честно; половина конфига на диске — это ровно
            // тот случай, ради которого она отвечает.
            self.msg = "ОШИБКА ЗАПИСИ — файл на диске неполон!".to_string();
        }
        self.full = true;
    }

    // ── движение ──

    fn left_key(&mut self) {
        if self.cx > 0 {
            self.cx -= 1;
        } else if self.cy > 0 {
            self.cy -= 1;
            self.cx = self.line_len(self.cy);
        }
    }

    fn right_key(&mut self) {
        if self.cx < self.line_len(self.cy) {
            self.cx += 1;
        } else if self.cy + 1 < self.lines.len() {
            self.cy += 1;
            self.cx = 0;
        }
    }

    fn vertical(&mut self, delta: isize) {
        let target = self.cy as isize + delta;
        self.cy = target.clamp(0, self.lines.len() as isize - 1) as usize;
        // Курсор за концом новой строки — прижимаем к концу. Колонку «как была» не помним
        // намеренно: помнить её и не показывать — источник сюрпризов при правке.
        self.cx = self.cx.min(self.line_len(self.cy));
    }

    fn key(&mut self, k: Key) {
        if k != Key::Quit {
            self.quit_armed = false;
        }
        match k {
            Key::Ch('\t') => {
                for _ in 0..TAB_WIDTH {
                    self.insert(' ');
                }
            }
            Key::Ch(c) => self.insert(c),
            Key::Enter => self.newline(),
            Key::Back => self.backspace(),
            Key::Del => self.delete(),
            Key::CutLine => self.cut_line(),
            Key::Left => self.left_key(),
            Key::Right => self.right_key(),
            Key::Up => self.vertical(-1),
            Key::Down => self.vertical(1),
            Key::PgUp => self.vertical(-(self.text_rows() as isize)),
            Key::PgDn => self.vertical(self.text_rows() as isize),
            Key::Home => self.cx = 0,
            Key::End => self.cx = self.line_len(self.cy),
            Key::Save => self.save(),
            Key::Refresh => {
                self.measure();
                self.full = true;
            }
            Key::Quit => {
                if self.changed && !self.quit_armed {
                    self.quit_armed = true;
                    self.msg = "есть несохранённое: ^S сохранить, ^Q ещё раз — выйти без него"
                        .to_string();
                    self.full = true;
                } else {
                    self.done = true;
                }
            }
        }
    }

    // ── вывод ──

    fn row_into(&self, out: &mut String, r: usize) {
        let _ = core::fmt::Write::write_fmt(out, format_args!("\x1b[{};1H", r + 1));
        match self.lines.get(self.top + r) {
            Some(line) => {
                for ch in line.chars().skip(self.left).take(self.width()) {
                    // Чужие табуляции показываем пробелом: свои мы не пишем (TAB_WIDTH), а
                    // ширина чужой зависит от колонки — арифметика ради символа, которого нет.
                    out.push(if ch == '\t' { ' ' } else { ch });
                }
            }
            // За последней строкой файла — тусклая тильда: пустая строка файла и место, где
            // файла уже нет, должны выглядеть по-разному.
            None => out.push_str("\x1b[90m~\x1b[0m"),
        }
        out.push_str("\x1b[K");
    }

    fn status_into(&self, out: &mut String) {
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        let left = format!(
            " {}{}  {}:{}  {} {} ",
            name,
            if self.changed { " *" } else { "" },
            self.cy + 1,
            self.cx + 1,
            self.lines.len(),
            lines_word(self.lines.len())
        );
        let right = if self.msg.is_empty() {
            " ^S сохранить  ^Q выход  ^K удалить строку ".to_string()
        } else {
            format!(" {} ", self.msg)
        };
        let w = self.width();
        let mut bar: String = left.chars().take(w).collect();
        let used = bar.chars().count();
        let tail: String = right.chars().rev().take(w.saturating_sub(used)).collect();
        let tail: String = tail.chars().rev().collect();
        for _ in 0..w.saturating_sub(used + tail.chars().count()) {
            bar.push(' ');
        }
        bar.push_str(&tail);
        // Инверсии (`SGR 7`) консоль ядра не знает — берём явные цвета: синий фон, белый текст.
        // Они одинаково видны и на VGA, и в гриде терминала.
        let _ = core::fmt::Write::write_fmt(
            out,
            format_args!("\x1b[{};1H\x1b[44;97m{}\x1b[0m\x1b[K", self.rows, bar),
        );
    }

    fn draw(&mut self) {
        let (old_top, old_left) = (self.top, self.left);
        let rows = self.text_rows();
        if self.cy < self.top {
            self.top = self.cy;
        }
        if self.cy >= self.top + rows {
            self.top = self.cy + 1 - rows;
        }
        let w = self.width();
        if self.cx < self.left {
            self.left = self.cx;
        }
        if self.cx >= self.left + w {
            self.left = self.cx + 1 - w;
        }
        // Прокрутка сдвигает ВЕСЬ текст; без неё меняется только строка под курсором — остальные
        // перерисовывать незачем. Терминал и так шлёт на экран лишь изменившиеся строки, но
        // кадр 80×25 — это ещё и четыре IPC-сообщения на каждую букву.
        let full = self.full || self.top != old_top || self.left != old_left;

        let mut out = String::new();
        if full {
            for r in 0..rows {
                self.row_into(&mut out, r);
            }
        } else {
            self.row_into(&mut out, self.cy - self.top);
        }
        self.status_into(&mut out);
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!("\x1b[{};{}H", self.cy - self.top + 1, self.cx - self.left + 1),
        );
        sys::write(out.as_bytes());
        self.full = false;
        self.msg.clear();
    }
}

// ── программа ───────────────────────────────────────────────────────────────

fn fail(msg: &str) -> ! {
    sys::write(msg.as_bytes());
    sys::write(b"\n");
    sys::exit(1);
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let mut argbuf = [0u8; 512];
    let n = sys::args(&mut argbuf);
    let path = argbuf[..n]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .nth(1)
        .and_then(|s| core::str::from_utf8(s).ok())
        .map(|s| s.to_string());
    let Some(path) = path else {
        sys::write("ved: экранный редактор\n\n".as_bytes());
        sys::write("  ved <файл>   открыть (нет файла — создастся при сохранении)\n".as_bytes());
        sys::write("\nПуть относительный — от каталога шелла (`pwd`).\n".as_bytes());
        sys::exit(2);
    };
    // Относительный путь разбираем от каталога, объявленного шеллом (Веха 120.1). Без этого
    // `ved terminal.vv` после `cd /etc/system` открывал ПУСТОЙ `/terminal.vv` — и молчал об
    // этом, потому что несуществующий файл для редактора законен: это новый файл.
    let mut pbuf = [0u8; 512];
    let n = px::resolve(path.as_bytes(), &mut pbuf);
    let path = core::str::from_utf8(&pbuf[..n]).map(|s| s.to_string()).unwrap_or(path);

    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    if ep == sys::NO_CAP {
        fail("ved: нет файловой персоналии");
    }

    let mut ed = Ed {
        ep,
        lines: vec![String::new()],
        cx: 0,
        cy: 0,
        top: 0,
        left: 0,
        cols: 80,
        rows: 25,
        changed: false,
        msg: String::new(),
        quit_armed: false,
        full: true,
        done: false,
        path,
    };
    ed.measure();

    match read_file(ep, ed.path.as_bytes()) {
        Some(bytes) => {
            let Ok(text) = core::str::from_utf8(&bytes) else {
                // Открыть двоичный файл значит гарантированно испортить его при сохранении:
                // всё, что не UTF-8, редактор не покажет и обратно не соберёт.
                fail("ved: файл не UTF-8 — редактор его не откроет");
            };
            // Хвостовой перевод строки — конец последней строки, а не пустая строка после неё.
            let text = text.strip_suffix('\n').unwrap_or(text);
            ed.lines = text.split('\n').map(|l| l.to_string()).collect();
            if ed.lines.is_empty() {
                ed.lines.push(String::new());
            }
        }
        // Путь называем ПОЛНОСТЬЮ: «новый файл» без пути — это ровно тот случай, когда человек
        // думает, что открыл существующий, а открыл пустоту рядом с ним (опечатка или не тот
        // каталог). В заголовке место экономится, здесь — нет.
        None => ed.msg = format!("новый файл: {}", ed.path),
    }

    sys::write(b"\x1b[2J");
    let mut input = Input::default();
    let mut buf = [0u8; 64];
    while !ed.done {
        ed.draw();
        let n = sys::read_stdin(&mut buf);
        if n == 0 {
            break; // ввод кончился (хост ушёл) — выходим, а не крутимся впустую
        }
        for &b in &buf[..n.min(buf.len())] {
            if let Some(k) = input.feed(b) {
                ed.key(k);
                if ed.done {
                    break;
                }
            }
        }
    }

    // Экран за собой убираем: следующим здесь печатает приглашение шелл.
    sys::write(b"\x1b[0m\x1b[2J\x1b[H");
    sys::exit(0);
}
