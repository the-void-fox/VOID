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
//! ## Что она показывает (Веха 146.1 — ЯРЛЫКИ)
//!
//! Список — это **ярлыки**: корни store `app/<арх>/*`, данные о программе рядом с самой
//! программой (человеческое имя и подпись). Ярлык заводит себе та программа, которая ОТКРЫВАЕТ
//! ОКНО: запуск `bench` из строки запуска не даёт на экране ничего — ни окна, ни вывода, — и
//! показывать его среди приложений значило бы обещать то, чего не будет.
//!
//! Первая версия (Веха 146) показывала все корни `bin/<арх>/*`, потому что данных для отбора в
//! системе не существовало. Зашитый перечень «настоящих приложений» соврал бы о содержимом store;
//! ярлык не врёт — его пишет автор программы, и он сеется в store вместе с ней ([[shortcuts]]).
//!
//! Спрятанное не потеряно, и это важнее самого отбора:
//!
//! - **набранное ищется и среди программ без ярлыка** — но только если среди ярлыков не нашлось
//!   ничего. Так шум не лезет в глаза, а `bench` находится ровно тогда, когда его ищут;
//! - **набранное, не совпавшее ни с чем, запускается КАК ЕСТЬ**: это строка запуска, а не только
//!   список. Слова после первого уезжают программе аргументами.

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

/// Корни ЯРЛЫКОВ этой архитектуры (`app/<арх>/<имя>`, Веха 146.1) — данные о программе рядом с
/// программой. Сеет их ядро оттуда же, откуда программы (`seed_programs`).
#[cfg(target_arch = "x86_64")]
const APP_PREFIX: &[u8] = b"app/x86_64/";
#[cfg(target_arch = "riscv64")]
const APP_PREFIX: &[u8] = b"app/riscv64/";

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

/// Одна запись списка — программа store, с ярлыком или без.
struct Item {
    /// Корень программы в store (`term`) — то, что на самом деле запускается. Имя для человека
    /// берётся из ярлыка и может быть каким угодно, а запускается всегда ЭТО.
    root: String,
    /// Как её зовут для человека («Терминал»). Без ярлыка — тот же корень: выдумывать имя
    /// программе не из чего.
    title: String,
    /// Подпись под названием.
    info: String,
    /// Есть ли ярлык. Программы без ярлыка в списке не показываются, пока их не ищут по имени.
    shortcut: bool,
}

impl Item {
    /// Подходит ли под набранное. Ярлык ищется и по имени для человека, и по корню: «терм» и
    /// «term» обязаны находить одно и то же — иначе человеку пришлось бы помнить, на каком языке
    /// названа программа.
    fn matches(&self, word: &str) -> bool {
        self.root.to_lowercase().contains(word) || self.title.to_lowercase().contains(word)
    }
}

/// Значение ключа манифеста ярлыка: строки вида `<ключ> <значение>`.
///
/// Свой разбор, а не `ui::conf::entry`: там строка конфига поколения — `<вид> <ключ> <значение>`,
/// потому что в одном тексте живут записи разных видов. Здесь весь файл — про одну программу, и
/// вид был бы третьим словом, которое всегда одинаково.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for line in text.lines() {
        let Some(v) = line.trim().strip_prefix(key) else { continue };
        let Some(v) = v.strip_prefix(' ') else { continue };
        let v = v.trim();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

struct App {
    sw: i32,
    sh: i32,
    /// Набранное.
    text: String,
    items: Vec<Item>,
    /// Индексы подходящих под набранное — пересчитываются на каждое нажатие.
    hits: Vec<usize>,
    /// Показан ВТОРОЙ ярус — программы без ярлыка (среди ярлыков не нашлось ничего). Подпись
    /// внизу обязана об этом сказать: список приложений и список корней store — разные ответы на
    /// один и тот же набор букв.
    raw: bool,
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
    ///
    /// Ярусов два (Веха 146.1). Сначала ЯРЛЫКИ — то, что откроет окно. Если среди них не нашлось
    /// ничего, ищем среди всех корней store: спрятать программу от глаз и спрятать её от поиска —
    /// разные вещи, и вторая превратила бы отбор в ложь о содержимом системы.
    fn filter(&mut self) {
        let q = self.text.trim().to_lowercase();
        let word = q.split(' ').next().unwrap_or("").to_string();
        self.hits.clear();
        self.raw = false;
        for (i, it) in self.items.iter().enumerate() {
            if it.shortcut && (word.is_empty() || it.matches(&word)) {
                self.hits.push(i);
            }
        }
        if self.hits.is_empty() && !word.is_empty() {
            self.raw = true;
            for (i, it) in self.items.iter().enumerate() {
                if !it.shortcut && it.matches(&word) {
                    self.hits.push(i);
                }
            }
        }
        // Точное совпадение — наверх: набрав `term` целиком, человек хочет `term`, а не
        // `terminal-что-то`, оказавшийся в списке раньше по алфавиту.
        if let Some(p) = self.hits.iter().position(|&i| self.items[i].root == word) {
            self.hits.swap(0, p);
        }
        self.sel = 0;
        self.top = 0;
    }

    /// Сколько ярлыков всего — знаменатель для подписи внизу.
    fn shortcuts(&self) -> usize {
        self.items.iter().filter(|it| it.shortcut).count()
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
            Some(&i) => Some((self.items[i].root.clone(), args)),
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
            let it = &self.items[i];
            // Буква значка — из имени для человека: у «Терминала» это «Т», а не «t» от корня.
            let letter = it.title.chars().next().map_or(String::from("?"), |c| {
                c.to_uppercase().collect::<String>()
            });
            if u.entry(rr, &it.title, &it.info, &letter, sel, hot) {
                self.sel = self.top + k;
                hit = Some(i);
            }
        }

        // Подпись внизу — не украшение: она единственная отвечает на «а всё ли я вижу». С двумя
        // ярусами (Веха 146.1) у неё появилась вторая обязанность — сказать, КАКОЙ список сейчас
        // перед глазами: ярлыки или корни store.
        let foot = inner.cut_top(u.font.line_h() + th.px(4));
        let n = self.hits.len();
        let s = if n == 0 && !self.text.trim().is_empty() {
            alloc::format!("Enter запустит «{}»", self.text.trim())
        } else if self.raw {
            // Знаменатель — ВСЕ программы store, а не «те, у кого нет ярлыка»: человек ищет в
            // системе, а не в остатке от отбора, и второе число должно отвечать на «сколько их
            // всего».
            alloc::format!("{} из {} программ store", n, self.items.len())
        } else if self.text.trim().is_empty() {
            alloc::format!("ярлыков: {}", n)
        } else {
            alloc::format!("{} из {} ярлыков", n, self.shortcuts())
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
            // Сперва ЯРЛЫКИ: их немного, и по ним потом узнаются программы, у которых ярлык есть.
            for s in roots::suffixes(&text, APP_PREFIX) {
                let Ok(root) = core::str::from_utf8(s) else { continue };
                let mut name = APP_PREFIX.to_vec();
                name.extend_from_slice(s);
                // Ярлык без читаемого содержимого — не повод прятать программу: показываем её
                // корнем, как если бы ярлыка не было вовсе.
                let text = ui::conf::read_root(scap, &name).unwrap_or_default();
                let text = String::from_utf8(text).unwrap_or_default();
                items.push(Item {
                    root: root.to_string(),
                    title: field(&text, "name").unwrap_or(root).to_string(),
                    info: field(&text, "info").unwrap_or("приложение").to_string(),
                    shortcut: true,
                });
            }
            items.sort_by(|a, b| a.title.cmp(&b.title));
            let apps = items.len();
            // Дальше — ВСЕ корни программ. Те, у кого ярлык уже есть, вторыми не заводятся.
            for s in roots::suffixes(&text, PROG_PREFIX) {
                let Ok(root) = core::str::from_utf8(s) else { continue };
                if items.iter().any(|it| it.root == root) {
                    continue;
                }
                items.push(Item {
                    root: root.to_string(),
                    title: root.to_string(),
                    info: String::from("программа store, без ярлыка"),
                    shortcut: false,
                });
            }
            // Порядок корней в списке store — порядок хэш-таблицы, а не алфавит.
            items[apps..].sort_by(|a, b| a.root.cmp(&b.root));
        }
    }
    if items.is_empty() {
        // Не молча: пустой список без объяснения человек читает как «система сломалась», а это
        // всего лишь отказ по правам на store.
        say("launcher: корни store не читаются — списка не будет, но набрать имя можно\n");
    }

    let mut app = App {
        sw: sw as i32,
        sh: sh as i32,
        text: String::new(),
        items,
        hits: Vec::new(),
        raw: false,
        sel: 0,
        top: 0,
        ptr: None,
        click: None,
        last: Rect::ZERO,
        mo: Motion::new(ui::anim::duration_from_config(&generation)),
    };
    app.filter();
    // Веха 146.1 — движение открытия НАЧИНАЕТСЯ ОТ НУЛЯ, и сказать это надо ЗДЕСЬ.
    //
    // [`Motion`] заводит число там, где его впервые спросили: первый же `val(A_OPEN, 256)` создал
    // бы слот сразу на 256, и карточка появлялась бы готовой — что и происходило всю Веху 146.
    // Панели этот случай не встречался: её меню рождается ЗАКРЫТЫМ, и слот заводится на нуле сам.
    app.mo.set(A_OPEN, 0);

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
                let name = app.items[i].root.clone();
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
