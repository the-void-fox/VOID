//! VGA-текст 80×25 (буфер 0xB8000) — консоль для РЕАЛЬНОГО железа (Веха 41).
//!
//! У ноутбука/мини-ПК нет физического COM-порта — вывод в serial уходит в никуда. VGA-буфер
//! в текстовом режиме есть у любой BIOS-машины (и у QEMU): каждая ячейка — байт символа +
//! байт атрибута (цвет). Ядро печатает в консоль И в COM1 (для QEMU/отладки), И сюда (для
//! экрана реальной машины) — оба дёшевы. Курсор — статик; вывод строки идёт с выключенными
//! прерываниями (`_print`), поэтому гонок нет (машина однопроцессорная).
//!
//! Экран доступен по 0xB8000 и до, и после `paging::init`: трамплин (entry.s) отображает
//! первые 4 ГиБ, а прямое отображение ядра покрывает младший мегабайт (там и лежит буфер).

const VGA: usize = 0xB8000;
const W: usize = 80;
const H: usize = 25;

/// Атрибут по умолчанию: светло-серый на чёрном (как классический текстовый BIOS).
const ATTR_DEFAULT: u8 = 0x07;

static mut ROW: usize = 0;
static mut COL: usize = 0;
static mut ATTR: u8 = ATTR_DEFAULT;

#[inline]
unsafe fn put_cell(row: usize, col: usize, ch: u8) {
    let p = (VGA + (row * W + col) * 2) as *mut u8;
    core::ptr::write_volatile(p, ch);
    core::ptr::write_volatile(p.add(1), ATTR);
}

unsafe fn newline() {
    COL = 0;
    ROW += 1;
    if ROW >= H {
        scroll();
        ROW = H - 1;
    }
}

/// Сдвинуть все строки на одну вверх, очистить последнюю (кольцевого буфера нет — экран мал).
unsafe fn scroll() {
    for row in 1..H {
        for col in 0..W {
            let src = (VGA + (row * W + col) * 2) as *const u16;
            let dst = (VGA + ((row - 1) * W + col) * 2) as *mut u16;
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
            if COL >= W {
                newline();
            }
        }
    }
}

/// Веха 41 — напечатать символ Unicode, переведя его в байт **CP866** (в этой раскладке
/// загружен шрифт знакогенератора, [`load_font`]): ASCII — как есть, кириллица и псевдографика
/// баннеров — по таблице, прочее — `?`. Так на реальном VGA-экране видна русская консоль.
pub fn put_char(c: char) {
    let byte = match c {
        '\n' | '\r' | '\u{8}' => c as u8,
        ' '..='~' => c as u8, // ASCII 0x20..0x7E
        // Кириллица (раскладка CP866): А-Я, а-п, р-я, Ё/ё.
        'А'..='Я' => 0x80 + (c as u32 - 'А' as u32) as u8, // U+0410..042F → 0x80..0x9F
        'а'..='п' => 0xA0 + (c as u32 - 'а' as u32) as u8, // U+0430..043F → 0xA0..0xAF
        'р'..='я' => 0xE0 + (c as u32 - 'р' as u32) as u8, // U+0440..044F → 0xE0..0xEF
        'Ё' => 0xF0,
        'ё' => 0xF1,
        // Псевдографика рамок (баннер, FATAL-бокс) → коды CP437/866.
        '═' => 0xCD, '║' => 0xBA, '╔' => 0xC9, '╗' => 0xBB, '╚' => 0xC8, '╝' => 0xBC,
        '╟' => 0xC7, '╢' => 0xB6, '╠' => 0xCC, '╣' => 0xB9, '╦' => 0xCB, '╩' => 0xCA, '╬' => 0xCE,
        '─' => 0xC4, '│' => 0xB3, '┌' => 0xDA, '┐' => 0xBF, '└' => 0xC0, '┘' => 0xD9,
        '├' => 0xC3, '┤' => 0xB4, '┬' => 0xC2, '┴' => 0xC1, '┼' => 0xC5,
        '█' => 0xDB, '░' => 0xB0, '▒' => 0xB1, '▓' => 0xB2, '•' => 0x07, '°' => 0xF8,
        // Типографика, часто встречающаяся в наших строках → близкие глифы CP866/437.
        '—' | '–' => 0xC4, // тире → горизонтальная линия
        '·' => 0xFA,       // средняя точка (разделитель в отчётах)
        '«' => 0xAE, '»' => 0xAF,
        '→' => 0x1A, '←' => 0x1B, '↑' => 0x18, '↓' => 0x19, // стрелки (глифы 0x18..0x1B в CP437/866)
        '…' => b'.',
        _ => b'?',
    };
    unsafe { emit(byte) };
}

/// Очистить экран и увести курсор в начало (для `clear` из vsh — Веха 41+).
pub fn clear() {
    unsafe {
        let blank = ((ATTR as u16) << 8) | b' ' as u16;
        for i in 0..W * H {
            core::ptr::write_volatile((VGA + i * 2) as *mut u16, blank);
        }
        ROW = 0;
        COL = 0;
    }
}

/// Задать атрибут (цвет) последующего текста: младший ниббл — цвет символа, старший — фон
/// (стандартная палитра VGA). Задел под цветной вывод (Веха полировки TTY).
#[allow(dead_code)]
pub fn set_attr(attr: u8) {
    unsafe { ATTR = attr };
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
            let dst = (0xA0000 + c * 32) as *mut u8;
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
