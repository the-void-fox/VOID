//! `welcome` — окно «добро пожаловать» (Веха 172): что это за система и как её выключить.
//!
//! ## Почему это обычное окно ленты, а не мастер
//!
//! Плавающих окон и модальности в VOID нет вовсе, поэтому «поверх всего с кнопкой Далее» здесь
//! просто некуда положить. Макет владельца из этого и вырос: не шаги мастера, а **экраны**,
//! которые листают вниз, — по одному на мысль, с шевроном внизу как приглашением крутить дальше.
//! Ширина берётся у колонки, высота — почти весь экран; всё остальное воздух.
//!
//! ## Почему нет галочки «показывать при входе»
//!
//! Была бы — и в системе появилось бы второе место, где что-то включается: конфиг и ещё вот эта
//! галочка. Владелец предложил лучше: окно названо в конфиге списком автозапуска
//! (`/etc/system/autostart.vv`), и последний экран показывает эту самую строку. Убрал имя, сказал
//! `rebuild` — окно больше не появится. Так рассказ о том, «как в VOID настраивается система»,
//! не пересказывается словами, а делается руками — на самом себе.
//!
//! ## Что здесь намеренно НЕ показано
//!
//! Ни спиннеров (запрет ADR 0016), ни картинок из сети, ни прогресса. Вся графика — знак системы
//! под заглавным экраном и шеврон, обе фигуры из наших `.vg` и обе монохромные.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use void_user as sys;
use void_user::win::{sym, Event, Window};

#[path = "../ui/mod.rs"]
mod ui;

use ui::{Align, Rect, Rgba, Ui};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 2 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Имя движущейся величины — прокрутки. Оно одно: больше здесь ничего не едет.
const ANIM_SCROLL: u32 = 1;

/// Строка экрана: чем она является, тем и рисуется.
enum Line {
    /// Заглавие экрана — крупным кеглем, по центру.
    Title(&'static str),
    /// Абзац; переносится по словам.
    Text(&'static str),
    /// Пункт нумерованного списка.
    Item(u32, &'static str),
    /// Пара «подпись — значение»: сочетания клавиш.
    Pair(&'static str, &'static str),
    /// Строка конфига — показывается как есть, с отступом.
    Code(&'static str),
    /// Воздух между блоками, высотой в столько строк.
    Gap(i32),
}

use Line::{Code, Gap, Item, Pair, Text, Title};

/// Экраны. Порядок — порядок прокрутки; первый особенный: вместо строк на нём знак системы.
const SCREENS: &[&[Line]] = &[
    &[],
    &[
        Text(
            "Операционная система без root и без пользователей: всё, что программа может \
             сделать, лежит у неё в руках отдельными правами — и эти права видно.",
        ),
        Gap(3),
        Pair("Строка запуска", "Super+D"),
        Pair("Терминал", "Super+Return"),
        Pair("Обзор столов", "Super+Tab"),
    ],
    &[
        Title("Чем VOID отличается"),
        Gap(4),
        Item(1, "Нет root и нет пользователей."),
        Item(2, "Система — это конфиг."),
        Item(3, "Ядро микро. Композитор, драйверы, шелл — обычные программы."),
    ],
    &[
        Title("Как это выключить"),
        Gap(3),
        Text("Это окно — обычная программа. Открылось оно потому, что названо в конфиге:"),
        Gap(1),
        Code("/etc/system/autostart.vv"),
        Code("open = [\"welcome\"]"),
        Gap(1),
        Text(
            "Убери имя из списка, скажи rebuild — и окна больше не будет. Так меняется всё \
             остальное: gens покажет поколения, switch вернёт прежнее.",
        ),
    ],
];

struct App {
    w: i32,
    h: i32,
    /// На каком экране мы сейчас. Прокрутка едет к `at * h`.
    at: usize,
    /// Крупные кегли: у [`ui::Font`] размер один, а макету нужны заглавие и знак.
    big: ui::Font,
    huge: ui::Font,
    /// Имя файла шрифта из темы — по нему пересобираются крупные кегли при смене ширины.
    font_name: Option<String>,
    /// При какой ширине окна кегли считались в прошлый раз (0 — ещё ни разу).
    fitted: i32,
    /// Куда попадает щелчок по шеврону. Считается при рисовании — как и всё в immediate-mode.
    chevron: Rect,
}

impl App {
    /// Перейти на экран `n`, если он есть. `true` — что-то изменилось.
    fn go(&mut self, n: i32) -> bool {
        let n = n.clamp(0, SCREENS.len() as i32 - 1) as usize;
        let moved = n != self.at;
        self.at = n;
        moved
    }

    /// Пересобрать крупные кегли под ширину окна.
    ///
    /// Считать их долями `font_px` нельзя: у встроенного шрифта 8×16 кегль растёт ЦЕЛЫМИ
    /// кратными (`scale = px / 16`), поэтому «в два раза больше пятнадцати» — это ровно тот же
    /// размер, что и был. Поэтому меряем: строим шрифт по грубой прикидке, спрашиваем ширину
    /// слова и поправляем кегль один раз. Одна поправка достаточна — зависимость линейная.
    fn fit_fonts(&mut self, body: i32) {
        let name = self.font_name.as_deref();
        self.big = ui::Font::load(name, (body * 2).clamp(16, 96) as u32);
        let want = self.w * 3 / 5; // «VOID» занимает три пятых ширины — как в макете
        let mut px = (want / 2).clamp(16, 400);
        let mut f = ui::Font::load(name, px as u32);
        let have = f.width("VOID");
        if have > 0 {
            px = (px * want / have).clamp(16, 400);
            f = ui::Font::load(name, px as u32);
        }
        self.huge = f;
        self.fitted = self.w;
    }

    /// Заглавный экран: знак системы бледной подложкой и слово VOID поверх — как в макете.
    fn draw_mark(&mut self, u: &mut Ui, top: i32) {
        let mid = top + self.h / 2;
        // Знак — КРУПНЕЕ слова и еле виден: здесь он фон, а не иллюстрация.
        let side = (self.w * 11 / 20).min(self.h / 2);
        let t = u.th.text;
        u.icon(
            Rect::new(self.w / 2 - side / 2, mid - side / 2, side, side),
            ui::icon::LOGO,
            Rgba::new(t.r, t.g, t.b, 24),
        );
        let col = u.th.muted;
        let w = self.huge.width("VOID");
        let base = mid + self.huge.ascent() / 2;
        self.huge.draw(&mut u.c, (self.w - w) / 2, base, "VOID", col);
    }

    /// Один текстовый экран. `top` — где начинается его полоса в координатах окна.
    fn draw_screen(&mut self, u: &mut Ui, screen: usize, top: i32) {
        let pad = (self.w / 8).clamp(24, 96);
        let line = u.font.line_h();
        let left = pad;
        let width = self.w - pad * 2;
        // Колонка значений отбивается от самой длинной подписи, а не на глаз: иначе «Обзор
        // столов» и «Терминал» разъехались бы при другом кегле или шрифте.
        let key_w = SCREENS[screen]
            .iter()
            .filter_map(|i| if let Pair(k, _) = i { Some(*k) } else { None })
            .map(|k| u.text_w(k))
            .max()
            .unwrap_or(0);
        let mut y = top + self.h / 4;
        for item in SCREENS[screen] {
            match item {
                Title(s) => {
                    let col = u.th.muted;
                    let w = self.big.width(s);
                    let base = y + self.big.ascent();
                    self.big.draw(&mut u.c, (self.w - w) / 2, base, s, col);
                    y += self.big.line_h();
                }
                Text(s) => y = wrap(u, left, y, width, s, u.th.muted),
                Item(k, s) => {
                    let tag = alloc::format!("{k}.");
                    let col = u.th.muted;
                    u.label(Rect::new(left, y, line * 2, line), &tag, col, Align::Left);
                    y = wrap(u, left + line * 2, y, width - line * 2, s, col);
                }
                Pair(k, v) => {
                    let (mu, tx) = (u.th.muted, u.th.text);
                    u.label(Rect::new(left, y, key_w, line), k, mu, Align::Left);
                    let vx = left + key_w + line;
                    u.label(Rect::new(vx, y, width - key_w - line, line), v, tx, Align::Left);
                    y += line;
                }
                Code(s) => {
                    let col = u.th.text;
                    u.label(Rect::new(left + line, y, width - line, line), s, col, Align::Left);
                    y += line;
                }
                Gap(k) => y += line * k,
            }
        }
    }

    /// Шеврон внизу экрана — приглашение крутить дальше. На последнем экране его нет: обещать
    /// продолжение, которого не будет, хуже, чем не обещать ничего.
    fn draw_chevron(&mut self, u: &mut Ui, top: i32) {
        let side = (self.w / 10).clamp(24, 72);
        let r = Rect::new(self.w / 2 - side / 2, top + self.h * 3 / 4, side, side);
        self.chevron = r;
        let col = if u.hot(r) { u.th.text } else { u.th.muted };
        u.icon(r, ui::icon::DOWN, col);
    }
}

/// Нарисовать абзац с переносом ПО СЛОВАМ. Возвращает `y` следующей строки.
///
/// Перенос здесь, а не в тулките, намеренно: у списка, панели и меню строки не переносятся вовсе,
/// и первый потребитель этой функции — она сама. Появится второй — переедет в `void-ui`.
fn wrap(u: &mut Ui, x: i32, y: i32, w: i32, s: &str, col: Rgba) -> i32 {
    let line = u.font.line_h();
    let mut y = y;
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let probe =
            if cur.is_empty() { String::from(word) } else { alloc::format!("{cur} {word}") };
        if u.text_w(&probe) > w && !cur.is_empty() {
            u.label(Rect::new(x, y, w, line), &cur, col, Align::Left);
            y += line;
            cur = String::from(word);
        } else {
            cur = probe;
        }
    }
    if !cur.is_empty() {
        u.label(Rect::new(x, y, w, line), &cur, col, Align::Left);
        y += line;
    }
    y
}

impl ui::Client for App {
    fn event(&mut self, e: Event, _input: &ui::Input) -> ui::Scope {
        let moved = match e {
            Event::Key { sym: code, ch, down, .. } if down => match code {
                sym::DOWN | sym::PAGE_DOWN | sym::RETURN => self.go(self.at as i32 + 1),
                sym::UP | sym::PAGE_UP => self.go(self.at as i32 - 1),
                sym::HOME => self.go(0),
                sym::END => self.go(SCREENS.len() as i32 - 1),
                _ if ch == b' ' as u16 => self.go(self.at as i32 + 1),
                _ => false,
            },
            // Колесо листает ЭКРАНАМИ, а не пикселями: экран здесь единица смысла, и остановка
            // посреди двух мыслей не значит ничего.
            Event::Wheel { delta, .. } => self.go(self.at as i32 + if delta < 0 { 1 } else { -1 }),
            Event::Button { x, y, down, .. } if down => {
                let r = self.chevron;
                let (x, y) = (x as i32, y as i32);
                let inside = x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
                inside && self.go(self.at as i32 + 1)
            }
            Event::Resize { w, h } => {
                self.w = w as i32;
                self.h = h as i32;
                true
            }
            _ => false,
        };
        if moved {
            ui::Scope::All
        } else {
            ui::Scope::No
        }
    }

    fn draw(&mut self, u: &mut Ui) -> ui::Scope {
        if self.fitted != self.w {
            let body = u.font.line_h();
            self.fit_fonts(body);
        }
        let bg = u.th.bg;
        u.background(bg);
        // Прокрутка едет сама (Веха 168.2): цель — верх нужного экрана, а значение этого кадра
        // считает тулкит и сам просит следующий кадр, пока не доедет.
        let off = u.anim(ANIM_SCROLL, self.at as i32 * self.h);
        for n in 0..SCREENS.len() {
            let top = n as i32 * self.h - off;
            if top >= self.h || top + self.h <= 0 {
                continue; // экран целиком за краем окна — рисовать нечего
            }
            if n == 0 {
                self.draw_mark(u, top);
            } else {
                self.draw_screen(u, n, top);
            }
            if n + 1 < SCREENS.len() {
                self.draw_chevron(u, top);
            }
        }
        ui::Scope::No
    }

    // Веха 151 — в СЕАНС мы не записываемся, и это единственное место, где приветствие ведёт
    // себя не как обычное окно. Причина в том, что оно обещает человеку на последнем экране:
    // «убери имя из автозапуска — окна больше не будет». Сеанс возвращает то, что лежало на
    // столе, НЕЗАВИСИМО от конфига, — и обещание стало бы ложью ровно для тех, кто оставил окно
    // открытым (проверено: с записью в сеанс оно возвращалось и на пустом `autostart`).
    //
    // Поэтому у приветствия ровно один выключатель, и он в конфиге. Своё место в ленте оно при
    // этом теряет — небольшая цена за то, чтобы система не врала о себе в первом же окне.
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let (_generation, th, mut font) = ui::app::boot();

    // Размер окна — пожелание: композитор всё равно уложит его в колонку ленты и пришлёт
    // `Resize` с настоящим. Крупные кегли поэтому и считаются в первом кадре, а не здесь.
    let (w, h) = (640u16, 760u16);
    let Some(mut surf) = Window::create(w, h, "Добро пожаловать") else {
        ui::app::say("welcome: композитора нет (WM в окружении)");
        sys::exit(1);
    };

    // Имя файла шрифта берём то же, что у темы: иначе крупный текст поехал бы другой гарнитурой,
    // чем мелкий, и это было бы видно.
    let name = th.font.clone();
    let mut app = App {
        w: w as i32,
        h: h as i32,
        at: 0,
        big: ui::Font::load(name.as_deref(), th.font_px),
        huge: ui::Font::load(name.as_deref(), th.font_px),
        font_name: name,
        fitted: 0,
        chevron: Rect::ZERO,
    };
    ui::app::run(&mut surf, &th, &mut font, &mut app);
    sys::exit(0);
}
