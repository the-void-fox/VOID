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
//! ## Кадр знает свой ОБЪЁМ (Веха 148.2)
//!
//! Владелец пожаловался, что подсветка не успевает за курсором. Виновато было не рисование:
//! программа отвечала отдельным кадром на КАЖДОЕ движение мыши, а мышь шлёт их десятками в
//! секунду. Отсюда три правила, которые стоит перенять любому окну на этом тулките:
//!
//! 1. **события сгребаются пачкой** — первое ждём, остальные забираем `poll_event` до пустоты, и
//!    только потом рисуем один кадр по итоговому состоянию;
//! 2. **кадра нет там, где на экране ничего не меняется** — движение внутри одной строки списка
//!    не меняет ни пикселя, и помнить для этого достаточно номер видимой строки под курсором;
//! 3. **у кадра есть объём** ([`Need`]) — подсветка и прокрутка меняют только левую колонку,
//!    значит и рисуем, и объявляем композитору только её (`Ui::clip`).
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
use void_user::win::{self, sym, Event, Window};

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

/// Веха 148.2 — **раскладка кадра**. Считается ОДНИМ кодом и для рисования, и для попадания
/// мышью: разойдись они — и подсветка встала бы не туда, куда попадает клик (композитор на этих
/// граблях уже стоял, Веха 123).
#[derive(Default)]
struct Lay {
    head: Rect,
    foot: Rect,
    /// Вся левая колонка вместе с полосой прокрутки. Она же — клип для [`Need::List`].
    col: Rect,
    /// Место строк (колонка без полосы).
    list: Rect,
    bar: Rect,
    /// Правая половина — подробности.
    body: Rect,
    row_h: i32,
    rows: usize,
}

struct App {
    w: i32,
    h: i32,
    items: Vec<Item>,
    /// Веха 148.3 — набранное, отбор, выбор и прокрутка — общим виджетом ([`ui::List`]).
    ls: ui::List,
    detail: Option<Detail>,
    /// Для какого корня прочитаны подробности — чтобы не читать их снова на каждый кадр.
    detail_of: Option<usize>,
    /// Веха 148.3 — раскладка ПРОШЛОГО кадра. Считать её в событии больше нечем: тема и шрифт
    /// приезжают вместе с холстом, а решать «изменилось ли что-то» надо до него.
    lay: Lay,
    store: Option<usize>,
    /// Веха 150 — что было скопировано последним нажатием `Ctrl+C`. Держим ради подвала: у
    /// копирования обязан быть ВИДИМЫЙ ответ, иначе непонятно, случилось ли что-нибудь вообще.
    /// `None` — ничего не копировали (или уже нажали что-то ещё).
    copied: Option<String>,
    /// Веха 150.1 — где нажали кнопку и какой корень под ней: `(x, y, элемент)`. `None` — кнопку
    /// не держат либо нажали мимо строк.
    ///
    /// Запоминаем ТОЧКУ НАЖАТИЯ, потому что перетаскиванию нужен ПОРОГ: без него любой щелчок по
    /// строке был бы перетаскиванием, и ярлык вспыхивал бы под курсором на каждый выбор.
    press: Option<(i32, i32, usize)>,
    /// Тащим прямо сейчас — второй раз в том же нажатии не начинаем.
    dragging: bool,
}

/// Сколько пикселей руки отделяют ЩЕЛЧОК от ПРОТЯЖКИ. Рука дрожит на пару точек даже тогда, когда
/// человек уверен, что не двигал её, — поэтому порог есть у всех и везде примерно такой.
const DRAG_START: i32 = 6;

impl App {
    /// Разрезать окно на места виджетов.
    fn measure(&self, font: &Font, th: &Theme) -> Lay {
        let font_h = font.line_h();
        let row_h = font_h + th.px(10);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);
        // Поле поиска во всю ширину: список длиннее экрана всегда, и искать в нём приходится
        // чаще, чем листать.
        let head = all.cut_top(font_h + th.px(14));
        all.cut_top(th.gap);
        let foot = all.cut_bottom(font_h + th.px(6));
        let mut body = all;
        // Список — ЛЕВАЯ ТРЕТЬ, но не уже 260 и не шире 420 точек: длинные имена корней
        // (`pkg/profile/…/gen12`) в узкой колонке превращаются в многоточие, а широкая отнимает
        // место у содержимого, ради которого сюда и смотрят.
        let list_w = (body.w / 3).clamp(th.px(260), th.px(420));
        let col = body.cut_left(list_w);
        body.cut_left(th.gap);
        let mut list = col;
        let bar = list.cut_right(th.px(6));
        let rows = (list.h / row_h).max(1) as usize;
        Lay { head, foot, col, list, bar, body, row_h, rows }
    }

    /// Пересобрать список подходящих под набранное. Подстрока без учёта регистра — то же
    /// правило, что в строке запуска ([[launcher]]): два разных поиска в одной системе человек
    /// запоминать не обязан.
    fn filter(&mut self) {
        let q = self.ls.query.trim().to_lowercase();
        self.ls.hits = (0..self.items.len())
            .filter(|&i| q.is_empty() || self.items[i].name.to_lowercase().contains(&q))
            .collect();
        self.ls.refiltered();
        self.detail_of = None;
    }

    /// Прочитать подробности выбранного корня, если они ещё не прочитаны.
    fn sync_detail(&mut self) {
        let Some(i) = self.ls.current() else {
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

    /// Веха 150.1 — не пора ли начать ТАЩИТЬ выбранный корень.
    ///
    /// Груз тот же, что у `Ctrl+C`, — ИМЯ корня: имя человек и несёт в терминал, а содержимое у
    /// него перед глазами справа. Композитор откажет, если кнопку уже отпустили или курсор ушёл с
    /// нашего окна ([`win::OP_DRAG`]), и это не беда: перетаскивание просто не началось.
    fn start_drag(&mut self, ptr: Option<(i32, i32)>) {
        let (Some((px, py, i)), Some((x, y))) = (self.press, ptr) else { return };
        if self.dragging || (x - px).abs() + (y - py).abs() < DRAG_START {
            return;
        }
        let name = self.items[i].name.clone();
        self.dragging = self
            .store
            .is_some_and(|cap| win::drag(cap, win::CLIP_TEXT, name.as_bytes(), &name));
    }

    /// Нарисовать кадр. `true` — выбор сменился прямо в нём (клик по строке), и кадр надо
    /// собрать заново: подсветка и подробности считаются ДО того, как виджет ответит на клик.
    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        let font_h = u.font.line_h();
        // Окно, а не слой: фон рисуем сами (см. `Ui::background`).
        u.background(th.bg);

        u.field(lay.head, &self.ls.query, "поиск по имени корня", true);

        if let Some(t) = u.scrollbar(
            lay.bar.inset_xy(th.px(1), th.px(2)),
            self.ls.top,
            lay.rows,
            self.ls.hits.len(),
            u.held(),
        ) {
            self.ls.top = t;
        }

        let was = self.ls.sel;
        let mut list = lay.list;
        for k in 0..lay.rows {
            let rr = list.cut_top(lay.row_h);
            let Some(&i) = self.ls.hits.get(self.ls.top + k) else { continue };
            let sel = if self.ls.top + k == self.ls.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            if u.entry(rr, &self.items[i].name, "", "", sel, hot) {
                self.ls.sel = self.ls.top + k;
            }
        }
        let picked = self.ls.sel != was;

        // ── правая половина: что за корнем ────────────────────────────────────────────────
        let (body, foot) = (lay.body, lay.foot);
        // Веха 148.2 — кадр «изменился только список» до правой половины не доходит вовсе.
        // Клип отсёк бы её и сам, но не отменил бы сборку строк: `format!` на содержимое —
        // десяток выделений памяти, и платить за них ради невидимого незачем.
        if u.c.clip().intersect(body).is_empty() {
            return picked;
        }
        let inner = u.card(body);
        let mut d = inner.inset_xy(0, th.pad);
        match (self.ls.current(), &self.detail) {
            (Some(i), Some(det)) => {
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
            (Some(i), None) => {
                u.label(d.cut_top(font_h), &self.items[i].name, th.text, Align::Left);
                u.label(d.cut_top(font_h * 2), "объект не читается", th.muted, Align::Left);
            }
            _ => {
                u.label(d.cut_top(font_h), "ничего не найдено", th.muted, Align::Left);
            }
        }

        let s = match &self.copied {
            Some(name) => alloc::format!("скопировано: {}", name),
            None if self.ls.query.trim().is_empty() => {
                alloc::format!(
                    "корней в store: {}  ·  Ctrl+C — скопировать имя, мышью — перетащить",
                    self.items.len()
                )
            }
            None => alloc::format!("{} из {} корней", self.ls.hits.len(), self.items.len()),
        };
        let col = if self.copied.is_some() { th.accent } else { th.muted };
        u.label(foot, &s, col, Align::Right);
        picked
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

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            // Веха 150 — СКОПИРОВАТЬ имя выбранного корня. `Ctrl+C`, как везде; сам буфер —
            // объект store, а композитору уезжает только его content-id ([`win::clip_put`]).
            //
            // Копируем ИМЯ, а не содержимое: имя — это то, что человек понесёт в терминал
            // (`cat <имя>`), а содержимое у него и так перед глазами справа.
            Event::Key { sym: code, mods, down, .. }
                if down && code == b'c' as u16 && mods & win::modk::CTRL != 0 =>
            {
                let Some(i) = self.ls.current() else { return ui::Scope::No };
                let name = self.items[i].name.clone();
                self.copied = match self.store {
                    Some(cap) if win::clip_put(cap, win::CLIP_TEXT, name.as_bytes()) => Some(name),
                    // Права на store нет либо композитор отказал — молчать нельзя: человек
                    // нажал и вправе знать, что не вышло.
                    _ => Some(String::from("не вышло скопировать")),
                };
                ui::Scope::All
            }
            Event::Key { sym: code, ch, down, .. } if down => {
                // Любая другая клавиша снимает отметку о копировании: подвал снова про список.
                self.copied = None;
                // Escape очищает поиск, а не закрывает окно: закрытие — дело композитора
                // (`Super+Q`), и приложение, которое умирает от Escape, теряет набранное раньше,
                // чем человек успевает передумать. Всё остальное — обычная навигация списка.
                if code == sym::ESCAPE {
                    if self.ls.query.is_empty() {
                        return ui::Scope::No;
                    }
                    self.ls.query.clear();
                    self.filter();
                    return ui::Scope::All;
                }
                match self.ls.key(code, ch) {
                    ui::Hit::None => ui::Scope::No,
                    ui::Hit::Moved => ui::Scope::All,
                    ui::Hit::Query => {
                        self.filter();
                        ui::Scope::All
                    }
                }
            }
            Event::Wheel { delta, .. } => {
                if self.ls.wheel(delta) {
                    ui::Scope::Part(self.lay.col)
                } else {
                    ui::Scope::No
                }
            }
            Event::Motion { .. } => {
                // Движение с зажатой кнопкой — это либо протяжка полосы, либо начало
                // перетаскивания корня. Разбирает их место нажатия: по строке списка — тащим,
                // по полосе прокрутки — крутим ([`App::press`] заводится только над строкой).
                self.start_drag(input.ptr);
                if input.held.is_some() {
                    return ui::Scope::Part(self.lay.col);
                }
                if self.ls.motion(input.ptr) {
                    ui::Scope::Part(self.lay.col)
                } else {
                    ui::Scope::No
                }
            }
            Event::Button { x, y, down, buttons } => {
                if down && buttons & 1 != 0 {
                    // Запоминаем корень ПОД НАЖАТИЕМ, а не выбранный: выбор меняет этот же
                    // щелчок, и меняет он его кадром позже — потащили бы прошлое.
                    let p = Some((x as i32, y as i32));
                    self.press = self
                        .ls
                        .row_at(p)
                        .and_then(|k| self.ls.hits.get(self.ls.top + k).copied())
                        .map(|i| (x as i32, y as i32, i));
                    // Сбрасываем и здесь: отпускание кнопки к нам не приходит, если курсор к
                    // тому времени ушёл в чужое окно, — а именно так перетаскивание и кончается.
                    self.dragging = false;
                } else if !down {
                    self.press = None;
                    self.dragging = false;
                }
                ui::Scope::All
            }
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                self.ls.scroll_to_sel();
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let th = u.th.clone();
        self.lay = self.measure(u.font, &th);
        self.ls.measure(self.lay.list, self.lay.row_h, self.lay.rows);
        self.sync_detail();
        let lay = core::mem::take(&mut self.lay);
        let picked = self.paint(u, &th, &lay);
        self.lay = lay;
        // Выбор сменился щелчком — просим кадр ЦЕЛИКОМ: подробности справа теперь другие, а
        // считались они до того, как виджет ответил на клик.
        if picked { ui::Scope::All } else { ui::Scope::No }
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();

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

    let (w, h) = (900u16, 620u16);
    let Some(mut surf) = Window::create(w, h, "корни store") else {
        say("roots: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        items,
        ls: ui::List::default(),
        detail: None,
        detail_of: None,
        lay: Lay::default(),
        store,
        copied: None,
        press: None,
        dragging: false,
    };
    app.filter();

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}

