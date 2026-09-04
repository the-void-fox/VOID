//! `fm` — ФАЙЛОВЫЙ МЕНЕДЖЕР (Веха 166) по макету владельца (`Reference/Design/FileManager.png`).
//!
//! ## Что он показывает и чего не показывает
//!
//! Дерево файлов у VOID одно и приходит от `posixfs` — персоны поверх store ([[posixfs]]). Прав
//! у менеджера ровно столько, сколько дали: эндпоинт файлового сервера, и всё. Ни списка дисков,
//! ни точек монтирования, ни владельцев с группами здесь нет — не потому, что «пока не сделано»,
//! а потому, что в системе их не существует: юзеров в VOID нет вовсе ([[void-no-users-root]]),
//! а «диск» ровно один и виден целиком.
//!
//! ## Три части, как в макете
//!
//! - **полоса навигации**: назад, вперёд, вверх, адрес хлебными крошками, поиск, вид;
//! - **боковая колонка**: закладки — места, куда ходят чаще всего;
//! - **содержимое**: сетка значков (как в макете) или список с размерами.
//!
//! Крошки — не украшение: путь в VOID длинный (`/nix/store/<хэш>-<имя>/…`), и строка целиком в
//! полосу не влезает никогда. Крошка отвечает на «где я» и на «отсюда наверх» одним движением.
//!
//! ## Открывать ли файлы
//!
//! Пока нет: открыть файл значит выбрать программу, а выбирать её не из чего — сопоставления
//! «расширение → программа» в системе не существует, и придумывать его здесь, в менеджере,
//! значило бы спрятать системное решение в одном приложении. Поэтому файл ВЫДЕЛЯЕТСЯ (видно имя
//! и размер), а открывается каталог. Появится сопоставление — появится и открытие.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;
use void_user::posix as px;
use void_user::win::{sym, Event, Window};

#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
use ui::{Align, Font, Rect, Theme, Ui};

/// Кадр окна плюс имена каталога: сотни записей — это сотни коротких строк.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 24 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок на ответ `readdir`. `/nix/store` — самый большой каталог системы, и он же тот, ради
/// которого менеджер и открывают; 256 КиБ хватает на несколько тысяч имён.
const DIR_BUF: usize = 256 * 1024;

fn say(s: &str) {
    sys::write_console(s.as_bytes());
}

/// Запись каталога. Тип берётся из хвостового `/` в ответе сервера — второго вызова `stat` на
/// каждое имя мы себе позволить не можем: в `/nix/store` это тысячи IPC на один кадр.
struct Entry {
    name: String,
    dir: bool,
    /// Размер файла в байтах; `None` — не спрашивали (каталог либо ещё не смотрели).
    size: Option<usize>,
}

/// Места, куда ходят чаще всего. Список ФИКСИРОВАННЫЙ и проверенный: каждая закладка перед
/// показом опрашивается `stat`, и та, которой нет, не рисуется. Закладка на несуществующее —
/// это кнопка, ведущая в ошибку, а «здесь ничего нет» человек должен узнавать не так.
const MARKS: [(&str, &str); 5] = [
    ("/", "корень"),
    ("/etc", "конфиг"),
    ("/etc/system", "поколения"),
    ("/nix/store", "store"),
    ("/bin", "программы"),
];

#[derive(Default)]
struct Lay {
    bar: Rect,
    back: Rect,
    fwd: Rect,
    up: Rect,
    addr: Rect,
    find: Rect,
    view: Rect,
    side: Rect,
    body: Rect,
    /// Поле содержимого за вычетом полей — в нём и лежит сетка (или список).
    inner: Rect,
    /// Сетка: размер ячейки и сколько их в ряду.
    cell: (i32, i32),
    cols: i32,
    /// Список: высота строки.
    row_h: i32,
    /// Сколько ячеек (строк) помещается по вертикали.
    page: usize,
}

struct App {
    w: i32,
    h: i32,
    /// Эндпоинт файлового сервера. Без него менеджер не имеет смысла и говорит это вслух.
    ep: usize,
    cwd: String,
    entries: Vec<Entry>,
    /// Номера записей, прошедших отбор по набранному.
    hits: Vec<usize>,
    sel: usize,
    top: usize,
    /// Куда возвращаться и откуда возвращаться. Две стопки, как у браузера: «назад» кладёт в
    /// «вперёд», любой новый переход «вперёд» очищает.
    back: Vec<String>,
    fwd: Vec<String>,
    query: String,
    /// Сетка значков (как в макете) или список с размерами.
    grid: bool,
    /// Что сказал сервер, если каталог не открылся, и не обрезан ли список.
    err: Option<String>,
    cut: bool,
    marks: Vec<(String, String)>,
    lay: Lay,
}

impl App {
    /// Склеить путь: `/` + `etc` = `/etc`, `/etc` + `system` = `/etc/system`.
    fn join(base: &str, name: &str) -> String {
        if base.ends_with('/') {
            alloc::format!("{base}{name}")
        } else {
            alloc::format!("{base}/{name}")
        }
    }

    /// Каталог выше. У корня выше нет — возвращает его же.
    fn parent(path: &str) -> String {
        match path.trim_end_matches('/').rfind('/') {
            Some(0) | None => String::from("/"),
            Some(i) => path[..i].to_string(),
        }
    }

    /// Перечитать текущий каталог. Порядок — каталоги вперёд, дальше по имени: так же его
    /// показывает `ls`, и человеку не приходится держать в голове два разных порядка.
    fn read(&mut self) {
        self.entries.clear();
        self.err = None;
        self.cut = false;
        if self.ep == sys::NO_CAP {
            self.err = Some(String::from("нет права на файловый сервер"));
            self.refilter();
            return;
        }
        let mut buf = alloc::vec![0u8; DIR_BUF];
        let (n, want) = px::readdir_ex(self.ep, self.cwd.as_bytes(), &mut buf);
        if n == usize::MAX {
            self.err = Some(alloc::format!("каталог не открылся: {}", self.cwd));
            self.refilter();
            return;
        }
        self.cut = want > n;
        for line in buf[..n.min(buf.len())].split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let dir = line.last() == Some(&b'/');
            let raw = if dir { &line[..line.len() - 1] } else { line };
            if raw.is_empty() || raw == b"." || raw == b".." {
                continue;
            }
            self.entries.push(Entry {
                name: String::from_utf8_lossy(raw).into_owned(),
                dir,
                size: None,
            });
        }
        self.entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
        self.refilter();
    }

    /// Пересобрать отбор по набранному, сохранив выбор в пределах списка.
    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.hits = (0..self.entries.len())
            .filter(|&i| q.is_empty() || self.entries[i].name.to_lowercase().contains(&q))
            .collect();
        self.sel = self.sel.min(self.hits.len().saturating_sub(1));
        self.top = 0;
    }

    /// Перейти в каталог, запомнив дорогу назад.
    fn go(&mut self, path: String) {
        if path == self.cwd {
            return;
        }
        self.back.push(core::mem::replace(&mut self.cwd, path));
        self.fwd.clear();
        self.query.clear();
        self.sel = 0;
        self.read();
    }

    /// Назад и вперёд по стопкам.
    fn step(&mut self, forward: bool) {
        let (from, to) = if forward {
            (&mut self.fwd, &mut self.back)
        } else {
            (&mut self.back, &mut self.fwd)
        };
        let Some(p) = from.pop() else { return };
        to.push(core::mem::replace(&mut self.cwd, p));
        self.query.clear();
        self.sel = 0;
        self.read();
    }

    /// Открыть выбранное: каталог — войти, файл — узнать размер (открывать нечем, см. шапку).
    fn open_sel(&mut self) {
        let Some(&i) = self.hits.get(self.sel) else { return };
        if self.entries[i].dir {
            let p = Self::join(&self.cwd, &self.entries[i].name);
            self.go(p);
        } else {
            self.measure_sel(i);
        }
    }

    /// Спросить размер ОДНОГО файла — того, на который смотрят. Спрашивать у всех разом нельзя:
    /// в `/nix/store` это тысячи IPC на кадр, и окно вставало бы на секунды при каждом входе.
    fn measure_sel(&mut self, i: usize) {
        if self.entries[i].size.is_some() || self.entries[i].dir {
            return;
        }
        let p = Self::join(&self.cwd, &self.entries[i].name);
        if let Some((_, size, _)) = px::stat_ex(self.ep, p.as_bytes()) {
            self.entries[i].size = Some(size);
        }
    }

    /// Крошки текущего пути: (подпись, путь). Первая — корень.
    fn crumbs(&self) -> Vec<(String, String)> {
        let mut out = alloc::vec![(String::from("/"), String::from("/"))];
        let mut acc = String::new();
        for part in self.cwd.split('/').filter(|s| !s.is_empty()) {
            acc.push('/');
            acc.push_str(part);
            out.push((part.to_string(), acc.clone()));
        }
        out
    }

    /// Разрезать окно. Числа сняты с макета (`FileManager.svg`): поле 7, полоса 27, колонка 150.
    fn measure(&self, font: &Font, th: &Theme) -> Lay {
        let font_h = font.line_h();
        let m = th.px(7);
        let mut all = Rect::new(0, 0, self.w, self.h).inset(m);
        let bar = all.cut_top(font_h + th.px(11));
        all.cut_top(m);
        let mut lay = Lay { bar, ..Lay::default() };
        // Кнопки навигации слева, поиск и вид справа, адрес — всё, что между ними.
        let mut inner = bar.inset_xy(th.px(4), 0);
        let ico = (bar.h - th.px(8)).max(th.px(12));
        let step = ico + th.px(8);
        lay.back = Rect::new(inner.x, bar.y + (bar.h - ico) / 2, ico, ico);
        lay.fwd = Rect::new(inner.x + step, lay.back.y, ico, ico);
        lay.up = Rect::new(inner.x + 2 * step, lay.back.y, ico, ico);
        inner.cut_left(3 * step + th.px(4));
        lay.view = Rect::new(inner.right() - ico, lay.back.y, ico, ico);
        lay.find = Rect::new(lay.view.x - step, lay.back.y, ico, ico);
        inner.cut_right(2 * step);
        lay.addr = Rect::new(inner.x, bar.y + th.px(4), inner.w, bar.h - 2 * th.px(4));

        // Боковая колонка — фиксированной ширины, как в макете; остальное содержимому.
        let side_w = th.px(150).min(all.w / 3);
        lay.side = all.cut_left(side_w);
        all.cut_left(m);
        lay.body = all;

        // Сетка: ячейка вмещает значок и подпись под ним.
        let cw = th.px(88);
        let ch = th.px(34) + font_h + th.px(10);
        let inner = lay.body.inset(th.px(8));
        lay.inner = inner;
        lay.cols = (inner.w / cw).max(1);
        lay.cell = (inner.w / lay.cols, ch);
        lay.row_h = font_h + th.px(8);
        lay.page = if self.grid {
            ((inner.h / ch).max(1) * lay.cols) as usize
        } else {
            (inner.h / lay.row_h).max(1) as usize
        };
        lay
    }

    /// Место ячейки `k` (номер В ОТОБРАННОМ, считая от `top`).
    fn cell_rect(&self, lay: &Lay, k: usize) -> Rect {
        let inner = lay.inner;
        if self.grid {
            let (cw, ch) = lay.cell;
            let (cx, cy) = (k as i32 % lay.cols, k as i32 / lay.cols);
            Rect::new(inner.x + cx * cw, inner.y + cy * ch, cw, ch)
        } else {
            Rect::new(inner.x, inner.y + k as i32 * lay.row_h, inner.w, lay.row_h)
        }
    }

    /// Нарисовать кадр. `true` — состояние изменилось прямо в кадре (клик), нужен ещё проход.
    fn paint(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.background(th.band);
        let mut dirty = false;
        dirty |= self.paint_bar(u, th, lay);
        dirty |= self.paint_side(u, th, lay);
        dirty |= self.paint_body(u, th, lay);
        dirty
    }

    /// Полоса навигации: три кнопки, адрес крошками, поиск и вид.
    fn paint_bar(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.card(lay.bar);
        let mut dirty = false;
        let hot = |u: &Ui, r: Rect| if u.hot(r) { 256 } else { 0 };
        // Кнопка, которой некуда вести, НЕ ПОДСВЕЧИВАЕТСЯ под курсором — но и не исчезает:
        // прыгающий ряд кнопок читался бы хуже, чем неотзывчивая стрелка на своём месте.
        let can = !self.back.is_empty();
        let h = if can { hot(u, lay.back) } else { 0 };
        if u.icon_button(lay.back, ui::icon::BACK, h, false) && can {
            self.step(false);
            dirty = true;
        }
        let can = !self.fwd.is_empty();
        let h = if can { hot(u, lay.fwd) } else { 0 };
        if u.icon_button(lay.fwd, ui::icon::FORWARD, h, false) && can {
            self.step(true);
            dirty = true;
        }
        let can = self.cwd != "/";
        let h = if can { hot(u, lay.up) } else { 0 };
        if u.icon_button(lay.up, ui::icon::UP, h, false) && can {
            let p = Self::parent(&self.cwd);
            self.go(p);
            dirty = true;
        }

        // Адрес: поле цвета фона, в нём крошки плитками. Набранное показывается ТУТ ЖЕ вместо
        // крошек — поиск и адрес отвечают на один и тот же вопрос «что я сейчас вижу».
        let rad = th.radius.min(lay.addr.h / 2);
        let (bg, br) = (u.tint(th.band), u.tint(th.border));
        u.c.rrect_bordered(lay.addr, rad, th.line, bg, br);
        let mut a = lay.addr.inset_xy(th.px(4), 0);
        if !self.query.is_empty() {
            u.label(a, &alloc::format!("поиск: {}", self.query), th.text, Align::Left);
        } else {
            let crumbs = self.crumbs();
            let ch = lay.addr.h - th.px(6);
            for (k, (label, path)) in crumbs.iter().enumerate() {
                let w = u.text_w(label) + th.px(8);
                if a.w < w + th.px(10) {
                    break;
                }
                let r = Rect::new(a.x, lay.addr.y + th.px(3), w, ch);
                let last = k + 1 == crumbs.len();
                let c = u.tint(if last { th.band_on } else { th.bg });
                u.c.rrect(r, th.px(3), c);
                u.label(r, label, if last { th.text } else { th.muted }, Align::Center);
                if u.clicked(r) {
                    let p = path.clone();
                    self.go(p);
                    dirty = true;
                }
                a.cut_left(w);
                if !last {
                    let sep = a.cut_left(th.px(10));
                    u.label(sep, ">", th.muted, Align::Center);
                }
            }
        }

        // Поиск — ПЕРЕКЛЮЧАТЕЛЬ набранного: нажали при пустом — просто подсказка «набирай»,
        // нажали при набранном — сбросили. Отдельного поля нет: адресная строка одна.
        let h = hot(u, lay.find);
        if u.icon_button(lay.find, ui::icon::SEARCH, h, false) && !self.query.is_empty() {
            self.query.clear();
            self.refilter();
            dirty = true;
        }
        let h = hot(u, lay.view);
        if u.icon_button(lay.view, ui::icon::MENU, h, false) {
            self.grid = !self.grid;
            self.top = 0;
            dirty = true;
        }
        dirty
    }

    /// Боковая колонка: закладки.
    fn paint_side(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.card(lay.side);
        let font_h = u.font.line_h();
        let mut d = lay.side.inset(th.px(7));
        let mut dirty = false;
        let mut head = d.cut_top(font_h + th.px(4));
        let ico = head.cut_left(font_h);
        u.icon(ico, ui::icon::STAR, th.muted);
        head.cut_left(th.px(4));
        u.label(head, "закладки", th.muted, Align::Left);
        d.cut_top(th.px(4));
        let row_h = font_h + th.px(3);
        for k in 0..self.marks.len() {
            if d.h < row_h {
                break;
            }
            let r = d.cut_top(row_h);
            d.cut_top(th.px(3));
            let (path, label) = (self.marks[k].0.clone(), self.marks[k].1.clone());
            let here = self.cwd == path;
            let bg = if here {
                th.band_on
            } else if u.hot(r) {
                th.band
            } else {
                th.band.with_a(0x80)
            };
            let c = u.tint(bg);
            u.c.rrect(r, th.px(3), c);
            // Подпись ОДНА, человеческая — как в макете. Полный путь рядом не помещался и
            // обрывался многоточием ровно на том месте, которым закладки и различаются
            // («/et… поколения»); а где ты сейчас, отвечают крошки в полосе.
            let t = r.inset_xy(th.px(6), 0);
            u.label(t, &label, if here { th.text } else { th.muted }, Align::Left);
            if u.clicked(r) {
                self.go(path);
                dirty = true;
            }
        }
        dirty
    }

    /// Содержимое: сетка значков либо список с размерами.
    fn paint_body(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.card(lay.body);
        let font_h = u.font.line_h();
        let mut dirty = false;
        if let Some(e) = &self.err {
            let mut d = lay.body.inset(th.pad);
            let msg = e.clone();
            u.label(d.cut_top(font_h + th.px(4)), &msg, th.text, Align::Left);
            u.label(d.cut_top(font_h), "проверь путь и права", th.muted, Align::Left);
            return false;
        }
        if self.hits.is_empty() {
            let mut d = lay.body.inset(th.pad);
            let msg = if self.entries.is_empty() {
                "каталог пуст"
            } else {
                "ничего не нашлось"
            };
            u.label(d.cut_top(font_h + th.px(4)), msg, th.muted, Align::Left);
            return false;
        }
        let total = self.hits.len();
        let end = (self.top + lay.page).min(total);
        for k in self.top..end {
            let i = self.hits[k];
            let r = self.cell_rect(lay, k - self.top);
            let sel = k == self.sel;
            if sel || u.hot(r) {
                let c = u.tint(if sel { th.band_on } else { th.text.with_a(0x10) });
                u.c.rrect(r, th.radius.min(r.h / 2), c);
            }
            let (name, dir, size) = {
                let e = &self.entries[i];
                (e.name.clone(), e.dir, e.size)
            };
            let art = if dir { ui::icon::FOLDER } else { ui::icon::FILE };
            let col = if dir { th.text } else { th.muted };
            if self.grid {
                let side = th.px(34);
                let ir = Rect::new(r.x + (r.w - side) / 2, r.y + th.px(5), side, side);
                u.icon(ir, art, col);
                let lr = Rect::new(r.x, ir.bottom() + th.px(2), r.w, font_h);
                u.label(lr, &name, th.text, Align::Center);
            } else {
                let mut t = r.inset_xy(th.px(6), 0);
                let ir = t.cut_left(font_h);
                u.icon(Rect::new(ir.x, ir.y + (r.h - font_h) / 2, font_h, font_h), art, col);
                t.cut_left(th.px(6));
                let val = t.cut_right(u.text_w("0000,0 МиБ"));
                u.label(t, &name, th.text, Align::Left);
                let s = match (dir, size) {
                    (true, _) => String::from("каталог"),
                    (false, Some(n)) => size_text(n),
                    (false, None) => String::new(),
                };
                u.label(val, &s, th.muted, Align::Right);
            }
            if u.clicked(r) {
                // Первый щелчок ВЫБИРАЕТ, щелчок по уже выбранному — открывает. Двойной щелчок
                // по времени завести не на чем: у событий композитора часов нет, а мерить их
                // самим значило бы завести своё понятие «двойного» вразрез с системным.
                if sel {
                    self.open_sel();
                } else {
                    self.sel = k;
                    self.measure_sel(i);
                }
                dirty = true;
            }
        }
        // Подвал списка живёт в самой карточке содержимого: сколько всего и не обрезано ли.
        if self.cut {
            let foot = Rect::new(lay.body.x, lay.body.bottom() - font_h - th.px(4), lay.body.w, font_h);
            u.label(
                foot.inset_xy(th.px(8), 0),
                &alloc::format!("показаны не все: сервер отдал {} имён", total),
                th.muted,
                Align::Right,
            );
        }
        dirty
    }

    /// Держать выбранное в видимом окне.
    fn scroll_to_sel(&mut self, page: usize, cols: usize) {
        if self.sel < self.top {
            self.top = self.sel - self.sel % cols.max(1);
        } else if self.sel >= self.top + page {
            let over = self.sel + 1 - page;
            self.top = over + (cols.max(1) - over % cols.max(1)) % cols.max(1);
        }
    }
}

/// Размер файла человеку: байты, КиБ, МиБ. Точность одна десятая — больше не читается, меньше
/// врёт («0 КиБ» у файла в 900 байт).
fn size_text(n: usize) -> String {
    if n < 1024 {
        alloc::format!("{n} Б")
    } else if n < 1024 * 1024 {
        alloc::format!("{},{} КиБ", n / 1024, (n % 1024) * 10 / 1024)
    } else {
        alloc::format!("{},{} МиБ", n / 1048576, (n % 1048576) * 10 / 1048576)
    }
}

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        match e {
            Event::Key { sym: code, ch, down, .. } if down => {
                let cols = if self.grid { self.lay.cols as usize } else { 1 };
                match code {
                    sym::RETURN => {
                        self.open_sel();
                        return ui::Scope::All;
                    }
                    sym::BACKSPACE => {
                        // Набранное важнее подъёма: пока идёт поиск, забой правит его.
                        if self.query.pop().is_some() {
                            self.refilter();
                        } else if self.cwd != "/" {
                            let p = App::parent(&self.cwd);
                            self.go(p);
                        }
                        return ui::Scope::All;
                    }
                    sym::ESCAPE if !self.query.is_empty() => {
                        self.query.clear();
                        self.refilter();
                        return ui::Scope::All;
                    }
                    sym::LEFT => self.sel = self.sel.saturating_sub(1),
                    sym::RIGHT => self.sel = (self.sel + 1).min(self.hits.len().saturating_sub(1)),
                    sym::UP => self.sel = self.sel.saturating_sub(cols),
                    sym::DOWN => {
                        self.sel = (self.sel + cols).min(self.hits.len().saturating_sub(1))
                    }
                    sym::HOME => self.sel = 0,
                    sym::END => self.sel = self.hits.len().saturating_sub(1),
                    _ => {
                        // Печатный знак — это поиск. Отдельного поля нет намеренно: набирать в
                        // менеджере больше нечего, а лишнее поле пришлось бы ещё и фокусировать.
                        let c = char::from_u32(ch as u32).filter(|c| !c.is_control());
                        let Some(c) = c else { return ui::Scope::No };
                        self.query.push(c);
                        self.refilter();
                        return ui::Scope::All;
                    }
                }
                let page = self.lay.page;
                self.scroll_to_sel(page, cols);
                if let Some(&i) = self.hits.get(self.sel) {
                    self.measure_sel(i);
                }
                ui::Scope::All
            }
            Event::Wheel { delta, .. } => {
                let cols = if self.grid { self.lay.cols as usize } else { 1 };
                let step = cols.max(1);
                if delta < 0 {
                    self.top = (self.top + step).min(self.hits.len().saturating_sub(1));
                } else {
                    self.top = self.top.saturating_sub(step);
                }
                ui::Scope::All
            }
            Event::Motion { .. } => {
                let _ = input;
                ui::Scope::All
            }
            Event::Button { .. } => ui::Scope::All,
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    /// Веха 151 — вернуться после перезагрузки в ТОТ ЖЕ каталог.
    fn persist(&mut self) -> Option<String> {
        Some(alloc::format!("fm {}", self.cwd))
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        let th = u.th.clone();
        self.lay = self.measure(u.font, &th);
        let lay = core::mem::take(&mut self.lay);
        let dirty = self.paint(u, &th, &lay);
        self.lay = lay;
        if dirty { ui::Scope::All } else { ui::Scope::No }
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();
    // Файловый сервер: по имени из окружения, а нет имени — стартовым правом 0, как у шелла.
    let ep = sys::cap_named("POSIXFS").unwrap_or_else(|| sys::start_cap(0));
    if ep == sys::NO_CAP {
        say("fm: нет эндпоинта файлового сервера — показывать нечего\n");
    }
    let (w, h) = (760u16, 620u16);
    let Some(mut surf) = Window::create(w, h, "Файлы") else {
        say("fm: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };

    // Аргумент — каталог, с которого начинать (им же возвращается сеанс, Веха 151).
    let av = sys::argv::Argv::take();
    let start = av.str(0).filter(|s| s.starts_with('/')).unwrap_or("/").to_string();

    // Закладки проверяются ОДИН раз, на старте: каталог, которого нет, в колонку не попадает.
    let marks = MARKS
        .iter()
        .filter(|(p, _)| px::stat(ep, p.as_bytes()).map(|(d, _)| d).unwrap_or(false))
        .map(|(p, l)| (String::from(*p), String::from(*l)))
        .collect();

    let mut app = App {
        w: w as i32,
        h: h as i32,
        ep,
        cwd: start,
        entries: Vec::new(),
        hits: Vec::new(),
        sel: 0,
        top: 0,
        back: Vec::new(),
        fwd: Vec::new(),
        query: String::new(),
        grid: true,
        err: None,
        cut: false,
        marks,
        lay: Lay::default(),
    };
    app.read();

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
