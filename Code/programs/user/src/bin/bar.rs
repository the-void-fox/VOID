//! `bar` — панель (Веха 140; переписана на тулкит `void-ui` Вехой 144).
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
//!
//! ## Веха 144 — острова вместо полосы
//!
//! Панель больше не полоса во всю ширину: это три скруглённых острова на обоях (так же выглядит
//! оболочка, которой владелец пользуется сегодня, — `IMG/mk7q48w.png`). Отсюда два следствия,
//! из-за которых веха и не свелась к перекраске:
//!
//! - поверхность стала ПРОЗРАЧНОЙ, а значит композитору понадобилось смешивание по альфе
//!   (`win::LAYER_ALPHA`) — до этого он копировал кадр слоя строками;
//! - перерисовывать всю поверхность на каждую минуту стало жалко, поэтому панель считает
//!   **подпись** каждого острова и трогает только тот, у которого она изменилась. Это же
//!   заставило тулкит с самого начала уметь damage, а не «нарисовать всё заново».

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win::{self, Event, Window};

// Тулкит — библиотека, и панель пользуется не всем, что в нём есть: следующий потребитель
// (меню, Веха 145) возьмёт остальное. Поэтому неиспользованное здесь не ошибка.
#[allow(dead_code)]
#[path = "../ui/mod.rs"]
mod ui;
use ui::{Align, Font, Rect, Theme, Ui};

/// Куча: настоящий шрифт приезжает файлом на мегабайты, и глифы кэшируются растрами. Куча
/// ленивая — под неё берётся адресное окно, а страницы приходят по мере нужды.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 24 * 1024 * 1024 }> = sys::heap::Heap::new();

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some((sw, _sh)) = win::screen() else {
        say("bar: композитора нет (WM в окружении) — панели не на чем висеть\n");
        sys::exit(1);
    };

    // Тема и шрифт — ДО поверхности: от них зависит высота панели, а высоту надо назвать в
    // запросе. Размер, посчитанный после, пришлось бы менять вторым вызовом на глазах у человека.
    let th = Theme::from_config(&ui::conf::generation().unwrap_or_default());
    let mut font = Font::load(th.font.as_deref(), th.font_px);

    // Сказать, что панель решила. Молчание здесь стоило бы человеку получаса: «почему шрифт не
    // тот» и «почему всё мелкое» — вопросы, ответ на которые панель знает, а он нет.
    say(&alloc::format!(
        "bar: шрифт {} {} px, масштаб {}%, высота {} px\n",
        if font.ttf() { "из пакета" } else { "встроенный 8×16" },
        th.font_px,
        th.scale,
        font.line_h() + 2 * th.px(5) + 2 * th.px(6),
    ));

    let mut bar = Bar::new(&th, font.line_h());
    let spec = win::Layer {
        layer: win::LAYER_TOP,
        anchor: win::ANCHOR_TOP | win::ANCHOR_LEFT | win::ANCHOR_RIGHT,
        // Занятая зона равна высоте: окна начинаются ПОД панелью. Ноль означал бы «панель поверх
        // окон», то есть первая строка терминала навсегда под ней.
        exclusive: bar.h as u16,
        // Веха 144 — острова скруглены, значит углы у них прозрачные. Без этого флага композитор
        // скопировал бы кадр как есть и нарисовал вокруг островов чёрный прямоугольник.
        alpha: true,
    };
    let Some(mut surf) = Window::layer(spec, sw, bar.h as u16, "панель") else {
        say("bar: композитор не дал поверхность слоя\n");
        sys::exit(1);
    };
    if !surf.watch() {
        say("bar: композитор не принял подписку на состояние — столы показаны не будут\n");
    }

    bar.fetch(&surf);
    if year_now() < 2000 {
        say("bar: часов у машины нет — время идёт с загрузки (см. SYS_TIME)\n");
    }
    bar.draw(&mut surf, &th, &mut font, None);

    let mut minute = minute_now();
    loop {
        // Спим до ближайшей минуты: часы меняются раз в минуту, и будить себя чаще незачем.
        // Событие композитора разбудит раньше — и это ровно то, чего мы ждём.
        match surf.next_event_timeout(ms_to_next_minute()) {
            // Состояние сменилось — спросить и перерисовать.
            Some(Event::Status) => {
                bar.fetch(&surf);
                bar.draw(&mut surf, &th, &mut font, None);
            }
            // Клик. Отвечаем ТОЛЬКО на нажатие: реагировать и на отпускание значило бы два
            // переключения на один щелчок.
            Some(Event::Button { x, y, down: true, .. }) => {
                bar.ptr = Some((x as i32, y as i32));
                bar.draw(&mut surf, &th, &mut font, Some((x as i32, y as i32)));
                bar.act(&surf);
            }
            Some(Event::Motion { x, y }) => {
                // Курсор УШЁЛ с панели — композитор говорит это координатами вне поверхности
                // (Веха 144). Без такого сообщения подсветка под курсором залипала бы навсегда:
                // событий «мыши больше нет над тобой» до этого не существовало.
                let ptr = (x != u16::MAX).then_some((x as i32, y as i32));
                if ptr != bar.ptr {
                    bar.ptr = ptr;
                    bar.draw(&mut surf, &th, &mut font, None);
                }
            }
            Some(Event::Resize { w, h }) if (w, h) != (surf.width, surf.height) => {
                if surf.resize_buf(w, h) {
                    bar.h = h as i32;
                    bar.forget();
                    bar.draw(&mut surf, &th, &mut font, None);
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
                    bar.draw(&mut surf, &th, &mut font, None);
                }
            }
        }
    }
}

/// Остров панели: где он и что на нём было нарисовано в прошлый раз.
///
/// Подпись — не украшение, а вся суть перерисовки по damage: сравнить одно число дешевле, чем
/// сравнивать состояние по полям, и невозможно забыть добавить в сравнение новое поле — оно
/// просто не попадёт в подпись, и это видно в одном месте.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Isle {
    rect: Rect,
    sig: u64,
}

/// Что панель показывает и где у неё что нарисовано.
struct Bar {
    h: i32,
    space: u8,
    spaces: u8,
    /// Раскладка клавиатуры: 0 — US, 1 — RU (Веха 143).
    layout: u8,
    title: String,
    /// Где курсор в координатах панели. `None` — не над ней.
    ptr: Option<(i32, i32)>,
    /// Острова прошлого кадра: столы, заголовок, часы.
    isles: [Isle; 3],
    /// Кадра ещё не было: холст надо очистить ЦЕЛИКОМ. Общая область приходит нулевой, но
    /// строить вид на чужой гарантии дешевле один раз проверить самим.
    fresh: bool,
    /// Место переключателя раскладки — он тоже кликается.
    lang_at: Rect,
    /// Что решил последний кадр: на какой стол перейти и трогать ли раскладку.
    go: Option<u8>,
    flip: bool,
}

/// Индексы островов в [`Bar::isles`].
const I_SPACES: usize = 0;
const I_TITLE: usize = 1;
const I_CLOCK: usize = 2;

impl Bar {
    fn new(th: &Theme, line_h: i32) -> Bar {
        // Высота считается ОТ ШРИФТА и от темы: разъехаться им нельзя (см. `bar_wanted` в `wm`).
        let isle_h = line_h + 2 * th.px(5);
        let h = isle_h + 2 * th.px(6);
        Bar {
            h,
            space: 0,
            spaces: 1,
            layout: 0,
            title: String::new(),
            ptr: None,
            isles: [Isle::default(); 3],
            fresh: true,
            lang_at: Rect::ZERO,
            go: None,
            flip: false,
        }
    }

    /// Забыть нарисованное: следующий кадр перерисует всё (смена размера экрана).
    fn forget(&mut self) {
        self.isles = [Isle::default(); 3];
        self.fresh = true;
    }

    /// Спросить композитор: стол, сколько столов, раскладка, заголовок окна в фокусе.
    fn fetch(&mut self, surf: &Window) {
        let mut buf = [0u8; win::TITLE_MAX];
        let Some(st) = surf.status(&mut buf) else { return };
        self.space = st.space;
        self.spaces = st.spaces.max(1);
        self.layout = st.layout;
        self.title = String::from(core::str::from_utf8(&buf[..st.title_len]).unwrap_or(""));
    }

    /// Исполнить то, что решил кадр: клик по столу или по раскладке.
    ///
    /// Отдельно от рисования намеренно: переключение стола вызывает у композитора новое
    /// состояние, а значит и новое событие нам — делать это посреди кадра значило бы рисовать
    /// по данным, которые уже устарели.
    fn act(&mut self, surf: &Window) {
        if let Some(n) = self.go.take() {
            surf.switch_space(n);
        }
        if core::mem::take(&mut self.flip) {
            // Через композитор, а не `SYS_KEYMAP` самой: право переключать раскладку у владельца
            // ЭКРАНА, а это он. Панель пробовала звать ядро напрямую — и получала отказ, потому
            // что она композитору не ровня, а ребёнок (Веха 144, найдено на первом же клике).
            surf.switch_layout();
        }
    }

    fn draw(
        &mut self,
        surf: &mut Window,
        th: &Theme,
        font: &mut Font,
        click: Option<(i32, i32)>,
    ) {
        let (w, h) = (surf.width as i32, surf.height as i32);
        let margin = th.px(6);
        let isle_h = h - 2 * margin;
        let clock = clock_text();
        let date = date_text();
        let lang = if self.layout == 0 { "EN" } else { "RU" };

        // ── раскладка ряда: сперва посчитать, потом рисовать ───────────────────────────────
        //
        // Считаем всё до единого пикселя ДО первого касания холста: остров, чья ширина зависит
        // от соседа, иначе рисовался бы по вчерашним числам.
        let pill_gap = th.px(4);
        let mut widths = Vec::with_capacity(self.spaces as usize);
        let mut spaces_w = 2 * th.pad;
        for i in 0..self.spaces {
            let label = alloc::format!("{}", i + 1);
            let pw = (font.width(&label) + th.px(14)).max(isle_h - th.px(8));
            if i > 0 {
                spaces_w += pill_gap;
            }
            spaces_w += pw;
            widths.push((label, pw));
        }
        let l_isle = Rect::new(margin, margin, spaces_w, isle_h);

        let lang_w = font.width(lang) + th.px(12);
        let clock_w = font.width(&clock);
        let date_w = font.width(&date);
        let r_w = 2 * th.pad + lang_w + th.px(8) + th.line + th.px(8) + clock_w + th.px(6) + date_w;
        let r_isle = Rect::new(w - margin - r_w, margin, r_w, isle_h);

        // Заголовок посередине — тем местом, что осталось между островами. Пустой заголовок
        // острова не рождает: пустая карточка посреди панели выглядела бы поломкой.
        let room = r_isle.x - l_isle.right() - 2 * margin;
        let t_isle = if self.title.is_empty() || room < th.px(60) {
            Rect::ZERO
        } else {
            let tw = (font.width(&self.title) + 2 * th.pad).min(room);
            let x = ((w - tw) / 2).clamp(l_isle.right() + margin, r_isle.x - margin - tw);
            Rect::new(x, margin, tw, isle_h)
        };

        // ── что перерисовывать ─────────────────────────────────────────────────────────────
        let hot = |r: Rect| self.ptr.is_some_and(|(x, y)| r.contains(x, y));
        let hot_pill = {
            let mut x = l_isle.x + th.pad;
            let mut idx = -1i32;
            for (i, (_, pw)) in widths.iter().enumerate() {
                if hot(Rect::new(x, l_isle.y, *pw, isle_h)) {
                    idx = i as i32;
                }
                x += pw + pill_gap;
            }
            idx
        };
        let want = [
            Isle { rect: l_isle, sig: sig(&[self.space as u64, self.spaces as u64, hot_pill as u64]) },
            Isle { rect: t_isle, sig: sig_str(&self.title) },
            Isle {
                rect: r_isle,
                sig: sig(&[
                    self.layout as u64,
                    sig_str(&clock),
                    sig_str(&date),
                    hot(self.lang_at) as u64,
                ]),
            },
        ];
        // Клик всегда рисует всё: он меняет и то, что под курсором, и то, что было активным, —
        // а «что именно» знает уже сам виджет, а не эта таблица.
        let all = click.is_some();
        let redraw: [bool; 3] = core::array::from_fn(|i| all || want[i] != self.isles[i]);
        if !redraw.iter().any(|&x| x) {
            return;
        }

        let mut u = Ui::new(surf.pixels(), w, h, th, font);
        u.input(self.ptr, click);

        // Стереть старое место острова вместе с новым: остров, ставший уже, оставил бы за собой
        // кусок себя прежнего — на прозрачной поверхности это не «след», а мусор поверх обоев.
        if core::mem::take(&mut self.fresh) {
            u.clear_all();
        } else {
            for i in 0..3 {
                if redraw[i] {
                    u.clear(self.isles[i].rect.union(want[i].rect));
                }
            }
        }

        if redraw[I_SPACES] {
            let mut inner = u.island(l_isle);
            for (i, (label, pw)) in widths.iter().enumerate() {
                // Попадание клика считает САМ виджет — по тому прямоугольнику, по которому он
                // и нарисован. До тулкита панель вела вторую таблицу клеток, и это была не
                // экономия, а два расчёта «где что», обязанных совпасть (обзор композитора уже
                // расходился так — Веха 123).
                let cell = inner.cut_left(*pw);
                let pill = Rect::new(cell.x, cell.y + th.px(4), cell.w, isle_h - 2 * th.px(4));
                if u.pill(pill, label, i as u8 == self.space) {
                    self.go = Some(i as u8);
                }
                inner.cut_left(pill_gap);
            }
        }

        if redraw[I_TITLE] && !t_isle.is_empty() {
            let inner = u.island(t_isle);
            u.label(inner, &self.title, th.text, Align::Center);
        }

        if redraw[I_CLOCK] {
            let mut inner = u.island(r_isle);
            let lang_at = inner.cut_left(lang_w);
            if u.button(lang_at, lang) {
                self.flip = true;
            }
            self.lang_at = lang_at;
            inner.cut_left(th.px(8));
            u.sep(inner.cut_left(th.line).inset_xy(0, th.px(5)));
            inner.cut_left(th.px(8));
            u.label(inner.cut_left(clock_w), &clock, th.text, Align::Left);
            inner.cut_left(th.px(6));
            u.label(inner, &date, th.muted, Align::Left);
        }

        let d = u.dirty();
        self.isles = want;
        if !d.is_empty() {
            surf.damage(d.x as u16, d.y as u16, d.w as u16, d.h as u16);
        }
    }
}

/// Подпись набора чисел (FNV-1a). Нужна только для сравнения «то же самое или нет».
fn sig(vals: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in vals {
        for b in v.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn sig_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Часы «ЧЧ:ММ» по UTC.
fn clock_text() -> String {
    let (_, _, _, hh, mm, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
    alloc::format!("{hh:02}:{mm:02}")
}

/// Дата «ДД.ММ» — рядом с часами и приглушённо: она нужна реже времени, но искать её в другом
/// месте человеку не должно приходиться.
fn date_text() -> String {
    let (_, mo, d, _, _, _) = sys::civil_from_unix(sys::time_ns() / 1_000_000_000);
    alloc::format!("{d:02}.{mo:02}")
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
