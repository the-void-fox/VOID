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

/// Вид строки текста. От него зависит и цвет, и подложка, и черта под ней.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Заголовок верхнего уровня — акцентом.
    Head,
    /// Подзаголовок.
    Part,
    Text,
    /// Строка блока кода: своя подложка на всю ширину.
    Code,
}

/// Готовая к рисованию строка — РОВНО ОДНА строка на экране.
///
/// Веха 223.3 — это условие, а не описание. Прокрутка ([`ui::Pane`]) считает высоту содержимого
/// как «записей на шаг строки», и запись, которая рисуется на той же высоте, что предыдущая,
/// лжёт о содержимом: ход прокрутки выходит длиннее текста, текст кончается раньше конца хода, а
/// при возврате вверх пустоту приходится отматывать назад. Ровно это здесь и было — ряд таблицы
/// хранился ЗАПИСЬЮ НА ЯЧЕЙКУ, а рисовался одной строкой, — поэтому ряд теперь одна запись со
/// всеми ячейками.
enum Line {
    /// Текст с отступом слева в точках.
    Text { text: String, kind: Kind, indent: i32 },
    /// Ряд таблицы: ячейки по колонкам таблицы `таблица` (ширины — в [`App::колонки`]).
    Row { table: usize, cells: Vec<String> },
}

/// Раздел руководства: имя корня и его текст (читается при первом открытии).
struct Part {
    name: String,
    blocks: Option<Vec<void_md::Block>>,
}

#[derive(Default)]
struct Lay {
    head: Rect,
    /// Вся левая колонка вместе с полосой.
    col: Rect,
    list: Rect,
    list_bar: Rect,
    /// Текст справа и его полоса — ПОЛОСА УЖЕ ТАКАЯ, КАКОЙ РИСУЕТСЯ (с полями внутрь).
    ///
    /// Веха 223.3 — поля вносятся здесь, а не на месте вызова: по этому же прямоугольнику
    /// считается попадание в полосу и геометрия бегунка, и прежде он был шире нарисованного —
    /// то есть второй мерой рядом с первой.
    body: Rect,
    body_bar: Rect,
    /// Высота строки ЛЕВОГО ПЕРЕЧНЯ.
    row_h: i32,
    rows: usize,
    /// Шаг строки ТЕКСТА справа. Другое число, чем [`Lay::row_h`], — и именно их путаница была
    /// багом прокрутки (см. [`ui::Pane`]).
    step: i32,
}

struct App {
    w: i32,
    h: i32,
    parts: Vec<Part>,
    ls: ui::List,
    /// Разложенные строки текущего раздела и ширина, под которую их раскладывали.
    строки: Vec<Line>,
    /// Веха 223.3 — прокрутка текста ЦЕЛИКОМ виджетом тулкита ([`ui::Pane`]).
    ///
    /// Сперва здесь был свой счётчик «первая видимая строка», и прокрутка шла СТРОКАМИ: строка
    /// либо нарисована целиком, либо не нарисована вовсе. Текст от этого дёргался, но главная
    /// беда была не в этом: высота содержимого считалась в двух местах разными мерами — кадр
    /// брал шаг строки текста, колесо — высоту строки левого перечня, — и ход прокрутки выходил
    /// на четверть длиннее текста. Владелец увидел ровно это: «пропадает слишком рано и
    /// возвращается слишком поздно».
    ///
    /// Полотно тулкита меры принимает ОДИН раз, и ни колесу, ни полосе их больше не передают —
    /// значит разойтись им нечем. Заодно оно само держит и якорь протяжки, и попадание в полосу,
    /// которые тут были переписаны заново.
    text: ui::Pane,
    laid_out_for: Option<(usize, i32)>,
    /// Ширины колонок таблиц — по одной записи на каждую таблицу, в порядке встречи.
    columns: Vec<Vec<i32>>,
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
        // Полосы обеих колонок — СРАЗУ такими, какими рисуются (поля внутрь здесь, а не на месте
        // вызова): по ним же считается попадание.
        let list_bar = list.cut_right(th.px(6)).inset_xy(th.px(1), th.px(2));
        let mut text = body;
        let band = text.cut_right(th.px(6));
        let rows = (list.h / row_h).max(1) as usize;
        Lay {
            head,
            col,
            list,
            list_bar,
            body: text,
            body_bar: band.inset_xy(th.px(1), th.px(2)),
            row_h,
            rows,
            step: font_h + th.px(4),
        }
    }

    fn filter(&mut self) {
        let q = self.ls.query.to_lowercase();
        self.ls.hits = (0..self.parts.len())
            .filter(|&i| q.is_empty() || self.parts[i].name.to_lowercase().contains(&q))
            .collect();
        self.ls.refiltered();
    }

    /// Прочитать текст раздела из store и разобрать. Делается ОДИН раз на раздел: разбор это
    /// проход по тексту, и повторять его на каждый кадр не за что.
    fn load(&mut self, i: usize) {
        if self.parts[i].blocks.is_some() {
            return;
        }
        let Some(scap) = self.store else { return };
        let root = alloc::format!("doc/{}", self.parts[i].name);
        let mut id = [0u8; 32];
        if sys::obj_get_root(scap, root.as_bytes(), &mut id) != 32 {
            return;
        }
        let mut buf = alloc::vec![0u8; 128 * 1024];
        let (got, full) = sys::obj_get_ex(scap, &id, &mut buf);
        if got == 0 || got != full {
            return;
        }
        buf.truncate(got);
        if let Ok(t) = String::from_utf8(buf) {
            self.parts[i].blocks = Some(void_md::parse(&t));
        }
    }

    /// Разложить блоки выбранного раздела под ширину `w` точек.
    ///
    /// Считается при смене раздела или ширины окна, а не каждый кадр: перенос по словам требует
    /// мерить КАЖДОЕ слово шрифтом, и делать это шестьдесят раз в секунду — чистый убыток.
    fn unpack(&mut self, u: &mut Ui, w: i32) {
        let Some(i) = self.ls.current() else { return };
        if self.laid_out_for == Some((i, w)) {
            return;
        }
        self.load(i);
        self.строки.clear();
        self.columns.clear();
        self.text.reset();
        self.laid_out_for = Some((i, w));
        let Some(blocks) = self.parts[i].blocks.take() else { return };
        let item_indent = u.text_w("— ");
        for b in &blocks {
            match b {
                void_md::Block::Heading(level, t) => {
                    // Пустая строка перед заголовком — воздух, которым разделы и отличаются
                    // друг от друга. Кроме самого первого: сверху и так поле.
                    if !self.строки.is_empty() {
                        self.empty();
                    }
                    let kind = if *level <= 1 { Kind::Head } else { Kind::Part };
                    self.строки.push(Line::Text { text: strip(t), kind, indent: 0 });
                }
                void_md::Block::Para(t) => {
                    self.empty();
                    self.wrap(u, &strip(t), w, 0, Kind::Text);
                }
                void_md::Block::List(items) => {
                    self.empty();
                    for p in items {
                        let prev = self.строки.len();
                        self.wrap(u, &strip(p), w - item_indent, item_indent, Kind::Text);
                        // Знак пункта — на первой строке, продолжение под текстом.
                        if let Some(Line::Text { text, indent, .. }) = self.строки.get_mut(prev)
                        {
                            text.insert_str(0, "— ");
                            *indent = 0;
                        }
                    }
                }
                void_md::Block::Code(строки) => {
                    self.empty();
                    let indent = u.text_w("  ");
                    for s in строки {
                        // Веха 223.3 — ДЛИННУЮ строку кода переносим, а не обрезаем.
                        //
                        // Код переносить не хочется: в нём значимы и выравнивание, и пробелы. Но
                        // окна у нас плиточные, половина экрана — обычная ширина, и команда с
                        // пояснением в неё не влезает. Обрезка многоточием выбрасывает ровно то,
                        // ради чего строку читают («switch gen11 — выбрать другое (действует п…»),
                        // а перенос лишь делает её некрасивой. Некрасивая и целая лучше.
                        if u.text_w(s) <= w - indent {
                            self.строки.push(Line::Text {
                                text: s.clone(),
                                kind: Kind::Code,
                                indent,
                            });
                        } else {
                            self.wrap(u, s, w - indent * 2, indent, Kind::Code);
                        }
                    }
                }
                void_md::Block::Table { head, rows: rows } => {
                    self.empty();
                    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut widths = alloc::vec![0i32; cols];
                    for r in rows {
                        for (k, c) in r.iter().enumerate() {
                            widths[k] = widths[k].max(u.text_w(&strip(c)));
                        }
                    }
                    let gap = u.text_w("    ");
                    let measure: i32 =
                        widths.iter().sum::<i32>() + gap * (cols as i32 - 1).max(0);
                    if cols == 0 {
                        continue;
                    }
                    if measure <= w {
                        // Влезает — колонками, как в исходнике. Ряд — ОДНА запись: он и рисуется
                        // одной строкой, а запись на ячейку делала содержимое выше, чем оно есть
                        // (см. [`Строка`]).
                        let n = self.columns.len();
                        self.columns.push(widths);
                        for r in rows {
                            self.строки.push(Line::Row {
                                table: n,
                                cells: r.iter().map(|c| strip(c)).collect(),
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
                        let indent = u.text_w("    ");
                        for r in rows.iter().skip(if *head { 1 } else { 0 }) {
                            let Some(key) = r.first() else { continue };
                            self.строки.push(Line::Text {
                                text: strip(key),
                                kind: Kind::Part,
                                indent: 0,
                            });
                            let tail: Vec<String> =
                                r.iter().skip(1).map(|c| strip(c)).filter(|c| !c.is_empty()).collect();
                            if !tail.is_empty() {
                                let text = tail.join(" · ");
                                self.wrap(u, &text, w - indent, indent, Kind::Text);
                            }
                        }
                    }
                }
            }
        }
        self.parts[i].blocks = Some(blocks);
    }

    fn empty(&mut self) {
        if !self.строки.is_empty() {
            self.строки.push(Line::Text {
                text: String::new(),
                kind: Kind::Text,
                indent: 0,
            });
        }
    }

    /// Перенос абзаца ПО СЛОВАМ под ширину в точках. Шрифт пропорциональный, поэтому мерить
    /// приходится каждое слово — знаков тут не посчитаешь.
    fn wrap(&mut self, u: &mut Ui, s: &str, w: i32, indent: i32, kind: Kind) {
        let space = u.text_w(" ");
        let mut line = String::new();
        let mut width = 0;
        for word in s.split_whitespace() {
            let ws = u.text_w(word);
            if !line.is_empty() && width + space + ws > w {
                self.строки.push(Line::Text {
                    text: core::mem::take(&mut line),
                    kind,
                    indent,
                });
                width = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                width += space;
            }
            line.push_str(word);
            width += ws;
        }
        if !line.is_empty() {
            self.строки.push(Line::Text { text: line, kind, indent });
        }
    }

    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.background(th.bg);
        u.field(lay.head, &self.ls.query, ui::t("поиск по разделам"), true);

        // ── перечень слева ──
        //
        // Полоса только РИСУЕТСЯ, как и у текста: начало перечня посчитано событием.
        self.ls.bar(u);
        let prev = self.ls.sel;
        let mut list = lay.list;
        for k in 0..lay.rows {
            let rr = list.cut_top(lay.row_h);
            let Some(&i) = self.ls.hits.get(self.ls.top + k) else { continue };
            let sel = if self.ls.top + k == self.ls.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            if u.entry(rr, &self.parts[i].name, "", "", sel, hot) {
                self.ls.sel = self.ls.top + k;
            }
        }

        // ── текст справа ──
        //
        // Полоса только РИСУЕТСЯ: смещение к этому моменту уже посчитано обработчиком события
        // (Веха 202.17) — иначе текст доезжал бы до бегунка на кадр-два позже руки.
        self.text.bar(u);
        // Содержимое режется областью, и прежний клип при этом СОХРАНЯЕТСЯ: кадр мог прийти с
        // объёмом `Scope::Part` (подсветка строки перечня), а `Ui::clip` ставит прямоугольник
        // НАПРЯМУЮ, не пересекая с тем, что было. Затереть им объём кадра значит рисовать и
        // объявлять композитору то, о чём объём не просил.
        let prev_clip = u.c.clip();
        u.clip(prev_clip.intersect(self.text.area()));
        let gap = u.text_w("    ");
        for (k, r) in self.text.visible() {
            // Крайние строки приходят с ПОЛНЫМ прямоугольником, наполовину их обрежет клип, —
            // в этом и разница с построчной прокруткой.
            match &self.строки[k] {
                Line::Text { text, kind, indent } => {
                    let indent = (*indent).max(0);
                    let inner = Rect::new(r.x + indent, r.y, r.w - indent, r.h);
                    match kind {
                        Kind::Head => {
                            u.label(inner, text, th.accent, Align::Left);
                        }
                        Kind::Part => {
                            u.label(inner, text, th.text, Align::Left);
                            // Черта под подзаголовком: иначе заголовок теряется в сплошном тексте.
                            let color = u.tint(th.border);
                            u.c.fill(Rect::new(r.x, r.bottom() - th.px(2), r.w, th.px(1)), color);
                        }
                        Kind::Text => {
                            if !text.is_empty() {
                                u.label(inner, text, th.text, Align::Left);
                            }
                        }
                        Kind::Code => {
                            let backdrop = u.tint(th.band);
                            u.c.fill(r, backdrop);
                            u.label(inner, text, th.muted, Align::Left);
                        }
                    }
                }
                Line::Row { table, cells } => {
                    let widths = &self.columns[*table];
                    let mut x = r.x;
                    for (i, c) in cells.iter().enumerate() {
                        let w = widths.get(i).copied().unwrap_or(0);
                        let color = if i == 0 { th.text } else { th.muted };
                        u.label(Rect::new(x, r.y, w, r.h), c, color, Align::Left);
                        x += w + gap;
                    }
                }
            }
        }
        u.clip(prev_clip);
        self.ls.sel != prev
    }
}

/// Снять знаки разметки, которые рисовать нечем (обратные кавычки, звёздочки жирного).
fn strip(s: &str) -> String {
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
                self.laid_out_for = None;
                ui::Scope::All
            }
            Event::Key { sym: code, ch, down, .. } if down => {
                if code == sym::ESCAPE {
                    if self.ls.query.is_empty() {
                        return ui::Scope::No;
                    }
                    self.ls.query.clear();
                    self.filter();
                    self.laid_out_for = None;
                    return ui::Scope::All;
                }
                match self.ls.key(code, ch) {
                    ui::Hit::None => ui::Scope::No,
                    ui::Hit::Moved => {
                        // Выбрали другой раздел — текст справа другой.
                        self.laid_out_for = None;
                        ui::Scope::All
                    }
                    ui::Hit::Query => {
                        self.filter();
                        self.laid_out_for = None;
                        ui::Scope::All
                    }
                }
            }
            Event::Wheel { delta, .. } => {
                // Колесо над перечнем крутит перечень, над текстом — текст. Иначе длинный
                // раздел нечем читать: курсор стоит там, куда смотрят.
                let above_list = input.ptr.map_or(false, |p| self.lay.col.contains(p.0, p.1));
                // Мер колесу не передаём ВОВСЕ — полотно знает их само (см. [`ui::Pane`]).
                // Прежде здесь стояла высота строки ЛЕВОГО ПЕРЕЧНЯ, и это был баг прокрутки.
                let moved =
                    if above_list { self.ls.wheel(delta) } else { self.text.wheel(delta) };
                if moved { ui::Scope::All } else { ui::Scope::No }
            }
            // Веха 223.3 — БЕЗ ЭТОГО ПЕРЕЧЕНЬ НЕ НАЖИМАЛСЯ МЫШЬЮ.
            //
            // Щелчок виджет замечает ВО ВРЕМЯ КАДРА: `Ui::entry` сверяет своё место с точкой
            // нажатия, которую несёт `Input`. Программа, не попросившая кадра на `Button`,
            // щелчка не увидит вовсе — и строки перечня выглядят мёртвыми, хотя клавишами
            // выбираются. Ровно это владелец и наблюдал.
            Event::Button { x, y, down, buttons, .. } => {
                if down && buttons & 1 != 0 {
                    // Две полосы, и каждая сама решает, её ли это нажатие: прыжок мимо бегунка
                    // или захват бегунка. Не попали ни в одну — щелчок достанется строке перечня
                    // во время кадра.
                    let p = (x as i32, y as i32);
                    if !self.text.press(p) {
                        self.ls.bar_press(p);
                    }
                } else if !down {
                    self.text.release();
                    self.ls.bar_release();
                }
                ui::Scope::All
            }
            Event::Motion { .. } => {
                // Схваченную полосу ведём СРАЗУ, до кадра: тогда содержимое едет вместе с
                // бегунком, а не догоняет его.
                if self.text.dragging() {
                    return match input.ptr {
                        Some(p) if self.text.motion(p) => ui::Scope::All,
                        _ => ui::Scope::No,
                    };
                }
                if self.ls.bar_dragging() {
                    return match input.ptr {
                        Some(p) if self.ls.bar_motion(p) => ui::Scope::Part(self.lay.col),
                        _ => ui::Scope::No,
                    };
                }
                // Иначе кадр нужен только если под курсором сменилась строка перечня.
                if self.ls.motion(input.ptr) {
                    ui::Scope::Part(self.lay.col)
                } else {
                    ui::Scope::No
                }
            }
            _ => ui::Scope::No,
        }
    }

    /// Веха 151 — чем вернуть окно после перезагрузки: разделом, на котором читали.
    fn persist(&mut self) -> Option<String> {
        Some(match self.ls.current().map(|i| self.parts[i].name.as_str()) {
            Some(name) => alloc::format!("docs {}", name),
            None => String::from("docs"),
        })
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let th = u.th.clone();
        self.lay = self.measure(u.font, &th);
        self.ls.measure(self.lay.list, self.lay.list_bar, self.lay.row_h, self.lay.rows);
        let width = self.lay.body.w;
        self.unpack(u, width);
        // Меры полотну даются ОДИН раз и ПОСЛЕ укладки: число строк известно только теперь.
        // Здесь же смещение зажимается в краях — окно могло стать выше, а раздел короче.
        self.text.measure(self.lay.body, self.lay.body_bar, self.lay.step, self.строки.len());
        let lay = core::mem::take(&mut self.lay);
        let switched = self.paint(u, &th, &lay);
        self.lay = lay;
        if switched {
            self.laid_out_for = None;
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
    let mut parts: Vec<Part> = Vec::new();
    if let Some(cap) = store {
        if let Some(list) = roots::text(cap) {
            for name in roots::suffixes(&list, b"doc/") {
                if let Ok(s) = core::str::from_utf8(name) {
                    parts.push(Part { name: s.to_string(), blocks: None });
                }
            }
        }
    }
    // Веха 223.2 — ОБЗОР ПЕРВЫМ, остальное по алфавиту. Руководство открывают не затем, чтобы
    // попасть на раздел, чьё имя раньше по азбуке: первым должен стоять тот, с которого читают.
    parts.sort_by(|a, b| {
        let weight = |name: &str| if name == "void" { 0 } else { 1 };
        weight(&a.name).cmp(&weight(&b.name)).then_with(|| a.name.cmp(&b.name))
    });
    if parts.is_empty() {
        say("[docs] разделов руководства нет (права на store?)\n");
    }

    let (w, h) = (960u16, 680u16);
    let Some(mut surf) = Window::create(w, h, ui::t("руководство")) else {
        say("[docs] композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    let mut app = App {
        w: w as i32,
        h: h as i32,
        parts,
        ls: ui::List::default(),
        строки: Vec::new(),
        laid_out_for: None,
        columns: Vec::new(),
        text: ui::Pane::default(),
        lay: Lay::default(),
        store,
    };
    app.filter();
    // Открыться на названном разделе — так нас возвращает сеанс и так же может позвать человек.
    if let Some(name) = sys::argv::Argv::take().str(0) {
        if let Some(k) = app.ls.hits.iter().position(|&i| app.parts[i].name == name) {
            app.ls.sel = k;
            app.ls.scroll_to_sel();
        }
    }
    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
