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

// Запасной шрифт 8×16 — заголовки окон. Настоящий (из пакета) придёт вместе с тулкитом.
#[path = "../bitfont.rs"]
mod bitfont;
use bitfont::BitmapFont;
use ereb_render::{RasterizedGlyph, Rasterizer, RenderStyle};

/// Кадр 1280×800 RGBA (4 МиБ) + копии содержимого окон.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Окно фреймбуфера в нашем адресном пространстве (как у `term`).
const FB_VA: usize = 0x5000_0000;

/// Веха, которой соответствует этот бинарь. Печатается на старте — и это не украшение:
/// «работает не то, что должно» дважды оказывалось «загрузилась прошлая сборка», и без строки
/// в журнале это выясняется разбором кода вместо одного взгляда в `klog`.
const BUILD: &str = "Веха 121.2";

// ── вид (ADR 0016: один палитра-источник, приложения цветов не знают) ────────
const C_DESKTOP: (u8, u8, u8) = (0x0d, 0x11, 0x17);
const C_FRAME: (u8, u8, u8) = (0x16, 0x1b, 0x22);
const C_FRAME_ACTIVE: (u8, u8, u8) = (0x24, 0x2c, 0x38);
const C_BORDER: (u8, u8, u8) = (0x30, 0x36, 0x3d);
const C_ACCENT: (u8, u8, u8) = (0x4c, 0x7d, 0xfd);
const C_TEXT: (u8, u8, u8) = (0xc9, 0xd1, 0xd9);

/// Высота титульной полосы и толщина рамки.
const TITLE_H: i32 = 22;
const BORDER: i32 = 1;

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
bind wm Super+Tab focus-next
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
    (1..=SPACES).contains(&d).then_some(d - 1)
}

/// Собрать раскладку: строки `bind wm …` из конфига поколения, иначе зашитая схема.
/// Второе значение — ОТКУДА она взялась.
///
/// Правило то же, что у терминала (Веха 100): хоть один `bind` в конфиге — и схема задаётся
/// ЦЕЛИКОМ оттуда. Иначе клавишу нельзя было бы отвязать. Но у правила есть цена, и она
/// всплыла на живом человеке (Веха 121.1): конфиг, посеянный ДО появления новых действий,
/// молча отменяет их все — раскладка есть, она просто старая. Поэтому источник теперь
/// называется вслух: «шесть сочетаний из конфига» при восемнадцати зашитых — это диагноз.
fn load_binds(scap: usize) -> (Vec<Bind>, bool) {
    let text = read_generation(scap).unwrap_or_default();
    let out = parse_binds(&text);
    if out.is_empty() {
        (parse_binds(DEFAULT_BINDS), false)
    } else {
        (out, true)
    }
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
    let mut font = BitmapFont::new(16);

    let mut wm = Wm {
        info,
        wins: Vec::new(),
        next_id: 1,
        cursor: (info.width as i32 / 2, info.height as i32 / 2),
        buttons: 0,
        cols: Vec::new(),
        cur: 0,
        scroll_x: 0,
        focus: None,
        transient: 0,
        damage: Vec::new(),
        scratch: Vec::new(),
        readbuf: Vec::new(),
        spaces: (0..SPACES).map(|_| Space::default()).collect(),
        space: 0,
    };

    // Рабочий стол целиком — единственная полная заливка за всю сессию.
    wm.fill_rect(0, 0, info.width as i32, info.height as i32, C_DESKTOP);
    wm.draw_cursor();

    // Клиенты — из argv. Право на себя отдаём под именем `WM`: терминал даёт детям `STDIO`,
    // мы даём окна, и путать эти два хоста нельзя.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf);
    let mut spawned = 0usize;
    for prog in abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).skip(1) {
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

    let (binds, from_config) = load_binds(store);
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
    let mut mouse = [sys::MouseEvent { dx: 0, dy: 0, buttons: 0 }; 32];
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
        let mut kev = [sys::KeyEvent { sym: 0, mods: 0, down: false, ascii: 0 }; 32];
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
        let got = if worked {
            sys::try_recv(&mut msg)
        } else {
            sys::recv_console(&mut msg, 200)
        };
        if let Some(m) = got {
            wm.request(&m, &msg, store);
        }

        // Кадр — ОДИН на оборот, в самом конце: к этому месту учтены все события пачки, все
        // ответы клиентов и все ушедшие окна. Пока рисовало каждое событие само, рука обгоняла
        // экран (Веха 120.2).
        wm.flush(&mut font);
    }
}

/// Одно окно.
struct Win {
    id: u32,
    owner: usize,
    /// Левый верхний угол РАМКИ на экране.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: String,
    /// Копия пикселей клиента (RGBA), прочитанная по content-id.
    pixels: Vec<u8>,
    /// Что уже прочитано — чтобы не читать то же самое дважды.
    cid: [u8; 32],
    /// Отложенный ответ на `OP_EVENT` (клиент спит в `SYS_CALL`).
    waiting: Option<usize>,
    /// События, накопленные до того, как клиент спросил.
    inbox: Vec<[u8; 8]>,
    /// Лежит ли окно на АКТИВНОМ рабочем столе (Веха 122). Окно с чужого стола живо, помнит
    /// свои пиксели и продолжает работать — его просто не видно и не потрогать.
    visible: bool,
}

impl Win {
    /// Прямоугольник рамки (вместе с титульной полосой).
    fn frame(&self) -> (i32, i32, i32, i32) {
        (self.x, self.y, self.w + 2 * BORDER, self.h + TITLE_H + 2 * BORDER)
    }
    /// Левый верхний угол СОДЕРЖИМОГО.
    fn content_at(&self) -> (i32, i32) {
        (self.x + BORDER, self.y + BORDER + TITLE_H)
    }
    fn hit_title(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w + 2 * BORDER
            && py >= self.y && py < self.y + TITLE_H + BORDER
    }
    fn hit_frame(&self, px: i32, py: i32) -> bool {
        let (fx, fy, fw, fh) = self.frame();
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

/// Сколько рабочих столов. Девять, потому что столько цифр на клавиатуре — у владельца в niri
/// `Super+1…9`. Динамические столы niri (создаются по мере надобности) отложены: сначала должен
/// появиться обзор, иначе про существование стола №7 узнать неоткуда.
const SPACES: usize = 9;

/// Пресеты ширины колонки — доли экрана. Те же, что у владельца в niri.
const WIDTHS: [(i32, i32); 4] = [(1, 3), (1, 2), (2, 3), (1, 1)];

/// Зазор между окнами и до края экрана.
const GAP: i32 = 8;

struct Wm {
    info: sys::VideoInfo,
    /// Порядок = z-order: последнее окно рисуется поверх и получает клики первым.
    wins: Vec<Win>,
    /// Лента колонок слева направо.
    cols: Vec<Column>,
    /// Колонка в фокусе.
    cur: usize,
    /// Сдвиг ленты относительно экрана: лента длиннее экрана, экран по ней ездит.
    scroll_x: i32,
    next_id: u32,
    cursor: (i32, i32),
    buttons: u8,
    focus: Option<u32>,
    /// Сколько байт временных буферов прочитано с прошлой уборки (см. `OP_ATTACH`).
    transient: usize,
    /// Что перерисовать в конце оборота (Веха 120.2). Раньше каждое событие рисовало САМО, и
    /// перетаскивание превращалось в тридцать перерисовок на один оборот цикла.
    damage: Vec<(i32, i32, i32, i32)>,
    /// Строка пикселей в RAM: собираем её здесь, а во фреймбуфер отдаём одной последовательностью.
    scratch: Vec<u32>,
    /// Буфер чтения объекта клиента: один на сессию, только растёт (см. `OP_ATTACH`).
    readbuf: Vec<u8>,
    /// Неактивные рабочие столы (активный — в полях выше).
    spaces: Vec<Space>,
    /// Номер активного стола.
    space: usize,
}

/// Потолок списка повреждений. Список нужен, чтобы движение курсора в углу не тянуло за собой
/// перерисовку окна в другом углу; но и длинный список вреден — накладные расходы на каждый
/// прямоугольник свои. При переполнении всё сливается в один охватывающий.
const DAMAGE_MAX: usize = 8;

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

    /// Нарисовать строку битмапным шрифтом. Возвращает ширину нарисованного.
    fn text(&self, font: &mut BitmapFont, x: i32, y: i32, s: &str, c: (u8, u8, u8)) -> i32 {
        let px = self.pack(c);
        let mut pen = x;
        for ch in s.chars() {
            let g: RasterizedGlyph = font.rasterize(ch, RenderStyle::Regular);
            for gy in 0..g.height as i32 {
                for gx in 0..g.width as i32 {
                    if g.bitmap[(gy * g.width as i32 + gx) as usize] > 127 {
                        self.put(pen + gx, y + gy, px);
                    }
                }
            }
            pen += font.metrics().width as i32;
        }
        fence();
        pen - x
    }

    // ── композиция ─────────────────────────────────────────────────────────────────────

    /// Отметить прямоугольник как требующий перерисовки (Веха 120.2).
    ///
    /// Раньше каждое событие рисовало само, немедленно. У мыши это означало до тридцати двух
    /// перерисовок на один оборот цикла — по числу событий в пачке, — и окно продолжало ехать
    /// уже после того, как человек отпустил кнопку: очередь событий отставала от руки.
    /// Теперь событие меняет только СОСТОЯНИЕ, а рисуется всё один раз в конце оборота.
    fn damage(&mut self, x: i32, y: i32, w: i32, h: i32) {
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

    /// Нарисовать всё накопленное. Ровно один раз за оборот цикла — это и есть «кадр».
    fn flush(&mut self, font: &mut BitmapFont) {
        if self.damage.is_empty() {
            return;
        }
        let rects = core::mem::take(&mut self.damage);
        for (x, y, w, h) in rects {
            self.repaint(font, x, y, w, h);
        }
    }

    /// Перерисовать прямоугольник экрана. Строка собирается ЦЕЛИКОМ в памяти — стол, окна в
    /// порядке z, курсор — и уходит во фреймбуфер одной последовательностью записей.
    ///
    /// Отсюда исчезло мерцание (Веха 120.3). Раньше область сначала заливалась цветом стола, а
    /// потом поверх рисовались окна: пиксель под окном писался ДВАЖДЫ, и глаз успевал поймать
    /// промежуточное состояние — при перетаскивании это выглядело как мигание окна. Теперь
    /// каждый пиксель экрана пишется ровно один раз за кадр.
    fn repaint(&mut self, font: &mut BitmapFont, x: i32, y: i32, w: i32, h: i32) {
        let Some((x0, y0, w, h)) = intersect(
            (x, y, w, h),
            (0, 0, self.info.width as i32, self.info.height as i32),
        ) else {
            return;
        };
        // Заголовки — глифами, поверх собранных строк: растеризовать шрифт внутри построчной
        // сборки значило бы делать это заново на каждую строку полосы.
        let mut titles: Vec<(i32, i32, bool, usize)> = Vec::new();
        for i in 0..self.wins.len() {
            let (fx, fy, fw, fh) = self.wins[i].frame();
            if self.wins[i].visible
                && intersect((fx, fy, fw, fh), (x0, y0, w, h)).is_some()
                && fy + 3 < y0 + h
                && fy + 3 + 16 > y0
            {
                titles.push((fx + 6, fy + 3, self.focus == Some(self.wins[i].id), i));
            }
        }

        // Строку берём ВО ВЛАДЕНИЕ на время сборки: иначе не собрать её, читая окна из `self`.
        let mut row = core::mem::take(&mut self.scratch);
        if row.len() < w as usize {
            row.resize(w as usize, 0);
        }
        for yy in y0..y0 + h {
            self.compose_row(&mut row[..w as usize], yy, x0);
            self.write_row(x0, yy, &row[..w as usize]);
        }
        self.scratch = row;
        fence();

        for (tx, ty, active, i) in titles {
            let c = if active { C_TEXT } else { C_BORDER };
            let title = core::mem::take(&mut self.wins[i].title);
            self.text(font, tx, ty, &title, c);
            self.wins[i].title = title;
        }
    }

    /// Собрать одну строку экрана: стол → окна в порядке z → курсор.
    fn compose_row(&self, out: &mut [u32], yy: i32, x0: i32) {
        let desktop = self.pack(C_DESKTOP);
        out.fill(desktop);
        let x1 = x0 + out.len() as i32;

        for win in &self.wins {
            if !win.visible {
                continue;
            }
            let (fx, fy, fw, fh) = win.frame();
            if yy < fy || yy >= fy + fh {
                continue;
            }
            let (Some(sx), Some(ex)) = (Some(fx.max(x0)), Some((fx + fw).min(x1))) else {
                continue;
            };
            if ex <= sx {
                continue;
            }
            let active = self.focus == Some(win.id);
            let border = self.pack(if active { C_ACCENT } else { C_BORDER });
            let title_bg = self.pack(if active { C_FRAME_ACTIVE } else { C_FRAME });
            let (ox, oy) = win.content_at();
            let content_row = yy - oy;
            let has_content =
                !win.pixels.is_empty() && content_row >= 0 && content_row < win.h;
            let edge_row = yy == fy || yy == fy + fh - 1;
            let title = yy < fy + BORDER + TITLE_H;
            for xx in sx..ex {
                let px = if edge_row || xx == fx || xx == fx + fw - 1 {
                    border
                } else if title || !has_content {
                    title_bg
                } else {
                    let col = xx - ox;
                    let p = ((content_row * win.w + col) * 4) as usize;
                    if col >= 0 && col < win.w && p + 2 < win.pixels.len() {
                        self.pack((win.pixels[p], win.pixels[p + 1], win.pixels[p + 2]))
                    } else {
                        title_bg
                    }
                };
                out[(xx - x0) as usize] = px;
            }
        }

        // Курсор поверх всего: он не принадлежит ни одному окну.
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

    /// Ширина колонки в пикселях по её пресету.
    fn col_width(&self, c: &Column) -> i32 {
        let (n, d) = WIDTHS[c.width.min(WIDTHS.len() - 1)];
        (self.info.width as i32 - GAP) * n / d - GAP
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
            w.visible = false;
        }
        // Сначала — куда уехала лента. Колонка в фокусе обязана быть видна целиком; если она шире
        // экрана, показываем её левый край.
        let mut x = GAP;
        let mut starts: Vec<(i32, i32)> = Vec::new(); // (x, ширина)
        for c in &self.cols {
            let w = self.col_width(c);
            starts.push((x, w));
            x += w + GAP;
        }
        if let Some(&(cx, cw)) = starts.get(self.cur) {
            let view = self.info.width as i32;
            if cx - self.scroll_x < GAP {
                self.scroll_x = cx - GAP;
            }
            if cx + cw - self.scroll_x > view - GAP {
                self.scroll_x = cx + cw - view + GAP;
            }
        }
        if self.cols.is_empty() {
            self.scroll_x = 0;
        }

        let mut resized: Vec<(u32, i32, i32)> = Vec::new();
        for (ci, c) in self.cols.iter().enumerate() {
            let (cx, cw) = starts[ci];
            let n = c.ids.len().max(1) as i32;
            // Высота делится поровну; остаток отдаём последнему окну, чтобы низ был ровным.
            let cell = (screen_h - GAP) / n - GAP;
            for (wi, id) in c.ids.iter().enumerate() {
                let Some(k) = self.win_at(*id) else { continue };
                self.wins[k].visible = true;
                let y = GAP + wi as i32 * (cell + GAP);
                let h = if wi + 1 == c.ids.len() { screen_h - GAP - y } else { cell };
                // Клиенту назначается размер СОДЕРЖИМОГО: рамку и заголовок рисуем мы.
                let (cw2, ch2) = (cw - 2 * BORDER, h - TITLE_H - 2 * BORDER);
                let (cw2, ch2) = (cw2.max(32), ch2.max(32));
                self.wins[k].x = cx - self.scroll_x;
                self.wins[k].y = y;
                if self.wins[k].w != cw2 || self.wins[k].h != ch2 {
                    self.wins[k].w = cw2;
                    self.wins[k].h = ch2;
                    // Копия пикселей больше не описывает окно — заводим новую по размеру.
                    self.wins[k].pixels = vec![0u8; (cw2 * ch2 * 4) as usize];
                    self.wins[k].cid = [0u8; 32];
                    resized.push((*id, cw2, ch2));
                }
            }
        }
        for (id, w, h) in resized {
            if let Some(k) = self.win_at(id) {
                let ev = [win::EV_RESIZE, w as u8, (w >> 8) as u8, h as u8, (h >> 8) as u8, 0, 0, 0];
                self.send(k, ev, 5);
            }
        }
        self.damage(0, 0, self.info.width as i32, screen_h);
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
        let top = BORDER + TITLE_H;
        self.damage(fx, fy, fw, top);
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

        // Нажатие: выбрать окно под курсором. Перетаскивания в тайлинге нет — место окна
        // задаёт раскладка, а не рука; плавающий режим остаётся исключением на будущее
        // ([[wm-keys]]), и тащить окно понадобится ровно там.
        if was == 0 && e.buttons != 0 {
            if let Some(i) = self.wins.iter().position(|w| w.visible && w.hit_frame(self.cursor.0, self.cursor.1))
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
        if (e.dx != 0 || e.dy != 0) && self.buttons == 0 {
            if let Some(i) = self.wins.iter().position(|w| w.visible && w.hit_frame(self.cursor.0, self.cursor.1))
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

        // Событие окну под курсором.
        if let Some(i) = self.wins.iter().rposition(|w| w.visible && w.hit_frame(self.cursor.0, self.cursor.1)) {
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
        if !e.down {
            return;
        }
        if let Some(b) = binds.iter().find(|b| b.sym == e.sym && b.mods == e.mods) {
            self.action(&b.action.clone(), store, me);
            return;
        }
        // Аккорд с Super, которому не нашлось действия, программе не отдаём: иначе промах по
        // раскладке печатал бы букву посреди текста.
        if e.mods & 8 != 0 || e.ascii == 0 {
            return;
        }
        let Some(id) = self.focus else { return };
        let Some(i) = self.wins.iter().position(|w| w.id == id) else { return };
        self.send(i, [win::EV_KEY, e.ascii, 0, 0, 0, 0, 0, 0], 2);
    }

    /// Выполнить действие раскладки.
    fn action(&mut self, name: &str, store: usize, me: usize) {
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
            // ── рабочие столы (Веха 122) ──
            //
            // Переключение — это ОБМЕН ЛЕНТАМИ: активная уходит в хранилище, оттуда приходит
            // другая. Вся раскладка продолжает работать с одной лентой и про столы не знает.
            _ if name.starts_with("workspace-") => {
                let Some(n) = digit(name) else { return };
                if n == self.space {
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
                self.sync_focus();
                self.relayout();
            }
            _ if name.starts_with("move-to-workspace-") => {
                let Some(n) = digit(name) else { return };
                let Some(id) = self.focused_id() else { return };
                if n == self.space {
                    return;
                }
                self.unlink(id);
                // На чужом столе окно всегда становится НОВОЙ колонкой: класть его в чью-то
                // чужую колонку значило бы менять раскладку стола, которого человек не видит.
                self.spaces[n].cols.push(Column { ids: vec![id], width: 1, focus: 0 });
                self.spaces[n].cur = self.spaces[n].cols.len() - 1;
                self.sync_focus();
                self.relayout();
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
        let mut i = 0;
        while i < self.wins.len() {
            let dead = matches!(sys::wait(self.wins[i].owner, true), sys::Wait::Exited(_));
            if !dead {
                i += 1;
                continue;
            }
            let id = self.wins[i].id;
            self.wins.remove(i);
            self.unlink(id);
            self.sync_focus();
            self.relayout();
        }
    }

    /// Перерисовать окно, потерявшее фокус (его рамка обязана погаснуть).
    fn unfocus_repaint(&mut self, lost: Option<u32>, now: Option<u32>) {
        let Some(id) = lost else { return };
        if Some(id) == now {
            return;
        }
        if let Some(i) = self.wins.iter().position(|w| w.id == id) {
            self.damage_chrome(i); // погасла только рамка — содержимое окна не менялось
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

    // ── запросы клиентов ───────────────────────────────────────────────────────────────

    fn request(&mut self, m: &sys::Message, buf: &[u8], store: usize) {
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
                let win = Win {
                    id,
                    owner: m.sender,
                    x: 60 + n * 40,
                    y: 60 + n * 40,
                    w,
                    h,
                    title: String::from(title),
                    pixels: Vec::new(),
                    cid: [0u8; 32],
                    waiting: None,
                    inbox: Vec::new(),
                    visible: true,
                };
                self.wins.push(win);
                // Новое окно — НОВАЯ КОЛОНКА справа от текущей: так работает niri, и так же
                // ведёт себя лента при `Super+Return`. Класть его в текущую колонку значило бы
                // делить экран по вертикали без просьбы.
                let at = if self.cols.is_empty() { 0 } else { self.cur + 1 };
                self.cols.insert(at, Column { ids: vec![id], width: 1, focus: 0 });
                self.cur = at;
                sys::reply(m.reply_cap, &id.to_le_bytes());
                self.sync_focus();
                self.relayout();
            }
            win::OP_ATTACH => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let rx = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                let ry = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                let rw = u16::from_le_bytes([buf[8], buf[9]]) as i32;
                let rh = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                let mut cid = [0u8; 32];
                cid.copy_from_slice(&buf[12..44]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    // Тот же content-id — содержимое то же, читать нечего. Это и есть выгода
                    // адресации по содержимому: «перерисовал в то же самое» стоит ноль.
                    if self.wins[i].cid != cid {
                        self.wins[i].cid = cid;
                        let (ww, wh) = (self.wins[i].w, self.wins[i].h);
                        if self.wins[i].pixels.len() != (ww * wh * 4) as usize {
                            self.wins[i].pixels = vec![0u8; (ww * wh * 4) as usize];
                        }
                        let need = (rw * rh * 4) as usize;
                        // Буфер чтения ОДИН на всю сессию и только растёт (Веха 120.3). Раньше он
                        // заводился заново на каждый кадр — полтора мегабайта, взятые и
                        // отпущенные сотни раз подряд вперемешку с мелочью. Куча со слиянием
                        // соседей это переживает не всегда: мелкая аллокация, попавшая в середину
                        // только что освобождённого большого блока, делит его навсегда. Итог —
                        // «memory allocation of 1785604 bytes failed» посреди работы.
                        if self.readbuf.len() < need {
                            self.readbuf.resize(need, 0);
                        }
                        let px = &mut self.readbuf[..need];
                        let got = sys::obj_get(store, &cid, px);
                        if got == need {
                            // Вклеиваем полосу на её место в копии окна.
                            for row in 0..rh {
                                let dy = ry + row;
                                if dy < 0 || dy >= wh {
                                    continue;
                                }
                                let src = (row * rw * 4) as usize;
                                let dst = ((dy * ww + rx) * 4) as usize;
                                let n = (rw * 4).min((ww - rx) * 4).max(0) as usize;
                                if dst + n <= self.wins[i].pixels.len() && src + n <= px.len() {
                                    self.wins[i].pixels[dst..dst + n]
                                        .copy_from_slice(&px[src..src + n]);
                                }
                            }
                        }
                        // Пиксели скопированы — объект больше не нужен НИКОМУ. Он не привязан
                        // корнем, значит уже мусор; считаем его и время от времени просим ядро
                        // прибраться, иначе куча ядра кончится (Веха 118: полный кадр каждое
                        // нажатие клавиши убивал систему за восемь букв).
                        self.transient += need;
                        if self.transient > 4 * 1024 * 1024 {
                            self.transient = 0;
                            sys::obj_gc(store);
                        }
                    }
                }
                sys::reply(m.reply_cap, &[]);
            }
            win::OP_COMMIT => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                sys::reply(m.reply_cap, &[]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    let (ox, oy) = self.wins[i].content_at();
                    let dx = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                    let dy = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                    let dw = u16::from_le_bytes([buf[8], buf[9]]) as i32;
                    let dh = u16::from_le_bytes([buf[10], buf[11]]) as i32;
                    self.damage(ox + dx, oy + dy, dw.max(1), dh.max(1));
                }
            }
            // Прокрутка: пиксели уже у нас — сдвигаем свою копию, клиент присылает лишь
            // освободившуюся строку (Веха 120.3). До этого прокрутка на одну строку означала
            // пересылку ВСЕГО окна.
            win::OP_SCROLL => {
                let id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let y0 = u16::from_le_bytes([buf[4], buf[5]]) as i32;
                let y1 = u16::from_le_bytes([buf[6], buf[7]]) as i32;
                let dy = i16::from_le_bytes([buf[8], buf[9]]) as i32;
                sys::reply(m.reply_cap, &[]);
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    let (ww, wh) = (self.wins[i].w, self.wins[i].h);
                    let y0 = y0.clamp(0, wh);
                    let y1 = y1.clamp(y0, wh);
                    let stride = (ww * 4) as usize;
                    let pix = &mut self.wins[i].pixels;
                    if dy > 0 && y1 - y0 > dy && pix.len() >= (y1 * ww * 4) as usize {
                        let from = ((y0 + dy) * ww * 4) as usize;
                        let to = (y1 * ww * 4) as usize;
                        let dst = (y0 * ww * 4) as usize;
                        pix.copy_within(from..to, dst);
                        // Освободившийся хвост НЕ чистим: клиент сейчас пришлёт туда новые
                        // строки, а мигание пустой полосой видно.
                        let _ = stride;
                        let (ox, oy) = self.wins[i].content_at();
                        self.damage(ox, oy + y0, ww, y1 - y0);
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
                if let Some(i) = self.wins.iter().position(|w| w.id == id) {
                    self.wins.remove(i);
                    self.unlink(id);
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
