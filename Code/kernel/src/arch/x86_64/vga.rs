//! VGA-текст 80×25 (буфер 0xB8000) — консоль для РЕАЛЬНОГО железа (Веха 41).
//!
//! У ноутбука/мини-ПК нет физического COM-порта — вывод в serial уходит в никуда. VGA-буфер
//! в текстовом режиме есть у любой BIOS-машины (и у QEMU): каждая ячейка — байт символа +
//! байт атрибута (цвет). Ядро печатает в консоль И в COM1 (для QEMU/отладки), И сюда (для
//! экрана реальной машины) — оба дёшевы. Курсор — статик; вывод строки идёт с выключенными
//! прерываниями (`_print`), поэтому гонок нет (машина однопроцессорная).
//!
//! Веха 87 — ядро живёт в верхней половине, поэтому к буферу (он в младшем мегабайте
//! ФИЗИЧЕСКОЙ памяти) обращаемся через direct-map, [`dm`]. Так экран доступен и до
//! `paging::init` (трамплин строит direct-map первых 4 ГиБ), и после.

/// Физический адрес VGA-буфера — только как аргумент [`dm`], напрямую разыменовывать нельзя.
const VGA: usize = 0xB8000;

/// Указатель ядра на физический адрес нижней памяти (direct-map).
#[inline(always)]
fn dm(pa: usize) -> usize {
    crate::arch::phys_to_virt(pa)
}
const W: usize = 80;
const H: usize = 25;

// ── Веха 96: тот же слой ANSI поверх ДВУХ задников ───────────────────────────────────────────
// Разбор escape-кодов, курсор и перевод UTF-8 → CP866 не зависят от того, чем нарисована ячейка,
// поэтому они остались здесь, а «нарисовать знакоместо» ушло за развилку: текстовый буфер
// `0xB8000` (этот файл) либо пиксельный фреймбуфер ([`super::fb`], если GRUB дал графический
// режим). Геометрия из константы стала функцией — у пикселей она своя и известна лишь в рантайме.

/// Ширина консоли в знакоместах.
#[inline]
fn w() -> usize {
    if super::fb::present() { super::fb::cols() } else { W }
}

/// Высота консоли в знакоместах.
#[inline]
fn h() -> usize {
    if super::fb::present() { super::fb::rows() } else { H }
}

/// Атрибут по умолчанию: светло-серый на чёрном (как классический текстовый BIOS).
const ATTR_DEFAULT: u8 = 0x07;

static mut ROW: usize = 0;
static mut COL: usize = 0;
static mut ATTR: u8 = ATTR_DEFAULT;

#[inline]
unsafe fn put_cell(row: usize, col: usize, ch: u8) {
    if super::fb::present() {
        // Веха 97: экран мог уйти процессу — тогда ядро молчит (вывод остаётся в serial).
        if !super::fb::owned_by_user() {
            super::fb::put_cell(row, col, ch, ATTR);
        }
        return;
    }
    let p = (dm(VGA) + (row * W + col) * 2) as *mut u8;
    core::ptr::write_volatile(p, ch);
    core::ptr::write_volatile(p.add(1), ATTR);
}

unsafe fn newline() {
    COL = 0;
    ROW += 1;
    if ROW >= h() {
        scroll();
        ROW = h() - 1;
    }
}

/// Сдвинуть все строки на одну вверх, очистить последнюю (кольцевого буфера нет — экран мал).
unsafe fn scroll() {
    if super::fb::present() {
        if !super::fb::owned_by_user() {
            super::fb::scroll(ATTR);
        }
        return;
    }
    for row in 1..H {
        for col in 0..W {
            let src = (dm(VGA) + (row * W + col) * 2) as *const u16;
            let dst = (dm(VGA) + ((row - 1) * W + col) * 2) as *mut u16;
            core::ptr::write_volatile(dst, core::ptr::read_volatile(src));
        }
    }
    for col in 0..W {
        put_cell(H - 1, col, b' ');
    }
}

/// Вывести уже-CP866-байт в текущую позицию (ASCII-подмножество + управляющие). Внутреннее.
unsafe fn emit(byte: u8) {
    match byte {
        b'\n' => newline(),
        b'\r' => COL = 0,
        0x08 => {
            if COL > 0 {
                COL -= 1;
                put_cell(ROW, COL, b' ');
            }
        }
        _ => {
            put_cell(ROW, COL, byte);
            COL += 1;
            if COL >= w() {
                newline();
            }
        }
    }
}

// ── ANSI-разбор для VGA (Веха 43): цвета/clear/курсор. На serial escape-коды толкует терминал
//    QEMU, а здесь переводим их в атрибуты VGA и действия, НЕ рисуя сами байты последовательности.
//    Так цвет и `clear` работают одинаково и в эмуляторе, и на реальном экране.
#[derive(Clone, Copy, PartialEq)]
enum Ansi {
    Normal,
    Esc, // видели ESC (0x1B)
    Csi, // видели ESC[ — копим параметры до финального байта
}
const PARAMS_MAX: usize = 16;
static mut ANSI: Ansi = Ansi::Normal;
static mut PARAMS: [u8; PARAMS_MAX] = [0; PARAMS_MAX];
static mut PLEN: usize = 0;
static mut FG: u8 = 7; // цвет символа (VGA-палитра)
static mut BG: u8 = 0; // цвет фона
static mut BOLD: bool = false; // яркость символа (VGA-бит 3)

/// ANSI-цвет (0..7) → VGA-цвет: у VGA другой порядок (красный=4, синий=1…).
const ANSI2VGA: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

/// Собрать текущий атрибут из fg/bg/bold.
unsafe fn recompose() {
    ATTR = (BG << 4) | (if BOLD { 0x08 } else { 0 }) | FG;
}

/// Применить накопленный SGR (`ESC[ … m`): цвета/яркость/сброс.
unsafe fn apply_sgr() {
    let mut nums = [0u32; 8];
    let mut nn = 0usize;
    let mut cur = 0u32;
    for &b in &PARAMS[..PLEN] {
        if b == b';' {
            if nn < 8 {
                nums[nn] = cur;
                nn += 1;
            }
            cur = 0;
        } else if b.is_ascii_digit() {
            cur = cur * 10 + (b - b'0') as u32;
        }
    }
    if nn < 8 {
        nums[nn] = cur;
        nn += 1;
    }
    if PLEN == 0 {
        nums[0] = 0;
        nn = 1; // пустой SGR = reset
    }
    for &n in &nums[..nn] {
        match n {
            0 => {
                FG = 7;
                BG = 0;
                BOLD = false;
            }
            1 => BOLD = true,
            22 => BOLD = false,
            30..=37 => FG = ANSI2VGA[(n - 30) as usize],
            39 => FG = 7,
            40..=47 => BG = ANSI2VGA[(n - 40) as usize],
            49 => BG = 0,
            90..=97 => {
                FG = ANSI2VGA[(n - 90) as usize];
                BOLD = true;
            }
            _ => {}
        }
    }
    recompose();
}

/// Стереть от курсора до конца строки (`ESC[K`).
unsafe fn erase_line() {
    for col in COL..w() {
        put_cell(ROW, col, b' ');
    }
}

/// Первый числовой параметр CSI (для `ESC[nD` и т.п.); нет параметра → `default`, `0` → 1.
unsafe fn first_param(default: usize) -> usize {
    let mut v = 0usize;
    let mut any = false;
    for &b in &PARAMS[..PLEN] {
        if b == b';' {
            break;
        }
        if b.is_ascii_digit() {
            v = v * 10 + (b - b'0') as usize;
            any = true;
        }
    }
    if any {
        v.max(1)
    } else {
        default
    }
}

/// Веха 41 — перевести Unicode-символ в байт **CP866** (в этой раскладке загружен шрифт
/// знакогенератора, [`load_font`]): ASCII — как есть, кириллица и псевдографика — по таблице,
/// прочее — `?`.
fn map_cp866(c: char) -> u8 {
    match c {
        '\n' | '\r' | '\u{8}' => c as u8,
        ' '..='~' => c as u8, // ASCII 0x20..0x7E
        'А'..='Я' => 0x80 + (c as u32 - 'А' as u32) as u8,
        'а'..='п' => 0xA0 + (c as u32 - 'а' as u32) as u8,
        'р'..='я' => 0xE0 + (c as u32 - 'р' as u32) as u8,
        'Ё' => 0xF0,
        'ё' => 0xF1,
        '═' => 0xCD, '║' => 0xBA, '╔' => 0xC9, '╗' => 0xBB, '╚' => 0xC8, '╝' => 0xBC,
        '╟' => 0xC7, '╢' => 0xB6, '╠' => 0xCC, '╣' => 0xB9, '╦' => 0xCB, '╩' => 0xCA, '╬' => 0xCE,
        '─' => 0xC4, '│' => 0xB3, '┌' => 0xDA, '┐' => 0xBF, '└' => 0xC0, '┘' => 0xD9,
        '├' => 0xC3, '┤' => 0xB4, '┬' => 0xC2, '┴' => 0xC1, '┼' => 0xC5,
        '█' => 0xDB, '░' => 0xB0, '▒' => 0xB1, '▓' => 0xB2, '•' => 0x07, '°' => 0xF8,
        '—' | '–' => 0xC4,
        '·' => 0xFA,
        '«' => 0xAE, '»' => 0xAF,
        '→' => 0x1A, '←' => 0x1B, '↑' => 0x18, '↓' => 0x19,
        '…' => b'.',
        _ => b'?',
    }
}

/// Веха 41/43 — вывести символ на VGA. Обычные — в CP866 текущим цветом; ANSI-последовательности
/// (`ESC[…m` цвет, `ESC[2J` очистка, `ESC[H` в начало, `ESC[K` до конца строки) перехватываются
/// и НЕ рисуются (на serial те же коды толкует терминал).
pub fn put_char(c: char) {
    unsafe {
        match ANSI {
            Ansi::Normal => {
                if c == '\u{1b}' {
                    ANSI = Ansi::Esc;
                } else {
                    emit(map_cp866(c));
                }
            }
            Ansi::Esc => {
                if c == '[' {
                    PLEN = 0;
                    ANSI = Ansi::Csi;
                } else {
                    ANSI = Ansi::Normal; // не CSI — игнорируем
                }
            }
            Ansi::Csi => {
                if c.is_ascii_digit() || c == ';' {
                    if PLEN < PARAMS_MAX {
                        PARAMS[PLEN] = c as u8;
                        PLEN += 1;
                    }
                } else {
                    match c {
                        'm' => apply_sgr(),
                        'J' => clear(),                // ESC[2J — очистить экран (курсор в начало)
                        'H' | 'f' => { ROW = 0; COL = 0; }
                        'K' => erase_line(),
                        // Веха 45 — перемещение курсора (для редактирования строки в vsh).
                        'A' => ROW = ROW.saturating_sub(first_param(1)),
                        'B' => ROW = (ROW + first_param(1)).min(h() - 1),
                        'C' => COL = (COL + first_param(1)).min(w() - 1),
                        'D' => COL = COL.saturating_sub(first_param(1)),
                        _ => {}
                    }
                    ANSI = Ansi::Normal;
                }
            }
        }
    }
}

/// Очистить экран текущим цветом и увести курсор в начало (для `clear` из vsh — Веха 43).
pub fn clear() {
    unsafe {
        if super::fb::present() {
            if !super::fb::owned_by_user() {
                super::fb::clear(ATTR);
            }
        } else {
            let blank = ((ATTR as u16) << 8) | b' ' as u16;
            for i in 0..W * H {
                core::ptr::write_volatile((dm(VGA) + i * 2) as *mut u16, blank);
            }
        }
        ROW = 0;
        COL = 0;
    }
}

/// Веха 43 — синхронизировать АППАРАТНЫЙ курсор VGA с позицией письма (CRTC 0x0E/0x0F). Зовётся
/// после каждой строки — иначе мигающий курсор «висит» там, где его оставил BIOS, а не где пишем.
pub fn sync_cursor() {
    unsafe {
        // Веха 96: в пиксельном режиме аппаратного курсора нет — рисуем свой (подчёркивание).
        if super::fb::present() {
            if !super::fb::owned_by_user() {
                super::fb::cursor(ROW, COL);
            }
            return;
        }
        let pos = (ROW * W + COL) as u16;
        outb(0x3D4, 0x0F);
        outb(0x3D5, (pos & 0xff) as u8);
        outb(0x3D4, 0x0E);
        outb(0x3D5, (pos >> 8) as u8);
    }
}

#[inline]
unsafe fn outb(port: u16, v: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack));
}

/// Веха 41 — загрузить шрифт в знакогенератор VGA (плоскость 2), заменив ПЗУ-шрифт BIOS (CP437,
/// без кириллицы) на CP866 из [`super::font`]. Классическая процедура: дать CPU доступ к
/// шрифт-памяти по 0xA0000 (секвенсор + графконтроллер), записать 256 глифов по 16 байт
/// (каждый слот — 32 байта), вернуть регистры в текстовый режим. Зовётся ДО первого вывода,
/// на трамплинных таблицах (0xA0000 отображён идентично). На QEMU безвредно, на металле —
/// именно это делает русскую консоль читаемой.
pub fn load_font() {
    let font = &super::font::CP866_8X16;
    unsafe {
        // ── дать доступ к плоскости 2 (шрифт) по 0xA0000 ──
        outb(0x3C4, 0x00); outb(0x3C5, 0x01); // Seq: синхронный сброс
        outb(0x3C4, 0x02); outb(0x3C5, 0x04); //   map mask = плоскость 2
        outb(0x3C4, 0x04); outb(0x3C5, 0x07); //   memory mode: последовательный доступ
        outb(0x3C4, 0x00); outb(0x3C5, 0x03); //   конец сброса
        outb(0x3CE, 0x04); outb(0x3CF, 0x02); // GC: read map = плоскость 2
        outb(0x3CE, 0x05); outb(0x3CF, 0x00); //   graphics mode = 0 (без odd/even)
        outb(0x3CE, 0x06); outb(0x3CF, 0x00); //   misc: карта 0xA0000, алфанум

        // ── записать глифы: символ c → 0xA0000 + c*32, 16 байт из font[c*16..] ──
        for c in 0..256 {
            let dst = (dm(0xA0000) + c * 32) as *mut u8;
            for row in 0..16 {
                core::ptr::write_volatile(dst.add(row), font[c * 16 + row]);
            }
        }

        // ── вернуть регистры в обычный текстовый режим (плоскости 0/1, odd/even, 0xB8000) ──
        outb(0x3C4, 0x00); outb(0x3C5, 0x01);
        outb(0x3C4, 0x02); outb(0x3C5, 0x03); // map mask = плоскости 0,1
        outb(0x3C4, 0x04); outb(0x3C5, 0x03); // memory mode = odd/even
        outb(0x3C4, 0x00); outb(0x3C5, 0x03);
        outb(0x3CE, 0x04); outb(0x3CF, 0x00); // read map = 0
        outb(0x3CE, 0x05); outb(0x3CF, 0x10); // graphics mode = odd/even
        outb(0x3CE, 0x06); outb(0x3CF, 0x0E); // misc = 0xB8000/32K, алфанум
    }
}
