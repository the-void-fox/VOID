//! `wm` — композитор окон VOID (Веха 117, [[0007-graphics-native-compositor]]).
//!
//! ```text
//!   клиент ─рисует в свою память─→ obj_put ─content-id─→ ATTACH+COMMIT ─→ композитор
//!   мышь/клавиши ─→ композитор ─→ событие ОКНУ под курсором (отложенный ответ)
//!   композитор ─→ damage-прямоугольники ─→ фреймбуфер
//! ```
//!
//! ## Что здесь принципиального
//!
//! - **Окно — это буфер-ОБЪЕКТ плюс право звать нас**, а не участок чужой памяти
//!   ([[0018-gpu-ladder]]). Клиент называет content-id; мы читаем объект ОДИН РАЗ и держим у
//!   себя копию. Пока содержимое не менялось, повторный `commit` не стоит ничего: content-id
//!   тот же — перечитывать нечего.
//! - **Двигает окна композитор.** Перемещение не трогает клиента вообще: его пиксели уже у нас.
//!   Это и есть правило ADR 0016 «анимируем трансформации, а не содержимое» — здесь оно не
//!   пожелание, а следствие устройства.
//! - **Рисуем по damage, и ровно раз за оборот цикла.** Кадр никогда не перерисовывается целиком:
//!   закрашиваются только изменившиеся прямоугольники (старое и новое место окна, содержимое,
//!   курсор). События их лишь ОТМЕЧАЮТ ([`Wm::damage`]), рисует один [`Wm::flush`] в конце
//!   оборота — иначе пачка из тридцати событий мыши означала бы тридцать перерисовок, и рука
//!   обгоняла бы экран (Веха 120.2).
//! - **Рамки рисуем МЫ** (server-side decorations). Так у всех окон один вид без единой строчки
//!   в приложениях — ровно то, ради чего затевался общий тулкит (ADR 0016).
//! - **Обои и бар — КЛИЕНТЫ, а не наши поля** (Веха 139, [[layers]]). Поверхность слоя
//!   ([`win::OP_LAYER`]) — то же окно (`Win` с `layer: Some(…)`), только место ей назначает
//!   якорь, а не раскладка, стопка берётся из слоя, и оформления у неё нет. Иначе композитор
//!   обязан был бы уметь читать store и разбирать PNG — в программе, где лежат буферы ВСЕХ окон.
//!
//! ## Чего здесь пока нет
//!
//! Анимаций, буфера обмена, drag&drop и плитки запуска: место под них в протоколе есть
//! (ADR 0016), но веха про другое — про то, чтобы окна вообще появились и жили.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use void_user as sys;
use void_user::win;

/// Кадр 1280×800 RGBA (4 МиБ) + копии содержимого окон.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Окно фреймбуфера в нашем адресном пространстве (как у `term`).
const FB_VA: usize = 0x5000_0000;

/// Отметка сборки — коммит и дата, подставляет `build.rs`. Печатается на старте.
///
/// Раньше здесь стояла константа с номером вехи — и она соврала при первой же возможности:
/// следующая веха её не подняла, а владелец дважды перезаписал флешку, выясняя, почему система
/// «старая». Она была новая. Число, которое надо не забыть поднять руками, однажды не поднимут.
const BUILD: &str = env!("VOID_BUILD");

// ── вид (ADR 0016: один палитра-источник, приложения цветов не знают) ────────
const C_DESKTOP: (u8, u8, u8) = (0x0d, 0x11, 0x17);
/// Веха 137 — рамка полосы стола в обзоре и её же вариант для стола в фокусе.
const C_BAND: (u8, u8, u8) = (0x1e, 0x28, 0x33);
const C_BAND_ON: (u8, u8, u8) = (0x2f, 0x4a, 0x6b);
const C_BORDER: (u8, u8, u8) = (0x30, 0x36, 0x3d);
const C_ACCENT: (u8, u8, u8) = (0x4c, 0x7d, 0xfd);

// ── анимации (Веха 125) ──────────────────────────────────────────────────────
//
// Требование владельца: «чтобы чувствовалось, будто ты сам двигаешь эту камеру» — резкое
// ускорение вначале и плавное торможение. Это ease-out: движение начинается на полной скорости
// и гасится к цели. Обратная кривая (плавный старт) читается как «система подумала и поехала»,
// а нужно «поехало сразу, а доводит уже само».

/// Сколько длится переезд окна, открытие и закрытие. Числа niri: за 150 мс глаз успевает
/// проследить связь «было → стало», а ждать уже не начинает.
const MOVE_MS: u64 = 450;
const OPEN_MS: u64 = 380;
const CLOSE_MS: u64 = 320;
/// Обзор ездит спокойнее: он показывает всю систему, и резкость там суетлива.
const OV_MS: u64 = 450;

/// Насколько окно «поджато» в начале открытия и в конце закрытия — в 1/256 от размера.
/// Небольшое (как в niri): большое превращает появление окна в аттракцион.
const POP: i32 = 232;

/// Ease-out кубический: `1 − (1−t)³`, всё в 1/1024.
fn ease_out(t: i32) -> i32 {
    let inv = (1024 - t.clamp(0, 1024)) as i64;
    (1024 - (inv * inv * inv) / (1024 * 1024)) as i32
}

/// Линейная доля пути между `a` и `b` (`p` — 0..1024).
fn lerp(a: i32, b: i32, p: i32) -> i32 {
    a + (b - a) * p / 1024
}

/// То, ЧТО РИСУЕТСЯ, — в отличие от того, что назначила раскладка. Между ними и живёт анимация.
#[derive(Clone, Copy, PartialEq)]
struct Shown {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Непрозрачность в 1/256.
    a: u32,
}

impl Shown {
    /// Поджатый к центру вариант — начало открытия и конец закрытия.
    fn popped(&self) -> Shown {
        let (w, h) = (self.w * POP / 256, self.h * POP / 256);
        Shown { x: self.x + (self.w - w) / 2, y: self.y + (self.h - h) / 2, w, h, a: 0 }
    }
    fn mix(&self, to: &Shown, p: i32) -> Shown {
        Shown {
            x: lerp(self.x, to.x, p),
            y: lerp(self.y, to.y, p),
            w: lerp(self.w, to.w, p).max(2 * BORDER + 2),
            h: lerp(self.h, to.h, p).max(2 * BORDER + 2),
            a: lerp(self.a as i32, to.a as i32, p).clamp(0, 256) as u32,
        }
    }
    fn rect(&self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.w, self.h)
    }
}

/// Толщина рамки и радиус скругления (Веха 124 — вид взят из noctalia владельца).
///
/// ТИТУЛЬНОЙ ПОЛОСЫ БОЛЬШЕ НЕТ. В тайлинге она не несла ничего: окно не таскают мышью, крестика
/// нет, а имя программы и так видно по содержимому. Взамен — заметная рамка (она же индикатор
/// фокуса) и крупное скругление. Побочно исчезло мерцание заголовка: единственные пиксели,
/// которые писались дважды за кадр, были как раз его глифы.
const BORDER: i32 = 2;
const RADIUS: i32 = 12;

// ── раскладка (Веха 119, [[wm-keys]]) ────────────────────────────────────────
//
// Умолчания взяты из рабочего конфига владельца (niri): те же клавиши, те же смыслы. Схема
// живёт в КОНФИГЕ поколения строками `bind wm <клавиши> <действие>` — как у терминала с Вехи
// 100; зашитая здесь нужна ровно затем, чтобы система поднималась на пустом конфиге.
//
// Крестика закрытия нет и не будет: окно закрывает `Super+Q` (решение владельца).
const DEFAULT_BINDS: &str = "\
bind wm Super+Return spawn-term
bind wm Super+Q close-window
bind wm Super+H focus-column-left
bind wm Super+L focus-column-right
bind wm Super+Up focus-window-up
bind wm Super+Down focus-window-down
bind wm Super+Shift+H move-column-left
bind wm Super+Shift+L move-column-right
bind wm Super+Shift+Up move-window-up
bind wm Super+Shift+Down move-window-down
bind wm Super+BracketLeft move-to-column-left
bind wm Super+BracketRight move-to-column-right
bind wm Super+R width-next
bind wm Super+Equal width-plus
bind wm Super+Minus width-minus
bind wm Super+F maximize-column
bind wm Super+Tab toggle-overview
bind wm Escape close-overview
bind wm Return close-overview
bind wm Super+1 workspace-1
bind wm Super+2 workspace-2
bind wm Super+3 workspace-3
bind wm Super+4 workspace-4
bind wm Super+5 workspace-5
bind wm Super+6 workspace-6
bind wm Super+7 workspace-7
bind wm Super+8 workspace-8
bind wm Super+9 workspace-9
bind wm Super+Shift+1 move-to-workspace-1
bind wm Super+Shift+2 move-to-workspace-2
bind wm Super+Shift+3 move-to-workspace-3
bind wm Super+Shift+4 move-to-workspace-4
bind wm Super+Shift+5 move-to-workspace-5
bind wm Super+Shift+6 move-to-workspace-6
bind wm Super+Shift+7 move-to-workspace-7
bind wm Super+Shift+8 move-to-workspace-8
bind wm Super+Shift+9 move-to-workspace-9
bind wm Super+Shift+Q quit
";

/// Код клавиши Super — тот же, что кладёт ядро (`ps2.rs::keysym`).
const SYM_SUPER: u16 = 0x133;

/// Разобранная строка раскладки.
struct Bind {
    sym: u16,
    mods: u8,
    action: String,
}

/// `"Super+Shift+Q"` → (код клавиши, маска). Имена модификаторов — как в конфиге niri.
fn parse_combo(tok: &str) -> Option<(u16, u8)> {
    let mut mods = 0u8;
    let mut last = tok;
    for part in tok.split('+') {
        match part {
            "Super" | "Mod" => mods |= 8,
            "Shift" => mods |= 1,
            "Ctrl" | "Control" => mods |= 2,
            "Alt" => mods |= 4,
            other => last = other,
        }
    }
    let sym = match last {
        "Return" | "Enter" => 0x101,
        "Escape" | "Esc" => 0x102,
        "Tab" => 0x103,
        "Backspace" => 0x104,
        "Delete" => 0x105,
        "Left" => 0x110,
        "Right" => 0x111,
        "Up" => 0x112,
        "Down" => 0x113,
        "Home" => 0x114,
        "End" => 0x115,
        "PageUp" => 0x116,
        "PageDown" => 0x117,
        "Space" => b' ' as u16,
        // Имена как в конфиге niri: `=` и `-` в строке аккорда путались бы с разделителем `+`.
        "Equal" => b'=' as u16,
        "Minus" => b'-' as u16,
        "BracketLeft" => b'[' as u16,
        "BracketRight" => b']' as u16,
        s => {
            let c = s.chars().next()?;
            if s.chars().count() != 1 {
                return None;
            }
            // Клавиша именуется своим НЕсдвинутым символом: `Super+L` и буква `l` — про одну и
            // ту же клавишу, и различать их регистром значило бы завести две раскладки.
            (c.to_ascii_lowercase() as u32) as u16
        }
    };
    Some((sym, mods))
}

/// Номер рабочего стола из имени действия (`workspace-3` → 2). Считаем от нуля внутри, от
/// единицы снаружи: на клавиатуре нет нулевого стола.
fn digit(name: &str) -> Option<usize> {
    let d = name.rsplit('-').next()?.parse::<usize>().ok()?;
    (1..=SPACE_KEYS).contains(&d).then_some(d - 1)
}

/// Собрать раскладку: строки `bind wm …` из конфига поколения, иначе зашитая схема.
/// Второе значение — ОТКУДА она взялась.
///
/// Правило то же, что у терминала (Веха 100): хоть один `bind` в конфиге — и схема задаётся
/// ЦЕЛИКОМ оттуда. Иначе клавишу нельзя было бы отвязать. Но у правила есть цена, и она
/// всплыла на живом человеке (Веха 121.1): конфиг, посеянный ДО появления новых действий,
/// молча отменяет их все — раскладка есть, она просто старая. Поэтому источник теперь
/// называется вслух: «шесть сочетаний из конфига» при восемнадцати зашитых — это диагноз.
fn load_binds(text: &str) -> (Vec<Bind>, bool) {
    let out = parse_binds(text);
    if out.is_empty() {
        (parse_binds(DEFAULT_BINDS), false)
    } else {
        (out, true)
    }
}

/// Веха 139 — имя картинки для обоев: строка `desktop wallpaper <объект store>` из конфига
/// поколения. Ничего нет — обоев нет, и это нормальный вид системы, а не отсутствие настройки.
///
/// Хвост строки берём ЦЕЛИКОМ, не разбивая по пробелам: имя корня в store — произвольная строка,
/// и пробел в ней не наше дело. Именно из-за разбора по пробелам имя нельзя было передать через
/// `arg:` в самом конфиге (`init::apply_with` режет строку на токены) — здесь конец пути, и
/// резать его второй раз незачем.
fn wallpaper_name(text: &str) -> Option<&str> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("desktop wallpaper") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn parse_binds(text: &str) -> Vec<Bind> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut w = line.split_whitespace();
        if w.next() != Some("bind") || w.next() != Some("wm") {
            continue;
        }
        let (Some(combo), Some(action)) = (w.next(), w.next()) else { continue };
        if let Some((sym, mods)) = parse_combo(combo) {
            out.push(Bind { sym, mods, action: String::from(action) });
        }
    }
    out
}

/// Активное поколение конфига — тем же способом, каким его читает терминал.
fn read_generation(scap: usize) -> Option<String> {
    let mut id = [0u8; 32];
    if sys::obj_get_root(scap, b"system/current", &mut id) != 32 {
        return None;
    }
    let mut name = [0u8; 64];
    let n = sys::obj_get(scap, &id, &mut name);
    if n == 0 || n > name.len() {
        return None;
    }
    let mut root = alloc::vec::Vec::from(&b"system/"[..]);
    root.extend_from_slice(&name[..n]);
    if sys::obj_get_root(scap, &root, &mut id) != 32 {
        return None;
    }
    let mut buf = vec![0u8; 64 * 1024];
    let n = sys::obj_get(scap, &id, &mut buf);
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

fn main_loop() -> ! {
    let Some(fb_cap) = find_fb_cap() else {
        sys::write_console("[wm] нет права на экран (mmio:fb в конфиге)\n".as_bytes());
        sys::exit(1);
    };
    let Some(info) = sys::video_info(fb_cap) else {
        sys::write_console("[wm] ядро не отдало описание видеорежима\n".as_bytes());
        sys::exit(1);
    };
    if !sys::mmio_map(fb_cap, FB_VA) {
        sys::write_console("[wm] не удалось замапить фреймбуфер\n".as_bytes());
        sys::exit(1);
    }
    sys::write_console(alloc::format!("[wm] композитор VOID, {}\n", BUILD).as_bytes());
    let me = sys::self_endpoint();
    let store = store_cap();

    let mut wm = Wm {
        info,
        work: (0, 0, info.width as i32, info.height as i32),
        wins: Vec::new(),
        next_id: 1,
        cursor: (info.width as i32 / 2, info.height as i32 / 2),
        buttons: 0,
        cols: Vec::new(),
        cur: 0,
        scroll_x: 0,
        scroll_from: 0,
        scroll_to: 0,
        scroll_at: 0,
        scroll_dur: 0,
        focus: None,
        slots: 0,
        damage: Vec::new(),
            shadow: vec![0u32; info.width * info.height],
        present_all: false,
        // Веха 137 — стол ровно один, пустой. Дальше их число ведёт `tidy_spaces`.
        ov_bands: Vec::new(),
        spaces: vec![Space::default()],
        space: 0,
        overview: false,
        ov_cam: (0, 0),
        cam_from: (0, 0),
        cam_to: (0, 0),
        cam_at: 0,
        cam_dur: 0,
        super_held: false,
        ov: Vec::new(),
        ov_t: 0,
        ov_from: 0,
        ov_to: 0,
        ov_at: 0,
        ov_dur: 0,
        ov_band_y: 0,
    };

    // Рабочий стол целиком — единственная полная заливка за всю сессию.
    wm.fill_rect(0, 0, info.width as i32, info.height as i32, C_DESKTOP);
    wm.draw_cursor();

    // Конфиг поколения читаем ОДИН раз: из него и раскладка клавиш, и обои.
    let generation = read_generation(store).unwrap_or_default();

    // Веха 139 — обои. Это ТАКОЙ ЖЕ клиент, как терминал, только просит он поверхность слоя, а
    // не окно; композитор о картинках по-прежнему не знает ничего. Имя картинки уезжает
    // аргументом — здесь это можно, а в конфиге поколения нельзя (см. `wallpaper_name`).
    if let Some(name) = wallpaper_name(&generation) {
        let mut arg = Vec::from(name.as_bytes());
        arg.push(0);
        match sys::spawn_with_endpoint(store, b"wall", &arg, me, b"WM\0") {
            Some(_) => {
                sys::write_console("[wm] обои: ".as_bytes());
                sys::write_console(name.as_bytes());
                sys::write_console(b"\n");
            }
            None => sys::write_console("[wm] обои не запустились (нет bin/wall?)\n".as_bytes()),
        }
    }

    // Клиенты — из argv. Право на себя отдаём под именем `WM`: терминал даёт детям `STDIO`,
    // мы даём окна, и путать эти два хоста нельзя.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf);
    let mut spawned = 0usize;
    for prog in abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1) {
        // Аргумента у клиента из `apps` быть не может, и это не наше ограничение: конфиг
        // поколения — текст, разбираемый по ПРОБЕЛАМ (`init::apply_with`), поэтому `arg:img wall`
        // доедет сюда двумя токенами. Обои этой стены не заметили: у них своя строка конфига
        // (`desktop wallpaper …`), которую мы читаем сами и передаём дальше уже аргументом.
        match sys::spawn_with_endpoint(store, prog, &[], me, b"WM\0") {
            Some(pid) => {
                sys::write_console("[wm] запущен клиент ".as_bytes());
                sys::write_console(prog);
                sys::write_console(b"\n");
                let _ = pid;
                spawned += 1;
            }
            None => {
                sys::write_console("[wm] не запустился клиент ".as_bytes());
                sys::write_console(prog);
                sys::write_console(b"\n");
            }
        }
    }
    if spawned == 0 {
        sys::write_console("[wm] клиентов нет — пустой рабочий стол\n".as_bytes());
    }

    let (binds, from_config) = load_binds(&generation);
    let builtin = parse_binds(DEFAULT_BINDS).len();
    sys::write_console(
        alloc::format!(
            "[wm] раскладка: {} сочетаний {} (зашитая знает {})\n",
            binds.len(),
            if from_config { "ИЗ КОНФИГА ПОКОЛЕНИЯ" } else { "— зашитая схема" },
            builtin,
        )
        .as_bytes(),
    );
    if from_config && binds.len() < builtin {
        sys::write_console(
            "[wm] конфиг знает МЕНЬШЕ действий, чем система: он посеян раньше.              `ved /etc/system/terminal.vv` → wm_keys = [] вернёт схему по умолчанию\n"
                .as_bytes(),
        );
    }

    let mut msg = [0u8; 1024];
    let mut mouse = [sys::MouseEvent { dx: 0, dy: 0, buttons: 0, wheel: 0 }; 32];
    loop {
        let mut worked = false;

        // ── мышь ───────────────────────────────────────────────────────────────────────
        let mn = sys::mouse_read(&mut mouse);
        if mn > 0 {
            worked = true;
            for e in &mouse[..mn] {
                wm.on_mouse(e);
            }
        }

        // ── клавиши: сперва АККОРДЫ, потом обычный ввод окну в фокусе ──────────────────
        let mut kev = [sys::KeyEvent { sym: 0, mods: 0, down: false, ch: 0 }; 32];
        let kn = sys::key_read(&mut kev);
        if kn > 0 {
            worked = true;
            for e in &kev[..kn] {
                wm.key_event(e, &binds, store, me);
            }
        }

        // ── ушедшие клиенты ────────────────────────────────────────────────────────────
        // Окно живёт, пока жив его хозяин. Полагаться на прощание нельзя: программа может
        // упасть, и тогда её рамка осталась бы на экране навсегда — с картинкой, за которой
        // никого нет. Спрашиваем ядро, а не верим на слово.
        wm.reap();

        // ── запросы клиентов ───────────────────────────────────────────────────────────
        // Спим только когда делать нечего — и просыпаемся по клавише ИЛИ движению мыши
        // (Веха 115 научила ядро будить на мышь тех же, кого будит клавиша).
        // Пока что-то движется, спать до события нельзя: кадры анимации рисуем мы сами.
        // Восемь миллисекунд — это около ста двадцати кадров в секунду, с запасом.
        let moving = wm.animate(sys::monotonic_ns());
        let got = if worked {
            sys::try_recv(&mut msg)
        } else {
            sys::recv_console(&mut msg, if moving { 8 } else { 200 })
        };
        if let Some(m) = got {
            wm.request(&m, &msg);
        }

        // Кадр — ОДИН на оборот, в самом конце: к этому месту учтены все события пачки, все
        // ответы клиентов и все ушедшие окна. Пока рисовало каждое событие само, рука обгоняла
        // экран (Веха 120.2).
        wm.flush();
    }
}

/// Веха 129 — окно ВА композитора под ЧУЖИЕ буферы кадров: между своей кучей (`0x6000_0000`) и
/// своим стеком (вершина `0x8000_0000`, под ним 64 страницы). Шаг 8 МиБ — полноэкранный кадр
/// 1600×1200×4 это 7.3 МиБ, то есть окно любого допустимого размера в слот влезает.
///
/// Тридцать один слот — это потолок на ОДНОВРЕМЕННО отображённые буферы, а не на число окон за
/// сеанс: слоты возвращаются, когда буфер отпущен.
const SHM_BASE: usize = 0x7000_0000;
const SHM_STRIDE: usize = 8 * 1024 * 1024;
const SHM_SLOTS: usize = 31;

/// Одно окно.
struct Win {
    id: u32,
    owner: usize,
    /// Левый верхний угол РАМКИ на экране.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    /// Заголовок от клиента. Композитор его больше не рисует (полосы заголовка нет), но
    /// хранит: он понадобится подписям в обзоре и списку окон.
    #[allow(dead_code)]
    title: String,
    /// Веха 129 — ОБЩИЙ буфер кадра клиента, отображённый у нас: адрес, длина, право и слот
    /// нашего окна ВА. `buf == 0` — буфера нет (клиент его не дал).
    ///
    /// Копии больше нет вовсе. До этой вехи здесь лежал `Vec<u8>`, куда вклеивались куски,
    /// прочитанные из store по content-id, — и платили мы не за пиксели, а за бухгалтерию вокруг
    /// них ([[compositor-damage]]). Теперь это ТЕ ЖЕ страницы, в которые пишет клиент, и
    /// отображены они ТОЛЬКО НА ЧТЕНИЕ: композитор в чужой кадр не пишет, и это свойство железа,
    /// а не обещание.
    buf: usize,
    buf_len: usize,
    cap: usize,
    slot: usize,
    /// Размер, в котором клиент РИСУЕТ (раскладка буфера). Отдельно от назначенного размера окна
    /// (Веха 126.1): между «раскладка решила, что окно теперь другое» и «клиент завёл буфер под
    /// новый размер» проходит время, и всё это время надо показывать СТАРУЮ картинку, растянув
    /// её. Раньше копия обнулялась сразу — окно чернело, а при складывании в стопку это
    /// выглядело как артефакты.
    bw: i32,
    bh: i32,
    /// Отложенный ответ на `OP_EVENT` (клиент спит в `SYS_CALL`).
    waiting: Option<usize>,
    /// События, накопленные до того, как клиент спросил.
    inbox: Vec<[u8; 8]>,
    /// Что рисуется сейчас (Веха 125). Между `shown` и рамкой из раскладки и живёт анимация.
    shown: Shown,
    /// Откуда началось нынешнее движение и куда идёт.
    from: Shown,
    to: Shown,
    /// Когда началось и сколько длится; `dur == 0` — стоим на месте.
    at: u64,
    dur: u64,
    /// Окно ещё не раскладывали ни разу — появиться оно должно ростом, а не рывком с нуля.
    fresh: bool,
    /// Окно ЗАКРЫВАЕТСЯ: хозяина уже нет, но пиксели держим, пока доигрывает сжатие.
    closing: bool,
    /// Лежит ли окно на АКТИВНОМ рабочем столе (Веха 122). Окно с чужого стола живо, помнит
    /// свои пиксели и продолжает работать — его просто не видно и не потрогать.
    visible: bool,
    /// Веха 139 — поверхность СЛОЯ (обои, бар), а не окно ленты. `None` — обычное окно.
    ///
    /// Одна структура на оба вида намеренно. Всё, что делает окно окном, — буфер, события,
    /// damage, смерть хозяина — у слоя ровно то же самое; разница только в том, кто назначает
    /// место и где оно в стопке. Заведи мы второй список, каждая из этих общих вещей
    /// существовала бы в двух копиях и разошлась бы на первой же правке.
    layer: Option<LayerCfg>,
}

/// Чего поверхность слоя попросила у композитора (Веха 139) — см. [`win::Layer`].
#[derive(Clone, Copy)]
struct LayerCfg {
    layer: u8,
    anchor: u8,
    /// Сколько пикселей у своего края поверхность отнимает у окон.
    exclusive: i32,
}

impl Win {
    /// Кадр клиента, как его видим мы: RGBA8888 по строкам `bw × bh`. Пусто — буфера нет.
    fn px(&self) -> &[u8] {
        if self.buf == 0 {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.buf as *const u8, self.buf_len) }
    }

    /// Обычное ли это окно (участвует в ленте, берёт фокус, получает клавиши).
    fn tiled(&self) -> bool {
        self.layer.is_none()
    }
    /// Слой НИЖЕ окон ленты: обои и подложка.
    fn under(&self) -> bool {
        self.layer.is_some_and(|l| l.layer < win::LAYER_TOP)
    }
    /// Слой ВЫШЕ окон ленты: бар, панель, всплывающее.
    fn over(&self) -> bool {
        self.layer.is_some_and(|l| l.layer >= win::LAYER_TOP)
    }
    /// Толщина рамки и радиус скругления. У слоя их нет: обои в рамке с уголками — это не
    /// оформление, а ошибка, и бар со скруглёнными углами оставил бы щели у краёв экрана.
    fn deco(&self) -> (i32, i32) {
        if self.tiled() {
            (BORDER, RADIUS)
        } else {
            (0, 0)
        }
    }

    /// Начать движение к `target`.
    fn start(&mut self, target: Shown, ms: u64, now: u64) {
        self.from = self.shown;
        self.to = target;
        self.at = now;
        self.dur = ms * 1_000_000;
    }

    /// Продвинуть анимацию. `true` — на этом кадре окно ДВИНУЛОСЬ (значит его старое и новое
    /// места надо перерисовать).
    ///
    /// Отвечаем «двинулось» и на последнем шаге тоже. Веха 126.1: раньше последний шаг возвращал
    /// `false`, и если анимация укладывалась в ОДИН кадр (кадр длиннее её срока — на медленной
    /// машине или при заминке), окно молча перескакивало, ничего не пометив, а на старом месте
    /// оставался его снимок. Именно это владелец видел, складывая окна в стопку.
    fn tick(&mut self, now: u64) -> bool {
        if self.dur == 0 {
            return false;
        }
        let t = ((now.saturating_sub(self.at)) * 1024 / self.dur).min(1024) as i32;
        self.shown = self.from.mix(&self.to, ease_out(t));
        if t >= 1024 {
            self.shown = self.to;
            self.dur = 0;
        }
        true
    }

    /// Прямоугольник рамки.
    fn frame(&self) -> (i32, i32, i32, i32) {
        let (b, _) = self.deco();
        (self.x, self.y, self.w + 2 * b, self.h + 2 * b)
    }
    /// Левый верхний угол СОДЕРЖИМОГО.
    fn content_at(&self) -> (i32, i32) {
        let (b, _) = self.deco();
        (self.x + b, self.y + b)
    }
    /// Попадание — по ВИДИМОМУ положению: человек целится в то, что нарисовано.
    fn hit_frame(&self, px: i32, py: i32, scroll: i32) -> bool {
        let (fx, fy, fw, fh) = self.shown.rect();
        let fx = fx - scroll;
        px >= fx && px < fx + fw && py >= fy && py < fy + fh
    }
}

/// Колонка ленты (Веха 121): окна одно под другим плюс ширина колонки.
///
/// Модель — скроллируемый тайлинг niri ([[wm-keys]]): окна живут в колонках на бесконечной ленте,
/// экран — окно просмотра, которое по ней ездит. Плавающие окна остаются исключением, а не
/// основой; сегодня их нет вовсе.
struct Column {
    /// Идентификаторы окон сверху вниз.
    ids: Vec<u32>,
    /// Индекс в [`WIDTHS`].
    width: usize,
    /// Какое окно колонки в фокусе.
    focus: usize,
}

/// Отложенный рабочий стол: его лента ждёт своей очереди (Веха 122).
///
/// АКТИВНЫЙ стол живёт прямо в полях [`Wm`], а остальные — здесь. Так вся раскладка работает с
/// одной лентой и не знает про столы вовсе; переключение — это обмен лентами, и он в одном месте.
#[derive(Default)]
struct Space {
    cols: Vec<Column>,
    cur: usize,
    scroll_x: i32,
}

/// Сколько столов достижимо ПРЯМЫМ переходом с клавиатуры — по числу цифр (`Super+1…9`).
/// Столов при этом может быть и больше, и меньше: их число живёт своей жизнью (см. [`Wm::tidy_spaces`]).
const SPACE_KEYS: usize = 9;

/// Пресеты ширины колонки — доли экрана. Те же, что у владельца в niri.
const WIDTHS: [(i32, i32); 4] = [(1, 3), (1, 2), (2, 3), (1, 1)];

/// Зазор между окнами и до края экрана.
const GAP: i32 = 8;

/// Меньше этого рабочую область не ужать никакой панели (Веха 139): клиент вправе попросить
/// занятую зону во весь экран, но остаться совсем без места для окон система не должна.
const MIN_WORK: i32 = 64;

/// Масштаб обзора — ровно половина. Фиксированный: см. `build_overview`.
const OV_NUM: i32 = 1;
const OV_DEN: i32 = 2;
/// Зазор между полосами столов в обзоре. Заметно больше оконного: он и разделяет столы.
const OV_GAP: i32 = 48;

struct Wm {
    info: sys::VideoInfo,
    /// Веха 139 — РАБОЧАЯ ОБЛАСТЬ `(x, y, w, h)`: экран минус зоны, занятые слоями (бар).
    /// Лента живёт в ней, а не на экране; без слоёв с занятой зоной она равна экрану.
    work: (i32, i32, i32, i32),
    /// Веха 137 — полосы столов в обзоре: `(x, y, w, h, активный)` в координатах обзора.
    /// Нужны, чтобы стол было ВИДНО, когда на нём нет окон: с динамическими столами последний
    /// всегда пустой, и без полосы про него неоткуда узнать — ровно то возражение, из-за
    /// которого динамические столы и откладывали.
    ov_bands: Vec<(i32, i32, i32, i32, bool)>,
    /// Порядок = z-order: последнее окно рисуется поверх и получает клики первым.
    wins: Vec<Win>,
    /// Лента колонок слева направо.
    cols: Vec<Column>,
    /// Колонка в фокусе.
    cur: usize,
    /// Сдвиг ленты относительно экрана: лента длиннее экрана, экран по ней ездит.
    ///
    /// Веха 125.2 — сдвиг АНИМИРУЕТСЯ отдельно от окон, и это принципиально. Раньше он входил
    /// в координату каждого окна, поэтому «лента поехала» означало «каждое окно поехало само по
    /// себе, из своего места»: окна прибывали вразнобой, наслаивались и оставляли следы. Лента
    /// — одно целое, и двигаться обязана как целое; окна анимируются только когда меняют место
    /// В ЛЕНТЕ.
    scroll_x: i32,
    scroll_from: i32,
    scroll_to: i32,
    scroll_at: u64,
    scroll_dur: u64,
    next_id: u32,
    cursor: (i32, i32),
    buttons: u8,
    focus: Option<u32>,
    /// Занятые слоты окна ВА под буферы клиентов — по биту на слот (Веха 129).
    slots: u32,
    /// Что перерисовать в конце оборота (Веха 120.2). Раньше каждое событие рисовало САМО, и
    /// перетаскивание превращалось в тридцать перерисовок на один оборот цикла.
    damage: Vec<(i32, i32, i32, i32)>,
    /// Строка пикселей в RAM: собираем её здесь, а во фреймбуфер отдаём одной последовательностью.
    /// ТЕНЕВОЙ КАДР (Веха 126): весь экран в обычной памяти. Кадр собирается здесь, а на экран
    /// уходит блитом. Затевалось ради разрывов, а понадобилось ради СТОИМОСТИ: прокрутка ленты
    /// перерисовывала миллион пикселей на кадр, а с теневым кадром она — сдвиг памяти плюс
    /// сборка открывшейся полосы.
    shadow: Vec<u32>,
    /// Отдать на экран весь кадр (после сдвига памяти изменилось всё).
    present_all: bool,
    /// Неактивные рабочие столы (активный — в полях выше).
    spaces: Vec<Space>,
    /// Номер активного стола.
    space: usize,
    /// Включён ли ОБЗОР (Веха 123): столы уменьшены и видны разом.
    overview: bool,
    /// Держат ли сейчас Super. Нужно колесу: `Super+колесо` крутит столы, а голое колесо
    /// принадлежит программе. Маска модификаторов есть у событий КЛАВИШ, у мыши её нет —
    /// поэтому состояние ведём здесь, по нажатиям и отпусканиям.
    super_held: bool,
    /// Где стоит камера обзора. Хранится, а не вычисляется каждый раз, — и в этом суть
    /// (Веха 123.2): камера едет за ОСОЗНАННЫМ выбором (клавиши, колесо, вход в обзор), а
    /// наведение мышью только переносит фокус.
    ov_cam: (i32, i32),
    /// Анимация камеры обзора: откуда, куда, когда началась и сколько длится.
    cam_from: (i32, i32),
    cam_to: (i32, i32),
    cam_at: u64,
    cam_dur: u64,
    /// Что и куда нарисовано в обзоре. Считается один раз при каждом изменении — и рисованием,
    /// и попаданием мыши пользуется ОДИН этот список: два расчёта «где что» означали бы, что
    /// клик приходит не в то окно, которое человек видит.
    ov: Vec<OvItem>,
    /// Переход «лента ↔ обзор» (Веха 126.6): 0 — лента, 1024 — обзор. Одного числа хватает на
    /// всё: обзор — это ОДНА картинка, и наезд камеры на свой стол выражается одним
    /// преобразованием ([`Wm::ov_xform`]), а не анимацией каждого окна по отдельности.
    ov_t: i32,
    ov_from: i32,
    ov_to: i32,
    ov_at: u64,
    ov_dur: u64,
    /// Начало полосы СВОЕГО стола в координатах обзора — точка, в которую наезжает камера.
    /// Считается там же, где сам обзор: иначе разошлось бы с ним при первой же правке.
    ov_band_y: i32,
}

/// Окно в обзоре: куда его уменьшили и с какого стола оно родом.
struct OvItem {
    id: u32,
    space: usize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Потолок списка повреждений. Список нужен, чтобы движение курсора в углу не тянуло за собой
/// перерисовку окна в другом углу; но и длинный список вреден — накладные расходы на каждый
/// прямоугольник свои. При переполнении всё сливается в один охватывающий.
const DAMAGE_MAX: usize = 8;

/// Целочисленный квадратный корень (двоичный поиск по битам — не цикл по единице: считаем его
/// на каждый пиксель углов).
fn isqrt(v: i32) -> i32 {
    if v <= 0 {
        return 0;
    }
    let mut r = 0i32;
    let mut bit = 1i32 << 30;
    let mut rem = v;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= r + bit {
            rem -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}

/// Знаковое расстояние от пикселя до края скруглённого прямоугольника, в 1/256 пикселя
/// (отрицательное — внутри). Веха 125.1.
///
/// Здесь была ошибка, из-за которой владелец видел «то толстую, то тонкую обводку», пропадающие
/// рамки и мусор в углах: форма и её сглаживание считались ДВУМЯ разными способами — бинарный
/// отступ края отдельно, доля покрытия отдельно, — и в углах они расходились. Обводка при этом
/// отмерялась от бинарного отступа, поэтому её толщина гуляла от строки к строке.
///
/// Теперь одно число отвечает на все вопросы сразу: попал ли пиксель в окно (`d < 0`), насколько
/// он накрыт (доля от `d`), и рамка это или содержимое (`d > -BORDER`). Формула стандартная для
/// скруглённого прямоугольника; считаем в полупикселях, чтобы не терять точность на целых.
fn rrect_sd(px: i32, py: i32, x: i32, y: i32, w: i32, h: i32, r: i32) -> i32 {
    // Всё в удвоенных единицах: центр прямоугольника попадает между пикселями.
    let ax = (2 * px - (2 * x + w - 1)).abs();
    let ay = (2 * py - (2 * y + h - 1)).abs();
    let qx = ax - (w - 1 - 2 * r);
    let qy = ay - (h - 1 - 2 * r);
    let (mx, my) = (qx.max(0), qy.max(0));
    let len = isqrt(mx * mx + my * my); // тоже в удвоенных
    let inside = qx.max(qy).min(0);
    (len + inside - 2 * r) * 128 // ×128 = перевод удвоенных в 1/256
}

/// Пересечение двух прямоугольников (x, y, w, h). `None` — не пересекаются.
fn intersect(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> Option<(i32, i32, i32, i32)> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// Охватывающий прямоугольник двух.
fn union(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

impl Wm {
    // ── пиксели ────────────────────────────────────────────────────────────────────────
    //
    // Пишем ПРЯМО во фреймбуфер (он write-combining с Вехи 116.1 — 1,3+ ГБ/с на железе), без
    // теневого кадра: лишняя копия экрана стоила бы 4 МиБ памяти и второго прохода по ним.

    #[inline]
    fn pack(&self, c: (u8, u8, u8)) -> u32 {
        let mut out = 0u32;
        for (i, chan) in [c.0, c.1, c.2].iter().enumerate() {
            let (pos, size) = self.info.rgb[i];
            let size = size.clamp(1, 8);
            out |= ((*chan as u32) >> (8 - size)) << pos;
        }
        out
    }

    /// Разобрать пиксель обратно в RGB — нужно смешиванию (сглаживание углов, дальше прозрачность).
    fn unpack(&self, px: u32) -> (u8, u8, u8) {
        let mut out = [0u8; 3];
        for (i, o) in out.iter_mut().enumerate() {
            let (pos, size) = self.info.rgb[i];
            let size = size.clamp(1, 8);
            let v = (px >> pos) & ((1u32 << size) - 1);
            // Растягиваем обратно до восьми бит: 5-битное «31» обязано стать 255, а не 248.
            *o = ((v * 255) / ((1u32 << size) - 1).max(1)) as u8;
        }
        (out[0], out[1], out[2])
    }

    /// Смешать цвет с тем, что уже лежит в пикселе, в пропорции `a` (0..256).
    fn blend(&self, dst: u32, src: (u8, u8, u8), a: u32) -> u32 {
        if a >= 256 {
            return self.pack(src);
        }
        if a == 0 {
            return dst;
        }
        let d = self.unpack(dst);
        let mix = |s: u8, d: u8| ((s as u32 * a + d as u32 * (256 - a)) / 256) as u8;
        self.pack((mix(src.0, d.0), mix(src.1, d.1), mix(src.2, d.2)))
    }

    #[inline]
    fn put(&self, x: i32, y: i32, px: u32) {
        if x < 0 || y < 0 || x >= self.info.width as i32 || y >= self.info.height as i32 {
            return;
        }
        let bpp = self.info.bpp / 8;
        let dst = FB_VA + y as usize * self.info.pitch + x as usize * bpp;
        unsafe {
            match bpp {
                4 => core::ptr::write_volatile(dst as *mut u32, px),
                2 => core::ptr::write_volatile(dst as *mut u16, px as u16),
                _ => {
                    core::ptr::write_volatile(dst as *mut u8, px as u8);
                    core::ptr::write_volatile((dst + 1) as *mut u8, (px >> 8) as u8);
                    core::ptr::write_volatile((dst + 2) as *mut u8, (px >> 16) as u8);
                }
            }
        }
    }

    /// Записать `n` пикселей одного цвета подряд, начиная с (x, y). Границы проверены ОДИН раз
    /// на строку, а не на пиксель: именно это и стоило дорого — не сама запись во фреймбуфер.
    ///
    /// По 8 байт, пока есть пары: замер на X54C (Веха 116.1) дал 3036 МБ/с против 1784 на
    /// четырёхбайтных записях. Write-combining любит длинные последовательные пачки.
    fn fill_run(&self, x: i32, y: i32, n: usize, px: u32) {
        if y < 0 || y >= self.info.height as i32 || n == 0 {
            return;
        }
        let bpp = self.info.bpp / 8;
        if bpp != 4 {
            for i in 0..n as i32 {
                self.put(x + i, y, px);
            }
            return;
        }
        let mut dst = FB_VA + y as usize * self.info.pitch + x as usize * bpp;
        let two = (px as u64) | ((px as u64) << 32);
        let mut i = 0usize;
        unsafe {
            while i + 1 < n {
                core::ptr::write_volatile(dst as *mut u64, two);
                dst += 8;
                i += 2;
            }
            if i < n {
                core::ptr::write_volatile(dst as *mut u32, px);
            }
        }
    }

    /// Записать готовую строку пикселей из RAM.
    fn write_row(&self, x: i32, y: i32, src: &[u32]) {
        if y < 0 || y >= self.info.height as i32 || src.is_empty() {
            return;
        }
        let bpp = self.info.bpp / 8;
        if bpp != 4 {
            for (i, px) in src.iter().enumerate() {
                self.put(x + i as i32, y, *px);
            }
            return;
        }
        let mut dst = FB_VA + y as usize * self.info.pitch + x as usize * bpp;
        let mut i = 0usize;
        unsafe {
            while i + 1 < src.len() {
                let two = (src[i] as u64) | ((src[i + 1] as u64) << 32);
                core::ptr::write_volatile(dst as *mut u64, two);
                dst += 8;
                i += 2;
            }
            if i < src.len() {
                core::ptr::write_volatile(dst as *mut u32, src[i]);
            }
        }
    }

    fn fill_rect(&self, x: i32, y: i32, w: i32, h: i32, c: (u8, u8, u8)) {
        let px = self.pack(c);
        let x0 = x.max(0);
        let x1 = (x + w).min(self.info.width as i32);
        if x1 <= x0 {
            return;
        }
        for yy in y.max(0)..(y + h).min(self.info.height as i32) {
            self.fill_run(x0, yy, (x1 - x0) as usize, px);
        }
        fence();
    }

    // ── композиция ─────────────────────────────────────────────────────────────────────

    /// Отметить прямоугольник как требующий перерисовки (Веха 120.2).
    ///
    /// Раньше каждое событие рисовало само, немедленно. У мыши это означало до тридцати двух
    /// перерисовок на один оборот цикла — по числу событий в пачке, — и окно продолжало ехать
    /// уже после того, как человек отпустил кнопку: очередь событий отставала от руки.
    /// Теперь событие меняет только СОСТОЯНИЕ, а рисуется всё один раз в конце оборота.
    fn damage(&mut self, x: i32, y: i32, w: i32, h: i32) {
        // Пиксель запаса со всех сторон: сглаженный край живёт на границе прямоугольника, и
        // ровно по границе его обрезало бы — отсюда оставшиеся «кусочки обводки».
        let (x, y, w, h) = (x - 1, y - 1, w + 2, h + 2);
        let Some(r) = intersect(
            (x, y, w, h),
            (0, 0, self.info.width as i32, self.info.height as i32),
        ) else {
            return;
        };
        // Пересекающиеся области сливаем: рисовать одни и те же пиксели дважды за оборот незачем.
        for d in self.damage.iter_mut() {
            if intersect(*d, r).is_some() {
                *d = union(*d, r);
                return;
            }
        }
        if self.damage.len() >= DAMAGE_MAX {
            let mut all = r;
            for d in self.damage.drain(..) {
                all = union(all, d);
            }
            self.damage.push(all);
            return;
        }
        self.damage.push(r);
    }

    /// Продвинуть все анимации на текущий момент; `true` — что-то ещё движется.
    ///
    /// Область под движущимся окном помечается по ОБЪЕДИНЕНИЮ старого и нового положения: за
    /// окном обязан затираться след, и это ровно та же арифметика, что при перетаскивании
    /// (Веха 120.2) — только шаг задаёт время, а не рука.
    fn animate(&mut self, now: u64) -> bool {
        let mut moving = false;
        let mut done: Vec<u32> = Vec::new();
        for i in 0..self.wins.len() {
            let sc = self.scroll_x;
            let was = self.wins[i].shown.rect();
            let was = (was.0 - sc, was.1, was.2, was.3);
            if self.wins[i].tick(now) {
                moving |= self.wins[i].dur != 0;
                let r = self.wins[i].shown.rect();
                self.damage(was.0, was.1, was.2, was.3);
                self.damage(r.0 - sc, r.1, r.2, r.3);
            } else if self.wins[i].closing {
                // Помечаем ВЕСЬ путь сжатия, а не последний кадр: окно уменьшалось из полного
                // размера, и стереть нужно всё, что оно занимало. Последний кадр покрывает лишь
                // поджатый прямоугольник — остальное осталось бы призраком на экране (нашлось
                // проверкой: область просто не перерисовывалась).
                let r = union(self.wins[i].from.rect(), self.wins[i].shown.rect());
                self.damage(r.0 - sc, r.1, r.2, r.3);
                done.push(self.wins[i].id);
            }
        }
        // Досжавшиеся окна убираем — их хозяина давно нет.
        for id in done {
            if let Some(k) = self.win_at(id) {
                // Буфер отпускаем здесь, а не при смерти клиента: всё время сжатия окно рисуется
                // своей последней картинкой, а лежит она в общей области. Ушли оба держателя —
                // страницы вернулись системе.
                self.drop_buf(k);
                self.wins.remove(k);
            }
        }
        // Лента: пока она едет, меняется положение ВСЕГО на экране — помечаем экран целиком.
        // Это дороже точечных прямоугольников, но честно: половина экрана и так меняется, а
        // попытка обойтись кусочками и оставляла те самые следы.
        if self.scroll_dur != 0 {
            let prev_scroll = self.scroll_x;
            let t = ((now.saturating_sub(self.scroll_at)) * 1024 / self.scroll_dur).min(1024) as i32;
            self.scroll_x = lerp(self.scroll_from, self.scroll_to, ease_out(t));
            if t >= 1024 {
                self.scroll_x = self.scroll_to;
                self.scroll_dur = 0;
            } else {
                moving = true;
            }
            // Лента сдвинулась на `dx` — в теневом кадре это СДВИГ ПАМЯТИ, а собрать заново
            // надо лишь открывшуюся полосу с краю. Полная пересборка здесь и делала движение
            // рваным: миллион пикселей на кадр против нескольких тысяч.
            let dx = self.scroll_x - prev_scroll;
            let (sw, shh) = (self.info.width as i32, self.info.height as i32);
            if dx != 0 && dx.abs() < sw && !self.overview {
                let pitch = sw as usize;
                let mut sh = core::mem::take(&mut self.shadow);
                let n = dx.unsigned_abs() as usize;
                for yy in 0..shh as usize {
                    let row = &mut sh[yy * pitch..(yy + 1) * pitch];
                    if dx > 0 {
                        row.copy_within(n.., 0); // лента уехала влево
                    } else {
                        row.copy_within(..pitch - n, n);
                    }
                }
                self.shadow = sh;
                // Открывшаяся полоса + запас на сглаженные края.
                let strip = if dx > 0 { (sw - n as i32 - 2, n as i32 + 2) } else { (0, n as i32 + 2) };
                self.damage(strip.0, 0, strip.1, shh);
                self.present_all = true;
            } else {
                self.damage(0, 0, sw, shh);
            }
        }

        // Камера обзора едет так же, но своим сроком: обзор показывает всю систему, и резкость
        // там суетлива.
        if self.overview && self.cam_dur != 0 {
            let t = ((now.saturating_sub(self.cam_at)) * 1024 / self.cam_dur).min(1024) as i32;
            let p = ease_out(t);
            self.ov_cam = (
                lerp(self.cam_from.0, self.cam_to.0, p),
                lerp(self.cam_from.1, self.cam_to.1, p),
            );
            if t >= 1024 {
                self.ov_cam = self.cam_to;
                self.cam_dur = 0;
            } else {
                moving = true;
            }
            self.build_overview(false);
        }

        // Переход «лента ↔ обзор» (Веха 126.6). Двигается ВСЁ на экране, поэтому кадр
        // помечаем целиком — как и при движении ленты: попытка обойтись кусочками оставляла
        // бы следы, а половина экрана всё равно меняется.
        if self.ov_dur != 0 {
            let t = ((now.saturating_sub(self.ov_at)) * 1024 / self.ov_dur).min(1024) as i32;
            self.ov_t = lerp(self.ov_from, self.ov_to, ease_out(t));
            self.damage(0, 0, self.info.width as i32, self.info.height as i32);
            if t >= 1024 {
                self.ov_t = self.ov_to;
                self.ov_dur = 0;
                if self.ov_to == 0 {
                    // Выход доигран: дальше рисует лента, список обзора больше не нужен.
                    self.overview = false;
                    self.ov.clear();
                }
            } else {
                moving = true;
            }
        }
        moving
    }

    /// Начать переход обзора. `to`: 1024 — в обзор, 0 — обратно в ленту.
    /// Считаем ОТ ТЕКУЩЕГО положения, а не от края: нажатый посреди перехода `Super+Tab`
    /// разворачивает движение с того места, где оно есть, а не дёргает картинку к началу.
    fn ov_start(&mut self, to: i32) {
        self.ov_from = self.ov_t;
        self.ov_to = to;
        self.ov_at = sys::monotonic_ns();
        self.ov_dur = OV_MS * 1_000_000;
    }

    /// Уйти из обзора: лента встаёт в КОНЕЧНОЕ положение сразу, а видимый переход к ней
    /// доигрывает преобразование обзора.
    ///
    /// Порядок именно такой, и он не произволен: преобразование целится в конечные
    /// прямоугольники ленты, и если бы те в этот момент ещё ехали своей анимацией, окна
    /// приехали бы мимо — а в конце перехода их дёрнуло бы на место.
    fn leave_overview(&mut self) {
        self.relayout();
        self.settle();
        // Пересчитать элементы под новую ленту (в обзоре могли выбрать другой стол), но КАМЕРУ
        // не трогать: она осталась там, где человек смотрел, — значит прыжка не будет.
        self.build_overview(false);
        self.ov_start(0);
    }

    /// Досрочно доиграть движение ленты и окон — поставить всё туда, где оно окажется.
    fn settle(&mut self) {
        self.scroll_x = self.scroll_to;
        self.scroll_dur = 0;
        for w in self.wins.iter_mut() {
            if w.dur != 0 {
                w.shown = w.to;
                w.dur = 0;
            }
        }
    }

    /// Начать закрытие окна: пиксели держим, пока доигрывает сжатие (Веха 125).
    ///
    /// Раньше окно исчезало в тот же миг, когда умирал его хозяин. Чтобы оно успело сжаться,
    /// композитор обязан ПЕРЕЖИВАТЬ клиента — на сто тридцать миллисекунд у окна появляется
    /// состояние «закрывается, хозяина уже нет».
    fn begin_close(&mut self, id: u32) {
        let now = sys::monotonic_ns();
        let Some(k) = self.win_at(id) else { return };
        if self.wins[k].closing {
            return;
        }
        // Веха 139 — поверхность слоя уходит СРАЗУ, без сжатия: обои, уползающие в точку, это
        // не «оживление», а поломка на экране; а бар, доигрывающий анимацию после смерти
        // хозяина, всё это время держал бы занятую зону, и лента дёргалась бы дважды.
        if self.wins[k].layer.is_some() {
            let (rx, ry, rw, rh) = self.wins[k].shown.rect();
            self.drop_buf(k);
            self.wins.remove(k);
            self.damage(rx, ry, rw, rh);
            self.recompute_work();
            return;
        }
        self.wins[k].closing = true;
        self.wins[k].visible = true;
        let target = self.wins[k].shown.popped();
        self.wins[k].start(target, CLOSE_MS, now);
        self.unlink(id);
        // Веха 137 — закрылось последнее окно стола, и стол должен исчезнуть (кроме того, на
        // котором стоим, и последнего). Делается ЗДЕСЬ, а не внутри `unlink`: тот же `unlink`
        // зовут при переезде окна между столами, и убрать стол посреди переезда значило бы
        // сдвинуть номер назначения у себя под руками.
        self.tidy_spaces();
    }

    /// Нарисовать всё накопленное. Ровно один раз за оборот цикла — это и есть «кадр».
    fn flush(&mut self) {
        if self.damage.is_empty() {
            return;
        }
        let rects = core::mem::take(&mut self.damage);
        for (x, y, w, h) in &rects {
            self.repaint(*x, *y, *w, *h);
        }
        // На экран — либо те же области, либо весь кадр (после сдвига памяти изменилось всё).
        let pitch = self.info.width as usize;
        let sh = core::mem::take(&mut self.shadow);
        if self.present_all {
            self.present_all = false;
            for yy in 0..self.info.height as i32 {
                let off = yy as usize * pitch;
                self.write_row(0, yy, &sh[off..off + pitch]);
            }
        } else {
            for (x, y, w, h) in &rects {
                for yy in *y..*y + *h {
                    let off = yy as usize * pitch + *x as usize;
                    self.write_row(*x, yy, &sh[off..off + *w as usize]);
                }
            }
        }
        self.shadow = sh;
        fence();
    }

    /// Перерисовать прямоугольник экрана. Строка собирается ЦЕЛИКОМ в памяти — стол, окна в
    /// порядке z, курсор — и уходит во фреймбуфер одной последовательностью записей.
    ///
    /// Отсюда исчезло мерцание (Веха 120.3). Раньше область сначала заливалась цветом стола, а
    /// потом поверх рисовались окна: пиксель под окном писался ДВАЖДЫ, и глаз успевал поймать
    /// промежуточное состояние — при перетаскивании это выглядело как мигание окна. Теперь
    /// каждый пиксель экрана пишется ровно один раз за кадр.
    fn repaint(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let Some((x0, y0, w, h)) = intersect(
            (x, y, w, h),
            (0, 0, self.info.width as i32, self.info.height as i32),
        ) else {
            return;
        };
        // Собираем в ТЕНЕВОЙ кадр; на экран он уйдёт блитом в `flush`.
        let mut sh = core::mem::take(&mut self.shadow);
        let pitch = self.info.width as usize;
        for yy in y0..y0 + h {
            let off = yy as usize * pitch + x0 as usize;
            self.compose_row(&mut sh[off..off + w as usize], yy, x0);
        }
        self.shadow = sh;

    }

    /// Собрать одну строку экрана: стол → нижние слои → окна → верхние слои → курсор.
    fn compose_row(&self, out: &mut [u32], yy: i32, x0: i32) {
        let desktop = self.pack(C_DESKTOP);
        out.fill(desktop);
        let x1 = x0 + out.len() as i32;

        // Веха 139 — слои ПОД окнами. Рисуются и в ленте, и в обзоре, и порядок здесь не
        // порядок в `wins`, а порядок СЛОЁВ: композитор поднимает окно в стопке при фокусе, и
        // всплывшее окно не должно уметь оказаться под обоями.
        self.draw_layers(out, yy, x0, x1, false);
        if self.overview {
            self.compose_row_overview(out, yy, x0, x1);
            self.draw_layers(out, yy, x0, x1, true);
            self.draw_cursor_row(out, yy, x0, x1);
            return;
        }

        for win in &self.wins {
            if !win.visible || !win.tiled() {
                continue;
            }
            // Рисуем ПО АНИМИРОВАННОМУ прямоугольнику, а не по тому, что назначила раскладка
            // (Веха 125). Отсюда следствие: содержимое приходится масштабировать — во время
            // переезда и сжатия окно на экране не совпадает со своим буфером.
            let (sxr, fy, fw, fh) = win.shown.rect();
            let active = self.focus == Some(win.id);
            let rect = (sxr - self.scroll_x, fy, fw, fh);
            self.draw_win_row(out, yy, x0, x1, rect, win, active, win.shown.a);
        }

        self.draw_layers(out, yy, x0, x1, true);
        self.draw_cursor_row(out, yy, x0, x1);
    }

    /// Строка поверхностей слоя: `over = false` — те, что под окнами, `true` — те, что над.
    ///
    /// Место у слоя ФИКСИРОВАННОЕ и в экранных координатах: ни прокрутка ленты, ни обзор его не
    /// трогают. Обои, уезжающие вместе с лентой, были бы не обоями, а очень широким окном.
    fn draw_layers(&self, out: &mut [u32], yy: i32, x0: i32, x1: i32, over: bool) {
        for win in &self.wins {
            if if over { !win.over() } else { !win.under() } {
                continue;
            }
            self.draw_win_row(out, yy, x0, x1, win.shown.rect(), win, false, 256);
        }
    }

    /// Одна строка ОДНОГО окна по экранному прямоугольнику — общая для ленты и обзора.
    ///
    /// Вынесено из [`Self::compose_row`] Вехой 126.6. До неё обзор рисовал окна своим кодом:
    /// квадратная рамка в один пиксель, без скруглений и сглаживания. Это были ДВА РАЗНЫХ ВИДА
    /// одного окна, и переход между ними анимировать нечем — как ни двигай прямоугольник, в
    /// момент переключения картинка менялась скачком. С общим кодом остаётся ровно одна
    /// разница — прямоугольник, — а прямоугольник анимируется.
    ///
    /// Масштаб берётся из соотношения прямоугольника и буфера окна: выборка ближайшего пикселя.
    /// Усреднение было бы красивее, но стоит нескольких чтений на каждый выводимый пиксель, а
    /// обзор перерисовывается на каждое движение курсора; текст в уменьшенном окне всё равно
    /// нечитаем — важно узнать окно по форме и цвету.
    #[allow(clippy::too_many_arguments)]
    fn draw_win_row(
        &self,
        out: &mut [u32],
        yy: i32,
        x0: i32,
        x1: i32,
        (fx, fy, fw, fh): (i32, i32, i32, i32),
        win: &Win,
        active: bool,
        alpha: u32,
    ) {
        {
            // Веха 139 — рамка и скругление берутся У ОКНА, а не из констант: у поверхности
            // слоя их нет вовсе, и с нулевой рамкой этот же код превращается в честное
            // «содержимое от края до края» — без единой отдельной ветки на слои.
            let (bord, rad) = win.deco();
            if yy < fy || yy >= fy + fh || fw <= 2 * bord || fh <= 2 * bord {
                return;
            }
            let (sx, ex) = (fx.max(x0), (fx + fw).min(x1));
            if ex <= sx {
                return;
            }
            let border = self.unpack(self.pack(if active { C_ACCENT } else { C_BORDER }));
            let (cw, ch) = (fw - 2 * bord, fh - 2 * bord);
            let src_row = (yy - fy - bord) * win.bh / ch.max(1);
            let px = win.px();
            let has_content = !px.is_empty() && src_row >= 0 && src_row < win.bh;
            // Расстояние до края нужно ТОЛЬКО у краёв. В середине окна ответ известен заранее,
            // а корень там стоил бы дороже всего остального вместе взятого: полноэкранная
            // перерисовка — это миллион пикселей, и миллион квадратных корней на кадр
            // превращали плавное движение в рывки (Веха 125.3).
            let top = yy - fy;
            let corner_row = top < rad || fy + fh - 1 - yy < rad;
            let edge_row = top < bord || fy + fh - 1 - yy < bord;
            let opaque = alpha >= 256;
            for xx in sx..ex {
                let dh = xx - fx;
                let near_x = dh < rad || fx + fw - 1 - xx < rad;
                let (cover, inner) = if corner_row && near_x {
                    let d = rrect_sd(xx, yy, fx, fy, fw, fh, rad);
                    (
                        (128 - d).clamp(0, 256) as u32,
                        (128 - (d + bord * 256)).clamp(0, 256) as u32,
                    )
                } else if edge_row || dh < bord || fx + fw - 1 - xx < bord {
                    (256, 0) // прямая часть рамки
                } else {
                    (256, 256) // содержимое
                };
                let cover = cover * alpha / 256;
                if cover == 0 {
                    continue;
                }
                let content = if has_content && inner != 0 {
                    let col = (xx - fx - bord) * win.bw / cw.max(1);
                    let p = ((src_row * win.bw + col) * 4) as usize;
                    if col >= 0 && col < win.bw && p + 2 < px.len() {
                        (px[p], px[p + 1], px[p + 2])
                    } else {
                        border
                    }
                } else {
                    border
                };
                let i = (xx - x0) as usize;
                // Быстрый путь: непрозрачный пиксель в середине — просто цвет, без смешивания.
                if opaque && cover >= 256 && (inner >= 256 || inner == 0) {
                    out[i] = self.pack(if inner == 0 { border } else { content });
                    continue;
                }
                let px = self.blend(out[i], border, cover);
                out[i] = if inner == 0 { px } else { self.blend(px, content, inner * cover / 256) };
            }
        }
    }

    /// Обзор: те же окна тем же кодом ([`Self::draw_win_row`]), но через преобразование обзора.
    fn compose_row_overview(&self, out: &mut [u32], yy: i32, x0: i32, x1: i32) {
        let (a, bx, by) = self.ov_xform();
        // Полосы столов — ПОД окнами и только в самом обзоре (`ov_t`): на переходе они бы
        // разъезжались вместе с камерой и мигали по краям кадра.
        if self.ov_t > 512 {
            for &(px, py, pw, ph, active) in &self.ov_bands {
                let (rx, ry) = (px * a / 1024 + bx, py * a / 1024 + by);
                let (rw, rh) = ((pw * a / 1024).max(2), (ph * a / 1024).max(2));
                if yy < ry || yy >= ry + rh {
                    continue;
                }
                let edge = yy == ry || yy == ry + rh - 1;
                let c = if active { C_BAND_ON } else { C_BAND };
                let (lo, hi) = (rx.max(x0), (rx + rw).min(x1));
                for x in lo..hi {
                    let i = (x - x0) as usize;
                    let on_side = x == rx || x == rx + rw - 1;
                    if edge || on_side {
                        out[i] = self.pack(c);
                    }
                }
            }
        }
        for it in &self.ov {
            let Some(k) = self.win_at(it.id) else { continue };
            let win = &self.wins[k];
            let rect = (
                it.x * a / 1024 + bx,
                it.y * a / 1024 + by,
                (it.w * a / 1024).max(2 * BORDER + 2),
                (it.h * a / 1024).max(2 * BORDER + 2),
            );
            let active = self.focus == Some(it.id) && it.space == self.space;
            self.draw_win_row(out, yy, x0, x1, rect, win, active, 256);
        }
    }

    /// Преобразование обзора для текущего кадра: `экран = обзор × A / 1024 + B` (Веха 126.6).
    ///
    /// При `ov_t = 1024` (обзор целиком) это тождество — обзор рисуется ровно так, как посчитан.
    /// При `ov_t = 0` оно возвращает окна активного стола ТОЧНО туда, где они лежат в ленте:
    /// поэтому крайний кадр перехода совпадает с кадром ленты пиксель в пиксель, и переключения
    /// режима на экране не видно вовсе — видно только движение.
    ///
    /// Полосы прочих столов при этом уезжают за край сами собой, без единой строки кода на это:
    /// соседняя полоса отстоит на `band_h + OV_GAP`, а увеличение вдвое уносит её за высоту
    /// экрана. Ровно это и значит «камера наезжает на свой стол».
    fn ov_xform(&self) -> (i32, i32, i32) {
        let p = self.ov_t; // 0 — лента, 1024 — обзор
        let a = lerp(1024 * OV_DEN / OV_NUM, 1024, p);
        // Куда должно смещаться, чтобы при `p = 0` совпасть с лентой (вывод — в заметке
        // [[overview]]): по горизонтали камера обзора минус прокрутка ленты, по вертикали —
        // начало полосы СВОЕГО стола.
        let b0x = self.ov_cam.0 * OV_DEN / OV_NUM - self.scroll_x;
        let b0y = -self.ov_band_y * OV_DEN / OV_NUM;
        (a, lerp(b0x, 0, p), lerp(b0y, 0, p))
    }

    /// Курсор поверх всего: он не принадлежит ни одному окну.
    fn draw_cursor_row(&self, out: &mut [u32], yy: i32, x0: i32, x1: i32) {
        let cy = yy - self.cursor.1;
        if cy >= 0 && cy < CUR_H {
            let fill = self.pack(if self.buttons != 0 { C_ACCENT } else { (255, 255, 255) });
            let edge = self.pack((0, 0, 0));
            for (col, ch) in CURSOR[cy as usize].bytes().enumerate() {
                if ch == b' ' {
                    continue;
                }
                let xx = self.cursor.0 + col as i32;
                if xx >= x0 && xx < x1 {
                    out[(xx - x0) as usize] = if ch == b'#' { edge } else { fill };
                }
            }
        }
    }

    // ── раскладка колонками (Веха 121) ─────────────────────────────────────────────────────

    fn win_at(&self, id: u32) -> Option<usize> {
        self.wins.iter().position(|w| w.id == id)
    }

    /// Ширина колонки в пикселях по её пресету — от РАБОЧЕЙ ОБЛАСТИ, а не от экрана.
    fn col_width(&self, c: &Column) -> i32 {
        let (n, d) = WIDTHS[c.width.min(WIDTHS.len() - 1)];
        (self.work.2 - GAP) * n / d - GAP
    }

    /// Веха 139 — где лежит поверхность слоя: якорь + запрошенный размер → прямоугольник экрана.
    ///
    /// Прижата к обоим краям оси — растянута по ней (обои прижаты ко всем четырём и занимают
    /// экран целиком). Прижата к одному — стоит у него своим размером. Ни к одному — по центру:
    /// это не «правильное» поведение, а единственное осмысленное на бессмысленный запрос.
    ///
    /// Считается от ЭКРАНА, а не от рабочей области: слой сам её и определяет, и считать его
    /// место от неё значило бы бесконечно уточнять само себя.
    fn layer_rect(&self, l: &LayerCfg, w: i32, h: i32) -> (i32, i32, i32, i32) {
        let (sw, sh) = (self.info.width as i32, self.info.height as i32);
        let axis = |anchor_lo: bool, anchor_hi: bool, size: i32, screen: i32| match (
            anchor_lo, anchor_hi,
        ) {
            (true, true) => (0, screen),
            (true, false) => (0, size.min(screen)),
            (false, true) => ((screen - size).max(0), size.min(screen)),
            (false, false) => (((screen - size) / 2).max(0), size.min(screen)),
        };
        let (x, rw) = axis(
            l.anchor & win::ANCHOR_LEFT != 0, l.anchor & win::ANCHOR_RIGHT != 0, w, sw,
        );
        let (y, rh) = axis(
            l.anchor & win::ANCHOR_TOP != 0, l.anchor & win::ANCHOR_BOTTOM != 0, h, sh,
        );
        (x, y, rw, rh)
    }

    /// Веха 139 — пересчитать РАБОЧУЮ ОБЛАСТЬ: экран минус зоны, занятые слоями.
    ///
    /// Это и есть весь смысл «занятой зоны»: бар сверху не накрывает окна, а сдвигает их —
    /// иначе первая строка терминала уходила бы под панель. Отсчитывается зона от края ЭКРАНА,
    /// а не от края поверхности: так два бара на одном краю просто не складываются в лесенку, а
    /// перекрываются, и это честнее, чем зависимость раскладки от порядка запуска клиентов.
    fn recompute_work(&mut self) {
        let (sw, sh) = (self.info.width as i32, self.info.height as i32);
        let (mut x0, mut y0, mut x1, mut y1) = (0, 0, sw, sh);
        for w in &self.wins {
            let Some(l) = w.layer else { continue };
            if l.exclusive <= 0 || w.closing {
                continue;
            }
            // Зона имеет смысл только у прижатого к ОДНОМУ краю оси: «панель посередине отняла
            // тридцать пикселей» не значит ничего — непонятно, с какой стороны.
            let (t, b) = (l.anchor & win::ANCHOR_TOP != 0, l.anchor & win::ANCHOR_BOTTOM != 0);
            let (le, r) = (l.anchor & win::ANCHOR_LEFT != 0, l.anchor & win::ANCHOR_RIGHT != 0);
            match (t, b) {
                (true, false) => y0 = y0.max(l.exclusive),
                (false, true) => y1 = y1.min(sh - l.exclusive),
                _ => {}
            }
            match (le, r) {
                (true, false) => x0 = x0.max(l.exclusive),
                (false, true) => x1 = x1.min(sw - l.exclusive),
                _ => {}
            }
        }
        // Панель во весь экран не должна оставлять раскладку с отрицательной шириной: окну
        // всегда есть куда лечь, даже если это выглядит тесно.
        let work = (x0, y0, (x1 - x0).max(MIN_WORK), (y1 - y0).max(MIN_WORK));
        if work != self.work {
            self.work = work;
            self.relayout();
        }
    }

    /// Геометрия ленты в ЕЁ СОБСТВЕННЫХ координатах: рамки окон и полная ширина ленты.
    ///
    /// Чистая функция, ничего не меняющая, — и это важно: по ней живут ДВА потребителя,
    /// раскладка экрана и обзор. Две копии одной арифметики разъехались бы на первой же правке
    /// (обзор показывал бы не то, что получится при выходе из него).
    fn strip_layout(&self, cols: &[Column]) -> (Vec<(u32, i32, i32, i32, i32)>, i32) {
        // Веха 139 — считаем от РАБОЧЕЙ ОБЛАСТИ. Её начало входит прямо в координаты ленты, а не
        // прибавляется где-то потом: иначе смещение пришлось бы помнить и рисованию, и попаданию
        // мыши, и обзору — трём местам сразу, то есть трём случаям разойтись.
        let (ox, oy, _, oh) = self.work;
        let mut out = Vec::new();
        let mut x = ox + GAP;
        for c in cols {
            let cw = self.col_width(c);
            let n = c.ids.len().max(1) as i32;
            let cell = (oh - GAP) / n - GAP;
            for (wi, id) in c.ids.iter().enumerate() {
                let y = oy + GAP + wi as i32 * (cell + GAP);
                let h = if wi + 1 == c.ids.len() { oy + oh - GAP - y } else { cell };
                out.push((*id, x, y, cw, h));
            }
            x += cw + GAP;
        }
        (out, x)
    }

    /// Пересчитать геометрию всех окон и прокрутку ленты; всем, у кого размер изменился, послать
    /// `EV_RESIZE`.
    ///
    /// Один пересчёт на любое изменение — вместо правки координат в каждом действии. Так
    /// «переехала колонка», «сменилась ширина», «закрылось окно» и «появилось новое» не могут
    /// разойтись в понимании того, где что лежит.
    fn relayout(&mut self) {
        let screen_h = self.info.height as i32;
        // Видно ровно то, что лежит на активном столе. Считаем это здесь, а не при
        // переключении: раскладка и так обходит все окна активной ленты — второй список
        // «кто виден» разошёлся бы с первым при первой же правке.
        for w in self.wins.iter_mut() {
            // Закрывающееся окно остаётся видимым: его уже нет в раскладке, но сжатие доигрывает.
            // Поверхность слоя видна ВСЕГДА: обои и бар не принадлежат столу (Веха 139).
            w.visible = w.closing || !w.tiled();
        }
        let (frames, _) = self.strip_layout(&self.cols);

        // Куда уехала лента: колонка в фокусе обязана быть видна целиком; если она шире рабочей
        // области, показываем её левый край.
        let (ox, _, ow, _) = self.work;
        let mut x = ox + GAP;
        let mut want = self.scroll_to;
        for (i, c) in self.cols.iter().enumerate() {
            let cw = self.col_width(c);
            if i == self.cur {
                if x - want < ox + GAP {
                    want = x - ox - GAP;
                }
                if x + cw - want > ox + ow - GAP {
                    want = x + cw - (ox + ow) + GAP;
                }
            }
            x += cw + GAP;
        }
        if self.cols.is_empty() {
            want = 0;
        }
        if want != self.scroll_to {
            self.scroll_from = self.scroll_x;
            self.scroll_to = want;
            self.scroll_at = sys::monotonic_ns();
            self.scroll_dur = MOVE_MS * 1_000_000;
        }

        let mut resized: Vec<(u32, i32, i32)> = Vec::new();
        for (id, fx, fy, fw, fh) in frames {
            let Some(k) = self.win_at(id) else { continue };
            self.wins[k].visible = true;
            // Клиенту назначается размер СОДЕРЖИМОГО: рамку и заголовок рисуем мы.
            let (cw2, ch2) = ((fw - 2 * BORDER).max(32), (fh - 2 * BORDER).max(32));
            self.wins[k].x = fx;
            self.wins[k].y = fy;
            let target = Shown {
                x: fx,
                y: fy,
                w: cw2 + 2 * BORDER,
                h: ch2 + 2 * BORDER,
                a: 256,
            };
            let now = sys::monotonic_ns();
            if self.wins[k].fresh {
                // Первое появление: растём из поджатого и проявляемся.
                self.wins[k].fresh = false;
                self.wins[k].shown = target.popped();
                self.wins[k].start(target, OPEN_MS, now);
            } else if self.wins[k].to != target {
                self.wins[k].start(target, MOVE_MS, now);
            }
            if self.wins[k].w != cw2 || self.wins[k].h != ch2 {
                self.wins[k].w = cw2;
                self.wins[k].h = ch2;
                // Буфер НЕ трогаем: пусть старая картинка тянется, пока клиент не заведёт новый
                // (`OP_REBUF`). Обнулять его здесь значило бы чернить окно на всё время между
                // решением раскладки и ответом клиента — на складывании окон в стопку это
                // выглядело как артефакты (Веха 126.1).
                resized.push((id, cw2, ch2));
            }
        }
        let _ = screen_h;
        for (id, w, h) in resized {
            if let Some(k) = self.win_at(id) {
                let ev = [win::EV_RESIZE, w as u8, (w >> 8) as u8, h as u8, (h >> 8) as u8, 0, 0, 0];
                self.send(k, ev, 5);
            }
        }
        self.damage(0, 0, self.info.width as i32, screen_h);
    }

    /// Пересчитать раскладку ОБЗОРА (Вехи 123, 123.1).
    ///
    /// Масштаб ФИКСИРОВАННЫЙ ([`OV_NUM`]/[`OV_DEN`]), а не «лишь бы всё влезло» — так сделано в
    /// niri, и владелец попросил так же. Разница не косметическая: при подгонке под содержимое
    /// каждое новое окно уменьшало ВСЕ остальные, то есть привычная картинка менялась от того,
    /// что где-то открылся ещё один терминал. При фиксированном окна просто уходят за край, и
    /// узнаваемость раскладки не зависит от их числа.
    ///
    /// Экран — это камера над столбцом столов: она наводится на стол в фокусе (по вертикали) и
    /// на окно в фокусе (по горизонтали). Всё, что не попало в кадр, честно остаётся за краем.
    fn build_overview(&mut self, recenter: bool) {
        self.ov.clear();
        self.ov_bands.clear();
        let (sw, sh) = (self.info.width as i32, self.info.height as i32);
        let band_h = sh * OV_NUM / OV_DEN;
        // Веха 137 — столы динамические, и пустых среди них не бывает нигде, кроме конца и
        // активного (это и есть инвариант `tidy_spaces`). Поэтому показываем ВСЕ: фильтр,
        // который здесь стоял, теперь ничего бы не отсекал, а прятал бы расхождение, если оно
        // всё-таки появится.
        let shown: Vec<usize> = (0..self.space_count()).collect();
        let me = shown.iter().position(|&i| i == self.space).unwrap_or(0) as i32;

        // Камера наводится ТОЛЬКО по осознанному выбору — клавишами, колесом, при входе в
        // обзор (Веха 123.2). Наведение мышью её не двигает, и это не лень, а необходимость:
        // стоило камере ехать за курсором, как окна разъезжались под ним, под курсором
        // оказывалось следующее, оно уезжало в центр — и так без конца. Среднее из трёх окон
        // выбрать было нельзя в принципе: оно перескакивало раньше, чем в него попадали.
        if recenter {
            let cam_y = GAP + me * (band_h + OV_GAP) + band_h / 2 - sh / 2;
            let focus_x = self
                .focused_id()
                .and_then(|id| {
                    let (frames, _) = self.strip_layout(&self.cols);
                    frames.iter().find(|f| f.0 == id).map(|f| (f.1 + f.3 / 2) * OV_NUM / OV_DEN)
                })
                .unwrap_or(sw / 2);
            self.aim_camera((focus_x - sw / 2, cam_y));
        }
        let (cam_x, cam_y) = self.ov_cam;

        for (bi, &sp) in shown.iter().enumerate() {
            let by = GAP + bi as i32 * (band_h + OV_GAP) - cam_y;
            // Своя полоса запоминается ДО отсечения: она может быть за кадром (камера уехала на
            // другой стол), а переходу её начало нужно в любом случае — это точка наезда.
            if sp == self.space {
                self.ov_band_y = by;
            }
            if by + band_h < 0 || by > sh {
                continue; // полоса целиком за кадром — считать её нечего
            }
            self.ov_bands.push((0, by, sw, band_h, sp == self.space));
            let cols: &[Column] =
                if sp == self.space { &self.cols } else { &self.spaces[sp].cols };
            let (frames, _) = self.strip_layout(cols);
            for (id, fx, fy, fw, fh) in frames {
                self.ov.push(OvItem {
                    id,
                    space: sp,
                    x: fx * OV_NUM / OV_DEN - cam_x,
                    y: by + fy * OV_NUM / OV_DEN,
                    w: (fw * OV_NUM / OV_DEN).max(1),
                    h: (fh * OV_NUM / OV_DEN).max(1),
                });
            }
        }
        self.damage(0, 0, sw, sh);
    }

    /// Сколько столов существует СЕЙЧАС (Веха 137). Активный тоже занимает свой слот в
    /// `spaces` — его лента лишь временно вынута в поля `Wm`.
    fn space_count(&self) -> usize {
        self.spaces.len()
    }

    /// Пуст ли стол `i`. Активный спрашивать надо не у `spaces`: его лента вынута в поля `Wm`,
    /// и в массиве на его месте лежит пустышка. Об эту разницу спотыкается всё, что считает
    /// столы, — поэтому вопрос задаётся ровно в одном месте.
    fn space_is_empty(&self, i: usize) -> bool {
        if i == self.space {
            self.cols.is_empty()
        } else {
            self.spaces.get(i).is_none_or(|s| s.cols.is_empty())
        }
    }

    /// Веха 137 — привести число столов в порядок по правилу niri: **в конце всегда ровно один
    /// пустой стол, а опустевшие в середине исчезают**.
    ///
    /// Раньше столов было ровно девять — по числу цифр на клавиатуре. Это не «упрощение», а
    /// другая модель: в ней пустые столы всегда занимают места в обзоре, а десятый стол не
    /// существует, даже когда он нужен. Динамические столы откладывались до обзора («иначе про
    /// существование стола №7 узнать неоткуда»), и с Вехой 123 эта причина отпала.
    ///
    /// Активный стол не убираем, даже пустой: человек на нём стоит. Он исчезнет сам, когда с
    /// него уйдут, — как и в niri.
    fn tidy_spaces(&mut self) {
        let mut i = 0;
        while i < self.spaces.len() {
            let last = i + 1 == self.spaces.len();
            if !last && i != self.space && self.space_is_empty(i) {
                self.spaces.remove(i);
                if self.space > i {
                    self.space -= 1;
                }
                continue; // на месте удалённого теперь следующий — его и проверяем
            }
            i += 1;
        }
        if self.spaces.is_empty() {
            self.spaces.push(Space::default());
            self.space = 0;
        } else if !self.space_is_empty(self.spaces.len() - 1) {
            self.spaces.push(Space::default());
        }
    }

    /// Сделать стол `n` активным: ленты меняются местами (см. [[workspaces]]).
    ///
    /// Веха 137: после перехода число столов пересматривается — стол, с которого ушли пустым,
    /// исчезает, а за последним занятым появляется новый пустой.
    fn switch_space(&mut self, n: usize) {
        if n == self.space || n >= self.space_count() {
            return;
        }
        self.spaces[self.space] = Space {
            cols: core::mem::take(&mut self.cols),
            cur: self.cur,
            scroll_x: self.scroll_x,
        };
        let s = core::mem::take(&mut self.spaces[n]);
        self.cols = s.cols;
        self.cur = s.cur;
        self.scroll_x = s.scroll_x;
        self.space = n;
        self.tidy_spaces();
    }

    /// Навести камеру на новое место — не прыжком, а движением (Веха 125).
    fn aim_camera(&mut self, to: (i32, i32)) {
        if to == self.ov_cam && self.cam_dur == 0 {
            return;
        }
        self.cam_from = self.ov_cam;
        self.cam_to = to;
        self.cam_at = sys::monotonic_ns();
        self.cam_dur = OV_MS * 1_000_000;
    }

    /// Подвинуть камеру обзора минимально — так, чтобы окно `id` поместилось на экране целиком.
    ///
    /// Ничего не делает, если оно и так видно. Окно больше экрана прижимается левым/верхним
    /// краем: показать его целиком нельзя, а метаться между краями — хуже, чем не двигаться.
    fn ensure_visible(&mut self, id: u32) {
        let (sw, sh) = (self.info.width as i32, self.info.height as i32);
        let Some(it) = self.ov.iter().find(|it| it.id == id) else { return };
        let (mut dx, mut dy) = (0, 0);
        if it.x < GAP {
            dx = it.x - GAP;
        } else if it.x + it.w > sw - GAP {
            dx = (it.x + it.w - (sw - GAP)).min(it.x - GAP);
        }
        if it.y < GAP {
            dy = it.y - GAP;
        } else if it.y + it.h > sh - GAP {
            dy = (it.y + it.h - (sh - GAP)).min(it.y - GAP);
        }
        if dx == 0 && dy == 0 {
            return;
        }
        let to = (self.ov_cam.0 + dx, self.ov_cam.1 + dy);
        self.aim_camera(to);
    }

    /// Колонка и место окна в ней.
    fn locate(&self, id: u32) -> Option<(usize, usize)> {
        self.cols
            .iter()
            .enumerate()
            .find_map(|(ci, c)| c.ids.iter().position(|&x| x == id).map(|wi| (ci, wi)))
    }

    /// Окно в фокусе по нынешней колонке.
    fn focused_id(&self) -> Option<u32> {
        let c = self.cols.get(self.cur)?;
        c.ids.get(c.focus.min(c.ids.len().saturating_sub(1))).copied()
    }

    /// Синхронизировать `focus` с раскладкой и перерисовать обводку обоих окон.
    fn sync_focus(&mut self) {
        let was = self.focus;
        self.focus = self.focused_id();
        if was == self.focus {
            return;
        }
        for id in [was, self.focus].into_iter().flatten() {
            if let Some(k) = self.win_at(id) {
                self.damage_chrome(k);
            }
        }
    }

    /// Убрать окно из ленты ЛЮБОГО стола; пустая колонка исчезает.
    ///
    /// Ищем везде, а не только на активном: программа с соседнего стола вправе завершиться, и
    /// её окно обязано исчезнуть оттуда, а не остаться призраком до перехода на тот стол.
    fn unlink(&mut self, id: u32) {
        for sp in self.spaces.iter_mut() {
            for ci in (0..sp.cols.len()).rev() {
                if let Some(wi) = sp.cols[ci].ids.iter().position(|&x| x == id) {
                    sp.cols[ci].ids.remove(wi);
                    if sp.cols[ci].ids.is_empty() {
                        sp.cols.remove(ci);
                        if sp.cur >= sp.cols.len() {
                            sp.cur = sp.cols.len().saturating_sub(1);
                        }
                    } else if sp.cols[ci].focus >= sp.cols[ci].ids.len() {
                        sp.cols[ci].focus = sp.cols[ci].ids.len() - 1;
                    }
                }
            }
        }
        if let Some((ci, wi)) = self.locate(id) {
            self.cols[ci].ids.remove(wi);
            if self.cols[ci].ids.is_empty() {
                self.cols.remove(ci);
                if self.cur >= self.cols.len() {
                    self.cur = self.cols.len().saturating_sub(1);
                }
            } else if self.cols[ci].focus >= self.cols[ci].ids.len() {
                self.cols[ci].focus = self.cols[ci].ids.len() - 1;
            }
        }
    }

    /// Перерисовать только ОБВОДКУ окна: рамку и титульную полосу.
    ///
    /// Смена фокуса меняет ровно их. Перерисовывать ради цвета рамки всё окно значит переписать
    /// сотни тысяч пикселей вместо нескольких тысяч — а фокус переезжает на каждый клик.
    fn damage_chrome(&mut self, i: usize) {
        let (fx, fy, fw, fh) = self.wins[i].frame();
        self.damage(fx, fy, fw, BORDER);
        self.damage(fx, fy + fh - BORDER, fw, BORDER);
        self.damage(fx, fy, BORDER, fh);
        self.damage(fx + fw - BORDER, fy, BORDER, fh);
    }

    // ── курсор (тот же приём, что в `term`: он не часть кадра) ────────────────────────

    fn draw_cursor(&self) {
        let px_fill = self.pack(if self.buttons != 0 { C_ACCENT } else { (255, 255, 255) });
        let px_edge = self.pack((0, 0, 0));
        for (row, line) in CURSOR.iter().enumerate() {
            for (col, ch) in line.bytes().enumerate() {
                if ch == b' ' {
                    continue;
                }
                let px = if ch == b'#' { px_edge } else { px_fill };
                self.put(self.cursor.0 + col as i32, self.cursor.1 + row as i32, px);
            }
        }
        fence();
    }

    // ── ввод ───────────────────────────────────────────────────────────────────────────

    fn on_mouse(&mut self, e: &sys::MouseEvent) {
        let old = self.cursor;
        self.cursor.0 = (self.cursor.0 + e.dx as i32).clamp(0, self.info.width as i32 - 1);
        self.cursor.1 = (self.cursor.1 + e.dy as i32).clamp(0, self.info.height as i32 - 1);
        let was = self.buttons;
        self.buttons = e.buttons;

        // В обзоре клик выбирает окно и ВЫХОДИТ к нему — ради этого обзор и открывают.
        // Попадание считается по тому же списку, по которому обзор нарисован: отдельный расчёт
        // «где что» означал бы, что клик приходит не в то окно, которое человек видит.
        // Пока ВЫХОД из обзора доигрывает, ввод обзору уже не принадлежит: на экране идёт
        // наезд камеры, а список попаданий посчитан для неподвижной картинки — клик пришёл бы
        // не в то окно, которое человек видит. `ov_to != 0` и значит «мы всё ещё в обзоре».
        if was == 0 && e.buttons != 0 && self.overview && self.ov_to != 0 {
            if let Some(it) = self.ov.iter().find(|it| {
                self.cursor.0 >= it.x
                    && self.cursor.0 < it.x + it.w
                    && self.cursor.1 >= it.y
                    && self.cursor.1 < it.y + it.h
            }) {
                let (id, sp) = (it.id, it.space);
                if sp != self.space {
                    self.action_inner(&alloc::format!("workspace-{}", sp + 1), 0, 0);
                }
                if let Some((ci, wi)) = self.locate(id) {
                    self.cur = ci;
                    self.cols[ci].focus = wi;
                }
                self.sync_focus();
                self.leave_overview();
            }
            return;
        }

        // Нажатие: выбрать окно под курсором. Перетаскивания в тайлинге нет — место окна
        // задаёт раскладка, а не рука; плавающий режим остаётся исключением на будущее
        // ([[wm-keys]]), и тащить окно понадобится ровно там.
        if was == 0 && e.buttons != 0 {
            if let Some(i) = self.wins.iter().position(|w| w.visible && w.tiled() && w.hit_frame(self.cursor.0, self.cursor.1, self.scroll_x))
            {
                let id = self.wins[i].id;
                if let Some((ci, wi)) = self.locate(id) {
                    self.cur = ci;
                    self.cols[ci].focus = wi;
                    self.sync_focus();
                    self.relayout();
                }
            }
        }

        // Фокус за указателем (Веха 121.1, просьба владельца): навёл — работаешь здесь.
        // Только на ДВИЖЕНИИ: иначе всплывшее под неподвижным курсором окно перехватывало бы
        // фокус у того, с кем человек работает.
        // В обзоре наведение тоже переносит фокус — и камера едет следом, ставя выбранное в
        // центр (просьба владельца, как в niri). Считаем по тому же списку, по которому обзор
        // нарисован, и перестраиваем его только при СМЕНЕ окна: иначе камера дёргалась бы на
        // каждое движение мыши.
        if (e.dx != 0 || e.dy != 0) && self.buttons == 0 && self.overview && self.ov_to != 0 {
            let hit = self.ov.iter().find(|it| {
                self.cursor.0 >= it.x
                    && self.cursor.0 < it.x + it.w
                    && self.cursor.1 >= it.y
                    && self.cursor.1 < it.y + it.h
            });
            if let Some((id, sp)) = hit.map(|it| (it.id, it.space)) {
                if self.focus != Some(id) {
                    if sp != self.space {
                        self.switch_space(sp);
                    }
                    if let Some((ci, wi)) = self.locate(id) {
                        self.cur = ci;
                        self.cols[ci].focus = wi;
                    }
                    self.sync_focus();
                    self.build_overview(false);
                    // …и подвинуть камеру РОВНО настолько, чтобы выбранное влезло целиком
                    // (идея владельца, Веха 124). Это безопасно там, где «поставить в центр»
                    // ломалось: минимальная подвижка имеет неподвижную точку — как только окно
                    // влезло, следующее наведение на него не двигает ничего. Центрирование
                    // такой точки не имеет, поэтому и качалось.
                    self.ensure_visible(id);
                }
            }
        }

        if (e.dx != 0 || e.dy != 0) && self.buttons == 0 && !self.overview {
            if let Some(i) = self.wins.iter().position(|w| w.visible && w.tiled() && w.hit_frame(self.cursor.0, self.cursor.1, self.scroll_x))
            {
                let id = self.wins[i].id;
                if self.focus != Some(id) {
                    if let Some((ci, wi)) = self.locate(id) {
                        self.cur = ci;
                        self.cols[ci].focus = wi;
                        self.sync_focus();
                    }
                }
            }
        }

        // Курсор: пометить И СТАРОЕ место, И НОВОЕ (Веха 121.2).
        //
        // Здесь была настоящая ошибка, и внесла её Веха 120.3. До неё `repaint` дорисовывал
        // курсор в конце БЕЗУСЛОВНО, поэтому хватало пометить старое место. Когда курсор
        // переехал в построчную сборку кадра, он стал рисоваться только внутри перерисованных
        // областей — а новое место в них не попадало. При медленном движении области
        // пересекались и всё выглядело целым; при быстром курсор терял куски, а на большом
        // скачке пропадал совсем. Ровно это владелец и увидел.
        self.damage(old.0, old.1, CUR_W, CUR_H);
        self.damage(self.cursor.0, self.cursor.1, CUR_W, CUR_H);

        // Колесо: в обзоре — просто крутить столы, вне обзора — с Super (просьба владельца).
        // Без модификатора вне обзора колесо принадлежит программе: прокрутка страницы важнее.
        if e.wheel != 0 && (self.overview || self.super_held) {
            let step = if e.wheel > 0 { -1i32 } else { 1i32 };
            let mut to = self.space as i32 + step;
            to = to.clamp(0, self.space_count() as i32 - 1);
            if to as usize != self.space {
                self.switch_space(to as usize);
                self.sync_focus();
                if self.overview {
                    self.build_overview(true);
                } else {
                    self.relayout();
                }
            }
            return;
        }

        // Событие окну под курсором. Поверхности слоя ввод НЕ достаётся (Веха 139): обои кликать
        // не по чему, а бару клики понадобятся своей вехой — и вместе с ними придётся решить,
        // как курсор над панелью не уводит фокус у окна, под которым он оказался. Отдавать
        // события «пока просто так» значило бы принять это решение вслепую.
        if let Some(i) = self.wins.iter().rposition(|w| w.visible && w.tiled() && w.hit_frame(self.cursor.0, self.cursor.1, self.scroll_x)) {
            let (ox, oy) = self.wins[i].content_at();
            let (lx, ly) = ((self.cursor.0 - ox) as u16, (self.cursor.1 - oy) as u16);
            if was != e.buttons {
                let ev = [win::EV_BUTTON, lx as u8, (lx >> 8) as u8, ly as u8, (ly >> 8) as u8,
                          e.buttons, (e.buttons != 0) as u8, 0];
                self.send(i, ev, 7);
            } else {
                let ev = [win::EV_MOTION, lx as u8, (lx >> 8) as u8, ly as u8, (ly >> 8) as u8,
                          0, 0, 0];
                self.send(i, ev, 5);
            }
        }
    }

    /// Клавиша: сперва ищем АККОРД в раскладке, и только если его нет — отдаём символ окну.
    ///
    /// Порядок принципиален: `Super+Q` не должен доехать до программы буквой `q`. Отпускания
    /// клавиш окну не отдаём вовсе — программам нужен текст, а не состояние клавиатуры.
    fn key_event(&mut self, e: &sys::KeyEvent, binds: &[Bind], store: usize, me: usize) {
        // Super держат или отпустили — это нужно колесу, и знать об этом надо ДО отсева
        // отпусканий: иначе Super «залипнет» нажатым навсегда.
        if e.sym == SYM_SUPER {
            self.super_held = e.down;
        }
        if !e.down {
            return;
        }
        if let Some(b) = binds.iter().find(|b| b.sym == e.sym && b.mods == e.mods) {
            // `Escape` и `Return` привязаны БЕЗ модификатора — они закрывают обзор. Вне обзора
            // забирать их у программы нельзя: редактору Escape нужен ему, а не нам. Поэтому
            // действие, осмысленное только в обзоре, вне его не срабатывает и клавиша уходит
            // дальше как обычная.
            if !(b.action == "close-overview" && !self.overview) {
                self.action(&b.action.clone(), store, me);
                return;
            }
        }
        // Аккорд с Super, которому не нашлось действия, программе не отдаём: иначе промах по
        // раскладке печатал бы букву посреди текста.
        if e.mods & 8 != 0 {
            return;
        }
        // Веха 127: клавиша уходит в окно ЦЕЛИКОМ — код, модификаторы, символ. Прежде здесь
        // стояло `|| e.ascii == 0`, то есть всё, что не печатает символ, молча выбрасывалось:
        // стрелки, Home/End, PageUp. Именно поэтому в окне не работали ни они, ни собственные
        // клавиши терминала.
        let Some(id) = self.focus else { return };
        let Some(i) = self.wins.iter().position(|w| w.id == id) else { return };
        let ch = e.ch;
        let ev = [
            win::EV_KEY,
            e.sym as u8,
            (e.sym >> 8) as u8,
            e.mods,
            ch as u8,
            (ch >> 8) as u8,
            e.down as u8,
            0,
        ];
        self.send(i, ev, 7);
    }

    /// Выполнить действие раскладки.
    ///
    /// В обзоре действия те же: он показывает ту же ленту, только уменьшенной. Отдельная схема
    /// «клавиши обзора» означала бы вторую модель управления ради одного экрана.
    fn action(&mut self, name: &str, store: usize, me: usize) {
        let was_overview = self.overview;
        self.action_inner(name, store, me);
        // Обзор перестраиваем ПОСЛЕ действия: фокус мог переехать на другой стол, а картинка
        // обязана показывать то, что есть сейчас.
        if self.overview && was_overview && self.ov_to != 0 && name != "toggle-overview" {
            self.build_overview(true);
        }
    }

    fn action_inner(&mut self, name: &str, store: usize, me: usize) {
        match name {
            "spawn-term" => {
                if sys::spawn_with_endpoint(store, b"term", &[], me, b"WM\0").is_none() {
                    sys::write_console("[wm] терминал не запустился\n".as_bytes());
                }
            }
            "close-window" => {
                // Закрываем ОКНО, а не процесс: клиенту говорят «закройся», и он решает сам.
                // Убить его силой мы могли бы (он наш ребёнок), но тогда несохранённое пропадёт
                // молча — а это ровно то, чего порядочная система не делает.
                if let Some(id) = self.focus {
                    if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                        self.send(i, [win::EV_CLOSE, 0, 0, 0, 0, 0, 0, 0], 1);
                    }
                }
            }
            // ── навигация по ленте (Веха 121) ──
            "focus-column-left" | "focus-column-right" => {
                if self.cols.is_empty() {
                    return;
                }
                let last = self.cols.len() - 1;
                self.cur = if name.ends_with("right") {
                    (self.cur + 1).min(last)
                } else {
                    self.cur.saturating_sub(1)
                };
                self.sync_focus();
                self.relayout();
            }
            "focus-window-up" | "focus-window-down" => {
                let Some(c) = self.cols.get_mut(self.cur) else { return };
                if c.ids.is_empty() {
                    return;
                }
                let last = c.ids.len() - 1;
                c.focus = if name.ends_with("down") {
                    (c.focus + 1).min(last)
                } else {
                    c.focus.saturating_sub(1)
                };
                self.sync_focus();
            }
            // Переносить колонку целиком — это МЕНЯТЬ ЕЁ МЕСТО В ЛЕНТЕ, а не двигать пиксели.
            "move-column-left" | "move-column-right" => {
                if self.cols.len() < 2 {
                    return;
                }
                let to = if name.ends_with("right") {
                    (self.cur + 1).min(self.cols.len() - 1)
                } else {
                    self.cur.saturating_sub(1)
                };
                if to != self.cur {
                    self.cols.swap(self.cur, to);
                    self.cur = to;
                }
                self.relayout();
            }
            "move-window-up" | "move-window-down" => {
                let Some(c) = self.cols.get_mut(self.cur) else { return };
                if c.ids.len() < 2 {
                    return;
                }
                let to = if name.ends_with("down") {
                    (c.focus + 1).min(c.ids.len() - 1)
                } else {
                    c.focus.saturating_sub(1)
                };
                if to != c.focus {
                    c.ids.swap(c.focus, to);
                    c.focus = to;
                }
                self.relayout();
            }
            // Окно из своей колонки — в соседнюю (или в новую с краю ленты).
            "move-to-column-left" | "move-to-column-right" => {
                let Some(id) = self.focused_id() else { return };
                let right = name.ends_with("right");
                let ci = self.cur;
                self.unlink(id);
                let to = if right { ci + 1 } else { ci.saturating_sub(1) };
                if to >= self.cols.len() || (right && to == ci) {
                    self.cols.insert(to.min(self.cols.len()), Column { ids: vec![id], width: 1, focus: 0 });
                    self.cur = to.min(self.cols.len() - 1);
                } else {
                    self.cols[to].ids.push(id);
                    self.cols[to].focus = self.cols[to].ids.len() - 1;
                    self.cur = to;
                }
                self.sync_focus();
                self.relayout();
            }
            "width-next" => {
                if let Some(c) = self.cols.get_mut(self.cur) {
                    c.width = (c.width + 1) % WIDTHS.len();
                }
                self.relayout();
            }
            "width-plus" | "width-minus" => {
                if let Some(c) = self.cols.get_mut(self.cur) {
                    c.width = if name.ends_with("plus") {
                        (c.width + 1).min(WIDTHS.len() - 1)
                    } else {
                        c.width.saturating_sub(1)
                    };
                }
                self.relayout();
            }
            "maximize-column" => {
                if let Some(c) = self.cols.get_mut(self.cur) {
                    // Развернуть — это переключатель: второе нажатие возвращает половину экрана.
                    c.width = if c.width == WIDTHS.len() - 1 { 1 } else { WIDTHS.len() - 1 };
                }
                self.relayout();
            }
            // Синонимы движения по ленте. По КРУГУ не ходим (Веха 121.1, просьба владельца):
            // дошёл до края — там и остался. Заворот полезен там, где окон не видно, а у нас
            // лента перед глазами, и прыжок с конца в начало читается как промах, а не как
            // помощь.
            "focus-next" => self.action("focus-column-right", store, me),
            "focus-prev" => self.action("focus-column-left", store, me),
            // ── обзор (Веха 123) ──
            "toggle-overview" => {
                if !self.overview {
                    self.overview = true;
                    self.build_overview(true);
                    self.ov_start(1024);
                } else if self.ov_to != 0 {
                    // Выходя, показываем стол ТОГО окна, что выбрано: обзор для того и нужен —
                    // ткнуть в окно и оказаться при нём, а не вернуться откуда пришёл.
                    self.leave_overview();
                }
            }
            // ── рабочие столы (Веха 122) ──
            //
            // Переключение — это ОБМЕН ЛЕНТАМИ: активная уходит в хранилище, оттуда приходит
            // другая. Вся раскладка продолжает работать с одной лентой и про столы не знает.
            _ if name.starts_with("workspace-") => {
                let Some(n) = digit(name) else { return };
                // Веха 137 — столов может быть меньше девяти, и цифра сверх их числа не ошибка,
                // а просьба «в самый дальний». Последний стол всегда пустой, так что `Super+9`
                // на системе с двумя столами означает «на чистый», а не «никуда».
                let n = n.min(self.space_count().saturating_sub(1));
                if n == self.space {
                    return;
                }
                self.switch_space(n);
                self.sync_focus();
                self.relayout();
            }
            _ if name.starts_with("move-to-workspace-") => {
                let Some(n) = digit(name) else { return };
                let Some(id) = self.focused_id() else { return };
                let n = n.min(self.space_count().saturating_sub(1));
                if n == self.space {
                    return;
                }
                self.unlink(id);
                // На чужом столе окно всегда становится НОВОЙ колонкой: класть его в чью-то
                // чужую колонку значило бы менять раскладку стола, которого человек не видит.
                self.spaces[n].cols.push(Column { ids: vec![id], width: 1, focus: 0 });
                self.spaces[n].cur = self.spaces[n].cols.len() - 1;
                // Окно уехало на последний (пустой) стол — значит пустого в конце больше нет и
                // его надо завести; а стол, с которого окно ушло последним, исчезнет.
                self.tidy_spaces();
                self.sync_focus();
                self.relayout();
            }
            // Escape/Enter закрывают обзор — и НИЧЕГО не делают вне его: перехватывать эти
            // клавиши у программ было бы воровством (в редакторе Escape нужен ему, не нам).
            "close-overview" => {
                if self.overview && self.ov_to != 0 {
                    self.leave_overview();
                }
            }
            "quit" => {
                sys::write_console("[wm] выход по запросу\n".as_bytes());
                sys::exit(0);
            }
            other => {
                sys::write_console(alloc::format!("[wm] нет такого действия: {}\n", other).as_bytes());
            }
        }
    }

    /// Убрать окна процессов, которых больше нет.
    fn reap(&mut self) {
        for i in 0..self.wins.len() {
            // Закрывающееся окно уже осиротело — спрашивать про его хозяина незачем.
            if self.wins[i].closing {
                continue;
            }
            if matches!(sys::wait(self.wins[i].owner, true), sys::Wait::Exited(_)) {
                let id = self.wins[i].id;
                self.begin_close(id);
                self.sync_focus();
                self.relayout();
                return; // список изменился — доберём на следующем обороте
            }
        }
    }

    /// Отдать событие клиенту: сразу, если он ждёт, иначе в очередь.
    fn send(&mut self, i: usize, ev: [u8; 8], len: usize) {
        match self.wins[i].waiting.take() {
            Some(cap) => {
                sys::reply(cap, &ev[..len]);
            }
            None => {
                // Потолок разный по СМЫСЛУ события: движения мыши устаревают мгновенно (копить
                // их сотнями значит показывать клиенту прошлое), а клавиши терять нельзя —
                // человек их уже нажал. Строка, вставленная в консоль целиком, приезжает
                // десятками байт разом, и потолок в 16 съедал её середину.
                let cap = if ev[0] == win::EV_KEY { 256 } else { 16 };
                if self.wins[i].inbox.len() < cap {
                    let mut rec = [0u8; 8];
                    rec[..len].copy_from_slice(&ev[..len]);
                    rec[7] = len as u8;
                    self.wins[i].inbox.push(rec);
                }
            }
        }
    }

    // ── буферы кадров клиентов (Веха 129) ──────────────────────────────────────────────

    /// Отобразить у себя область, право на которую прислал клиент. `None` — права нет, область
    /// не та или свободных слотов не осталось.
    ///
    /// Ошибка здесь — НЕ повод падать: право приходит от клиента, а клиент может прислать что
    /// угодно. Окно просто останется без картинки (рамка и место в раскладке у него будут).
    fn map_client_buf(&mut self, cap: usize, w: i32, h: i32) -> Option<(usize, usize, usize)> {
        if cap == sys::NO_CAP {
            sys::write_console("[wm] окно без буфера: клиент не прислал права на область\n".as_bytes());
            return None;
        }
        let Some(slot) = (0..SHM_SLOTS).find(|i| self.slots & (1 << i) == 0) else {
            sys::write_console("[wm] окно без буфера: кончились слоты окна ВА\n".as_bytes());
            return None;
        };
        let va = SHM_BASE + slot * SHM_STRIDE;
        let Some(len) = sys::shm_map(cap, va) else {
            sys::write_console("[wm] окно без буфера: область не отобразилась\n".as_bytes());
            return None;
        };
        // Область обязана вмещать кадр объявленного размера: рисуем мы по ЕЁ содержимому, и
        // короткая область значила бы чтение за краем на каждой строке.
        if len < (w * h * 4) as usize {
            sys::write_console("[wm] окно без буфера: область короче объявленного кадра\n".as_bytes());
            sys::shm_unmap(cap, va);
            return None;
        }
        self.slots |= 1 << slot;
        Some((va, len, slot))
    }

    /// Отпустить буфер окна: снять отображение и вернуть слот. Ушли оба держателя — страницы
    /// вернулись системе.
    fn drop_buf(&mut self, i: usize) {
        let w = &mut self.wins[i];
        if w.buf == 0 {
            return;
        }
        sys::shm_unmap(w.cap, w.buf);
        let slot = w.slot;
        w.buf = 0;
        w.buf_len = 0;
        w.bw = 0;
        w.bh = 0;
        self.slots &= !(1 << slot);
    }

    // ── запросы клиентов ───────────────────────────────────────────────────────────────

    fn request(&mut self, m: &sys::Message, buf: &[u8]) {
        let op = m.op & 0xff;
        let len = m.len.min(buf.len());
        match op {
            win::OP_CREATE => {
                let w = u16::from_le_bytes([buf[0], buf[1]]).clamp(32, 1600) as i32;
                let h = u16::from_le_bytes([buf[2], buf[3]]).clamp(32, 1200) as i32;
                let title = core::str::from_utf8(&buf[4..len]).unwrap_or("окно");
                let id = self.next_id;
                self.next_id += 1;
                // Каскадом: каждое следующее окно правее и ниже — иначе они лягут друг на друга
                // и человек решит, что открылось одно.
                let n = self.wins.len() as i32;
                // Буфер кадра приезжает ПРАВОМ вместе с запросом: окно рождается уже с
                // картинкой. Не дали — окно всё равно будет, просто пустое.
                let (buf, buf_len, slot) = self.map_client_buf(m.cap, w, h).unwrap_or((0, 0, 0));
                let win = Win {
                    id,
                    owner: m.sender,
                    x: 60 + n * 40,
                    y: 60 + n * 40,
                    w,
                    h,
                    title: String::from(title),
                    buf,
                    buf_len,
                    cap: m.cap,
                    slot,
                    bw: if buf != 0 { w } else { 0 },
                    bh: if buf != 0 { h } else { 0 },
                    waiting: None,
                    inbox: Vec::new(),
                    shown: Shown { x: 0, y: 0, w: 0, h: 0, a: 0 },
                    from: Shown { x: 0, y: 0, w: 0, h: 0, a: 0 },
                    to: Shown { x: 0, y: 0, w: 0, h: 0, a: 0 },
                    at: 0,
                    dur: 0,
                    fresh: true,
                    closing: false,
                    visible: true,
                    layer: None,
                };
                self.wins.push(win);
                // Новое окно — НОВАЯ КОЛОНКА справа от текущей: так работает niri, и так же
                // ведёт себя лента при `Super+Return`. Класть его в текущую колонку значило бы
                // делить экран по вертикали без просьбы.
                let at = if self.cols.is_empty() { 0 } else { self.cur + 1 };
                self.cols.insert(at, Column { ids: vec![id], width: 1, focus: 0 });
                self.cur = at;
                // Веха 137 — окно село на последний (пустой) стол: пустого в конце больше нет,
                // и его надо завести, иначе «дальше» переходить будет некуда.
                self.tidy_spaces();
                sys::reply(m.reply_cap, &id.to_le_bytes());
                self.sync_focus();
                self.relayout();
            }
            // Веха 139 — ПОВЕРХНОСТЬ СЛОЯ: обои, бар, панель. Отличий от окна ровно два — место
            // назначает не раскладка, а якорь, и в ленту она не встаёт.
            win::OP_LAYER => {
                let spec = LayerCfg {
                    layer: buf[0].min(win::LAYER_OVERLAY),
                    anchor: buf[1],
                    // Занятая зона не может превышать экран: клиент присылает что угодно.
                    exclusive: (u16::from_le_bytes([buf[6], buf[7]]) as i32)
                        .min(self.info.height as i32)
                        .min(self.info.width as i32),
                };
                let w = u16::from_le_bytes([buf[2], buf[3]]).max(1) as i32;
                let h = u16::from_le_bytes([buf[4], buf[5]]).max(1) as i32;
                let title = core::str::from_utf8(buf.get(8..len).unwrap_or(&[])).unwrap_or("слой");
                let id = self.next_id;
                self.next_id += 1;
                let (buf, buf_len, slot) = self.map_client_buf(m.cap, w, h).unwrap_or((0, 0, 0));
                let (rx, ry, rw, rh) = self.layer_rect(&spec, w, h);
                let win = Win {
                    id,
                    owner: m.sender,
                    x: rx,
                    y: ry,
                    w: rw,
                    h: rh,
                    title: String::from(title),
                    buf,
                    buf_len,
                    cap: m.cap,
                    slot,
                    bw: if buf != 0 { w } else { 0 },
                    bh: if buf != 0 { h } else { 0 },
                    waiting: None,
                    inbox: Vec::new(),
                    // Слой не анимируется: он появляется там, где ему назначено. Выезжающие
                    // обои или подпрыгивающий бар — не «оживление», а рябь на каждом запуске.
                    shown: Shown { x: rx, y: ry, w: rw, h: rh, a: 256 },
                    from: Shown { x: rx, y: ry, w: rw, h: rh, a: 256 },
                    to: Shown { x: rx, y: ry, w: rw, h: rh, a: 256 },
                    at: 0,
                    dur: 0,
                    fresh: false,
                    closing: false,
                    visible: true,
                    layer: Some(spec),
                };
                self.wins.push(win);
                sys::reply(m.reply_cap, &id.to_le_bytes());
                // Клиент просил один размер, якорь назначил другой (растянуться на весь край) —
                // сказать ему об этом тем же событием, что и окнам: пусть заведёт буфер под то,
                // что ему на самом деле дали. До ответа рисуем растянутым — как и окно, которое
                // ещё не успело перерисоваться (Веха 126.1).
                if (rw, rh) != (w, h) {
                    let k = self.wins.len() - 1;
                    let ev = [win::EV_RESIZE, rw as u8, (rw >> 8) as u8, rh as u8, (rh >> 8) as u8,
                              0, 0, 0];
                    self.send(k, ev, 5);
                }
                self.damage(rx, ry, rw, rh);
                // Зона могла отнять место у ленты — пересчёт раскладки внутри.
                self.recompute_work();
            }
            // Веха 139 — размер экрана. Обоям и бару он нужен ДО того, как они заведут буфер, а
            // права на сам фреймбуфер у них нет и быть не должно.
            win::OP_SCREEN => {
                let mut rep = [0u8; 4];
                rep[0..2].copy_from_slice(&(self.info.width as u16).to_le_bytes());
                rep[2..4].copy_from_slice(&(self.info.height as u16).to_le_bytes());
                sys::reply(m.reply_cap, &rep);
            }
            // Веха 129 — НОВЫЙ буфер под изменившийся размер. Отпускаем старый и берём
            // присланный: между этими двумя строками окно остаётся без картинки ровно на время
            // одного вызова, и до конца оборота его всё равно никто не рисует.
            win::OP_REBUF => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let w = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                let h = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                if let Some(i) = self.wins.iter().position(|x| x.id == id) {
                    self.drop_buf(i);
                    if let Some((va, len, slot)) = self.map_client_buf(m.cap, w, h) {
                        let win = &mut self.wins[i];
                        win.buf = va;
                        win.buf_len = len;
                        win.cap = m.cap;
                        win.slot = slot;
                        win.bw = w;
                        win.bh = h;
                    }
                }
                // Ответ ПОСЛЕ отображения: клиент по возврату из вызова отпускает свою старую
                // область, и делать это, пока мы ещё в неё смотрим, нельзя.
                sys::reply(m.reply_cap, &[]);
            }
            win::OP_COMMIT => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                sys::reply(m.reply_cap, &[]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    let dx = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                    let dy = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                    let dw = u16::from_le_bytes([buf[8], buf[9]]) as i32;
                    let dh = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                    // Прямоугольник клиента — в КООРДИНАТАХ ЭКРАНА, а это не то же самое, что его
                    // место в раскладке. Считать надо от ПОКАЗАННОГО положения (`shown`) за
                    // вычетом сдвига ленты: лента длиннее экрана и по нему ездит.
                    //
                    // Здесь было `content_at()` — место окна в ЛЕНТЕ. Пока окон было два, лента
                    // никуда не уезжала (сдвиг 0) и разницы не было видно; третье окно её
                    // сдвигало, и повреждения всех последующих кадров уходили за край экрана.
                    // Выглядело это так, будто новое окно рождается ЧЁРНЫМ: рамку рисовала
                    // раскладка, а содержимое не перерисовывалось уже никогда.
                    let (fx, fy, fw, fh) = self.wins[i].shown.rect();
                    // Слой не ездит с лентой и не уменьшается обзором: его прямоугольник —
                    // сразу экранный, и прямоугольник клиента ложится в него как есть.
                    let tiled = self.wins[i].tiled();
                    let sc = if self.overview || !tiled { 0 } else { self.scroll_x };
                    let (b, _) = self.wins[i].deco();
                    let (want_w, want_h) = (self.wins[i].w + 2 * b, self.wins[i].h + 2 * b);
                    if (self.overview && tiled) || fw != want_w || fh != want_h {
                        // Окно в движении (растёт, едет, уменьшено обзором) — его пиксели легли на
                        // экран масштабированными, и попасть в них прямоугольником клиента нельзя.
                        // Помечаем окно целиком: кадр анимации всё равно перерисовывает его.
                        self.damage(fx - sc, fy, fw, fh);
                    } else {
                        self.damage(fx - sc + b + dx, fy + b + dy, dw.max(1), dh.max(1));
                    }
                }
            }
            // Неблокирующий опрос: у клиента свой реактор, спать в нашем вызове он не может.
            win::OP_POLL => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let ev = self.wins.iter_mut().find(|w| w.id == id).and_then(|w| {
                    (!w.inbox.is_empty()).then(|| w.inbox.remove(0))
                });
                match ev {
                    Some(rec) => {
                        let n = (rec[7] as usize).min(7);
                        sys::reply(m.reply_cap, &rec[..n]);
                    }
                    None => {
                        sys::reply(m.reply_cap, &[]);
                    }
                }
            }
            win::OP_EVENT => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let Some(i) = self.wins.iter().position(|w| w.id == id) else {
                    sys::reply(m.reply_cap, &[]);
                    return;
                };
                // ОЧЕРЕДЬ, а не стек: `pop` брал с конца, и набранное приезжало задом наперёд
                // (а при переполнении — вперемешку). Стоимость `remove(0)` при потолке в
                // сотни записей несущественна, а порядок ввода — свойство, которое нельзя терять.
                let first = if self.wins[i].inbox.is_empty() {
                    None
                } else {
                    Some(self.wins[i].inbox.remove(0))
                };
                match first {
                    // Есть накопленное — отвечаем сразу.
                    Some(rec) => {
                        let n = rec[7] as usize;
                        sys::reply(m.reply_cap, &rec[..n.min(7)]);
                    }
                    // Пусто — ответ ОТКЛАДЫВАЕТСЯ: клиент спит в `SYS_CALL`, как в чтении stdin.
                    None => self.wins[i].waiting = Some(m.reply_cap),
                }
            }
            win::OP_DESTROY => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                sys::reply(m.reply_cap, &[]);
                if self.win_at(id).is_some() {
                    self.begin_close(id);
                    self.sync_focus();
                    self.relayout();
                }
            }
            // Чужой запрос — ответить пусто, а не молчать: молчание повесило бы вызвавшего.
            _ => {
                let _ = m.sender;
                sys::reply(m.reply_cap, &[]);
            }
        }
    }
}

/// Курсор — тот же, что в `term` (Веха 115): контур плюс тело.
const CUR_W: i32 = 12;
const CUR_H: i32 = 19;
const CURSOR: [&str; 19] = [
    "#           ",
    "##          ",
    "#.#         ",
    "#..#        ",
    "#...#       ",
    "#....#      ",
    "#.....#     ",
    "#......#    ",
    "#.......#   ",
    "#........#  ",
    "#.....##### ",
    "#..#..#     ",
    "#.# #..#    ",
    "##  #..#    ",
    "#    #..#   ",
    "     #..#   ",
    "      #.#   ",
    "      ###   ",
    "            ",
];

/// Барьер записи: пачки write-combining не должны залёживаться (Веха 116.1).
#[cfg(target_arch = "x86_64")]
fn fence() {
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}
#[cfg(not(target_arch = "x86_64"))]
fn fence() {}


#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    main_loop()
}

/// Право на экран: по имени, иначе перебором стартовых (как в `term`).
fn find_fb_cap() -> Option<usize> {
    sys::cap_named("FB")
        .filter(|&c| sys::video_info(c).is_some())
        .or_else(|| {
            (0..8)
                .map(sys::start_cap)
                .find(|&c| c != sys::NO_CAP && sys::video_info(c).is_some())
        })
}

/// Право на store (нужно и для чтения буферов, и для запуска клиентов).
fn store_cap() -> usize {
    sys::cap_named("STORE").unwrap_or_else(|| sys::start_cap(1))
}
