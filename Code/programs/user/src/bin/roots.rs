//! `roots` — ВЬЮВЕР КОРНЕЙ STORE (Веха 148): первое родное приложение VOID.
//!
//! Слева список именованных корней, справа — что за ними стоит: content-id целиком, размер,
//! число частей у дерева и первые строки самого содержимого. Набранное фильтрует список,
//! колесо и клавиши его крутят.
//!
//! ## Почему именно оно первым
//!
//! План ([[gui-plan]]) предлагал файловый менеджер или вьювер корней, владелец выбрал второе.
//! Выбор оказался точным дважды. **Он показывает тезис системы**: в VOID нет файлов, есть
//! объекты, адресуемые содержимым, и именованные корни поверх них — а увидеть это до сих пор
//! было негде, кроме печати в консоль. И **он нагружает тулкит по-настоящему**: список длиннее
//! окна, прокрутка, выделение, поиск, две колонки, которые делят место между собой.
//!
//! ## Чего не хватило и что пришлось добавить
//!
//! - **колеса мыши в протоколе окон.** Композитор забирал его себе всегда, хотя ещё с Вехи 123.1
//!   было решено «без Super колесо принадлежит программе». Первый же длинный список это и
//!   обнаружил — `win::EV_WHEEL`;
//! - **полосы прокрутки в тулките** (`Ui::scrollbar`): список, у которого не видно, велик он или
//!   мал, заставляет человека угадывать.
//!
//! ## Чего он НЕ делает
//!
//! Не меняет ничего. Ни снять корень, ни переименовать, ни собрать мусор — только смотреть.
//! Право на store у него общее (`READ`), и первое приложение системы не должно быть тем, что
//! чинит store; а `WRITE` из окна — это отдельный разговор о подтверждениях и об отмене.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{Event, Window};

#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
#[allow(dead_code)]
#[path = "../roots.rs"]
mod roots;

use ui::{Align, Font, Rect, Theme, Ui};

/// Кадр окна плюс прочитанный кусок объекта: и то, и другое — мегабайты в худшем случае.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 16 * 1024 * 1024 }> = sys::heap::Heap::new();

const SYM_ESCAPE: u16 = 0x102;
const SYM_BACKSPACE: u16 = 0x104;
const SYM_UP: u16 = 0x112;
const SYM_DOWN: u16 = 0x113;
const SYM_HOME: u16 = 0x114;
const SYM_END: u16 = 0x115;
const SYM_PGUP: u16 = 0x116;
const SYM_PGDN: u16 = 0x117;

/// Сколько байт объекта читаем ради превью. Больше незачем: показываем мы всё равно экран, а
/// корень программы это мегабайты — тянуть их целиком значит платить за то, чего не покажем.
const PREVIEW: usize = 4096;

fn say(s: &str) {
    sys::write_console(s.as_bytes());
}

/// Один корень store.
struct Item {
    name: String,
}

/// Что удалось узнать о выбранном корне. Читается при СМЕНЕ выбора, а не каждый кадр: чтение
/// объекта — это syscall и копия куска, и делать её шестьдесят раз в секунду не за что.
struct Detail {
    id: [u8; 32],
    /// Настоящая длина объекта (не то, сколько мы прочитали).
    size: usize,
    /// Сколько у него частей: `0` — обычное значение, больше — дерево (Веха 94).
    parts: usize,
    /// Готовые строки превью и признак «это не текст».
    lines: Vec<String>,
    binary: bool,
}

struct App {
    w: i32,
    h: i32,
    items: Vec<Item>,
    hits: Vec<usize>,
    query: String,
    sel: usize,
    top: usize,
    detail: Option<Detail>,
    /// Для какого корня прочитаны подробности — чтобы не читать их снова на каждый кадр.
    detail_of: Option<usize>,
    ptr: Option<(i32, i32)>,
    click: Option<(i32, i32)>,
    store: Option<usize>,
}

impl App {
    /// Высота строки списка.
    fn row_h(&self, font: &Font, th: &Theme) -> i32 {
        font.line_h() + th.px(10)
    }

    /// Сколько строк видно разом.
    fn rows(&self, font: &Font, th: &Theme) -> usize {
        let head = font.line_h() + th.px(14) + th.gap;
        let foot = font.line_h() + th.px(6);
        ((self.h - 2 * th.pad - head - foot) / self.row_h(font, th)).max(1) as usize
    }

    /// Пересобрать список подходящих под набранное. Подстрока без учёта регистра — то же
    /// правило, что в строке запуска ([[launcher]]): два разных поиска в одной системе человек
    /// запоминать не обязан.
    fn filter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.hits = (0..self.items.len())
            .filter(|&i| q.is_empty() || self.items[i].name.to_lowercase().contains(&q))
            .collect();
        self.sel = 0;
        self.top = 0;
        self.detail_of = None;
    }

    /// Держать выбранное в видимой части списка.
    fn scroll_to_sel(&mut self, rows: usize) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
    }

    /// Прочитать подробности выбранного корня, если они ещё не прочитаны.
    fn sync_detail(&mut self) {
        let Some(&i) = self.hits.get(self.sel) else {
            self.detail = None;
            self.detail_of = None;
            return;
        };
        if self.detail_of == Some(i) {
            return;
        }
        self.detail_of = Some(i);
        self.detail = self.store.and_then(|cap| read_detail(cap, &self.items[i].name));
    }

    fn draw(&mut self, u: &mut Ui, th: &Theme) {
        let font_h = u.font.line_h();
        let rows = self.rows(&*u.font, th);
        let row_h = self.row_h(&*u.font, th);
        // Окно, а не слой: фон рисуем сами (см. `Ui::background`).
        u.background(th.bg);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);

        // Поле поиска во всю ширину: список длиннее экрана всегда, и искать в нём приходится
        // чаще, чем листать.
        let head = all.cut_top(font_h + th.px(14));
        u.field(head, &self.query, "поиск по имени корня", true);
        all.cut_top(th.gap);

        let foot = all.cut_bottom(font_h + th.px(6));
        let mut body = all;
        // Список — ЛЕВАЯ ТРЕТЬ, но не уже 260 и не шире 420 точек: длинные имена корней
        // (`pkg/profile/…/gen12`) в узкой колонке превращаются в многоточие, а широкая отнимает
        // место у содержимого, ради которого сюда и смотрят.
        let list_w = (body.w / 3).clamp(th.px(260), th.px(420));
        let mut list = body.cut_left(list_w);
        body.cut_left(th.gap);

        let bar = list.cut_right(th.px(6));
        u.scrollbar(bar.inset_xy(th.px(1), th.px(2)), self.top, rows, self.hits.len());

        for k in 0..rows {
            let rr = list.cut_top(row_h);
            let Some(&i) = self.hits.get(self.top + k) else { continue };
            let sel = if self.top + k == self.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            if u.entry(rr, &self.items[i].name, "", "", sel, hot) {
                self.sel = self.top + k;
            }
        }

        // ── правая половина: что за корнем ────────────────────────────────────────────────
        let inner = u.card(body);
        let mut d = inner.inset_xy(0, th.pad);
        match (self.hits.get(self.sel), &self.detail) {
            (Some(&i), Some(det)) => {
                u.label(d.cut_top(font_h + th.px(4)), &self.items[i].name, th.text, Align::Left);
                d.cut_top(th.px(6));
                // Content-id ЦЕЛИКОМ и в две строки: он и есть настоящее имя объекта, и показать
                // его огрызком значило бы соврать о том, чем система адресует содержимое.
                let hex = hex64(&det.id);
                u.label(d.cut_top(font_h), "content-id", th.muted, Align::Left);
                u.label(d.cut_top(font_h), &hex[..32], th.text, Align::Left);
                u.label(d.cut_top(font_h + th.px(4)), &hex[32..], th.text, Align::Left);
                u.row(d.cut_top(font_h + th.px(2)), "размер", &human(det.size));
                if det.parts > 0 {
                    let s = alloc::format!("{} частей", det.parts);
                    u.row(d.cut_top(font_h + th.px(2)), "дерево", &s);
                }
                d.cut_top(th.px(6));
                u.hsep(d.cut_top(th.px(6)));
                d.cut_top(th.px(4));
                let title = if det.binary { "первые байты" } else { "начало содержимого" };
                u.label(d.cut_top(font_h), title, th.muted, Align::Left);
                for line in det.lines.iter() {
                    if d.h < font_h {
                        break;
                    }
                    u.label(d.cut_top(font_h), line, th.text, Align::Left);
                }
            }
            (Some(&i), None) => {
                u.label(d.cut_top(font_h), &self.items[i].name, th.text, Align::Left);
                u.label(d.cut_top(font_h * 2), "объект не читается", th.muted, Align::Left);
            }
            _ => {
                u.label(d.cut_top(font_h), "ничего не найдено", th.muted, Align::Left);
            }
        }

        let s = if self.query.trim().is_empty() {
            alloc::format!("корней в store: {}", self.items.len())
        } else {
            alloc::format!("{} из {} корней", self.hits.len(), self.items.len())
        };
        u.label(foot, &s, th.muted, Align::Right);
    }
}

/// Прочитать подробности корня: content-id, размер, части, начало содержимого.
fn read_detail(cap: usize, name: &str) -> Option<Detail> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(cap, name.as_bytes(), &mut id) != 32 {
        return None;
    }
    let mut buf = vec![0u8; PREVIEW];
    let (got, want) = sys::obj_get_ex(cap, &id, &mut buf);
    if got > buf.len() {
        return None;
    }
    buf.truncate(got);
    let mut kids = [[0u8; 32]; 4];
    let parts = match sys::obj_children(cap, &id, &mut kids) {
        n if n == usize::MAX => 0,
        n => n,
    };
    let binary = !looks_text(&buf);
    let lines = if binary { hex_dump(&buf) } else { text_lines(&buf) };
    Some(Detail { id, size: want.max(got), parts, lines, binary })
}

/// Похоже ли на текст: печатаемых байт хотя бы девять из десяти. Мера грубая намеренно —
/// точного ответа не существует, а ошибка стоит ровно одного неудобного превью.
fn looks_text(b: &[u8]) -> bool {
    if b.is_empty() {
        return true;
    }
    let good = b
        .iter()
        .filter(|&&c| c == b'\n' || c == b'\r' || c == b'\t' || (0x20..0x7f).contains(&c) || c >= 0xc0 || (0x80..0xc0).contains(&c))
        .count();
    good * 10 >= b.len() * 9
}

/// Первые строки текста, обрезанные по длине: длинная строка конфига не должна уезжать за край.
fn text_lines(b: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(b);
    s.lines()
        .take(24)
        .map(|l| {
            let mut t: String = l.chars().take(64).collect();
            if l.chars().count() > 64 {
                t.push('…');
            }
            t
        })
        .collect()
}

/// Классический дамп: адрес, шестнадцать байт, они же буквами.
fn hex_dump(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    // По ВОСЕМЬ байт в строке, а не по шестнадцать: шестнадцать не помещаются в колонку
    // подробностей ни при каком разумном кегле, и строка обрезается ровно там, где начинается
    // самое читаемое — буквы справа.
    for (n, chunk) in b.chunks(8).take(20).enumerate() {
        let mut line = alloc::format!("{:04x}  ", n * 8);
        for byte in chunk.iter() {
            line.push_str(&alloc::format!("{:02x} ", byte));
        }
        for _ in chunk.len()..8 {
            line.push_str("   ");
        }
        line.push(' ');
        for &byte in chunk {
            line.push(if (0x20..0x7f).contains(&byte) { byte as char } else { '.' });
        }
        out.push(line);
    }
    out
}

/// Content-id строкой в 64 знака.
fn hex64(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in id {
        s.push_str(&alloc::format!("{:02x}", b));
    }
    s
}

/// Размер по-человечески. Байты до килобайта — точные: у объектов store они говорящие.
fn human(n: usize) -> String {
    if n < 1024 {
        alloc::format!("{} Б", n)
    } else if n < 1024 * 1024 {
        alloc::format!("{}.{} КиБ", n / 1024, n % 1024 * 10 / 1024)
    } else {
        alloc::format!("{}.{} МиБ", n / (1024 * 1024), n % (1024 * 1024) * 10 / (1024 * 1024))
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let generation = ui::conf::generation().unwrap_or_default();
    let th = Theme::from_config(&generation);
    let mut font = Font::load(th.font.as_deref(), th.font_px);

    let store = ui::conf::store_cap();
    let mut items: Vec<Item> = Vec::new();
    if let Some(cap) = store {
        if let Some(text) = roots::text(cap) {
            for line in text.split(|&b| b == b'\n') {
                if line.len() <= 14 {
                    continue;
                }
                let Ok(name) = core::str::from_utf8(&line[14..]) else { continue };
                items.push(Item { name: name.to_string() });
            }
        }
    }
    if items.is_empty() {
        say("roots: корни store не читаются — права на store нет\n");
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));

    let (mut w, mut h) = (900u16, 620u16);
    let Some(mut surf) = Window::create(w, h, "корни store") else {
        say("roots: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        items,
        hits: Vec::new(),
        query: String::new(),
        sel: 0,
        top: 0,
        detail: None,
        detail_of: None,
        ptr: None,
        click: None,
        store,
    };
    app.filter();

    let mut redraw = true;
    loop {
        if redraw {
            app.sync_detail();
            let (fw, fh) = (app.w, app.h);
            let mut u = Ui::new(surf.pixels(), fw, fh, &th, &mut font);
            u.input(app.ptr, app.click);
            app.draw(&mut u, &th);
            let d = u.dirty();
            if !d.is_empty() {
                surf.damage(d.x as u16, d.y as u16, d.w as u16, d.h as u16);
            }
            app.click = None;
            redraw = false;
        }

        let Some(ev) = surf.next_event() else { continue };
        let rows = app.rows(&font, &th);
        match ev {
            Event::Key { sym, ch, down, .. } if down => {
                redraw = true;
                let last = app.hits.len().saturating_sub(1);
                match sym {
                    SYM_UP => app.sel = app.sel.saturating_sub(1),
                    SYM_DOWN => app.sel = (app.sel + 1).min(last),
                    SYM_PGUP => app.sel = app.sel.saturating_sub(rows),
                    SYM_PGDN => app.sel = (app.sel + rows).min(last),
                    SYM_HOME => app.sel = 0,
                    SYM_END => app.sel = last,
                    SYM_BACKSPACE => {
                        app.query.pop();
                        app.filter();
                    }
                    // Escape очищает поиск, а не закрывает окно: закрытие — дело композитора
                    // (`Super+Q`), и приложение, которое умирает от Escape, теряет набранное
                    // раньше, чем человек успевает передумать.
                    SYM_ESCAPE => {
                        app.query.clear();
                        app.filter();
                    }
                    _ => match char::from_u32(ch as u32) {
                        Some(c) if !c.is_control() => {
                            app.query.push(c);
                            app.filter();
                        }
                        _ => redraw = false,
                    },
                }
                app.scroll_to_sel(rows);
            }
            // Веха 148 — колесо крутит СПИСОК, а не выбор: выбранное остаётся на месте, пока
            // человек смотрит, что рядом. Так же ведут себя списки везде, где их крутят мышью.
            Event::Wheel { delta, .. } => {
                let step = 3usize;
                let max = app.hits.len().saturating_sub(rows);
                app.top = if delta > 0 {
                    app.top.saturating_sub(step)
                } else {
                    (app.top + step).min(max)
                };
                redraw = true;
            }
            Event::Motion { x, y } => {
                app.ptr = if x == 0xffff { None } else { Some((x as i32, y as i32)) };
                redraw = true;
            }
            Event::Button { x, y, down, .. } if down => {
                app.click = Some((x as i32, y as i32));
                redraw = true;
            }
            Event::Resize { w: nw, h: nh } => {
                if (nw, nh) != (w, h) && surf.resize_buf(nw, nh) {
                    (w, h) = (nw, nh);
                    app.w = nw as i32;
                    app.h = nh as i32;
                    app.scroll_to_sel(app.rows(&font, &th));
                    redraw = true;
                }
            }
            Event::Close => {
                surf.destroy();
                sys::exit(0);
            }
            _ => {}
        }
    }
}
