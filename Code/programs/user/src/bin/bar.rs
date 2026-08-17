//! `bar` — панель (Веха 140): столы, заголовок окна в фокусе и часы на верхнем слое.
//!
//! Второй клиент слоя после обоев и первый, кому нужны КЛИКИ: столы в панели переключаются
//! мышью. Занятая зона у неё настоящая — окна начинаются под панелью, а не заезжают под неё
//! ([[layers]]).
//!
//! ## Почему панель — программа, а не часть композитора
//!
//! Тот же довод, что у обоев, и он стал сильнее: панель ходит за состоянием по протоколу
//! (`OP_STATUS`), рисует шрифтом и знает про календарь. Три чужих умения в программе, где лежат
//! буферы ВСЕХ окон. Здесь падение панели — это исчезнувшая полоска сверху и вернувшееся окнам
//! место; в композиторе оно было бы чёрным экраном.
//!
//! ## Откуда панель знает, что показывать
//!
//! Не опросом. `OP_WATCH` подписывает её на `EV_STATUS` — «состояние сменилось», — и подробности
//! она берёт `OP_STATUS`'ом уже по делу. Часы идут отдельно: минута не событие композитора,
//! поэтому панель спит в `next_event_timeout` ровно до следующей минуты и просыпается либо к ней,
//! либо раньше — от события. Опрос в цикле стоил бы системе простоя (см. Веху 139.3, где ровно
//! это жгло целое ядро).
//!
//! Часы показывают **UTC**: часовых поясов в VOID нет вовсе, а врать про местное время хуже, чем
//! честно показать всемирное. Если у машины нет RTC, время идёт с загрузки — панель говорит об
//! этом вслух при старте, иначе «02:14» выглядело бы как сломанные часы, а не как их отсутствие.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use void_user as sys;
use void_user::glyph;
use void_user::win::{self, Event, Window};

/// Куча небольшая: панель держит заголовок, строку часов и свой кадр (1920×28×4 — 215 КиБ).
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Высота панели: глиф 16 плюс по шесть сверху и снизу. Не в конфиге намеренно — она следует за
/// шрифтом, и разъехаться с ним не должна (см. `bar_wanted` в `wm`).
const BAR_H: u16 = 28;
/// Отступ от края и между частями.
const PAD: i32 = 8;

const C_BG: (u8, u8, u8) = (0x11, 0x16, 0x1d);
const C_FG: (u8, u8, u8) = (0xc4, 0xcf, 0xdb);
const C_DIM: (u8, u8, u8) = (0x5c, 0x68, 0x77);
const C_ACCENT: (u8, u8, u8) = (0x4c, 0x7d, 0xfd);
const C_ON_ACCENT: (u8, u8, u8) = (0x08, 0x0c, 0x12);

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some((sw, _sh)) = win::screen() else {
        say("bar: композитора нет (WM в окружении) — панели не на чем висеть\n");
        sys::exit(1);
    };

    let spec = win::Layer {
        layer: win::LAYER_TOP,
        anchor: win::ANCHOR_TOP | win::ANCHOR_LEFT | win::ANCHOR_RIGHT,
        // Занятая зона равна высоте: окна начинаются ПОД панелью. Ноль означал бы «панель поверх
        // окон», то есть первая строка терминала навсегда под ней.
        exclusive: BAR_H,
    };
    let Some(mut surf) = Window::layer(spec, sw, BAR_H, "панель") else {
        say("bar: композитор не дал поверхность слоя\n");
        sys::exit(1);
    };
    if !surf.watch() {
        say("bar: композитор не принял подписку на состояние — столы показаны не будут\n");
    }

    let mut st =
        State { space: 0, spaces: 1, layout: 0, title: String::new(), cells: Vec::new() };
    st.fetch(&surf);
    if year_now() < 2000 {
        say("bar: часов у машины нет — время идёт с загрузки (см. SYS_TIME)\n");
    }
    st.draw(&mut surf);

    let mut minute = minute_now();
    loop {
        // Спим до ближайшей минуты: часы меняются раз в минуту, и будить себя чаще незачем.
        // Событие композитора разбудит раньше — и это ровно то, чего мы ждём.
        match surf.next_event_timeout(ms_to_next_minute()) {
            // Состояние сменилось — спросить и перерисовать.
            Some(Event::Status) => {
                st.fetch(&surf);
                st.draw(&mut surf);
            }
            // Клик по столу. Отвечаем ТОЛЬКО на нажатие: реагировать и на отпускание значило бы
            // два переключения на один щелчок.
            Some(Event::Button { x, down: true, .. }) => {
                if let Some(n) = st.cell_at(x as i32) {
                    surf.switch_space(n);
                }
            }
            Some(Event::Resize { w, h }) if (w, h) != (surf.width, surf.height) => {
                if surf.resize_buf(w, h) {
                    st.draw(&mut surf);
                }
            }
            Some(Event::Close) => {
                surf.destroy();
                sys::exit(0);
            }
            // Срок вышел (или пришло что-то нам ненужное) — перерисовать, если сменилась минута.
            _ => {
                let m = minute_now();
                if m != minute {
                    minute = m;
                    st.draw(&mut surf);
                }
            }
        }
    }
}

/// Что панель показывает и где у неё что нарисовано.
struct State {
    space: u8,
    spaces: u8,
    /// Раскладка клавиатуры: 0 — US, 1 — RU (Веха 143).
    layout: u8,
    title: String,
    /// Клетки столов: `(x0, x1)` в координатах панели, по индексу = номер стола.
    ///
    /// Тот же список, по которому панель НАРИСОВАНА, — им же считается попадание клика. Два
    /// расчёта «где что» разошлись бы, и человек нажимал бы не на тот стол, который видит; на
    /// этих граблях композитор уже стоял (обзор, Веха 123).
    cells: Vec<(i32, i32)>,
}

impl State {
    /// Спросить композитор: стол, сколько столов, заголовок окна в фокусе.
    fn fetch(&mut self, surf: &Window) {
        let mut buf = [0u8; win::TITLE_MAX];
        let Some(st) = surf.status(&mut buf) else { return };
        self.space = st.space;
        self.spaces = st.spaces.max(1);
        self.layout = st.layout;
        self.title = String::from(core::str::from_utf8(&buf[..st.title_len]).unwrap_or(""));
    }

    /// Номер стола под точкой `x` панели. `None` — там не клетка стола.
    fn cell_at(&self, x: i32) -> Option<u8> {
        self.cells.iter().position(|&(a, b)| x >= a && x < b).map(|i| i as u8)
    }

    fn draw(&mut self, surf: &mut Window) {
        let (w, h) = (surf.width as i32, surf.height as i32);
        let mut c = Canvas { px: surf.pixels(), w, h };
        c.fill(0, 0, w, h, C_BG);
        // Нижняя черта: без неё панель сливается с тёмным окном под ней.
        c.fill(0, h - 1, w, 1, (0x22, 0x2a, 0x34));

        let ty = (h - glyph::H as i32) / 2;

        // Столы слева. Клетка активного залита — цвет виден издалека, а цифра нет.
        self.cells.clear();
        let mut x = PAD;
        for i in 0..self.spaces {
            let label = alloc::format!("{}", i + 1);
            let tw = glyph::text_width(&label, 1) as i32;
            let cw = tw + 2 * PAD;
            let active = i == self.space;
            if active {
                c.fill(x, 4, cw, h - 8, C_ACCENT);
            }
            c.text(x + PAD, ty, &label, if active { C_ON_ACCENT } else { C_DIM });
            self.cells.push((x, x + cw));
            x += cw + 2;
        }

        // Часы справа.
        let clock = clock_text();
        let clock_w = glyph::text_width(&clock, 1) as i32;
        c.text(w - PAD - clock_w, ty, &clock, C_FG);

        // Веха 143 — РАСКЛАДКА перед часами. Две буквы, а не флажок: флаг это страна, а не язык
        // ввода, и «какой сейчас язык» читается буквами быстрее, чем узнаётся картинка.
        // Активная раскладка написана ярко, чтобы отличаться от часов боковым зрением.
        let lang = if self.layout == 0 { "EN" } else { "RU" };
        let lang_w = glyph::text_width(lang, 1) as i32;
        let lang_x = w - PAD - clock_w - PAD - lang_w;
        c.text(lang_x, ty, lang, if self.layout == 0 { C_DIM } else { C_FG });

        // Заголовок посередине — тем, что осталось между столами и часами. Обрезаем ПО СИМВОЛАМ,
        // а не по байтам: заголовок это UTF-8, и разрезанный посреди буквы он превратился бы в
        // мусор (у нас заголовки русские).
        let left = x + PAD;
        let right = lang_x - PAD;
        if right > left && !self.title.is_empty() {
            let room = ((right - left) / glyph::W as i32).max(0) as usize;
            let shown: String = self.title.chars().take(room).collect();
            c.text(left, ty, &shown, C_FG);
        }

        surf.damage(0, 0, w as u16, h as u16);
    }
}

/// Кадр панели: RGBA8888 по строкам — то, во что смотрит композитор.
struct Canvas<'a> {
    px: &'a mut [u8],
    w: i32,
    h: i32,
}

impl Canvas<'_> {
    fn put(&mut self, x: i32, y: i32, c: (u8, u8, u8)) {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        if i + 3 < self.px.len() {
            self.px[i] = c.0;
            self.px[i + 1] = c.1;
            self.px[i + 2] = c.2;
            self.px[i + 3] = 0xff;
        }
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, c);
            }
        }
    }

    /// Строка шрифтом 8×16 из таблицы ядра. Без сглаживания: у растрового шрифта его не бывает,
    /// и в панели оно не нужно — текст здесь короткий и всегда на своём фоне.
    fn text(&mut self, x: i32, y: i32, s: &str, c: (u8, u8, u8)) {
        let mut cx = x;
        for ch in s.chars() {
            for (row, bits) in glyph::rows(ch).iter().enumerate() {
                for col in 0..glyph::W as i32 {
                    if bits & (0x80 >> col) != 0 {
                        self.put(cx + col, y + row as i32, c);
                    }
                }
            }
            cx += glyph::W as i32;
        }
    }
}

/// Часы «ЧЧ:ММ» по UTC.
fn clock_text() -> String {
    let (_, _, _, hh, mm, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
    alloc::format!("{hh:02}:{mm:02}")
}

fn minute_now() -> u64 {
    sys::time_ns() / 60_000_000_000
}

fn year_now() -> i64 {
    sys::civil_from_unix(sys::time_ns() / 1_000_000_000).0
}

/// Сколько миллисекунд до смены минуты. Не меньше секунды: ноль означал бы бесконечное
/// просыпание на границе, а лишняя секунда на часах без секундной стрелки не видна.
fn ms_to_next_minute() -> u32 {
    let secs = sys::time_ns() / 1_000_000_000 % 60;
    ((60 - secs) * 1000).max(1000) as u32
}

fn say(s: &str) {
    sys::write(s.as_bytes());
}
