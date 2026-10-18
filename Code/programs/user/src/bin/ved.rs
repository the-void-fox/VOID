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
//! ## Веха 230.3 — рисует ТУЛКИТОМ (`tui/`), а не escape-кодами
//!
//! Раньше редактор складывал кадр сам: перевод курсора, стирание строки, явные цвета строки
//! состояния — и всё это с оглядкой на то, что консоль ядра знает не все коды. Теперь он рисует
//! в БУФЕР ЯЧЕЕК, а на экран уезжает разница между кадрами ([[void-ui]] — тот же порядок, что в
//! оконном тулките, только ячейки вместо пикселей).
//!
//! Видно это сразу тремя вещами: редактор въезжает на ЗАПАСНОЙ экран (лента шелла после выхода
//! остаётся цела), перерисовка перестала зависеть от того, догадался ли он, что строки
//! сдвинулись, а строка состояния взяла обратный цвет вместо подобранного синего.
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
use void_user::posix as px;

#[path = "../tui/mod.rs"]
mod tui;

use tui::{Key, Style};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок файла — тот же, что у файловой персоналии (`DATA_MAX` в `bin/posixfs`). Проверяем
/// его САМИ при сохранении: узнать о переполнении из обрезанного файла — узнать слишком поздно.
const FILE_MAX: usize = 128 * 1024;

/// Tab вставляет пробелы: конфиги поколения набраны пробелами, а показ настоящих табуляций
/// требовал бы отдельной арифметики колонок ради символа, которого мы сами не пишем.
const TAB_WIDTH: usize = 2;

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

/// Прочитать файл целиком. `None` — открыть не удалось: файла нет, и это законно — редактор
/// откроет пустой буфер и создаст его при сохранении.
///
/// Чтение, оборвавшееся НА СЕРЕДИНЕ, — не `None` и не половина файла, а отказ открывать вовсе
/// (`fail`). У редактора «половина» опаснее всего: он покажет её как весь файл, а сохранение
/// запишет её поверх целого. Тот же довод, что и у отказа открывать не-UTF-8 ниже: показать то,
/// что мы не сможем собрать обратно, — значит дать испортить файл руками владельца.
///
/// Оборваться чтение может двумя способами: `px::read` отвечает [`usize::MAX`], когда вызов не
/// состоялся вовсе (persona умерла, право отозвано), и файл может оказаться длиннее нашего
/// потолка [`FILE_MAX`] — сохранить такой мы всё равно откажемся.
fn read_file(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = px::read(ep, fd, &mut buf);
        if n == usize::MAX {
            px::close(ep, fd);
            fail("ved: файл не дочитался (файловая персоналия не отвечает) — не открываю");
        }
        if n == 0 {
            break;
        }
        if out.len() + n > FILE_MAX {
            px::close(ep, fd);
            fail("ved: файл больше потолка редактора — не открываю, чтобы не сохранить обрезок");
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
    /// Человек попросил показать заново (`^L`): в тот же терминал мог написать кто-то ещё.
    redraw: bool,
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

    /// Клавиша тулкита — в правку.
    ///
    /// Аккорды Ctrl приходят сюда КАК ЕСТЬ (`Ctrl('s')`), а не разобранными в «сохранить»:
    /// разборщик клавиш один на систему, а что значит `^S`, решает каждая программа сама.
    fn key(&mut self, k: Key) {
        if k != Key::Ctrl('q') {
            self.quit_armed = false;
        }
        match k {
            // Tab вставляет пробелы: конфиги набраны пробелами, а показ настоящих табуляций
            // требовал бы арифметики колонок ради знака, которого мы сами не пишем.
            Key::Tab => {
                for _ in 0..TAB_WIDTH {
                    self.insert(' ');
                }
            }
            Key::Char(c) => self.insert(c),
            Key::Enter => self.newline(),
            Key::Back => self.backspace(),
            Key::Del => self.delete(),
            Key::Left | Key::Ctrl('b') => self.left_key(),
            Key::Right | Key::Ctrl('f') => self.right_key(),
            Key::Up | Key::Ctrl('p') => self.vertical(-1),
            Key::Down | Key::Ctrl('n') => self.vertical(1),
            Key::PgUp => self.vertical(-(self.text_rows() as isize)),
            Key::PgDn => self.vertical(self.text_rows() as isize),
            Key::Home | Key::Ctrl('a') => self.cx = 0,
            Key::End | Key::Ctrl('e') => self.cx = self.line_len(self.cy),
            Key::Ctrl('k') => self.cut_line(),
            Key::Ctrl('s') => self.save(),
            Key::Ctrl('q') => self.quit(),
            // `^L` — «покажи заново»: кадр считается с нуля. Нужен там, где в тот же терминал
            // написал кто-то ещё (ядро своим журналом, например).
            Key::Ctrl('l') => self.redraw = true,
            _ => {}
        }
    }

    /// Выход: при несохранённом сперва предупреждаем, вторым `^Q` выходим.
    fn quit(&mut self) {
        if self.changed && !self.quit_armed {
            self.quit_armed = true;
            self.msg =
                "есть несохранённое: ^S сохранить, ^Q ещё раз — выйти без него".to_string();
        } else {
            self.done = true;
        }
    }

    // ── вывод ──

    /// Нарисовать кадр в буфер ячеек. Что изменилось с прошлого раза, считает тулкит.
    ///
    /// Прокрутка правится ЗДЕСЬ, перед рисованием: курсор мог уехать за край любой правкой, и
    /// ловить это в каждой из них — двадцать мест вместо одного.
    fn draw(&mut self, buf: &mut tui::Buffer, area: tui::Rect) -> Option<(u16, u16)> {
        self.cols = area.w as usize;
        self.rows = area.h as usize;
        let rows = self.text_rows();
        let w = self.width();
        if self.cy < self.top {
            self.top = self.cy;
        }
        if self.cy >= self.top + rows {
            self.top = self.cy + 1 - rows;
        }
        if self.cx < self.left {
            self.left = self.cx;
        }
        if self.cx >= self.left + w {
            self.left = self.cx + 1 - w;
        }

        // `cut_bottom` отдаёт ОТРЕЗАННОЕ первым — строку состояния, а остаток вторым.
        let (bar, body) = area.cut_bottom(1);
        for r in 0..body.h {
            let y = body.y + r;
            match self.lines.get(self.top + r as usize) {
                Some(line) => {
                    // Чужие табуляции показываем пробелом: свои мы не пишем (TAB_WIDTH), а
                    // ширина чужой зависит от колонки — арифметика ради знака, которого нет.
                    let shown: String = line
                        .chars()
                        .skip(self.left)
                        .take(w)
                        .map(|c| if c == '\t' { ' ' } else { c })
                        .collect();
                    buf.put(body.x, y, &shown, body.w, Style::PLAIN);
                }
                // За последней строкой файла — тусклая тильда: пустая строка файла и место, где
                // файла уже нет, должны выглядеть по-разному.
                None => buf.set(body.x, y, '~', Style::MUTED),
            }
        }

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
            String::from(" ^S сохранить  ^Q выход  ^K удалить строку ")
        } else {
            format!(" {} ", self.msg)
        };
        // Обратный цвет вместо подобранного синего: он берёт цвета самого терминала, значит
        // читается на любой теме. Консоль ядра его понимает с Вехи 230.3.
        tui::status(buf, bar, &left, &right, Style::SELECTED);
        self.msg.clear();

        Some((
            body.x + (self.cx - self.left) as u16,
            body.y + (self.cy - self.top) as u16,
        ))
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
    let argv = sys::argv::Argv::take();
    let path = argv.str(0).map(|s| s.to_string());
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
        redraw: false,
        done: false,
        path,
    };

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

    let mut app = tui::App::open();
    while !ed.done {
        let area = app.screen().area();
        let cursor = {
            let buf = app.screen().frame();
            ed.draw(buf, area)
        };
        // Курсор ставим туда, где человек печатает: в редакторе это единственное, что говорит,
        // куда поедет следующий знак.
        app.screen().cursor(cursor);
        app.screen().flush();
        if ed.redraw {
            ed.redraw = false;
            app.screen().repaint();
            continue;
        }
        match app.next() {
            tui::Event::Key(k) => ed.key(k),
            // Окно стало другим: кадр считается заново — переносы, строка состояния и прокрутка
            // все считаются от размера. Экран тулкит уже перемерил.
            tui::Event::Resized => {}
            // Прервали. У редактора это не «умереть»: несохранённое дороже. Ведём себя как с
            // обычным выходом — предупредим, если есть что терять.
            tui::Event::Interrupted => ed.quit(),
            // Ввод кончился (хост ушёл) — выходим, а не крутимся впустую.
            tui::Event::Eof => break,
        }
    }

    // Экран за собой убираем и возвращаем ленту шелла — запасной экран для того и нужен.
    app.close();
    sys::exit(0);
}
