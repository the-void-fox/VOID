//! `docs` — РУКОВОДСТВО СИСТЕМЫ ОКНОМ (Веха 223.2).
//!
//! ## Почему окно, если есть команда
//!
//! Потому что это разные способы читать, а не один с украшением. `doc caps` отвечает на вопрос,
//! который уже задан: человек знает слово и хочет строку. Окно отвечает на вопрос, которого ещё
//! нет: «а что тут вообще есть» — и на него отвечает список слева, а не команда.
//!
//! Владелец назвал и образец раскладки: devdocs. Слева поиск и перечень, справа текст, и больше
//! ничего — ни вкладок, ни панелей инструментов. Читать — единственное, что здесь делают.
//!
//! ## Разметка разбирается общим кодом, а укладывается своим
//!
//! Разбор у окна и терминала один ([`void_md::parse`]), и это важно: разойдись они, один и тот же
//! текст означал бы в двух местах разное. А вот укладка у каждого своя, и иначе нельзя — терминал
//! считает ширину в знаках, окно в точках, и шрифт у него пропорциональный. Общая укладка значила
//! бы, что окно показывает текст, разложенный под чужую меру.
//!
//! ## Что здесь НЕ делается
//!
//! Ничего, кроме чтения: ни ссылок (их некуда вести — браузера в системе нет), ни правки (тексты
//! приезжают с образом и обновляются вместе с ядром), ни закладок.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{sym, Event, Window};

#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
#[allow(dead_code)]
#[path = "../roots.rs"]
mod roots;

use ui::{Align, Font, Rect, Theme, Ui};

/// Кадр окна плюс разобранный текст. Руководство — десятки килобайт, кадр — мегабайты.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 16 * 1024 * 1024 }> = sys::heap::Heap::new();

fn say(s: &str) {
    sys::write_console(s.as_bytes());
}

/// Вид строки на экране. От него зависит и шрифт, и цвет, и отступ.
#[derive(Clone, Copy, PartialEq)]
enum Вид {
    /// Заголовок верхнего уровня — акцентом.
    Глава,
    /// Подзаголовок.
    Раздел,
    Текст,
    /// Строка блока кода: своя подложка на всю ширину.
    Код,
    /// Ячейка таблицы: рисуется по своей колонке.
    Ячейка(usize),
}

/// Готовая к рисованию строка: текст, вид и отступ слева в точках.
struct Строка {
    текст: String,
    вид: Вид,
    отступ: i32,
}

/// Раздел руководства: имя корня и его текст (читается при первом открытии).
struct Раздел {
    имя: String,
    блоки: Option<Vec<void_md::Block>>,
}

#[derive(Default)]
struct Lay {
    head: Rect,
    /// Вся левая колонка вместе с полосой.
    col: Rect,
    list: Rect,
    list_bar: Rect,
    /// Текст справа и его полоса.
    body: Rect,
    body_bar: Rect,
    row_h: i32,
    rows: usize,
}

struct App {
    w: i32,
    h: i32,
    разделы: Vec<Раздел>,
    ls: ui::List,
    /// Разложенные строки текущего раздела и ширина, под которую их раскладывали.
    строки: Vec<Строка>,
    /// Первая видимая строка текста. Прокрутка здесь СТРОКАМИ, а не точками: полоса тулкита
    /// считает ряды, и вторая мера рядом с ней — лишний повод им разойтись.
    верх: usize,
    разложено_для: Option<(usize, i32)>,
    /// Ширины колонок таблиц — по одной записи на каждую таблицу, в порядке встречи.
    колонки: Vec<Vec<i32>>,
    lay: Lay,
    store: Option<usize>,
}

impl App {
    fn measure(&self, font: &Font, th: &Theme) -> Lay {
        let font_h = font.line_h();
        let row_h = font_h + th.px(10);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(th.pad);
        let head = all.cut_top(font_h + th.px(14));
        all.cut_top(th.gap);
        let mut body = all;
        // Перечень — левая колонка, как в devdocs. Шире 320 не нужна: имена разделов короткие,
        // а место нужно тексту, ради которого сюда и смотрят.
        let list_w = (body.w / 4).clamp(th.px(180), th.px(320));
        let col = body.cut_left(list_w);
        body.cut_left(th.gap);
        let mut list = col;
        let list_bar = list.cut_right(th.px(6));
        let mut текст = body;
        let body_bar = текст.cut_right(th.px(6));
        let rows = (list.h / row_h).max(1) as usize;
        Lay { head, col, list, list_bar, body: текст, body_bar, row_h, rows }
    }

    fn filter(&mut self) {
        let q = self.ls.query.to_lowercase();
        self.ls.hits = (0..self.разделы.len())
            .filter(|&i| q.is_empty() || self.разделы[i].имя.to_lowercase().contains(&q))
            .collect();
        self.ls.refiltered();
    }

    /// Прочитать текст раздела из store и разобрать. Делается ОДИН раз на раздел: разбор это
    /// проход по тексту, и повторять его на каждый кадр не за что.
    fn загрузить(&mut self, i: usize) {
        if self.разделы[i].блоки.is_some() {
            return;
        }
        let Some(scap) = self.store else { return };
        let корень = alloc::format!("doc/{}", self.разделы[i].имя);
        let mut id = [0u8; 32];
        if sys::obj_get_root(scap, корень.as_bytes(), &mut id) != 32 {
            return;
        }
        let mut buf = alloc::vec![0u8; 128 * 1024];
        let (got, full) = sys::obj_get_ex(scap, &id, &mut buf);
        if got == 0 || got != full {
            return;
        }
        buf.truncate(got);
        if let Ok(t) = String::from_utf8(buf) {
            self.разделы[i].блоки = Some(void_md::parse(&t));
        }
    }

    /// Разложить блоки выбранного раздела под ширину `w` точек.
    ///
    /// Считается при смене раздела или ширины окна, а не каждый кадр: перенос по словам требует
    /// мерить КАЖДОЕ слово шрифтом, и делать это шестьдесят раз в секунду — чистый убыток.
    fn разложить(&mut self, u: &mut Ui, w: i32) {
        let Some(i) = self.ls.current() else { return };
        if self.разложено_для == Some((i, w)) {
            return;
        }
        self.загрузить(i);
        self.строки.clear();
        self.колонки.clear();
        self.верх = 0;
        self.разложено_для = Some((i, w));
        let Some(блоки) = self.разделы[i].блоки.take() else { return };
        let отступ_пункта = u.text_w("— ");
        for b in &блоки {
            match b {
                void_md::Block::Heading(уровень, t) => {
                    // Пустая строка перед заголовком — воздух, которым разделы и отличаются
                    // друг от друга. Кроме самого первого: сверху и так поле.
                    if !self.строки.is_empty() {
                        self.пустая();
                    }
                    let вид = if *уровень <= 1 { Вид::Глава } else { Вид::Раздел };
                    self.строки.push(Строка { текст: снять(t), вид, отступ: 0 });
                }
                void_md::Block::Para(t) => {
                    self.пустая();
                    self.перенести(u, &снять(t), w, 0, Вид::Текст);
                }
                void_md::Block::List(пункты) => {
                    self.пустая();
                    for p in пункты {
                        let было = self.строки.len();
                        self.перенести(u, &снять(p), w - отступ_пункта, отступ_пункта, Вид::Текст);
                        // Знак пункта — на первой строке, продолжение под текстом.
                        if let Some(s) = self.строки.get_mut(было) {
                            s.текст.insert_str(0, "— ");
                            s.отступ = 0;
                        }
                    }
                }
                void_md::Block::Code(строки) => {
                    self.пустая();
                    for s in строки {
                        self.строки.push(Строка {
                            текст: s.clone(),
                            вид: Вид::Код,
                            отступ: u.text_w("  "),
                        });
                    }
                }
                void_md::Block::Table { head, rows: ряды } => {
                    self.пустая();
                    let столбцов = ряды.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut ширины = alloc::vec![0i32; столбцов];
                    for r in ряды {
                        for (k, c) in r.iter().enumerate() {
                            ширины[k] = ширины[k].max(u.text_w(&снять(c)));
                        }
                    }
                    let зазор = u.text_w("    ");
                    let мера: i32 =
                        ширины.iter().sum::<i32>() + зазор * (столбцов as i32 - 1).max(0);
                    if столбцов == 0 {
                        continue;
                    }
                    if мера <= w {
                        // Влезает — колонками, как в исходнике.
                        let n = self.колонки.len();
                        self.колонки.push(ширины);
                        for r in ряды {
                            for (k, c) in r.iter().enumerate() {
                                self.строки.push(Строка {
                                    текст: снять(c),
                                    вид: Вид::Ячейка(n),
                                    отступ: k as i32,
                                });
                            }
                            // Конец ряда: пустая ячейка с отрицательным номером столбца.
                            self.строки.push(Строка {
                                текст: String::new(),
                                вид: Вид::Ячейка(n),
                                отступ: -1,
                            });
                        }
                    } else {
                        // Веха 223.2 — НЕ ВЛЕЗАЕТ — значит не таблица, а пары.
                        //
                        // Резать колонку многоточием здесь нельзя: в таблице руководства второй
                        // столбец и есть объяснение, ради которого строку читают, и обрезать его
                        // значит выбросить смысл. Окно у нас плиточное, половина экрана — обычная
                        // ширина, и таблица в неё не влезает почти никогда.
                        //
                        // Пара читается в любой ширине: первый столбец строкой, остальные —
                        // абзацем под ним с отступом. Шапку пропускаем: подписи к колонкам без
                        // колонок — пара без смысла.
                        let отступ = u.text_w("    ");
                        for r in ряды.iter().skip(if *head { 1 } else { 0 }) {
                            let Some(ключ) = r.first() else { continue };
                            self.строки.push(Строка {
                                текст: снять(ключ),
                                вид: Вид::Раздел,
                                отступ: 0,
                            });
                            let хвост: Vec<String> =
                                r.iter().skip(1).map(|c| снять(c)).filter(|c| !c.is_empty()).collect();
                            if !хвост.is_empty() {
                                let текст = хвост.join(" · ");
                                self.перенести(u, &текст, w - отступ, отступ, Вид::Текст);
                            }
                        }
                    }
                }
            }
        }
        self.разделы[i].блоки = Some(блоки);
    }

    fn пустая(&mut self) {
        if !self.строки.is_empty() {
            self.строки.push(Строка { текст: String::new(), вид: Вид::Текст, отступ: 0 });
        }
    }

    /// Перенос абзаца ПО СЛОВАМ под ширину в точках. Шрифт пропорциональный, поэтому мерить
    /// приходится каждое слово — знаков тут не посчитаешь.
    fn перенести(&mut self, u: &mut Ui, s: &str, w: i32, отступ: i32, вид: Вид) {
        let пробел = u.text_w(" ");
        let mut строка = String::new();
        let mut ширина = 0;
        for слово in s.split_whitespace() {
            let ws = u.text_w(слово);
            if !строка.is_empty() && ширина + пробел + ws > w {
                self.строки.push(Строка { текст: core::mem::take(&mut строка), вид, отступ });
                ширина = 0;
            }
            if !строка.is_empty() {
                строка.push(' ');
                ширина += пробел;
            }
            строка.push_str(слово);
            ширина += ws;
        }
        if !строка.is_empty() {
            self.строки.push(Строка { текст: строка, вид, отступ });
        }
    }

    /// Шаг строки текста.
    fn шаг(&self, u: &Ui) -> i32 {
        u.font.line_h() + u.th.px(4)
    }

    /// Последняя строка, с которой ещё есть что показать.
    fn последняя(&self) -> usize {
        let видно = (self.lay.body.h / self.lay.row_h.max(1)).max(1) as usize;
        self.строки.len().saturating_sub(видно)
    }

    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.background(th.bg);
        u.field(lay.head, &self.ls.query, ui::t("поиск по разделам"), true);

        // ── перечень слева ──
        if let Some(t) = u.scrollbar_from(
            lay.list_bar.inset_xy(th.px(1), th.px(2)),
            self.ls.top,
            lay.rows,
            self.ls.hits.len(),
            u.held(),
            None,
        ) {
            self.ls.top = t;
        }
        let было = self.ls.sel;
        let mut list = lay.list;
        for k in 0..lay.rows {
            let rr = list.cut_top(lay.row_h);
            let Some(&i) = self.ls.hits.get(self.ls.top + k) else { continue };
            let sel = if self.ls.top + k == self.ls.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            if u.entry(rr, &self.разделы[i].имя, "", "", sel, hot) {
                self.ls.sel = self.ls.top + k;
            }
        }

        // ── текст справа ──
        let шаг = self.шаг(u);
        let видно = (lay.body.h / шаг).max(1) as usize;
        if let Some(t) = u.scrollbar(
            lay.body_bar.inset_xy(th.px(1), th.px(2)),
            self.верх,
            видно,
            self.строки.len(),
            u.held(),
        ) {
            self.верх = t;
        }
        u.clip(lay.body);
        let первая = self.верх;
        let mut y = lay.body.y;
        let mut x_ряда = 0i32;
        for s in self.строки.iter().skip(первая).take(видно) {
            let r = Rect::new(lay.body.x + s.отступ.max(0), y, lay.body.w - s.отступ.max(0), шаг);
            match s.вид {
                Вид::Глава => {
                    u.label(r, &s.текст, th.accent, Align::Left);
                    y += шаг;
                    x_ряда = 0;
                }
                Вид::Раздел => {
                    u.label(r, &s.текст, th.text, Align::Left);
                    // Черта под подзаголовком: заголовок иначе теряется в сплошном тексте.
                    let mut ч = Rect::new(lay.body.x, y + шаг - th.px(2), lay.body.w, th.px(1));
                    ч.w = lay.body.w;
                    u.c.fill(ч, u.tint(th.border));
                    y += шаг;
                    x_ряда = 0;
                }
                Вид::Текст => {
                    if !s.текст.is_empty() {
                        u.label(r, &s.текст, th.text, Align::Left);
                    }
                    y += шаг;
                    x_ряда = 0;
                }
                Вид::Код => {
                    let подложка = u.tint(th.band);
                    u.c.fill(Rect::new(lay.body.x, y, lay.body.w, шаг), подложка);
                    u.label(r, &s.текст, th.muted, Align::Left);
                    y += шаг;
                    x_ряда = 0;
                }
                Вид::Ячейка(n) => {
                    if s.отступ < 0 {
                        y += шаг;
                        x_ряда = 0;
                        continue;
                    }
                    let k = s.отступ as usize;
                    let ширины = &self.колонки[n];
                    let w = ширины.get(k).copied().unwrap_or(0);
                    let цвет = if k == 0 { th.text } else { th.muted };
                    u.label(Rect::new(lay.body.x + x_ряда, y, w, шаг), &s.текст, цвет, Align::Left);
                    x_ряда += w + u.text_w("    ");
                }
            }
        }
        u.clip(Rect::new(0, 0, self.w, self.h));
        self.ls.sel != было
    }
}

/// Снять знаки разметки, которые рисовать нечем (обратные кавычки, звёздочки жирного).
fn снять(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '`' => {}
            '*' if it.peek() == Some(&'*') => {
                it.next();
            }
            _ => out.push(c),
        }
    }
    out
}

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            Event::Resize { w, h, .. } => {
                self.w = w as i32;
                self.h = h as i32;
                // Ширина сменилась — перенос по словам стал другим.
                self.разложено_для = None;
                ui::Scope::All
            }
            Event::Key { sym: code, ch, down, .. } if down => {
                if code == sym::ESCAPE {
                    if self.ls.query.is_empty() {
                        return ui::Scope::No;
                    }
                    self.ls.query.clear();
                    self.filter();
                    self.разложено_для = None;
                    return ui::Scope::All;
                }
                match self.ls.key(code, ch) {
                    ui::Hit::None => ui::Scope::No,
                    ui::Hit::Moved => {
                        // Выбрали другой раздел — текст справа другой.
                        self.разложено_для = None;
                        ui::Scope::All
                    }
                    ui::Hit::Query => {
                        self.filter();
                        self.разложено_для = None;
                        ui::Scope::All
                    }
                }
            }
            Event::Wheel { delta, .. } => {
                // Колесо над перечнем крутит перечень, над текстом — текст. Иначе длинный
                // раздел нечем читать: курсор стоит там, куда смотрят.
                let над_списком =
                    input.ptr.map_or(false, |p| self.lay.col.contains(p.0, p.1));
                let было = (self.ls.top, self.верх);
                if над_списком {
                    self.ls.wheel(delta);
                } else {
                    let шаг = ui::Scroll::STEP_ROWS as usize;
                    self.верх = if delta > 0 {
                        self.верх.saturating_sub(шаг)
                    } else {
                        (self.верх + шаг).min(self.последняя())
                    };
                }
                if (self.ls.top, self.верх) == было {
                    ui::Scope::No
                } else {
                    ui::Scope::All
                }
            }
            _ => ui::Scope::No,
        }
    }

    /// Веха 151 — чем вернуть окно после перезагрузки: разделом, на котором читали.
    fn persist(&mut self) -> Option<String> {
        Some(match self.ls.current().map(|i| self.разделы[i].имя.as_str()) {
            Some(имя) => alloc::format!("docs {}", имя),
            None => String::from("docs"),
        })
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let th = u.th.clone();
        self.lay = self.measure(u.font, &th);
        self.ls.measure(self.lay.list, self.lay.row_h, self.lay.rows);
        let ширина = self.lay.body.w;
        self.разложить(u, ширина);
        let lay = core::mem::take(&mut self.lay);
        let сменили = self.paint(u, &th, &lay);
        self.lay = lay;
        if сменили {
            self.разложено_для = None;
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();
    let store = ui::conf::store_cap();

    // Разделы берутся из store, а не из таблицы здесь: они приезжают с образом, и знать их
    // наперёд окну неоткуда — ровно как команде `doc`.
    let mut разделы: Vec<Раздел> = Vec::new();
    if let Some(cap) = store {
        if let Some(list) = roots::text(cap) {
            for имя in roots::suffixes(&list, b"doc/") {
                if let Ok(s) = core::str::from_utf8(имя) {
                    разделы.push(Раздел { имя: s.to_string(), блоки: None });
                }
            }
        }
    }
    // Веха 223.2 — ОБЗОР ПЕРВЫМ, остальное по алфавиту. Руководство открывают не затем, чтобы
    // попасть на раздел, чьё имя раньше по азбуке: первым должен стоять тот, с которого читают.
    разделы.sort_by(|a, b| {
        let вес = |имя: &str| if имя == "void" { 0 } else { 1 };
        вес(&a.имя).cmp(&вес(&b.имя)).then_with(|| a.имя.cmp(&b.имя))
    });
    if разделы.is_empty() {
        say("docs: разделов руководства нет (права на store?)\n");
    }

    let (w, h) = (960u16, 680u16);
    let Some(mut surf) = Window::create(w, h, ui::t("руководство")) else {
        say("docs: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        разделы,
        ls: ui::List::default(),
        строки: Vec::new(),
        разложено_для: None,
        колонки: Vec::new(),
        верх: 0,
        lay: Lay::default(),
        store,
    };
    app.filter();
    // Открыться на названном разделе — так нас возвращает сеанс и так же может позвать человек.
    if let Some(имя) = sys::argv::Argv::take().str(0) {
        if let Some(k) = app.ls.hits.iter().position(|&i| app.разделы[i].имя == имя) {
            app.ls.sel = k;
            app.ls.scroll_to_sel();
        }
    }
    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
