//! `launcher` — СТРОКА ЗАПУСКА (Веха 146): что в системе есть и как это запустить.
//!
//! Третий клиент слоя после обоев и панели — и первый, кому нужна КЛАВИАТУРА. До этой вехи
//! клавиши доходили ровно до окна в фокусе, а слои получали только мышь; строка запуска в эту
//! щель и упёрлась. Флаг [`win::LAYER_KBD`] её и открывает.
//!
//! ## Почему это слой, а не окно
//!
//! Окно раскладки раздвинуло бы ленту: открыть строку запуска значило бы подвинуть всё, что
//! человек перед этим расставил, и вернуть обратно при закрытии. Поверхность слоя лежит поверх и
//! места у окон не занимает — экран остаётся тем же, просто на нём появляется стекло.
//!
//! Поверхность при этом **во весь экран**, хотя карточка занимает её середину. Тот же довод, что
//! у меню панели ([[menu]]): клик мимо обязан закрывать, а попаданий вне себя клиент не видит.
//!
//! ## Почему отдельная программа, а не часть панели
//!
//! Меню живёт в панели именно потому, что ждать событий можно только на ОДНОЙ поверхности. Здесь
//! этот довод работает в обратную сторону: строка запуска нужна и без панели (она может упасть,
//! её может не быть в конфиге), а держать её в чужом событийном цикле значит связать их судьбы.
//! Композитор запускает её по аккорду и получает переключатель бесплатно: открыта — просим
//! закрыться, закрыта — запускаем ([`wm`], действие `launcher-toggle`).
//!
//! ## Что она показывает
//!
//! Корни store `bin/<арх>/*` — то есть ВСЁ, что система умеет запустить, без прикрас. Здесь нет
//! списка «настоящих приложений»: такого знания в системе пока нет вовсе, и подделать его
//! зашитым перечнем значило бы соврать о содержимом store. Поиск делает шум неважным: набрал
//! «te» — остался `term`. Отбор по смыслу появится тогда, когда появятся ДАННЫЕ для него
//! (описание программы рядом с ней в store) — записано в [[known-gaps]].
//!
//! Набранное, не совпавшее ни с чем, запускается КАК ЕСТЬ: это строка запуска, а не только
//! список. Слова после первого уезжают программе аргументами.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{self, Event, Window};

// Тулкит и разбор корней store — общие модули по пути (см. `ui/mod.rs`, почему не крейт).
// Строка запуска берёт из тулкита далеко не всё: неиспользованное здесь не ошибка.
#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
#[allow(dead_code)]
#[path = "../roots.rs"]
mod roots;

use ui::{Align, Font, Motion, Rect, Theme, Ui};

/// Полноэкранный кадр 1280×800 RGBA — 4 МиБ; остальное на список и шрифт.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 24 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Корни программ этой архитектуры (`bin/<арх>/<имя>`, Веха 26).
#[cfg(target_arch = "x86_64")]
const PROG_PREFIX: &[u8] = b"bin/x86_64/";
#[cfg(target_arch = "riscv64")]
const PROG_PREFIX: &[u8] = b"bin/riscv64/";

/// Коды клавиш протокола окон (те же, что разбирает `term`).
const SYM_RETURN: u16 = 0x101;
const SYM_ESCAPE: u16 = 0x102;
const SYM_BACKSPACE: u16 = 0x104;
const SYM_UP: u16 = 0x112;
const SYM_DOWN: u16 = 0x113;
const SYM_PGUP: u16 = 0x116;
const SYM_PGDN: u16 = 0x117;

/// Сколько строк списка видно разом. Больше — карточка перестаёт помещаться на экран ноутбука,
/// меньше — поиск превращается в угадывание.
const ROWS: usize = 7;

/// Идентификатор анимации открытия в [`Motion`] (у клиента их может быть много, у нас одна).
const A_OPEN: u32 = 1;

fn say(s: &str) {
    sys::write_console(s.as_bytes());
}

/// Одна запись списка.
struct Item {
    name: String,
    /// Откуда она взялась — показывается подписью. Сегодня всегда «система»: пакеты профиля
    /// приносят свои `bin/` файлами, а не корнями store, и им нужен обход ФС (см. заметку).
    from: &'static str,
}

struct App {
    sw: i32,
    sh: i32,
    /// Набранное.
    text: String,
    items: Vec<Item>,
    /// Индексы подходящих под набранное — пересчитываются на каждое нажатие.
    hits: Vec<usize>,
    sel: usize,
    /// Первая видимая строка списка: список длиннее семи строк почти всегда.
    top: usize,
    /// Где курсор мыши (в координатах поверхности).
    ptr: Option<(i32, i32)>,
    click: Option<(i32, i32)>,
    /// Прямоугольник, нарисованный в прошлом кадре: его надо стереть, иначе карточка оставит
    /// за собой хвост, пока выезжает.
    last: Rect,
    mo: Motion,
}

impl App {
    /// Карточка целиком. `t` — насколько открылась (0..256): выезд это подъём, гаснущий вместе
    /// с прозрачностью, — так же, как у меню панели.
    fn card_rect(&self, th: &Theme, font: &Font, t: u32) -> Rect {
        let w = (self.sw * 2 / 5).clamp(th.px(360), th.px(560));
        let field = font.line_h() + th.px(14);
        let row = 2 * font.line_h() + th.px(14);
        let foot = font.line_h() + th.px(4);
        // Высота — ПО СПИСКУ, а не по потолку: карточка в полэкрана с одной строкой внутри
        // выглядит сломанной, а не просторной. Так же ведёт себя оболочка владельца.
        let h = 2 * th.pad + field + th.gap + self.rows() * row + foot;
        // Не по центру, а выше него: список растёт вниз, и карточка, посаженная в центр, всё
        // время выглядит съехавшей.
        let y = self.sh * 22 / 100;
        let lift = th.px(18) * (256 - t as i32) / 256;
        Rect::new((self.sw - w) / 2, y + lift, w, h)
    }

    /// Сколько строк списка показываем сейчас: столько, сколько нашлось, но не больше [`ROWS`].
    fn rows(&self) -> i32 {
        self.hits.len().min(ROWS) as i32
    }

    /// Пересобрать список подходящих. Совпадение — ПОДСТРОКА без учёта регистра: полноценный
    /// нечёткий поиск здесь был бы преждевременным, а подстрока честно объяснима человеку.
    fn filter(&mut self) {
        let q = self.text.trim().to_lowercase();
        let word = q.split(' ').next().unwrap_or("").to_string();
        self.hits.clear();
        for (i, it) in self.items.iter().enumerate() {
            if word.is_empty() || it.name.to_lowercase().contains(&word) {
                self.hits.push(i);
            }
        }
        // Точное совпадение — наверх: набрав `term` целиком, человек хочет `term`, а не
        // `terminal-что-то`, оказавшийся в списке раньше по алфавиту.
        if let Some(p) = self.hits.iter().position(|&i| self.items[i].name == word) {
            self.hits.swap(0, p);
        }
        self.sel = 0;
        self.top = 0;
    }

    /// Держать выбранную строку в видимом окне списка.
    fn scroll_to_sel(&mut self) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + ROWS {
            self.top = self.sel + 1 - ROWS;
        }
    }

    /// Что запустится по Enter: выбранная строка, а если список пуст — набранное как есть.
    fn target(&self) -> Option<(String, Vec<u8>)> {
        let mut words = self.text.split_whitespace();
        let first = words.next().unwrap_or("");
        let mut args: Vec<u8> = Vec::new();
        for w in words {
            args.extend_from_slice(w.as_bytes());
            args.push(0);
        }
        match self.hits.get(self.sel) {
            Some(&i) => Some((self.items[i].name.clone(), args)),
            None if !first.is_empty() => Some((first.to_string(), args)),
            None => None,
        }
    }

    /// Нарисовать кадр. Возвращает строку, по которой ЩЁЛКНУЛИ, — запускает её вызывающий:
    /// рисование не должно уметь запускать программы, иначе одно и то же действие оказалось бы
    /// в двух местах (клавиатура — в цикле, мышь — здесь).
    fn draw(&mut self, u: &mut Ui, th: &Theme, t: u32) -> Option<usize> {
        let r = self.card_rect(th, &*u.font, t);
        u.clear(self.last.union(r));
        self.last = r;
        u.fade(t);
        let mut inner = u.card(r);
        inner.cut_top(th.pad);

        let field = u.font.line_h() + th.px(14);
        let fr = inner.cut_top(field);
        u.field(fr, &self.text, "имя программы", true);
        inner.cut_top(th.gap);

        let row = 2 * u.font.line_h() + th.px(14);
        let mut hit = None;
        for k in 0..self.rows() as usize {
            let rr = inner.cut_top(row);
            let Some(&i) = self.hits.get(self.top + k) else { continue };
            let sel = if self.top + k == self.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            let letter = self.items[i].name.get(..1).unwrap_or("?").to_uppercase();
            if u.entry(rr, &self.items[i].name, self.items[i].from, &letter, sel, hot) {
                self.sel = self.top + k;
                hit = Some(i);
            }
        }

        // Счётчик внизу — не украшение: он единственный отвечает на «а всё ли я вижу».
        let foot = inner.cut_top(u.font.line_h() + th.px(4));
        let n = self.hits.len();
        let total = self.items.len();
        let s = if n == total {
            alloc::format!("{} программ", total)
        } else {
            alloc::format!("{} из {}", n, total)
        };
        u.label(foot, &s, th.muted, Align::Right);
        u.fade(256);
        hit
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some((sw, sh)) = win::screen() else {
        say("launcher: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };
    let generation = ui::conf::generation().unwrap_or_default();
    let th = Theme::from_config(&generation);
    let mut font = Font::load(th.font.as_deref(), th.font_px);

    // Список — ДО поверхности: пустая карточка, которая через мгновение наполняется, выглядит
    // подвисшей, а прочитать корни store быстрее, чем нарисовать первый кадр.
    let store = ui::conf::store_cap();
    let mut items: Vec<Item> = Vec::new();
    if let Some(scap) = store {
        if let Some(text) = roots::text(scap) {
            for s in roots::suffixes(&text, PROG_PREFIX) {
                if let Ok(name) = core::str::from_utf8(s) {
                    items.push(Item { name: name.to_string(), from: "система" });
                }
            }
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    if items.is_empty() {
        // Не молча: пустой список без объяснения человек читает как «система сломалась», а это
        // всего лишь отказ по правам на store.
        say("launcher: корни bin/* не читаются — списка не будет, но набрать имя можно\n");
    }

    let mut app = App {
        sw: sw as i32,
        sh: sh as i32,
        text: String::new(),
        items,
        hits: Vec::new(),
        sel: 0,
        top: 0,
        ptr: None,
        click: None,
        last: Rect::ZERO,
        mo: Motion::new(ui::anim::duration_from_config(&generation)),
    };
    app.filter();

    let Some(mut surf) = Window::layer(win::Layer::POPUP, sw, sh, "строка запуска") else {
        say("launcher: композитор не дал поверхность слоя\n");
        sys::exit(1);
    };

    let wm_ep = win::endpoint().unwrap_or(sys::NO_CAP);
    let mut closing = false;
    let mut redraw = true;
    loop {
        // Движение открытия: цель 256, пока живём, и 0, когда уходим. Закрытие доигрывает до
        // конца — иначе карточка исчезала бы рывком ровно в тот момент, когда человек на неё
        // смотрит.
        let now = sys::monotonic_ns();
        app.mo.begin(now);
        let t = app.mo.val(A_OPEN, if closing { 0 } else { 256 }) as u32;
        if closing && !app.mo.moving() {
            surf.destroy();
            sys::exit(0);
        }

        if redraw || app.mo.moving() {
            let (w, h) = (app.sw, app.sh);
            let mut u = Ui::new(surf.pixels(), w, h, &th, &mut font);
            u.input(app.ptr, app.click);
            let hit = app.draw(&mut u, &th, t);
            let d = u.dirty();
            if !d.is_empty() {
                // `damage` у нас и есть commit: композитор берёт кадр из общего буфера.
                surf.damage(d.x as u16, d.y as u16, d.w as u16, d.h as u16);
            }
            app.click = None;
            redraw = false;
            if let Some(i) = hit {
                let name = app.items[i].name.clone();
                launch(store, wm_ep, &name, &[]);
                closing = true;
            }
        }

        // Спим до события, а пока идёт движение — до следующего кадра. Опрос в цикле стоил бы
        // системе простоя (Веха 139.3), а движение без своего срока шло бы рывками.
        let ev = if app.mo.moving() {
            surf.next_event_timeout(ui::anim::FRAME_MS)
        } else {
            surf.next_event()
        };
        let Some(ev) = ev else { continue };
        match ev {
            Event::Key { sym, mods: _, ch, down } if down => {
                redraw = true;
                match sym {
                    SYM_ESCAPE => closing = true,
                    SYM_RETURN => {
                        if let Some((name, args)) = app.target() {
                            launch(store, wm_ep, &name, &args);
                        }
                        closing = true;
                    }
                    SYM_BACKSPACE => {
                        app.text.pop();
                        app.filter();
                    }
                    SYM_UP => {
                        app.sel = app.sel.saturating_sub(1);
                        app.scroll_to_sel();
                    }
                    SYM_DOWN => {
                        app.sel = (app.sel + 1).min(app.hits.len().saturating_sub(1));
                        app.scroll_to_sel();
                    }
                    SYM_PGUP => {
                        app.sel = app.sel.saturating_sub(ROWS);
                        app.scroll_to_sel();
                    }
                    SYM_PGDN => {
                        app.sel = (app.sel + ROWS).min(app.hits.len().saturating_sub(1));
                        app.scroll_to_sel();
                    }
                    _ => {
                        // Печатающая клавиша — и только она: управляющие символы в строку
                        // поиска попадать не должны, иначе Tab и Ctrl-что-нибудь молча
                        // «набирались» бы невидимыми знаками.
                        match char::from_u32(ch as u32) {
                            Some(c) if !c.is_control() => {
                                app.text.push(c);
                                app.filter();
                            }
                            _ => redraw = false,
                        }
                    }
                }
            }
            Event::Motion { x, y } => {
                app.ptr = if x == 0xffff { None } else { Some((x as i32, y as i32)) };
                redraw = true;
            }
            Event::Button { x, y, buttons: _, down } if down => {
                let r = app.card_rect(&th, &font, 256);
                if r.contains(x as i32, y as i32) {
                    // Строка под курсором отзовётся в `draw` — она же и запустится.
                    app.click = Some((x as i32, y as i32));
                    redraw = true;
                } else {
                    // Клик мимо карточки закрывает — ровно тот жест, ради которого поверхность
                    // и растянута на весь экран.
                    closing = true;
                }
            }
            Event::Close => closing = true,
            Event::Resize { w, h } => {
                app.sw = w as i32;
                app.sh = h as i32;
                redraw = true;
            }
            _ => {}
        }
    }
}

/// Запустить программу. Право на запуск у нас СВОЁ (`store` с `EXEC`), поэтому композитора не
/// просим: в capability-модели действие делает тот, у кого есть право, а не тот, кто «главный».
///
/// Эндпоинт композитора передаётся ребёнку под тем же именем `WM`, под которым мы получили его
/// сами, — иначе запущенная программа не нашла бы, у кого просить окно. Наш выход её не тронет:
/// право указывает на процесс КОМПОЗИТОРА, а отзывается при смерти лишь то, что указывает на
/// умершего (`cap::revoke_process`).
fn launch(store: Option<usize>, wm: usize, name: &str, args: &[u8]) {
    let Some(store) = store else {
        say("launcher: нет права на store — запускать нечем\n");
        return;
    };
    if sys::spawn_with_endpoint(store, name.as_bytes(), args, wm, b"WM\0").is_none() {
        // Говорим вслух: молчащая строка запуска неотличима от сломанной.
        say(&alloc::format!("launcher: не запустилось: {}\n", name));
    }
}
