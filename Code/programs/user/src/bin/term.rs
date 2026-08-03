//! `term` — терминал VOID на крейтах ereb (Веха 97, ADR 0014).
//!
//! Первый шаг из потолка 256 глифов: знакогенератор VGA держал ровно столько символов, и любой
//! символ вне CP866 показать было НЕГДЕ. Здесь экран — пиксели, а глифы растеризуются из
//! настоящего TrueType, поэтому доступен весь Unicode шрифта, включая иконки Nerd Font.
//!
//! **Это ещё не мультиплексор.** Одна панель на весь экран, ни вкладок, ни панелей, ни чужих
//! процессов: у VOID пока одна общая консоль и блокирующий `SYS_EXEC`, а мультиплексору нужны
//! приватные потоки байт на каждого ребёнка. Это следующий шаг фазы; здесь модель процессов не
//! меняется вовсе.
//!
//! Что программа делает: получает экран под capability, рисует грид ereb и гоняет в него
//! клавиатурный ввод через тот же разбор ANSI, что и настоящий терминал.
//!
//! ## Устройство
//!
//! ```text
//!   клавиши → SYS_READ ─→ vte::Parser ─→ ereb_core::Grid ─→ GridRenderer ─→ Surface (RAM)
//!                                                                              │
//!                                            фреймбуфер под cap ◀── blit ──────┘
//! ```
//!
//! Рисуем **в буфер в RAM, а потом переносим кадр**: фреймбуфер — некэшируемая память
//! устройства, и растеризовать глифы прямо в неё значило бы платить за каждый пиксель
//! отдельной транзакцией шины. Ядро в своей консоли не читает фреймбуфер по той же причине.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use ereb_core::Grid;
use ereb_render::{GlyphCache, GridRenderer, Palette, Rgb, Surface, TtfFont};
use void_user as sys;

/// Куча процесса. Считаем по-крупному: кадр 1280×800 RGBA — 4 МиБ, копия шрифта в куче — 2.6 МиБ,
/// кэш глифов — сотни килобайт. Арена резервируется лениво (`SYS_MAP`), неиспользованные
/// страницы ничего не стоят.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 32 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Шрифт вшит в бинарь (см. `fonts/README.md`): так первая версия не упирается в разбиение
/// больших объектов store на куски. Перенос в store — следующий шаг, тогда шрифт станет
/// сменяемым без пересборки.
static FONT: &[u8] = include_bytes!("../../fonts/FiraCodeNerdFontMono-Regular.ttf");

/// Окно фреймбуфера в НАШЕМ адресном пространстве: между образом (0x4000_0000) и кучей
/// (0x6000_0000) — 512 МиБ свободного места, кадру нужно единицы мегабайт.
const FB_VA: usize = 0x5000_0000;

/// Кегль в пикселях. 18 даёт ячейку 11×22 на FiraCode — примерно 116×36 знакомест на 1280×800.
const FONT_PX: u32 = 18;

/// Куда пишет наш собственный вывод (баннер и эхо) — прямо в грид через разбор ANSI, как если бы
/// это пришло из PTY. Отдельного пути «печатать в терминал» нет намеренно: пусть с первого дня
/// работает ровно тот тракт, которым потом пойдут настоящие программы.
struct Screen {
    grid: Grid,
    parser: vte::Parser,
}

impl Screen {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
    }
}

/// Точка входа программ VOID ([[process-contract]]): ядро передаёт первые два права в
/// регистрах, остальные — таблицей `SYS_STARTCAP`.
#[no_mangle]
pub extern "C" fn _start(cap0: usize, cap1: usize) -> ! {
    // ── экран под capability ────────────────────────────────────────────────────────────────
    // Право приходит стартовым (`mmio:fb` в конфиге init). Без него терминал не работает и не
    // должен: рисовать некуда, а тихо продолжать — худшее, что можно сделать.
    let fb_cap = match find_fb_cap(cap0, cap1) {
        Some(c) => c,
        None => {
            sys::write("[term] нет права на экран (mmio:fb в конфиге init) - выхожу\n".as_bytes());
            sys::exit(1);
        }
    };
    let info = match sys::video_info(fb_cap) {
        Some(i) => i,
        None => {
            sys::write("[term] ядро не отдало описание видеорежима - выхожу\n".as_bytes());
            sys::exit(1);
        }
    };
    if !sys::mmio_map(fb_cap, FB_VA) {
        sys::write("[term] не удалось замапить фреймбуфер - выхожу\n".as_bytes());
        sys::exit(1);
    }
    // С этого момента ЭКРАН НАШ: ядро замолчало и печатает только в serial (см. Веху 97).

    // ── шрифт, грид, рендер ─────────────────────────────────────────────────────────────────
    let font = match TtfFont::from_vec(FONT.to_vec(), FONT_PX) {
        Ok(f) => f,
        Err(_) => {
            sys::write("[term] шрифт не разобрался - выхожу\n".as_bytes());
            sys::exit(1);
        }
    };
    let mut cache = GlyphCache::new(font);
    let metrics = cache.metrics();
    let palette = Palette::default();
    let renderer = GridRenderer::new(palette, metrics);

    let cols = (info.width / metrics.width.max(1) as usize).max(1);
    let rows = (info.height / metrics.height.max(1) as usize).max(1);
    let (surf_w, surf_h) = renderer.pixel_size(cols, rows);
    let mut surface = Surface::new(surf_w, surf_h, palette.background);
    let mut screen = Screen {
        grid: Grid::new(cols, rows),
        parser: vte::Parser::new(),
    };

    banner(&mut screen, &info, cols, rows, metrics.width, metrics.height);

    // ── цикл: нарисовать кадр, дождаться клавиш, повторить ──────────────────────────────────
    let mut buf = [0u8; 64];
    loop {
        renderer.paint(&screen.grid, &mut cache, &mut surface);
        blit(&surface, &info);

        // `SYS_READ` блокирует до ввода — крутить кадры вхолостую незачем, картинка статична,
        // пока не нажали клавишу.
        let n = sys::read_stdin(&mut buf);
        if n == 0 {
            continue;
        }
        for &b in &buf[..n] {
            match b {
                // Ctrl-D — выйти. Экран вернётся ядру само: смерть владельца ядро отслеживает.
                0x04 => {
                    sys::write("[term] выход по Ctrl-D\n".as_bytes());
                    sys::exit(0);
                }
                // Enter приходит как CR (с PS/2-клавиатуры) или как LF (из serial). Грид ждёт
                // CR+LF: голый LF опускает строку, НЕ сбрасывая колонку, и текст уезжает
                // лесенкой вправо. В настоящем терминале это делает line discipline, у нас её
                // нет — переводим здесь.
                b'\r' | b'\n' => screen.feed(b"\r\n"),
                // Backspace (0x7F) — стереть символ слева: назад, пробел, назад.
                0x7f | 0x08 => screen.feed(b"\x08 \x08"),
                _ => screen.feed(&[b]),
            }
        }
    }
}

/// Найти стартовое право на экран. Ядро кладёт преоткрытые права подряд ([[process-contract]]);
/// какое из них — экран, определяем по тому, что `SYS_VIDEO_INFO` его принял. Так программа не
/// зависит от ПОРЯДКА прав в конфиге init.
fn find_fb_cap(cap0: usize, cap1: usize) -> Option<usize> {
    // Пробуем регистры (быстрый путь контракта), затем всю таблицу преоткрытых прав. Право
    // опознаём по тому, что его ПРИНЯЛ `SYS_VIDEO_INFO`, — так программа не зависит от порядка
    // токенов в конфиге init и не сломается, если рядом появится ещё одно устройство.
    let from_regs = [cap0, cap1].into_iter();
    let from_table = (0..8).map(sys::start_cap);
    from_regs
        .chain(from_table)
        .find(|&c| c != usize::MAX && sys::video_info(c).is_some())
}

/// Приветствие: показывает ровно то, ради чего веха и делалась.
fn banner(s: &mut Screen, info: &sys::VideoInfo, cols: usize, rows: usize, cw: u32, ch: u32) {
    s.feed(b"\x1b[1;36m");
    s.feed("╔══════════════════════════════════════════════════════╗\r\n".as_bytes());
    s.feed("║  VOID — терминал на настоящих глифах (Веха 97)        ║\r\n".as_bytes());
    s.feed("╚══════════════════════════════════════════════════════╝\x1b[0m\r\n\r\n".as_bytes());

    let mut line = alloc::string::String::new();
    use core::fmt::Write;
    let _ = write!(
        line,
        "  экран  : {}×{}, {} бит/пиксель, шаг строки {} Б\r\n",
        info.width, info.height, info.bpp, info.pitch
    );
    let _ = write!(line, "  ячейка : {}×{} px → грид {}×{} знакомест\r\n", cw, ch, cols, rows);
    let _ = write!(line, "  шрифт  : FiraCode Nerd Font Mono, {} px, растеризация без C\r\n\r\n", FONT_PX);
    s.feed(line.as_bytes());

    s.feed("  Кириллица читаема: съешь ещё этих мягких французских булок.\r\n".as_bytes());
    s.feed("  Рамки: ┌─┬─┐ ├─┼─┤ └─┴─┘ │ █ ▓ ▒ ░\r\n".as_bytes());
    // Ровно то, чего в текстовом режиме не могло быть НИКОГДА: глифы из Private Use Area.
    s.feed("  \x1b[1;33mNerd Font\x1b[0m: \u{e0b0}\u{e0b2} \u{f07b}\u{f15b}\u{f121} \u{f09b}\u{e795}\u{f0e7} \u{f023}\u{f0e0}\u{f02b}\r\n".as_bytes());
    s.feed("\r\n  \x1b[32mЦвета\x1b[0m: ".as_bytes());
    for c in 31..37 {
        let mut sgr = alloc::string::String::new();
        let _ = write!(sgr, "\x1b[{c}m\u{2588}\u{2588}");
        s.feed(sgr.as_bytes());
    }
    s.feed(b"\x1b[0m\r\n\r\n");
    s.feed("  Печатайте — эхо идёт через разбор ANSI. Ctrl-D — выход.\r\n\r\n".as_bytes());
}

/// Перенести кадр из RAM в фреймбуфер, упаковав пиксели в формат прошивки.
///
/// Формат НЕ зашит: раскладку полей R/G/B сообщает `SYS_VIDEO_INFO`, потому что VBE-режимы
/// бывают и 16-битными, и с полями не на «канонических» местах. Ядро на этой самой раскладке
/// один раз обожглось (Веха 96: серый текст выходил бирюзовым), поэтому здесь она читается,
/// а не предполагается.
fn blit(surface: &Surface, info: &sys::VideoInfo) {
    let src = surface.data();
    let sw = surface.width() as usize;
    let sh = surface.height() as usize;
    let bytes_pp = info.bpp / 8;
    let w = sw.min(info.width);
    let h = sh.min(info.height);

    for y in 0..h {
        let mut dst = FB_VA + y * info.pitch;
        let row = y * sw * 4;
        for x in 0..w {
            let p = row + x * 4;
            let px = pack(info, src[p], src[p + 1], src[p + 2]);
            unsafe {
                match bytes_pp {
                    4 => core::ptr::write_volatile(dst as *mut u32, px),
                    2 => core::ptr::write_volatile(dst as *mut u16, px as u16),
                    _ => {
                        core::ptr::write_volatile(dst as *mut u8, px as u8);
                        core::ptr::write_volatile((dst + 1) as *mut u8, (px >> 8) as u8);
                        core::ptr::write_volatile((dst + 2) as *mut u8, (px >> 16) as u8);
                    }
                }
            }
            dst += bytes_pp;
        }
    }
}

/// Упаковать RGB в машинное слово пикселя по раскладке прошивки.
#[inline]
fn pack(info: &sys::VideoInfo, r: u8, g: u8, b: u8) -> u32 {
    let mut out = 0u32;
    for (i, chan) in [r, g, b].iter().enumerate() {
        let (pos, size) = info.rgb[i];
        let size = size.clamp(1, 8);
        out |= ((*chan as u32) >> (8 - size)) << pos;
    }
    out
}

/// Заглушки, чтобы `Vec`/`Rgb` не считались неиспользованными в сборках без части путей.
#[allow(dead_code)]
fn _unused(_: Vec<u8>, _: Rgb) {
    let _ = vec![0u8; 1];
}
