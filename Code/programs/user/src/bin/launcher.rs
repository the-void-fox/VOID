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
use void_user::win::{self, sym, Event, Window};

// Тулкит и разбор корней store — общие модули по пути (см. `ui/mod.rs`, почему не крейт).
// Строка запуска берёт из тулкита далеко не всё: неиспользованное здесь не ошибка.
#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
#[allow(dead_code)]
#[path = "../roots.rs"]
mod roots;

use ui::app::say;
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

/// Сколько строк списка видно разом. Больше — карточка перестаёт помещаться на экран ноутбука,
/// меньше — поиск превращается в угадывание.
const ROWS: usize = 7;

/// Идентификатор анимации открытия в [`Motion`] (у клиента их может быть много, у нас одна).
const A_OPEN: u32 = 1;

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
/// Свой разбор, а не `void_conf`: там строка конфига поколения — `<вид> <ключ> <значение>`,
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
    items: Vec<Item>,
    /// Веха 148.3 — набранное, отбор, выбор, прокрутка и строка под курсором — общим виджетом
    /// ([`ui::List`]), тем же, что у вьювера корней.
    ls: ui::List,
    /// Показан ВТОРОЙ ярус — программы без ярлыка (среди ярлыков не нашлось ничего). Подпись
    /// внизу обязана об этом сказать: список приложений и список корней store — разные ответы на
    /// один и тот же набор букв.
    raw: bool,
    /// Прямоугольник, нарисованный в прошлом кадре: его надо стереть, иначе карточка оставит
    /// за собой хвост, пока выезжает.
    last: Rect,
    /// Веха 165 — кадр вызван ТОЛЬКО движением: ни набранное, ни выбор, ни курсор не менялись.
    ///
    /// Тогда трогать надо ровно ту полоску, которая открылась (или закрылась) с прошлого кадра:
    /// раскрытие клипом ничего не двигает, и всё, что выше края, уже нарисовано правильно. С
    /// проявлением так было НЕЛЬЗЯ — там каждый пиксель карточки менял яркость каждый кадр, и
    /// перерисовка всей карточки была не расточительством, а единственным вариантом.
    anim_only: bool,
    mo: Motion,

    // ── Веха 148.3: измерено в кадре, спрошено в событии ──────────────────────────────────
    //
    // Раскладку знает только `draw` (ей нужны тема и шрифт), а решать «изменилось ли что-то»
    // приходится в `event`, где их нет. Поэтому кадр оставляет после себя две величины —
    // и заодно они честнее пересчёта: во время выезда карточка ещё не на месте, и попадание
    // клика считается по тому, что НА ЭКРАНЕ, а не по тому, где карточка будет.
    /// Право на store — им же и запускаем.
    store: Option<usize>,
    wm_ep: usize,
    /// Уходим: движение доигрывает до конца, и только потом поверхность исчезает.
    closing: bool,
}

impl App {
    /// Карточка целиком, как она выглядит ОТКРЫТОЙ. Движение — в [`App::sheet`].
    fn card_rect(&self, th: &Theme, font: &Font) -> Rect {
        let w = (self.sw * 2 / 5).clamp(th.px(360), th.px(560));
        let field = font.line_h() + th.px(14);
        let row = 2 * font.line_h() + th.px(14);
        let foot = font.line_h() + th.px(4);
        // Высота — ПО СПИСКУ, а не по потолку: карточка в полэкрана с одной строкой внутри
        // выглядит сломанной, а не просторной. Так же ведёт себя оболочка владельца.
        let h = 2 * th.pad + field + th.gap + self.rows() * row + foot;
        // Не по центру, а выше него: список растёт вниз, и карточка, посаженная в центр, всё
        // время выглядит съехавшей.
        Rect::new((self.sw - w) / 2, self.sh * 22 / 100, w, h)
    }

    /// Веха 165 — сколько карточки РАЗВЕРНУЛОСЬ (`t` — 0..256).
    ///
    /// Раскрытие вниз от верхнего края вместо проявления — по той же причине, что у меню панели
    /// ([`bar`]): оболочка должна вести себя как вещество, а не как слайд. Здесь у этого есть
    /// и вторая, измеримая сторона — та самая «рваность», на которую жаловался владелец.
    ///
    /// Пока карточка проявлялась, её фон был ПОЛУПРОЗРАЧНЫМ каждый кадр движения. А
    /// полупрозрачное на пустой поверхности идёт по самой медленной ветке `blend`: деление на
    /// каждый канал каждого пикселя. Карточка — двести тысяч пикселей; шестьдесят кадров в
    /// секунду этого никакая машина не выдаёт, и кадры начинали пропускаться неровно. Раскрытие
    /// клипом рисует непрозрачным (заливка строками) и ровно ту часть, которая видна.
    fn sheet(&self, card: Rect, t: u32) -> Rect {
        Rect::new(card.x, card.y, card.w, card.h * t.min(256) as i32 / 256)
    }

    /// Сколько строк списка показываем сейчас: столько, сколько нашлось, но не больше [`ROWS`].
    fn rows(&self) -> i32 {
        self.ls.hits.len().min(ROWS) as i32
    }

    /// Пересобрать список подходящих. Совпадение — ПОДСТРОКА без учёта регистра: полноценный
    /// нечёткий поиск здесь был бы преждевременным, а подстрока честно объяснима человеку.
    ///
    /// Ярусов два (Веха 146.1). Сначала ЯРЛЫКИ — то, что откроет окно. Если среди них не нашлось
    /// ничего, ищем среди всех корней store: спрятать программу от глаз и спрятать её от поиска —
    /// разные вещи, и вторая превратила бы отбор в ложь о содержимом системы.
    fn filter(&mut self) {
        let q = self.ls.query.trim().to_lowercase();
        let word = q.split(' ').next().unwrap_or("").to_string();
        self.ls.hits.clear();
        self.raw = false;
        for (i, it) in self.items.iter().enumerate() {
            if it.shortcut && (word.is_empty() || it.matches(&word)) {
                self.ls.hits.push(i);
            }
        }
        if self.ls.hits.is_empty() && !word.is_empty() {
            self.raw = true;
            for (i, it) in self.items.iter().enumerate() {
                if !it.shortcut && it.matches(&word) {
                    self.ls.hits.push(i);
                }
            }
        }
        // Точное совпадение — наверх: набрав `term` целиком, человек хочет `term`, а не
        // `terminal-что-то`, оказавшийся в списке раньше по алфавиту.
        if let Some(p) = self.ls.hits.iter().position(|&i| self.items[i].root == word) {
            self.ls.hits.swap(0, p);
        }
        self.ls.refiltered();
    }

    /// Сколько ярлыков всего — знаменатель для подписи внизу.
    fn shortcuts(&self) -> usize {
        self.items.iter().filter(|it| it.shortcut).count()
    }

    /// Держать выбранную строку в видимом окне списка.
    /// Что запустится по Enter: выбранная строка, а если список пуст — набранное как есть.
    fn target(&self) -> Option<(String, Vec<u8>)> {
        let mut words = self.ls.query.split_whitespace();
        let first = words.next().unwrap_or("");
        let mut args: Vec<u8> = Vec::new();
        for w in words {
            args.extend_from_slice(w.as_bytes());
            args.push(0);
        }
        match self.ls.current() {
            Some(i) => Some((self.items[i].root.clone(), args)),
            None if !first.is_empty() => Some((first.to_string(), args)),
            None => None,
        }
    }

    /// Нарисовать кадр. Возвращает строку, по которой ЩЁЛКНУЛИ, — запускает её вызывающий:
    /// рисование не должно уметь запускать программы, иначе одно и то же действие оказалось бы
    /// в двух местах (клавиатура — в цикле, мышь — здесь).
    fn card(&mut self, u: &mut Ui, th: &Theme, t: u32) -> Option<usize> {
        let full = self.card_rect(th, &*u.font);
        let r = self.sheet(full, t);
        let prev = core::mem::replace(&mut self.last, r);
        if r.is_empty() {
            u.clear(prev);
            return None;
        }
        // Что изменилось с прошлого кадра. На движении — полоска у нижнего края: от меньшего
        // низа (минус скругление: угол карточки уехал вниз и стал серединой) до большего.
        let area = if self.anim_only && !prev.is_empty() {
            let lo = prev.bottom().min(r.bottom()) - th.radius;
            let hi = prev.bottom().max(r.bottom());
            Rect::new(r.x, lo, r.w, hi - lo)
        } else {
            prev.union(r)
        };
        // Клип СУЖАЕМ, а не назначаем: снаружи он мог быть уже нашего (подсветка строки под
        // курсором объявляет только полосу списка), и назначить свой значило бы перерисовать
        // всю карточку на каждый переезд мыши между строками.
        let keep = u.c.clip();
        u.clip(keep.intersect(area));
        u.clear(area);
        // Подложка рисуется по РАЗВЁРНУТОМУ, содержимое — по окончательному месту под клипом:
        // строки, которые ехали бы вместе с краем, читались бы как второе движение внутри
        // первого. Список разворачивается ИЗ поля ввода, а не вылетает из-под него.
        u.card(r);
        u.clip(keep.intersect(area).intersect(r));
        let mut inner = full.inset_xy(th.pad, 0);
        inner.cut_top(th.pad);

        let field = u.font.line_h() + th.px(14);
        let fr = inner.cut_top(field);
        u.field(fr, &self.ls.query, "имя программы", true);
        inner.cut_top(th.gap);

        let row = 2 * u.font.line_h() + th.px(14);
        // Раскладку списка оставляем списку: в событии ни темы, ни шрифта нет.
        let rows = self.rows() as usize;
        self.ls.measure(Rect::new(inner.x, inner.y, inner.w, row * self.rows()), row, rows);
        let mut hit = None;
        for k in 0..self.rows() as usize {
            let rr = inner.cut_top(row);
            let Some(&i) = self.ls.hits.get(self.ls.top + k) else { continue };
            let sel = if self.ls.top + k == self.ls.sel { 256 } else { 0 };
            let hot = if u.hot(rr) { 256 } else { 0 };
            let it = &self.items[i];
            // Буква значка — из имени для человека: у «Терминала» это «Т», а не «t» от корня.
            let letter = it.title.chars().next().map_or(String::from("?"), |c| {
                c.to_uppercase().collect::<String>()
            });
            if u.entry(rr, &it.title, &it.info, &letter, sel, hot) {
                self.ls.sel = self.ls.top + k;
                hit = Some(i);
            }
        }

        // Подпись внизу — не украшение: она единственная отвечает на «а всё ли я вижу». С двумя
        // ярусами (Веха 146.1) у неё появилась вторая обязанность — сказать, КАКОЙ список сейчас
        // перед глазами: ярлыки или корни store.
        let foot = inner.cut_top(u.font.line_h() + th.px(4));
        let n = self.ls.hits.len();
        let s = if n == 0 && !self.ls.query.trim().is_empty() {
            alloc::format!("Enter запустит «{}»", self.ls.query.trim())
        } else if self.raw {
            // Знаменатель — ВСЕ программы store, а не «те, у кого нет ярлыка»: человек ищет в
            // системе, а не в остатке от отбора, и второе число должно отвечать на «сколько их
            // всего».
            alloc::format!("{} из {} программ store", n, self.items.len())
        } else if self.ls.query.trim().is_empty() {
            alloc::format!("ярлыков: {}", n)
        } else {
            alloc::format!("{} из {} ярлыков", n, self.shortcuts())
        };
        u.label(foot, &s, th.muted, Align::Right);
        u.clip(keep);
        hit
    }
}

impl ui::Client for App {
    fn event(&mut self, e: Event, input: &ui::Input) -> ui::Scope {
        // Пришло событие — значит изменилось не только движение: карточку надо считать заново
        // целиком. Флаг снимается в [`ui::Client::tick`], и только там.
        self.anim_only = false;
        match e {
            Event::Key { sym: code, mods: _, ch, down } if down => {
                // Escape и Enter — НАШИ: у строки запуска они закрывают и запускают, а не
                // очищают поиск и не двигают выбор (у вьювера корней ровно наоборот).
                match code {
                    sym::ESCAPE => {
                        self.closing = true;
                        return ui::Scope::All;
                    }
                    sym::RETURN => {
                        if let Some((name, args)) = self.target() {
                            launch(self.store, self.wm_ep, &name, &args);
                        }
                        self.closing = true;
                        return ui::Scope::All;
                    }
                    _ => {}
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
            Event::Motion { .. } => {
                // Веха 148.3 — подсветка меняется на СМЕНЕ СТРОКИ, а не на каждом пикселе пути,
                // и перерисовывается при этом полоса списка, а не вся карточка.
                if self.ls.motion(input.ptr) {
                    ui::Scope::Part(self.ls.row_rect(0).union(self.ls.row_rect(ROWS - 1)))
                } else {
                    ui::Scope::No
                }
            }
            Event::Button { x, y, down: true, .. } => {
                // Попадание считаем по карточке ПРОШЛОГО кадра: во время выезда она ещё не на
                // месте, и «где она будет» — не тот ответ, которого ждёт рука.
                if self.last.contains(x as i32, y as i32) {
                    // Строка под курсором отзовётся в `draw` — она же и запустится.
                    ui::Scope::All
                } else {
                    // Клик мимо карточки закрывает — ровно тот жест, ради которого поверхность
                    // и растянута на весь экран.
                    self.closing = true;
                    ui::Scope::All
                }
            }
            Event::Resize { w, h } => {
                self.sw = w as i32;
                self.sh = h as i32;
                ui::Scope::All
            }
            _ => ui::Scope::No,
        }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        // Движение открытия: цель 256, пока живём, и 0, когда уходим. Закрытие доигрывает до
        // конца — иначе карточка исчезала бы рывком ровно в тот момент, когда человек на неё
        // смотрит.
        self.mo.begin(sys::monotonic_ns());
        let t = self.mo.val(A_OPEN, if self.closing { 0 } else { 256 }) as u32;
        let th = u.th.clone();
        if let Some(i) = self.card(u, &th, t) {
            let name = self.items[i].root.clone();
            launch(self.store, self.wm_ep, &name, &[]);
            self.closing = true;
            // Движение закрытия начинается ЗДЕСЬ, а не со следующего кадра. Цель этого кадра
            // (`val` выше) была ещё «открыто», и без этой строки `done` увидел бы неподвижную
            // карточку и убрал бы поверхность мгновенно — вместо ухода получился бы обрыв.
            self.mo.val(A_OPEN, 0);
        }
        ui::Scope::No
    }

    fn wake(&mut self) -> Option<u32> {
        // Пока что-то едет — просыпаться кадрами; иначе спать до события.
        (self.mo.moving() || self.closing).then_some(ui::anim::FRAME_MS)
    }

    fn tick(&mut self) -> ui::Scope {
        if self.mo.moving() || self.closing {
            self.anim_only = true;
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }

    fn done(&self) -> bool {
        // Уходим не по нажатию, а когда движение ДОИГРАЛО: иначе карточка исчезает рывком.
        self.closing && !self.mo.moving()
    }

    /// Веха 166 — просьбу композитора закрыться (второй `Super+D`) исполняем САМИ: карточка
    /// сворачивается тем же движением, что и по `Escape`. До этого один и тот же жест уходил
    /// то плавно, то рывком — смотря чем его сделали.
    fn close(&mut self) -> bool {
        self.closing = true;
        self.anim_only = false;
        true
    }
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some((sw, sh)) = win::screen() else {
        say("launcher: композитора нет (WM в окружении)\n");
        sys::exit(1);
    };
    let (generation, th, mut font) = ui::app::boot();

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
        items,
        ls: ui::List::default(),
        raw: false,
        last: Rect::ZERO,
        anim_only: false,
        mo: Motion::new(ui::anim::duration_from_config(&generation)),
        store,
        wm_ep: win::endpoint().unwrap_or(sys::NO_CAP),
        closing: false,
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

    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}

/// Запустить программу — ЧЕРЕЗ СТОРОЖА (`run`, Веха 147).
///
/// Право на запуск у нас своё (`store` с `EXEC`), и запускаем мы по-прежнему сами: в
/// capability-модели действие делает тот, у кого есть право, а не тот, кто «главный». Изменилось
/// одно — между нами и программой встал [`run`], потому что **досмотреть запуск может только
/// родитель**: код выхода отдаёт `SYS_WAIT` своему ребёнку, вывод идёт тому, кто дал `STDIO`.
/// Мы на эту роль не годимся: строка запуска обязана исчезнуть с экрана сразу после нажатия.
///
/// Ни `WM`, ни `STORE` сторожу передавать не надо: стартовые права и окружение наследуются, и
/// он находит и композитора, и store тем же способом, что и мы.
fn launch(store: Option<usize>, wm: usize, name: &str, args: &[u8]) {
    let _ = wm;
    let Some(store) = store else {
        say("launcher: нет права на store — запускать нечем\n");
        return;
    };
    // argv сторожа: имя программы, дальше её собственные аргументы — как в командной строке.
    let mut a: Vec<u8> = Vec::with_capacity(name.len() + 1 + args.len());
    a.extend_from_slice(name.as_bytes());
    a.push(0);
    a.extend_from_slice(args);
    if sys::spawn(store, b"run", &a).is_none() {
        // Говорим вслух: молчащая строка запуска неотличима от сломанной. Плитки в этом случае
        // не будет ничьей — сторож, которого не запустили, о себе не расскажет.
        say(&alloc::format!("launcher: сторож запуска не завёлся, программа {} не пущена\n", name));
    }
}
