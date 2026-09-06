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
use void_user::win::{self as win, sym, Event, Window};

#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
use ui::{Align, Font, Rect, Rgba, Theme, Ui};

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
    /// Закладки: где начинается первая строка, её высота, зазор и поле колонки.
    mark_y: i32,
    mark_h: i32,
    mark_gap: i32,
    mark_pad: i32,
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
    /// Веха 167 — ВЫДЕЛЕННОЕ: номера строк в отобранном списке. Пусто — не выделено ничего, и
    /// это не то же самое, что «выделена нулевая»: при входе в каталог `sel` равен нулю, и без
    /// различия первый значок открывался бы одним щелчком, а все остальные — двумя.
    marked: Vec<usize>,
    /// Якорь диапазона для `Shift`: откуда тянется выделение.
    anchor: usize,
    top: usize,
    /// Куда возвращаться и откуда возвращаться. Две стопки, как у браузера: «назад» кладёт в
    /// «вперёд», любой новый переход «вперёд» очищает.
    back: Vec<String>,
    fwd: Vec<String>,
    query: String,
    /// Сетка значков (как в макете) или список с размерами.
    grid: bool,
    /// Подпись содержимого каталога: по ней автообновление узнаёт, изменилось ли что-нибудь.
    sig: u64,
    /// Что сказал сервер, если каталог НЕ ОТКРЫЛСЯ, и не обрезан ли список.
    err: Option<String>,
    /// Ответ на последнее действие («скопировано», «нет такого пути»). Отдельно от `err`: тот
    /// значит «показывать нечего», а этот — «показать есть что, и вот ещё словечко».
    flash: Option<String>,
    cut: bool,
    marks: Vec<(String, String)>,
    /// Текст конфига поколения: из него берутся программы по умолчанию (`default …`).
    generation: String,
    lay: Lay,
    /// Веха 167 — что правим прямо сейчас и чем. Правка одна на весь менеджер: набирать в двух
    /// местах разом нельзя, а «половина букв в путь, половина в имя» — ровно это и было бы.
    edit: Option<(What, ui::Edit)>,
    /// Контекстное меню: где открыто и по какой записи (`None` — по пустому месту).
    menu: Option<(i32, i32, Tgt)>,
    /// Правка началась ЭТИМ ЖЕ щелчком (из меню): пока так, щелчок её не отменяет.
    edit_fresh: bool,
    /// Меню открыто ЭТИМ ЖЕ щелчком: пока так, его пункты кликов не принимают. Иначе нажатие
    /// правой кнопкой открывало бы меню и тут же выбирало в нём пункт под курсором.
    menu_fresh: bool,
    /// «Удалить» в меню взведено вторым нажатием. Диалогов в тулките нет, а один щелчок на
    /// необратимое — слишком дёшево.
    armed: bool,
    /// Право на store: под буфер обмена и перетаскивание возится не содержимое, а ИМЯ объекта,
    /// и класть объект копирующий обязан сам ([[void-ui]], Веха 150).
    store: Option<usize>,
    /// Откуда нажали и потащили ли уже: перетаскивание начинается не с нажатия, а с ДВИЖЕНИЯ
    /// с зажатой кнопкой — иначе каждый щелчок был бы началом перетаскивания.
    press: Option<(i32, i32, usize)>,
    dragging: bool,
    /// Веха 167.2 — РАМКА ВЫДЕЛЕНИЯ: откуда протянули и куда. `None` — не тянут.
    band: Option<((i32, i32), (i32, i32))>,
    /// Что было выделено ДО протяжки: с `Ctrl` рамка добавляет, без него — заменяет.
    band_base: Vec<usize>,
    /// Веха 167.3 — какую запись схлопнуть в одиночное выделение, если кнопку отпустят, так и
    /// не потащив. Нажатие по выделенной группе её сохраняет (иначе не потащить), а простой
    /// щелчок по ней всё же должен значить «выбрал вот эту».
    collapse_to: Option<usize>,
    /// Правку тянут мышью: протяжка действует, только если начали ВНУТРИ поля. Иначе выделять
    /// текст начинал бы всякий, кто провёл курсором над полем с зажатой кнопкой.
    edit_drag: bool,
}

/// Что выбрали в контекстном меню.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Open,
    Copy,
    Paste,
    Refresh,
    Delete,
    Rename,
    NewDir,
    NewFile,
    Mark,
    Unmark,
    Term,
}

/// По чему нажали правой кнопкой.
#[derive(Clone, Copy, PartialEq)]
enum Tgt {
    /// Запись содержимого (номер в отобранном списке).
    Entry(usize),
    /// Закладка в боковой колонке.
    Mark(usize),
    /// Пустое место: меню про текущий каталог.
    Empty,
}

/// Что именно правится в поле ввода.
#[derive(Clone, Copy, PartialEq)]
enum What {
    /// Путь в адресной строке.
    Path,
    /// Имя записи (номер в отобранном списке).
    Rename(usize),
    /// Имя того, чего ещё нет: каталога либо файла.
    Create { dir: bool },
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
        // Что было выбрано и куда прокручено — ЗАПОМИНАЕМ ИМЕНЕМ. Автообновление зовёт `read`
        // дважды в секунду, и сбрасывать выбор с прокруткой на каждый чужой файл значило бы
        // сделать менеджер непригодным ровно тогда, когда в каталоге что-то происходит.
        // Восстанавливаем ВЫДЕЛЕНИЕ, а не «строку под курсором»: без этого различия
        // автообновление через две секунды само выделяло нулевую запись, и первый же щелчок по
        // ней читался как второй — каталог открывался с одного нажатия.
        let keep = self
            .hits
            .get(self.sel)
            .filter(|_| !self.marked.is_empty())
            .map(|&i| self.entries[i].name.clone());
        let keep_top = self.top;
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
        // Подпись — по именам и виду: перечитали и получили то же самое значит «ничего не
        // изменилось», и кадра не надо. Размеры в подпись не входят: их мы спрашиваем у одного
        // файла, а не у всех, и знать о них нечего.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for e in &self.entries {
            for b in e.name.as_bytes().iter().chain(&[e.dir as u8]) {
                h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
            }
        }
        self.sig = h;
        self.refilter();
        if let Some(name) = keep {
            if let Some(k) = self.hits.iter().position(|&i| self.entries[i].name == name) {
                self.sel = k;
                self.anchor = k;
                self.marked = alloc::vec![k];
                self.top = keep_top.min(self.max_top());
            }
        }
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
        self.flash = None;
        self.sel = 0;
        self.marked.clear();
        self.anchor = 0;
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
        self.flash = None;
        self.sel = 0;
        self.marked.clear();
        self.anchor = 0;
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
        // Строки закладок — из тех же чисел, что и рисование: заголовок, поле, зазор.
        lay.mark_pad = th.px(7);
        lay.mark_h = font_h + th.px(3);
        lay.mark_gap = th.px(3);
        lay.mark_y = lay.side.y + lay.mark_pad + (font_h + th.px(4)) + th.px(4);
        all.cut_left(m);
        lay.body = all;

        // Сетка: ячейка вмещает значок и подпись под ним.
        let cw = th.px(88);
        // Две строки подписи: имена в store длинные, и одной строкой они режутся почти всегда.
        let ch = th.px(34) + 2 * font_h + th.px(10);
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
        // Меню — ПОСЛЕДНИМ: оно лежит поверх всего, и рисовать его раньше значит рисовать под.
        dirty |= self.paint_menu(u, th);
        // Щелчок мимо адресной строки бросает правку ПУТИ: человек передумал, а не ошибся.
        //
        // Только пути и только не в тот же кадр, в который правка началась. Оба ограничения
        // выстраданы: без первого щелчок мимо сбрасывал бы набор ИМЕНИ (а он начинается из
        // меню, то есть щелчком заведомо не по адресной строке), без второго — тот же самый
        // щелчок, что выбрал пункт «создать каталог», её же и отменял. Поле появлялось и
        // исчезало в одном кадре, снаружи это выглядело как «пункт меню не работает».
        if matches!(self.edit, Some((What::Path, _))) && !self.edit_fresh {
            if let Some((cx, cy)) = u.click() {
                if !lay.addr.contains(cx, cy) {
                    self.edit = None;
                    dirty = true;
                }
            }
        }
        self.edit_fresh = false;
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
        let mut jump: Option<String> = None;
        let mut caret_to: Option<usize> = None;
        let mut drag_to: Option<usize> = None;
        let rad = th.radius.min(lay.addr.h / 2);
        let (bg, br) = (u.tint(th.band), u.tint(th.border));
        u.c.rrect_bordered(lay.addr, rad, th.line, bg, br);
        let mut a = lay.addr.inset_xy(th.px(4), 0);
        // Веха 166.2 — ПРАВКА ПУТИ. Отдельного окна «перейти к» нет и не нужно: строка адреса
        // и так отвечает на «где я», логично, что она же принимает «куда идти». Пока правим,
        // крошек нет — иначе в одном месте было бы два разных ответа на один вопрос.
        if let Some((What::Path, e)) = &self.edit {
            let (click, drag) = u.edit_field(lay.addr, e, "путь");
            caret_to = click;
            drag_to = drag;
        } else if !self.query.is_empty() {
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
                    jump = Some(path.clone());
                }
                a.cut_left(w);
                if !last {
                    let sep = a.cut_left(th.px(10));
                    u.label(sep, ">", th.muted, Align::Center);
                }
            }
        }

        // Щелчок по адресной строке МИМО крошек — начать правку пути. Мимо, а не по: крошка
        // отвечает на «перейти туда», и отдавать ей ещё и «править» значило бы два смысла на
        // одном месте.
        if self.edit.is_none() && jump.is_none() && u.clicked(lay.addr) {
            self.begin_edit(What::Path);
            dirty = true;
        }
        // Щелчок ВНУТРИ правимого поля ставит курсор туда, куда ткнули; протяжка от него —
        // выделяет. Порядок важен: щелчок сбрасывает выделение, протяжка его строит, и в одном
        // кадре может прийти и то и другое (нажали и сразу повели).
        if let (Some(at), Some((_, e))) = (caret_to, self.edit.as_mut()) {
            e.put_caret(at);
            dirty = true;
        }
        if self.edit_drag {
            if let (Some(at), Some((_, e))) = (drag_to, self.edit.as_mut()) {
                e.drag_caret(at);
                dirty = true;
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
        if let Some(p) = jump {
            self.go(p);
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
        let mut act: Option<String> = None;
        for k in 0..self.marks.len() {
            let r = self.mark_rect(lay, k);
            if r.bottom() > lay.side.bottom() {
                break;
            }
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
                act = Some(path);
            }
        }
        // Переход — ПОСЛЕ обхода, по той же причине, что в содержимом: `go` перечитывает
        // каталог, и остаток ряда рисовался бы уже по другому состоянию.
        if let Some(p) = act {
            self.go(p);
            dirty = true;
        }
        dirty
    }

    /// Содержимое: сетка значков либо список с размерами.
    fn paint_body(&mut self, u: &mut Ui, th: &Theme, lay: &Lay) -> bool {
        u.card(lay.body);
        let font_h = u.font.line_h();
        let mut dirty = false;
        // Веха 167 — ПОЛОСА ПРАВКИ ИМЕНИ поверх содержимого: переименование и создание набирают
        // в ней. Не в самой ячейке: ячейка сетки шириной в девять знаков, и поле в ней было бы
        // уже, чем то, что в него набирают.
        if let Some((what @ (What::Rename(_) | What::Create { .. }), e)) = &self.edit {
            let hint = match what {
                What::Create { dir: true } => "имя каталога",
                What::Create { dir: false } => "имя файла",
                _ => "новое имя",
            };
            let r = Rect::new(lay.body.x + th.pad, lay.body.y + th.px(6), lay.body.w - 2 * th.pad, font_h + th.px(10));
            let (click, drag) = u.edit_field(r, e, hint);
            let inside = click.is_some();
            if let (Some(at), Some((_, e))) = (click, self.edit.as_mut()) {
                e.put_caret(at);
            }
            if inside {
                self.edit_drag = true;
            }
            if self.edit_drag {
                if let (Some(at), Some((_, e))) = (drag, self.edit.as_mut()) {
                    e.drag_caret(at);
                }
            }
            return true;
        }
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
        // На что нажали — ОТЛОЖЕННО, номером строки. Исполнять клик прямо здесь нельзя: вход в
        // каталог перечитывает `entries` и `hits` ПОСРЕДИ обхода, и следующий же виток берёт
        // старый номер в новом списке. Ровно на этом менеджер и падал: из корня (две записи) в
        // `/etc` (одна) — «index out of bounds: the len is 1 but the index is 1», процесс
        // умирал, а окно оставалось на экране и выглядело зависшим (Веха 166.1).
        //
        // Это тот же уговор, по которому [`ui::Client::after`] отделён от `draw`, и та же
        // причина: кадр рисует ПО СНИМКУ состояния, и менять снимок в середине кадра — значит
        // рисовать вторую половину по данным, которых первая не видела.
        let mut act: Option<(usize, u8)> = None;
        for k in self.top..end {
            let i = self.hits[k];
            let r = self.cell_rect(lay, k - self.top);
            // Выделенных может быть много, а КУРСОР один: выделенное залито, курсор ещё и
            // обведён. Без разницы между ними после `Shift`-полосы непонятно, откуда она
            // потянется дальше.
            let mark = self.marked.contains(&k);
            if mark || u.hot(r) {
                let c = u.tint(if mark { th.band_on } else { th.text.with_a(0x10) });
                u.c.rrect(r, th.radius.min(r.h / 2), c);
            }
            if k == self.sel && mark {
                let c = u.tint(th.accent.with_a(0x88));
                u.c.rrect_bordered(r, th.radius.min(r.h / 2), th.line.max(1), Rgba::CLEAR, c);
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
                // Подпись УЖЕ ячейки: иначе имя, влезшее впритык, касается соседнего и оба
                // читаются как одно слово.
                let lw = r.w - 2 * th.px(8);
                let (l1, l2) = wrap2(u.font, &name, lw);
                let lr = Rect::new(r.x + (r.w - lw) / 2, ir.bottom() + th.px(2), lw, font_h);
                u.label(lr, &l1, th.text, Align::Center);
                if !l2.is_empty() {
                    u.label(Rect::new(lr.x, lr.bottom(), lw, font_h), &l2, th.text, Align::Center);
                }
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
                act = Some((k, u.mods()));
            }
        }
        // Рамка — ПОВЕРХ значков: она про них, и прятать её под ними бессмысленно.
        if let Some((a, b)) = self.band {
            let r = Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs());
            let fill = u.tint(th.accent.with_a(0x28));
            let line = u.tint(th.accent.with_a(0x99));
            u.c.rrect_bordered(r, th.px(2), th.line.max(1), fill, line);
        }
        // Подвал содержимого: ответ на последнее действие, а без него — не обрезан ли список.
        let foot = Rect::new(lay.body.x, lay.body.bottom() - font_h - th.px(4), lay.body.w, font_h);
        match (&self.flash, self.cut) {
            (Some(m), _) => {
                let m = m.clone();
                u.label(foot.inset_xy(th.px(8), 0), &m, th.accent, Align::Right);
            }
            (None, true) => {
                let m = alloc::format!("показаны не все: сервер отдал {} имён", total);
                u.label(foot.inset_xy(th.px(8), 0), &m, th.muted, Align::Right);
            }
            _ => {}
        };
        // Кадр дорисован по СНИМКУ — теперь можно менять снимок. Первый щелчок ВЫБИРАЕТ, щелчок
        // по уже выбранному — открывает. Двойного щелчка по ВРЕМЕНИ у нас нет и самодельного не
        // будет: часов у событий композитора нет, а мерить их самим значит завести своё понятие
        // «двойного» вразрез с системным.
        if let Some((k, mods)) = act.filter(|_| self.menu.is_none()) {
            self.click_entry(k, mods);
            dirty = true;
        }
        dirty
    }

    /// Веха 167 — щелчок по записи с учётом модификаторов.
    ///
    /// Три жеста, и они не выдуманы: `Ctrl` добавляет по одному, `Shift` берёт полосу от якоря,
    /// простой щелчок начинает выделение заново. Второй щелчок по УЖЕ выделенной одиночке
    /// открывает — двойного щелчка по времени у нас нет ([[void-fm]]).
    fn click_entry(&mut self, k: usize, mods: u8) {
        let ctrl = mods & win::modk::CTRL != 0;
        let shift = mods & win::modk::SHIFT != 0;
        if ctrl {
            match self.marked.iter().position(|&x| x == k) {
                Some(i) => {
                    self.marked.remove(i);
                }
                None => self.marked.push(k),
            }
            self.sel = k;
            self.anchor = k;
            return;
        }
        if shift {
            let (a, b) = (self.anchor.min(k), self.anchor.max(k));
            self.marked = (a..=b).collect();
            self.sel = k;
            return;
        }
        if self.marked.len() == 1 && self.marked[0] == k && self.sel == k {
            self.open_sel();
            return;
        }
        // Веха 167.3 — нажатие по УЖЕ ВЫДЕЛЕННОМУ выделение НЕ РУШИТ: с него начинают
        // перетаскивание группы, и сбросить её здесь значит уронить в цель ровно один файл —
        // тот, за который взялись. Ровно это владелец и увидел после рамки.
        //
        // Схлопнуть выделение до одного всё-таки надо — но НА ОТПУСКАНИИ и только если не
        // потащили: «щёлкнул по одному из выделенных» и «взялся за выделенное» отличаются
        // именно тем, повёл ли человек рукой.
        if self.marked.len() > 1 && self.marked.contains(&k) {
            self.sel = k;
            self.anchor = k;
            self.collapse_to = Some(k);
            return;
        }
        self.pick_one(k);
        if let Some(&i) = self.hits.get(k) {
            self.measure_sel(i);
        }
    }

    /// Веха 167.2 — пересобрать выделение по РАМКЕ: всё, чью ячейку она задела.
    ///
    /// Задела, а не «накрыла целиком»: рамкой ведут наспех, и требовать полного накрытия значит
    /// требовать аккуратности там, где человек её не проявляет.
    fn mark_in_band(&mut self, a: (i32, i32), b: (i32, i32)) {
        let r = Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs());
        self.marked = self.band_base.clone();
        let end = (self.top + self.lay.page).min(self.hits.len());
        for k in self.top..end {
            let cell = self.cell_rect(&self.lay, k - self.top);
            if !cell.intersect(r).is_empty() && !self.marked.contains(&k) {
                self.marked.push(k);
            }
        }
        // Курсор ставим на последнюю задетую: с неё пойдёт следующая `Shift`-полоса.
        if let Some(&k) = self.marked.last() {
            self.sel = k;
            self.anchor = k;
        }
    }

    /// Выделить ровно одну запись и сделать её якорем.
    fn pick_one(&mut self, k: usize) {
        self.sel = k;
        self.anchor = k;
        self.marked.clear();
        self.marked.push(k);
    }

    /// Пути всего выделенного (или, если не выделено ничего, — пусто).
    fn marked_paths(&self) -> Vec<String> {
        let mut v: Vec<usize> = self.marked.clone();
        v.sort_unstable();
        v.dedup();
        v.iter().filter_map(|&k| self.path_of(k)).collect()
    }

    /// Веха 167 — ПЕРЕНЕСТИ сюда всё, что уронили. Возвращает, сколько перенеслось.
    ///
    /// Перенос, а не переход по пути: уронить объект в окно менеджера значит «положи его сюда»,
    /// и другого смысла у этого жеста нет. Переход остался у ВСТАВКИ (`Ctrl+V`) — там человек
    /// назвал путь, а не объект.
    ///
    /// Делает его `rename` файлового сервера: у него уже есть правило POSIX «цель — существующий
    /// каталог, значит внутрь него» (Веха 97.1), и второго такого правила заводить не надо.
    fn move_here(&mut self, paths: &[String]) {
        let (mut ok, mut fail) = (0usize, 0usize);
        for p in paths {
            // Своё же место — не работа, а недоразумение: молча ничего не делаем.
            if Self::parent(p) == self.cwd {
                continue;
            }
            if px::rename(self.ep, p.as_bytes(), self.cwd.as_bytes()) == 0 {
                ok += 1;
            } else {
                fail += 1;
            }
        }
        self.flash = Some(match (ok, fail) {
            (0, 0) => String::from("это уже здесь"),
            (n, 0) => alloc::format!("перенесено: {n}"),
            (0, f) => alloc::format!("не перенести: {f} (каталоги пока не переносятся)"),
            (n, f) => alloc::format!("перенесено: {n}, не вышло: {f}"),
        });
        if ok > 0 {
            self.read();
        }
    }

    /// Программа по умолчанию для роли: строка `default <роль> <имя>` конфига поколения.
    fn default_app(&self, role: &str) -> Option<String> {
        void_conf::of(&self.generation, "default")
            .find(|e| e.key() == role)
            .map(|e| String::from(e.tail().trim()))
            .filter(|s| !s.is_empty())
    }

    /// Веха 167 — запустить программу роли `role` над путём `arg`.
    ///
    /// Через СТОРОЖА (`run`), как это делает строка запуска: досмотреть запуск может только
    /// родитель, а менеджер на эту роль не годится — он живёт своей жизнью и ждать чужого кода
    /// выхода ему нечем ([[launcher]], Веха 147).
    fn open_with(&mut self, role: &str, arg: &str) {
        let Some(prog) = self.default_app(role) else {
            self.flash = Some(alloc::format!("в конфиге нет строки `default {role} …`"));
            return;
        };
        let Some(store) = self.store else {
            self.flash = Some(String::from("нет права на store — запускать нечем"));
            return;
        };
        let mut a: Vec<u8> = Vec::new();
        a.extend_from_slice(prog.as_bytes());
        a.push(0);
        a.extend_from_slice(arg.as_bytes());
        a.push(0);
        if sys::spawn(store, b"run", &a).is_none() {
            self.flash = Some(alloc::format!("не запустить: {prog}"));
        } else {
            self.flash = Some(alloc::format!("{prog}: {arg}"));
        }
    }

    /// Веха 167 — закладки ЖИВУТ ФАЙЛОМ (`/etc/fm.marks`, по пути на строку).
    ///
    /// Не в конфиге поколения, хотя соблазн был: закладка — не свойство системы, а привычка
    /// человека, и требовать ради неё `rebuild` со сменой поколения значило бы приравнять
    /// «добавил папку в боковую колонку» к «переставил службу». Файлом же её видно и правит
    /// любой редактор.
    const MARKS_FILE: &'static str = "/etc/fm.marks";

    /// Прочитать закладки; строки, которых на диске уже нет, отбрасываются молча.
    fn load_marks(&mut self) {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(text) = ui::font::read_path(Self::MARKS_FILE) {
            for line in String::from_utf8_lossy(&text).lines() {
                let p = line.trim();
                if p.starts_with('/') && px::stat(self.ep, p.as_bytes()).is_some_and(|(d, _)| d) {
                    out.push((String::from(p), Self::leaf_name(p)));
                }
            }
        }
        if out.is_empty() {
            // Пусто (или файла нет) — умолчание из мест, которые в системе есть всегда.
            out = MARKS
                .iter()
                .filter(|(p, _)| px::stat(self.ep, p.as_bytes()).is_some_and(|(d, _)| d))
                .map(|(p, l)| (String::from(*p), String::from(*l)))
                .collect();
        }
        self.marks = out;
    }

    /// Записать закладки обратно в файл.
    fn save_marks(&mut self) {
        let mut text = String::new();
        for (p, _) in &self.marks {
            text.push_str(p);
            text.push('\n');
        }
        let fd = px::open(self.ep, Self::MARKS_FILE.as_bytes(), px::O_TRUNC);
        if fd == usize::MAX {
            self.flash = Some(String::from("закладки не записать"));
            return;
        }
        px::write(self.ep, fd, text.as_bytes());
        px::close(self.ep, fd);
    }

    /// Имя последнего звена пути (для подписи закладки). У корня имени нет — так и назовём.
    fn leaf_name(p: &str) -> String {
        match p.trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()) {
            Some(n) => String::from(n),
            None => String::from("корень"),
        }
    }

    fn add_mark(&mut self, path: String) {
        if !px::stat(self.ep, path.as_bytes()).is_some_and(|(d, _)| d) {
            self.flash = Some(String::from("в закладки кладём каталоги"));
            return;
        }
        if self.marks.iter().any(|(p, _)| *p == path) {
            self.flash = Some(String::from("уже в закладках"));
            return;
        }
        let name = Self::leaf_name(&path);
        self.marks.push((path, name));
        self.save_marks();
        self.flash = Some(String::from("добавлено в закладки"));
    }

    fn drop_mark(&mut self, k: usize) {
        if k >= self.marks.len() {
            return;
        }
        self.marks.remove(k);
        self.save_marks();
        self.flash = Some(String::from("закладка убрана"));
    }

    /// Начать правку: адреса, имени записи или имени нового объекта.
    fn begin_edit(&mut self, what: What) {
        let text = match what {
            What::Path => self.cwd.clone(),
            What::Rename(k) => self
                .hits
                .get(k)
                .map(|&i| self.entries[i].name.clone())
                .unwrap_or_default(),
            What::Create { .. } => String::new(),
        };
        self.edit = Some((what, ui::Edit::all(text)));
        self.edit_fresh = true;
    }

    /// Довести правку до конца: применить набранное.
    fn finish_edit(&mut self, what: What, text: String) {
        let text = text.trim().to_string();
        match what {
            What::Path => {
                self.goto_path(&text);
                // Путь никуда не увёл — правку не закрываем: человеку править её же.
                if self.flash.is_some() {
                    self.edit = Some((What::Path, ui::Edit::tail(text)));
                }
            }
            What::Rename(k) => {
                if text.is_empty() || text.contains('/') {
                    self.flash = Some(String::from("имя без косых черт и не пустое"));
                    return;
                }
                // Каталог переименовать НЕЧЕМ, и сказать это надо до попытки. У файлового
                // сервера корни объектов названы ПУТЯМИ (`f/etc/x`, `d/etc/x`), поэтому
                // переименование каталога — это переименование корня каждого потомка вглубь;
                // операции для этого в протоколе нет. Молчаливое «не вышло» тут хуже отказа:
                // человек начинает искать причину в имени.
                if self.hits.get(k).is_some_and(|&i| self.entries[i].dir) {
                    self.flash = Some(String::from("каталоги пока не переименовываются"));
                    return;
                }
                let Some(old) = self.path_of(k) else { return };
                let new = Self::join(&self.cwd, &text);
                if px::rename(self.ep, old.as_bytes(), new.as_bytes()) == 0 {
                    self.flash = Some(alloc::format!("переименовано: {text}"));
                    self.read();
                } else {
                    self.flash = Some(String::from("переименовать не вышло"));
                }
            }
            What::Create { dir } => {
                if text.is_empty() || text.contains('/') {
                    self.flash = Some(String::from("имя без косых черт и не пустое"));
                    return;
                }
                let p = Self::join(&self.cwd, &text);
                let ok = if dir {
                    px::mkdir(self.ep, p.as_bytes()) == 0
                } else {
                    // Файл заводится ОТКРЫТИЕМ: у персоналии `open` создаёт пустой, и отдельной
                    // операции «создать файл» в протоколе нет. Закрыть обязательно — иначе он
                    // останется висеть в слоте сервера незаписанным.
                    let fd = px::open(self.ep, p.as_bytes(), 0);
                    if fd != usize::MAX {
                        px::close(self.ep, fd);
                        true
                    } else {
                        false
                    }
                };
                self.flash = Some(if ok {
                    alloc::format!("создано: {text}")
                } else {
                    String::from("создать не вышло")
                });
                if ok {
                    self.read();
                }
            }
        }
    }

    /// Полный путь записи `k` в отобранном списке.
    fn path_of(&self, k: usize) -> Option<String> {
        let i = *self.hits.get(k)?;
        Some(Self::join(&self.cwd, &self.entries.get(i)?.name))
    }

    /// Веха 166.2 — исполнить пункт контекстного меню.
    ///
    /// Все действия — над ОДНОЙ записью либо над текущим каталогом; ни одно не спрашивает имени,
    /// потому что спрашивать его пока негде: поля ввода в менеджере ровно одно, и оно адресное.
    /// Переименование и «создать каталог» приедут вместе со вторым — не раньше.
    fn do_act(&mut self, a: Act, target: Tgt) {
        let entry = match target {
            Tgt::Entry(k) => Some(k),
            _ => None,
        };
        match a {
            Act::Open => {
                if let Some(k) = entry {
                    self.pick_one(k);
                    self.open_sel();
                }
            }
            // В буфер уезжает ПУТЬ текстом: это то, что можно вставить куда угодно ещё —
            // в терминал, в редактор, в адресную строку. Возить содержимое файла было бы
            // догадкой о том, чего человек хотел.
            Act::Copy => {
                let p = match entry {
                    Some(k) => self.path_of(k).unwrap_or_else(|| self.cwd.clone()),
                    None => self.cwd.clone(),
                };
                self.flash_clip(&p);
            }
            Act::Paste => self.paste(),
            Act::Refresh => self.read(),
            Act::Rename => {
                if let Some(k) = entry {
                    self.begin_edit(What::Rename(k));
                }
            }
            Act::NewDir => self.begin_edit(What::Create { dir: true }),
            Act::NewFile => self.begin_edit(What::Create { dir: false }),
            // Закладка — это путь в списке слева. Хранится файлом (`/etc/fm.marks`), а не в
            // конфиге поколения: закладка не свойство СИСТЕМЫ, и требовать ради неё `rebuild`
            // было бы издевательством.
            Act::Mark => {
                let p = match entry.and_then(|k| self.path_of(k)) {
                    Some(p) => p,
                    None => self.cwd.clone(),
                };
                self.add_mark(p);
            }
            Act::Unmark => {
                if let Tgt::Mark(k) = target {
                    self.drop_mark(k);
                }
            }
            // Веха 167 — ОТКРЫТЬ В ТЕРМИНАЛЕ. Какая программа терминал, решает не менеджер, а
            // конфиг (`default terminal …` в `apps.vv`): «чем открывать» — выбор человека, и
            // зашивать его в приложение значило бы отобрать этот выбор у него.
            Act::Term => {
                let dir = match entry.and_then(|k| self.path_of(k)) {
                    // По каталогу — в него; по файлу — в его каталог (открыть файл терминалом
                    // нечем, а показать, где он лежит, — можно).
                    Some(p) if px::stat(self.ep, p.as_bytes()).is_some_and(|(d, _)| d) => p,
                    Some(p) => Self::parent(&p),
                    None => self.cwd.clone(),
                };
                self.open_with("terminal", &dir);
            }
            Act::Delete => {
                if entry.is_none() {
                    return;
                }
                // Сносится ВСЁ ВЫДЕЛЕННОЕ: меню открывалось по одному из них, но выделение
                // человек делал руками, и «удалить» при пяти выделенных значит пять.
                let paths = self.marked_paths();
                let (mut ok, mut fail) = (0usize, 0usize);
                for p in &paths {
                    // `unlink` у посикс-персоны сносит файл или ПУСТОЙ каталог. Непустой она не
                    // трогает, и это правильно: рекурсивное удаление — отдельное решение, а не
                    // побочный смысл того же пункта меню.
                    if px::unlink(self.ep, p.as_bytes()) == 0 {
                        ok += 1;
                    } else {
                        fail += 1;
                    }
                }
                self.flash = Some(match (ok, fail) {
                    (n, 0) => alloc::format!("удалено: {n}"),
                    (0, f) => alloc::format!("не удалить: {f} (каталог не пуст?)"),
                    (n, f) => alloc::format!("удалено: {n}, не вышло: {f}"),
                });
                if ok > 0 {
                    self.marked.clear();
                    self.read();
                }
            }
        }
    }

    /// Положить текст в буфер обмена и сказать об этом. Молчащее «скопировано» неотличимо от
    /// «ничего не произошло», а буфер снаружи не видно.
    fn flash_clip(&mut self, text: &str) {
        let ok = match self.store {
            Some(c) => win::clip_put(c, win::CLIP_TEXT, text.as_bytes()),
            None => false,
        };
        self.flash = Some(if ok {
            alloc::format!("скопировано: {text}")
        } else {
            String::from("буфер обмена недоступен (нет права на store?)")
        });
    }

    /// Текст из буфера обмена. `None` — пусто, не текст либо нет права на store.
    fn clip_text(&self) -> Option<String> {
        let store = self.store?;
        let mut buf = alloc::vec![0u8; 4096];
        let (kind, got, _) = win::clip_read(store, &mut buf)?;
        (kind == win::CLIP_TEXT)
            .then(|| String::from_utf8_lossy(&buf[..got.min(buf.len())]).trim().to_string())
    }

    /// Вставить из буфера: если там путь — перейти по нему. Каталог открывается, файл
    /// выделяется в своём каталоге.
    fn paste(&mut self) {
        match self.clip_text() {
            Some(t) => self.goto_path(&t),
            None => self.flash = Some(String::from("буфер обмена пуст")),
        }
    }

    /// Перейти по пути, откуда бы он ни пришёл: из буфера, из уроненного, из адресной строки.
    fn goto_path(&mut self, text: &str) {
        if !text.starts_with('/') {
            self.flash = Some(alloc::format!("это не путь: {text}"));
            return;
        }
        match px::stat(self.ep, text.as_bytes()) {
            Some((true, _)) => {
                self.flash = None;
                self.go(String::from(text));
            }
            // Файл — открыть его КАТАЛОГ и выделить сам файл: показать человеку то, что он
            // назвал, ближе, чем отказать.
            Some((false, _)) => {
                let dir = Self::parent(text);
                let name = text.rsplit('/').next().unwrap_or("").to_string();
                self.flash = None;
                self.go(dir);
                if let Some(k) = self.hits.iter().position(|&i| self.entries[i].name == name) {
                    self.pick_one(k);
                }
            }
            None => self.flash = Some(alloc::format!("нет такого пути: {text}")),
        }
    }

    /// Докуда можно прокрутить: чтобы ПОСЛЕДНЯЯ СТРАНИЦА была полной, а не «последний значок и
    /// пустота под ним».
    ///
    /// Потолком стояло `len - 1`, и колесо в любом каталоге уматывало список так, что на экране
    /// оставалась ровно одна запись — та самая «прокрутка всё прячет» (Веха 166.2). Число
    /// округляется вверх до границы РЯДА: иначе в сетке последний ряд оказывался бы срезанным
    /// по половине.
    fn max_top(&self) -> usize {
        let page = self.lay.page.max(1);
        let row = if self.grid { (self.lay.cols as usize).max(1) } else { 1 };
        let over = self.hits.len().saturating_sub(page);
        over.div_ceil(row) * row
    }

    /// Какая запись под точкой экрана. `None` — пустое место содержимого либо вообще не оно.
    ///
    /// Считается ТЕМ ЖЕ кодом, которым ячейки рисуются ([`App::cell_rect`]): два расчёта «где
    /// что» — это два случая разойтись, и система на них уже стояла.
    fn entry_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.lay.body.contains(x, y) {
            return None;
        }
        let end = (self.top + self.lay.page).min(self.hits.len());
        (self.top..end).find(|&k| self.cell_rect(&self.lay, k - self.top).contains(x, y))
    }

    /// Какая ЗАКЛАДКА под точкой. Считается тем же кодом, которым колонка рисуется.
    fn mark_at(&self, x: i32, y: i32) -> Option<usize> {
        if !self.lay.side.contains(x, y) {
            return None;
        }
        (0..self.marks.len()).find(|&k| self.mark_rect(&self.lay, k).contains(x, y))
    }

    /// Место строки закладки `k`. ОДИН расчёт на рисование и на попадание.
    ///
    /// Раскладка приезжает ПАРАМЕТРОМ, а не берётся из `self.lay`: на время кадра она оттуда
    /// вынута (`core::mem::take` в `draw`), и поле там — нули. На этом строки закладок один раз
    /// уже нарисовались в точке (0,0) размером ноль, то есть исчезли совсем.
    fn mark_rect(&self, lay: &Lay, k: usize) -> Rect {
        let (pad, gap) = (lay.mark_pad, lay.mark_gap);
        let h = lay.mark_h;
        Rect::new(lay.side.x + pad, lay.mark_y + k as i32 * (h + gap), lay.side.w - 2 * pad, h)
    }

    /// Веха 166.2 — начать ТАЩИТЬ: композитору уезжают пути текстом и подпись под курсор.
    ///
    /// Возится ровно то же, что кладётся в буфер обмена, — пути, по одному на строку. Значит
    /// уронить значок можно и в терминал (там он вставится строкой), и в другое окно менеджера
    /// (там он ПЕРЕЕДЕТ): получателю не нужно знать про файловый менеджер ничего.
    ///
    /// Веха 167 — тащится ВСЁ ВЫДЕЛЕННОЕ, а подпись под курсором считает их: тащить пять
    /// объектов и видеть имя одного — врать про то, что сейчас произойдёт.
    fn start_drag(&mut self) {
        let Some(store) = self.store else { return };
        let paths = self.marked_paths();
        if paths.is_empty() {
            return;
        }
        let label = match paths.len() {
            1 => Self::leaf_name(&paths[0]),
            n => alloc::format!("{n} объектов"),
        };
        win::drag(store, win::CLIP_TEXT, paths.join("\n").as_bytes(), &label);
    }

    /// Веха 166.2 — КОНТЕКСТНОЕ МЕНЮ по правой кнопке.
    ///
    /// Пункты собираются по МЕСТУ нажатия: по записи — про неё, по пустому месту — про текущий
    /// каталог. Меню, в котором половина пунктов серая, ничем не лучше меню, которого нет: серый
    /// пункт всё равно надо прочесть, чтобы узнать, что он недоступен.
    fn paint_menu(&mut self, u: &mut Ui, th: &Theme) -> bool {
        let Some((mx, my, target)) = self.menu else { return false };
        let font_h = u.font.line_h();
        let row = font_h + th.px(6);
        let mut items: Vec<(Act, String)> = Vec::new();
        let many = self.marked.len() > 1;
        match target {
            Tgt::Entry(k) => {
                let dir = self.hits.get(k).is_some_and(|&i| self.entries[i].dir);
                if dir && !many {
                    items.push((Act::Open, String::from("открыть")));
                }
                if !many {
                    items.push((Act::Rename, String::from("переименовать")));
                }
                items.push((Act::Copy, String::from("копировать путь")));
                if dir && !many {
                    items.push((Act::Term, String::from("открыть в терминале")));
                    items.push((Act::Mark, String::from("в закладки")));
                }
                items.push((
                    Act::Delete,
                    String::from(match (self.armed, many) {
                        (true, _) => "удалить? ещё раз",
                        (false, true) => "удалить выделенное",
                        (false, false) => "удалить",
                    }),
                ));
            }
            Tgt::Mark(_) => items.push((Act::Unmark, String::from("убрать закладку"))),
            Tgt::Empty => {
                items.push((Act::Term, String::from("открыть в терминале")));
                items.push((Act::NewDir, String::from("создать каталог")));
                items.push((Act::NewFile, String::from("создать файл")));
                items.push((Act::Paste, String::from("вставить путь")));
                items.push((Act::Copy, String::from("копировать путь каталога")));
                items.push((Act::Mark, String::from("этот каталог в закладки")));
                items.push((Act::Refresh, String::from("обновить")));
            }
        }
        let tw = items.iter().map(|(_, t)| u.text_w(t)).max().unwrap_or(0);
        let w = tw + 4 * th.pad;
        let h = row * items.len() as i32 + 2 * th.px(4);
        // У края экрана меню разворачивается ВНУТРЬ: половина меню за краем — это половина меню.
        let r = Rect::new(
            mx.min(self.w - w - th.px(4)).max(0),
            my.min(self.h - h - th.px(4)).max(0),
            w,
            h,
        );
        u.popup(r);
        let mut d = r.inset(th.px(4));
        let mut chosen: Option<Act> = None;
        for (a, text) in &items {
            let rr = d.cut_top(row);
            if u.hot(rr) {
                let c = u.tint(th.text.with_a(0x14));
                u.c.rrect(rr, th.radius.min(rr.h / 2), c);
            }
            let col = if *a == Act::Delete { th.danger } else { th.text };
            u.label(rr.inset_xy(th.pad, 0), text, col, Align::Left);
            if u.clicked(rr) && !self.menu_fresh {
                chosen = Some(*a);
            }
        }
        // Щелчок мимо меню закрывает его — тот же жест, что закрывает меню оболочки.
        let outside = u.click().is_some_and(|(cx, cy)| !r.contains(cx, cy));
        self.menu_fresh = false;
        if let Some(a) = chosen {
            // «Удалить» взводится, а не срабатывает: второй щелчок по тому же пункту — согласие.
            if a == Act::Delete && !self.armed {
                self.armed = true;
                return true;
            }
            self.armed = false;
            self.menu = None;
            self.do_act(a, target);
            return true;
        }
        if outside {
            self.menu = None;
            self.armed = false;
            return true;
        }
        false
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

/// Веха 166.2 — **имя в ДВЕ строки**: сколько влезло, остальное на вторую, и только если и там
/// не помещается — многоточием.
///
/// Резать сразу многоточием было проще, но неверно: имена в store длинные и различаются как раз
/// хвостом (`networking.vv` и `network.vv`), а обрезанные по одной ширине они сливаются в одно.
/// Перенос показывает на строку больше и режет заметно реже.
///
/// Многоточие — ТРИ ТОЧКИ, а не знак `…`: во встроенном шрифте 8×16 его глифа нет, и на экране
/// он выходит посторонней буквой (уже проверено на кавычках-ёлочках).
fn wrap2(font: &mut Font, s: &str, w: i32) -> (String, String) {
    if font.width(s) <= w {
        return (String::from(s), String::new());
    }
    let cut = fit(font, s, w);
    let (a, b) = s.split_at(cut);
    if font.width(b) <= w {
        (String::from(a), String::from(b))
    } else {
        (String::from(a), ellipsis(font, b, w))
    }
}

/// Байтовый индекс, до которого строка ещё влезает в `w`. Не меньше одного символа: ноль дал бы
/// пустую строку и вечный перенос одного и того же хвоста.
fn fit(font: &mut Font, s: &str, w: i32) -> usize {
    let mut last = 0;
    for (i, _) in s.char_indices().skip(1) {
        if font.width(&s[..i]) > w {
            break;
        }
        last = i;
    }
    if last == 0 {
        return s.char_indices().nth(1).map_or(s.len(), |(i, _)| i);
    }
    last
}

/// Хвост, не влезающий даже во вторую строку, — с тремя точками.
fn ellipsis(font: &mut Font, s: &str, w: i32) -> String {
    let dots = font.width("...");
    let cut = fit(font, s, (w - dots).max(0));
    let mut out = String::from(&s[..cut]);
    out.push_str("...");
    out
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
            Event::Key { sym: code, ch, mods, down } if down => {
                // Веха 166.2 — буфер обмена С КЛАВИАТУРЫ, теми же аккордами, что везде. Меню
                // мышью и аккорд клавишами — два входа в одно действие, и второй нужен ровно
                // потому, что до первого надо дотянуться рукой.
                //
                // Веха 167.3 — но ТОЛЬКО когда ничего не правят. Пока в поле стоит курсор,
                // `Ctrl+C` — про выделенный ТЕКСТ, и разбирает его правка ниже; этот же блок
                // стоял выше неё и перехватывал аккорд себе, копируя весь путь целиком. Снаружи
                // это выглядело как «выделил кусок, а скопировалось всё».
                if self.edit.is_none() && mods & win::modk::CTRL != 0 {
                    // Какая БУКВА нажата. Спрашивать одно поле нельзя: у печатающей клавиши код
                    // (`sym`) и есть её знак, а `ch` при зажатом Ctrl приходит то нулём, то
                    // управляющим кодом (`Ctrl+C` = 0x03) — смотря чем набрано. Терминал живёт
                    // по тому же правилу: текст берётся только без Ctrl.
                    let letter = match (code, ch) {
                        (c, _) if (0x41..=0x5a).contains(&c) => (c as u8) | 0x20,
                        (c, _) if (0x61..=0x7a).contains(&c) => c as u8,
                        (_, c) if (0x01..=0x1a).contains(&c) => (c as u8) + 0x60,
                        (_, c) if (0x41..=0x7a).contains(&c) => (c as u8) | 0x20,
                        _ => 0,
                    };
                    match letter {
                        b'c' => {
                            let p = self
                                .path_of(self.sel)
                                .filter(|_| !self.marked.is_empty())
                                .unwrap_or_else(|| self.cwd.clone());
                            self.flash_clip(&p);
                            return ui::Scope::All;
                        }
                        b'v' => {
                            self.paste();
                            return ui::Scope::All;
                        }
                        _ => return ui::Scope::No,
                    }
                }
                // Правка пути забирает клавиатуру целиком: набирать в двух местах сразу нельзя,
                // а «половина букв в путь, половина в поиск» — ровно это и было бы.
                if let Some((what, mut e)) = self.edit.take() {
                    // Внутри правки `Ctrl+C/V` — про ТЕКСТ, а не про файлы: рука на тех же
                    // клавишах, а смысл диктует место, где стоит курсор.
                    if mods & win::modk::CTRL != 0 {
                        let letter = match (code, ch) {
                            (c, _) if (0x41..=0x5a).contains(&c) => (c as u8) | 0x20,
                            (c, _) if (0x61..=0x7a).contains(&c) => c as u8,
                            (_, c) if (0x01..=0x1a).contains(&c) => (c as u8) + 0x60,
                            _ => 0,
                        };
                        match letter {
                            b'c' if !e.selected().is_empty() => {
                                let t = String::from(e.selected());
                                self.flash_clip(&t);
                            }
                            b'v' => {
                                if let Some(t) = self.clip_text() {
                                    e.insert_str(&t);
                                }
                            }
                            _ => {
                                // `Ctrl+A` и прочее разбирает сама правка.
                                e.key(code, ch, mods);
                            }
                        }
                        self.edit = Some((what, e));
                        return ui::Scope::All;
                    }
                    match e.key(code, ch, mods) {
                        ui::edit::Hit::Done => self.finish_edit(what, e.text.clone()),
                        ui::edit::Hit::Cancel => {}
                        _ => self.edit = Some((what, e)),
                    }
                    return ui::Scope::All;
                }
                // F2 — переименовать выбранное. Тот же смысл, что и пункт меню; аккорд нужен
                // потому, что до меню надо дотянуться рукой.
                // F2 — код клавиши из раскладки ядра (`F1..F10` = 0x120..0x129).
                if code == 0x121 && !self.marked.is_empty() {
                    self.begin_edit(What::Rename(self.sel));
                    return ui::Scope::All;
                }
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
                // Стрелка ведёт КУРСОР; с `Shift` за ним тянется полоса от якоря, без него
                // выделение начинается заново — то же правило, что у щелчка.
                if mods & win::modk::SHIFT != 0 {
                    let (a, b) = (self.anchor.min(self.sel), self.anchor.max(self.sel));
                    self.marked = (a..=b).collect();
                } else {
                    self.pick_one(self.sel);
                }
                let page = self.lay.page;
                self.scroll_to_sel(page, cols);
                if let Some(&i) = self.hits.get(self.sel) {
                    self.measure_sel(i);
                }
                ui::Scope::All
            }
            Event::Wheel { delta, .. } => {
                let step = if self.grid { (self.lay.cols as usize).max(1) } else { 1 };
                if delta < 0 {
                    self.top = (self.top + step).min(self.max_top());
                } else {
                    self.top = self.top.saturating_sub(step);
                }
                ui::Scope::All
            }
            Event::Motion { x, y } => {
                // Веха 167.2 — тянут РАМКУ: пересобрать выделение по тому, что она накрыла.
                if let (Some((a, _)), Some(_)) = (self.band, input.held) {
                    let b = (x as i32, y as i32);
                    self.band = Some((a, b));
                    self.mark_in_band(a, b);
                    return ui::Scope::All;
                }
                // Веха 166.2 — ПЕРЕТАСКИВАНИЕ начинается не с нажатия, а с движения при зажатой
                // кнопке: иначе каждый щелчок был бы началом перетаскивания, и выбрать значок
                // мышью стало бы нельзя. Порог в несколько точек — про дрожание руки.
                if let (Some((px_, py_, _)), Some((x, y))) = (self.press, input.held) {
                    if !self.dragging && (x - px_).abs() + (y - py_).abs() > 6 {
                        self.dragging = true;
                        self.start_drag();
                    }
                }
                ui::Scope::All
            }
            Event::Button { x, y, buttons, down, mods } => {
                let (x, y) = (x as i32, y as i32);
                if !down {
                    // Отпустили, не потащив, — значит это был простой щелчок по одному из
                    // выделенных: выделение схлопывается до него.
                    if let Some(k) = self.collapse_to.take() {
                        if !self.dragging {
                            self.pick_one(k);
                        }
                    }
                    self.press = None;
                    self.dragging = false;
                    self.band = None;
                    self.edit_drag = false;
                    return ui::Scope::All;
                }
                // Правая — КОНТЕКСТНОЕ МЕНЮ. Бит 1, как его кодирует ядро (`SYS_MOUSE_READ`).
                if buttons & 2 != 0 {
                    // Правая по НЕ выделенному сперва выделяет: меню обязано относиться к тому,
                    // на что человек показывает, а не к тому, что осталось выделенным до этого.
                    let _ = mods;
                    let t = match (self.entry_at(x, y), self.mark_at(x, y)) {
                        (Some(k), _) => {
                            // Правая по НЕ выделенному сперва выделяет: меню обязано относиться
                            // к тому, на что человек показывает, а не к тому, что осталось
                            // выделенным до этого.
                            if !self.marked.contains(&k) {
                                self.pick_one(k);
                            }
                            Tgt::Entry(k)
                        }
                        (None, Some(m)) => Tgt::Mark(m),
                        _ => Tgt::Empty,
                    };
                    self.menu = Some((x, y, t));
                    self.menu_fresh = true;
                    self.armed = false;
                    return ui::Scope::All;
                }
                // Веха 169.1 — пока открыто КОНТЕКСТНОЕ МЕНЮ, левый щелчок принадлежит ему.
                //
                // Меню лежит поверх содержимого, и его прямоугольник — часть `lay.body`. Поэтому
                // щелчок по пункту меню доезжал сюда как «нажали на пустое место списка» и
                // начинал РАМКУ выделения, а рамка начинается с чистого листа — то есть стирала
                // `marked`. Ровно это владелец и увидел: первый щелчок по «удалить» взводил пункт
                // и ОДНОВРЕМЕННО снимал выделение, а второму удалять было уже нечего.
                //
                // Что делать со щелчком, решает сам кадр меню ([`Fm::paint_menu`]): попал в
                // пункт — действие, мимо — закрыть. Здесь остаётся не мешать.
                if self.menu.is_some() {
                    return ui::Scope::All;
                }
                // Левая по записи — запомнить точку: с неё может начаться перетаскивание.
                // Левая по пустому месту содержимого — начало РАМКИ выделения.
                self.collapse_to = None;
                self.press = self.entry_at(x, y).map(|k| (x, y, k));
                if self.press.is_none() && self.lay.body.contains(x, y) && self.edit.is_none() {
                    self.band = Some(((x, y), (x, y)));
                    // С `Ctrl` рамка ДОБАВЛЯЕТ к выделенному, без него — начинает заново.
                    self.band_base =
                        if mods & win::modk::CTRL != 0 { self.marked.clone() } else { Vec::new() };
                    self.marked = self.band_base.clone();
                }
                self.edit_drag = self.edit.is_some() && self.lay.addr.contains(x, y);
                ui::Scope::All
            }
            // Веха 166.2 — в нас УРОНИЛИ. Возят текст, и если это путь — идём по нему: то же
            // самое, что делает «вставить», и по той же причине.
            Event::Drop { .. } => {
                let Some(store) = self.store else { return ui::Scope::No };
                let mut buf = alloc::vec![0u8; 16 * 1024];
                if let Some((win::CLIP_TEXT, got, _)) = win::drop_read(store, &mut buf) {
                    let text = String::from_utf8_lossy(&buf[..got.min(buf.len())]).into_owned();
                    let paths: Vec<String> = text
                        .lines()
                        .map(|l| l.trim())
                        .filter(|l| l.starts_with('/'))
                        .map(String::from)
                        .collect();
                    if paths.is_empty() {
                        self.flash = Some(String::from("уронили не путь"));
                    } else {
                        self.move_here(&paths);
                    }
                }
                ui::Scope::All
            }
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    /// Веха 167 — **АВТООБНОВЛЕНИЕ**. И сразу честно: это ОПРОС, а не подписка.
    ///
    /// Подписки у файлового сервера нет — ни события «каталог изменился», ни счётчика поколения
    /// каталога наружу он не отдаёт. Завести их можно (и когда-нибудь стоит: подписка дешевле
    /// опроса и точнее), но это протокол `posixfs`, а не менеджера, и делать вид, что у нас она
    /// есть, нельзя.
    ///
    /// Поэтому раз в две секунды каталог перечитывается, и КАДР рисуется только если подпись
    /// содержимого изменилась. Цена честная: один `readdir` в две секунды на открытое окно;
    /// выбор и прокрутка при этом сохраняются по имени (см. [`App::read`]).
    fn wake(&mut self) -> Option<u32> {
        Some(2000)
    }

    fn tick(&mut self) -> ui::Scope {
        // Пока набирают имя или открыто меню — не трогаем ничего: перечитывание сдвинет номера
        // строк под рукой, а именно на этом менеджер уже падал (Веха 166.1).
        if self.edit.is_some() || self.menu.is_some() {
            return ui::Scope::No;
        }
        let was = self.sig;
        self.read();
        if self.sig != was { ui::Scope::All } else { ui::Scope::No }
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
    let (generation, th, mut font) = ui::app::boot();
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


    let mut app = App {
        w: w as i32,
        h: h as i32,
        ep,
        cwd: start,
        entries: Vec::new(),
        hits: Vec::new(),
        sel: 0,
        marked: Vec::new(),
        anchor: 0,
        top: 0,
        back: Vec::new(),
        fwd: Vec::new(),
        query: String::new(),
        grid: true,
        sig: 0,
        err: None,
        flash: None,
        cut: false,
        marks: Vec::new(),
        generation,
        lay: Lay::default(),
        edit: None,
        menu: None,
        edit_fresh: false,
        menu_fresh: false,
        armed: false,
        store: ui::conf::store_cap(),
        press: None,
        dragging: false,
        band: None,
        band_base: Vec::new(),
        collapse_to: None,
        edit_drag: false,
    };
    // Закладки читаются ОДИН раз, на старте: каталог, которого нет, в колонку не попадает.
    app.load_marks();
    app.read();

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
