//! `term` — терминал-мультиплексор VOID на крейтах ereb (Вехи 97 и 99, ADR 0014).
//!
//! Веха 97 сняла потолок 256 глифов: экран стал пикселями, а глифы — настоящим TrueType.
//! Веха 99 добавила то, ради чего вся фаза и затевалась: **панели с живыми процессами**.
//!
//! ```text
//!   клавиатура ─→ префикс? ─да→ команда мультиплексора (разбить, перейти, закрыть)
//!                     └─нет→ отложенный ответ ребёнку ФОКУСНОЙ панели (его read_stdin)
//!
//!   ребёнок ──IPC(OP_STDOUT)──→ грид своей панели ──→ общий кадр ──→ фреймбуфер
//! ```
//!
//! ## Почему это устроено именно так
//!
//! - **Хост — обычный IPC-сервер.** Ничего нового: `net-srv` устроен так же, включая отложенные
//!   ответы. Ребёнок, спящий в `read_stdin`, спит в `SYS_CALL` к нам, и мы отвечаем ему, когда
//!   приходят клавиши. Никакого «драйвера терминала» в ядре не появилось.
//! - **Реактор не имеет права уснуть ни на одном источнике.** Клавиатура читается
//!   неблокирующе ([`sys::read_console_nonblock`], Веха 99), вывод детей — `try_recv`. Сон —
//!   только когда пусто и то и другое, и только со сроком.
//! - **Раскладка — `ereb-mux`**: дерево разбиений и навигация уже написаны и покрыты тестами в
//!   апстриме, своей геометрии не заводим.
//!
//! ## Управление
//!
//! Префикс — **Ctrl-A** (как в screen), дальше: `|` разбить вертикально · `-` горизонтально ·
//! `o` следующая панель · `x` закрыть · `q` выйти · Ctrl-A — послать сам Ctrl-A в панель.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use ereb_core::{Cell, Color, Grid, NamedColor};
use ereb_mux::{Area, PaneId, PaneRect, SplitDirection, SplitTree};
use ereb_render::{GlyphCache, GridRenderer, Palette, Surface, TtfFont};
use void_user as sys;
use void_user::{stdio, Wait};

/// Куча: кадр 1280×800 RGBA (4 МиБ), копия шрифта (2.6 МиБ), гриды панелей, кэш глифов.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 48 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Шрифт вшит в бинарь (см. `fonts/README.md`) — временно, до переноса в store.
static FONT: &[u8] = include_bytes!("../../fonts/FiraCodeNerdFontMono-Regular.ttf");

/// Окно фреймбуфера в нашем адресном пространстве: между образом и кучей.
const FB_VA: usize = 0x5000_0000;
const FONT_PX: u32 = 18;

/// Что запускаем в новой панели и с каким аргументом. Без `repl` vvsh печатает справку и
/// выходит — панель умирала мгновенно, и выглядело это как «мультиплексор не работает».
const SHELL: &[u8] = b"bin/vvsh";
const SHELL_ARGS: &[u8] = b"repl";

/// Префикс команд мультиплексора — Ctrl-A, как в screen.
const PREFIX: u8 = 0x01;

/// Панель: грид, разбор ANSI, ребёнок и его отложенный запрос ввода.
struct Pane {
    id: PaneId,
    grid: Grid,
    parser: vte::Parser,
    /// Номер процесса-ребёнка (`None` — запустить не удалось либо он уже завершился).
    child: Option<usize>,
    /// Ребёнок спит в `read_stdin` и ждёт ответа. Копим клавиши, пока он не спросит, и отвечаем
    /// сразу, как есть и запрос, и байты, — иначе ввод терялся бы между этими событиями.
    pending_read: Option<usize>,
    /// Не отданный ввод этой панели.
    inbox: Vec<u8>,
}

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let Some(fb_cap) = find_fb_cap() else {
        sys::write_console("[term] нет права на экран (mmio:fb в конфиге init)\n".as_bytes());
        sys::exit(1);
    };
    let Some(info) = sys::video_info(fb_cap) else {
        sys::write_console("[term] ядро не отдало описание видеорежима\n".as_bytes());
        sys::exit(1);
    };
    if !sys::mmio_map(fb_cap, FB_VA) {
        sys::write_console("[term] не удалось замапить фреймбуфер\n".as_bytes());
        sys::exit(1);
    }

    let Ok(font) = TtfFont::from_vec(FONT.to_vec(), FONT_PX) else {
        sys::write_console("[term] шрифт не разобрался\n".as_bytes());
        sys::exit(1);
    };
    let mut cache = GlyphCache::new(font);
    let metrics = cache.metrics();
    let palette = Palette::default();
    let renderer = GridRenderer::new(palette, metrics);

    // Геометрия в знакоместах; последняя строка — статус-бар мультиплексора.
    let cols = (info.width / metrics.width.max(1) as usize).max(8);
    let rows = (info.height / metrics.height.max(1) as usize).max(4);
    let (surf_w, surf_h) = renderer.pixel_size(cols, rows);
    let mut surface = Surface::new(surf_w, surf_h, palette.background);

    // Право на ЗАПУСК ищем перебором стартовых прав, а не по фиксированному индексу: порядок
    // токенов в конфиге init — дело конфига, и он уже менялся. Перебор здесь безопасен: неудачный
    // `spawn` ничего не делает, а удастся он ровно с тем правом, у которого есть EXEC на store.
    let me = sys::self_endpoint();

    let mut tree = SplitTree::leaf(PaneId(0));
    let mut next_id = 1usize;
    let mut rects = layout_of(&tree, cols, rows);
    let mut exec_cap = sys::NO_CAP;
    let mut panes: Vec<Pane> = vec![new_pane(PaneId(0), &rects, &mut exec_cap, me)];
    let mut focus = 0usize;

    // Прошлый кадр в ЯЧЕЙКАХ: по нему считаем, какие пиксельные строки реально изменились.
    // Без этого каждый чих перерисовывал весь экран — 4 МиБ записей в некэшируемую память на
    // КАЖДУЮ строку вывода. На железе это выглядело как «семидесятые».
    let mut prev_cells: Vec<Cell> = Vec::new();
    let mut prefix_armed = false;
    let mut keys = [0u8; 64];
    let mut msg = [0u8; stdio::CHUNK];
    let mut redraw = true;

    loop {
        let mut worked = false;

        // ── 1. клавиатура (никогда не блокируемся) ─────────────────────────────────────────
        let n = sys::read_console_nonblock(&mut keys);
        if n > 0 {
            worked = true;
            for i in 0..n {
                let k = keys[i];
                if prefix_armed {
                    prefix_armed = false;
                    match k {
                        b'|' | b'-' => {
                            let dir = if k == b'|' {
                                SplitDirection::Vertical
                            } else {
                                SplitDirection::Horizontal
                            };
                            split(&mut tree, &mut panes, &mut next_id, &mut focus, dir,
                                  cols, rows, &mut exec_cap, me);
                            rects = layout_of(&tree, cols, rows);
                            redraw = true;
                        }
                        b'o' => {
                            focus = (focus + 1) % panes.len().max(1);
                            redraw = true;
                        }
                        b'x' => {
                            close_pane(&mut tree, &mut panes, &mut focus);
                            if panes.is_empty() {
                                sys::write_console("[term] панелей не осталось — выход\n".as_bytes());
                                sys::exit(0);
                            }
                            rects = layout_of(&tree, cols, rows);
                            resize_all(&mut panes, &rects);
                            redraw = true;
                        }
                        b'q' => {
                            sys::write_console("[term] выход по Ctrl-A q\n".as_bytes());
                            sys::exit(0);
                        }
                        // Ctrl-A дважды — отдать сам Ctrl-A панели, иначе он был бы недоступен.
                        PREFIX => push_input(&mut panes, focus, PREFIX),
                        _ => {}
                    }
                } else if k == PREFIX {
                    prefix_armed = true;
                } else {
                    push_input(&mut panes, focus, k);
                }
            }
        }

        // ── 2. вывод и запросы ввода от детей ──────────────────────────────────────────────
        while let Some(m) = sys::try_recv(&mut msg) {
            worked = true;
            redraw |= handle(&mut panes, &m, &msg);
        }

        // ── 3. отдать накопленный ввод тем, кто его ждёт ───────────────────────────────────
        for p in panes.iter_mut() {
            if p.pending_read.is_some() && !p.inbox.is_empty() {
                let take = p.inbox.len().min(stdio::CHUNK);
                sys::reply(p.pending_read.take().unwrap(), &p.inbox[..take]);
                p.inbox.drain(..take);
                worked = true;
            }
        }

        // ── 4. умершие дети ────────────────────────────────────────────────────────────────
        for i in 0..panes.len() {
            if let Some(pid) = panes[i].child {
                if let Wait::Exited(_) = sys::wait(pid, true) {
                    let pane = &mut panes[i];
                    pane.child = None;
                    let note =
                        "\r\n\x1b[1;31m[процесс завершился — Ctrl-A x закрыть панель]\x1b[0m\r\n";
                    pane.parser.advance(&mut pane.grid, note.as_bytes());
                    redraw = true;
                    worked = true;
                }
            }
        }

        // ── 5. кадр ────────────────────────────────────────────────────────────────────────
        if redraw {
            let cells = compose(&mut surface, &renderer, &mut cache, &panes, &rects, focus,
                                cols, rows, palette);
            let (y0, y1) = dirty_rows(&prev_cells, &cells, cols, rows);
            if y0 <= y1 {
                let ch = metrics.height.max(1) as usize;
                blit_rows(&surface, &info, y0 * ch, ((y1 + 1) * ch).min(info.height));
            }
            prev_cells = cells;
            redraw = false;
        }

        // Спать только когда делать нечего — и коротко: клавиатура нас не разбудит, потому что
        // ждём мы на IPC. Осознанный компромисс: единого «ждать клавишу ИЛИ сообщение» в ядре
        // пока нет (записано долгом).
        if !worked {
            // Сообщение, пришедшее ВО СНЕ, обязано быть обработано здесь же. Выбросить его
            // нельзя: вместе с ним теряется одноразовое reply-право, и вызвавший ребёнок висит
            // навсегда — молча, без единой ошибки. Ровно на этом веха и споткнулась.
            if let Some(m) = sys::recv_timeout(&mut msg, 2) {
                redraw |= handle(&mut panes, &m, &msg);
            }
        }
    }
}

/// Обработать одно сообщение от ребёнка. Возвращает `true`, если кадр надо перерисовать.
///
/// Вынесено отдельно НЕ ради красоты: принимать сообщения приходится в двух местах — в опросе
/// и при пробуждении из сна, — и разошедшиеся копии этой обработки означали бы потерянные
/// reply-права и повисших детей.
fn handle(panes: &mut [Pane], m: &sys::Message, buf: &[u8]) -> bool {
    let who = panes.iter().position(|p| p.child == Some(m.sender));
    match who {
        Some(i) if m.op == stdio::OP_STDOUT => {
            let len = m.len.min(buf.len());
            let pane = &mut panes[i];
            // Перевод строки: программы шлют голый `\n`, а грид (как и любой терминал) ждёт
            // CR+LF — иначе строка опускается, НЕ возвращая курсор, и вывод идёт лесенкой
            // вправо. В Unix это делает драйвер tty (ONLCR); у нас драйвера нет, поэтому
            // трансляция здесь — терминал и есть её законное место.
            let mut from = 0usize;
            for at in 0..len {
                if buf[at] == b'\n' && (at == 0 || buf[at - 1] != b'\r') {
                    pane.parser.advance(&mut pane.grid, &buf[from..at]);
                    pane.parser.advance(&mut pane.grid, b"\r\n");
                    from = at + 1;
                }
            }
            pane.parser.advance(&mut pane.grid, &buf[from..len]);
            sys::reply(m.reply_cap, &[]);
            true
        }
        Some(i) if m.op == stdio::OP_STDIN => {
            // Отложенный ответ: держим право до появления клавиш (как в net-srv).
            panes[i].pending_read = Some(m.reply_cap);
            false
        }
        // Чужой или непонятный запрос — ответить пусто, а не молчать: молчание повесило бы
        // вызвавшего навсегда.
        _ => {
            sys::reply(m.reply_cap, &[]);
            false
        }
    }
}

/// Завести панель: грид под её размер плюс запущенный в ней шелл с нашим stdio.
fn new_pane(id: PaneId, rects: &[PaneRect], exec_cap: &mut usize, me: usize) -> Pane {
    let (w, h) = rect_size(rects, id);
    let child = spawn_shell(exec_cap, me);
    let mut pane = Pane {
        id,
        grid: Grid::new(w, h),
        parser: vte::Parser::new(),
        child,
        pending_read: None,
        inbox: Vec::new(),
    };
    if child.is_none() {
        let msg = "\x1b[1;31m[не удалось запустить шелл]\x1b[0m\r\n";
        pane.parser.advance(&mut pane.grid, msg.as_bytes());
    }
    pane
}

/// Запустить шелл, подобрав право на запуск. Найденное запоминается — перебирать на каждую
/// панель незачем.
fn spawn_shell(exec_cap: &mut usize, me: usize) -> Option<usize> {
    if *exec_cap != sys::NO_CAP {
        return sys::spawn_with_stdio(*exec_cap, SHELL, SHELL_ARGS, me);
    }
    for i in 0..8 {
        let c = sys::start_cap(i);
        if c == sys::NO_CAP {
            continue;
        }
        if let Some(pid) = sys::spawn_with_stdio(c, SHELL, SHELL_ARGS, me) {
            *exec_cap = c;
            return Some(pid);
        }
    }
    None
}

/// Разбить фокусную панель и завести в новой половине ещё один шелл.
#[allow(clippy::too_many_arguments)]
fn split(
    tree: &mut SplitTree, panes: &mut Vec<Pane>, next_id: &mut usize, focus: &mut usize,
    dir: SplitDirection, cols: usize, rows: usize, exec_cap: &mut usize, me: usize,
) {
    if panes.is_empty() {
        return;
    }
    let target = panes[*focus].id;
    let fresh = PaneId(*next_id);
    if !tree.split(target, dir, 0.5, fresh) {
        return;
    }
    *next_id += 1;
    let rects = layout_of(tree, cols, rows);
    resize_all(panes, &rects); // старые панели поменяли размер — грид обязан следовать
    panes.push(new_pane(fresh, &rects, exec_cap, me));
    *focus = panes.len() - 1;
}

/// Закрыть фокусную панель. Ребёнок останется сиротой и заметит это сам — по тому, что его
/// вызовы перестанут доходить; убивать процессы мы пока не умеем (записано долгом).
fn close_pane(tree: &mut SplitTree, panes: &mut Vec<Pane>, focus: &mut usize) {
    if panes.is_empty() {
        return;
    }
    let id = panes[*focus].id;
    if panes.len() > 1 && !tree.close(id) {
        return;
    }
    panes.remove(*focus);
    if *focus >= panes.len() {
        *focus = panes.len().saturating_sub(1);
    }
}

/// Подогнать гриды под текущую раскладку.
fn resize_all(panes: &mut [Pane], rects: &[PaneRect]) {
    for p in panes.iter_mut() {
        let (w, h) = rect_size(rects, p.id);
        if p.grid.cols() != w || p.grid.rows() != h {
            // `Grid::resize` переносит содержимое; раньше здесь создавался НОВЫЙ грид, и при
            // каждом разбиении соседние панели чернели — самая заметная ошибка первой версии.
            p.grid.resize(w, h);
        }
    }
}

/// Положить клавишу в ящик фокусной панели. Ответ уйдёт в шаге 3 реактора — там же, где
/// обслуживаются запросы, пришедшие РАНЬШЕ клавиш.
fn push_input(panes: &mut [Pane], focus: usize, byte: u8) {
    if let Some(p) = panes.get_mut(focus) {
        p.inbox.push(byte);
    }
}

/// Раскладка дерева в знакоместах; снизу оставлена строка под статус-бар.
fn layout_of(tree: &SplitTree, cols: usize, rows: usize) -> Vec<PaneRect> {
    tree.layout(
        Area { col: 0, row: 0, cols: cols as u16, rows: (rows - 1) as u16 },
        1, // зазор в знакоместо: панели должны быть видимо разделены
    )
}

/// Размер панели в знакоместах (минимум 1×1 — вырожденную раскладку рендер переживать не обязан).
fn rect_size(rects: &[PaneRect], id: PaneId) -> (usize, usize) {
    rects
        .iter()
        .find(|r| r.id == id)
        .map(|r| (r.area.cols.max(1) as usize, r.area.rows.max(1) as usize))
        .unwrap_or((1, 1))
}

/// Собрать кадр: панели по своим прямоугольникам + подсветка фокуса + статус-бар.
#[allow(clippy::too_many_arguments)]
fn compose(
    surface: &mut Surface, renderer: &GridRenderer, cache: &mut GlyphCache<TtfFont>,
    panes: &[Pane], rects: &[PaneRect], focus: usize, cols: usize, rows: usize, palette: Palette,
) -> Vec<Cell> {
    surface.clear(palette.background);
    // Общий кадр — мозаика из гридов панелей: у каждой свой, склеиваем по ячейкам.
    let mut cells = vec![Cell::default(); cols * rows];
    for (i, p) in panes.iter().enumerate() {
        let Some(r) = rects.iter().find(|r| r.id == p.id) else {
            continue;
        };
        for row in 0..r.area.rows as usize {
            for col in 0..r.area.cols as usize {
                let (x, y) = (r.area.col as usize + col, r.area.row as usize + row);
                if x < cols && y < rows {
                    cells[y * cols + x] = p.grid.view_cell(col, row, 0);
                }
            }
        }
        if i == focus {
            mark_focus(&mut cells, r, cols, rows);
        }
    }
    status_bar(&mut cells, panes, focus, cols, rows);
    // Рисуем в RAM целиком: это дёшево (кэшируемая память). Дорого — переносить на экран,
    // поэтому туда уедут только изменившиеся строки.
    renderer.paint_cells(&cells, cols, rows, cache, surface);
    cells
}

/// Диапазон изменившихся строк грида `[первая, последняя]`; `первая > последняя` — изменений нет.
/// Диапазоном, а не списком: вывод почти всегда идёт подряд, а один `blit` полосой дешевле, чем
/// десяток вызовов вразбивку.
fn dirty_rows(prev: &[Cell], now: &[Cell], cols: usize, rows: usize) -> (usize, usize) {
    if prev.len() != now.len() {
        return (0, rows.saturating_sub(1)); // первый кадр или сменилась геометрия — весь экран
    }
    let (mut first, mut last) = (usize::MAX, 0usize);
    for y in 0..rows {
        let r = y * cols..(y + 1) * cols;
        if prev[r.clone()] != now[r] {
            if first == usize::MAX {
                first = y;
            }
            last = y;
        }
    }
    if first == usize::MAX { (1, 0) } else { (first, last) }
}

/// Пометить фокусную панель по краям зазора: сплошную рамку рисовать негде — зазор между
/// панелями ровно одно знакоместо, а внутри панели каждая ячейка занята содержимым.
fn mark_focus(cells: &mut [Cell], r: &PaneRect, cols: usize, rows: usize) {
    let mut mark = |x: usize, y: usize, ch: char| {
        if x < cols && y < rows {
            let c = &mut cells[y * cols + x];
            c.ch = ch;
            c.fg = Color::Named(NamedColor::BrightCyan);
        }
    };
    let (x0, y0) = (r.area.col as usize, r.area.row as usize);
    let x1 = x0 + r.area.cols.saturating_sub(1) as usize;
    let y1 = y0 + r.area.rows.saturating_sub(1) as usize;
    if x0 > 0 {
        for y in y0..=y1 {
            mark(x0 - 1, y, '▏');
        }
    }
    if y0 > 0 {
        for x in x0..=x1 {
            mark(x, y0 - 1, '▁');
        }
    }
}

/// Статус-бар: сколько панелей, какая в фокусе, жив ли её процесс, подсказка по префиксу.
fn status_bar(cells: &mut [Cell], panes: &[Pane], focus: usize, cols: usize, rows: usize) {
    let y = rows - 1;
    let mut text = alloc::string::String::new();
    use core::fmt::Write;
    let _ = write!(text, " VOID · панель {}/{} ", focus + 1, panes.len());
    if let Some(p) = panes.get(focus) {
        let _ = write!(text, "· {} ", if p.child.is_some() { "живая" } else { "мертва" });
    }
    let _ = write!(text, "· Ctrl-A: | - o x q");
    let mut i = 0usize;
    for ch in text.chars() {
        if i >= cols {
            break;
        }
        let c = &mut cells[y * cols + i];
        c.ch = ch;
        c.fg = Color::Named(NamedColor::Black);
        c.bg = Color::Named(NamedColor::BrightCyan);
        i += 1;
    }
    while i < cols {
        let c = &mut cells[y * cols + i];
        c.ch = ' ';
        c.bg = Color::Named(NamedColor::BrightCyan);
        i += 1;
    }
}

/// Право на экран: опознаём по тому, что его ПРИНЯЛ `SYS_VIDEO_INFO` (проба безобидна).
fn find_fb_cap() -> Option<usize> {
    (0..8)
        .map(sys::start_cap)
        .find(|&c| c != sys::NO_CAP && sys::video_info(c).is_some())
}

/// Перенести кадр из RAM в фреймбуфер, упаковав пиксели в формат прошивки.
fn blit_rows(surface: &Surface, info: &sys::VideoInfo, y_from: usize, y_to: usize) {
    let src = surface.data();
    let sw = surface.width() as usize;
    let bytes_pp = info.bpp / 8;
    let w = sw.min(info.width);
    let h = (surface.height() as usize).min(info.height).min(y_to);
    for y in y_from..h {
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

/// Упаковать RGB по раскладке прошивки (её сообщает `SYS_VIDEO_INFO`; зашивать нельзя — Веха 96).
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
