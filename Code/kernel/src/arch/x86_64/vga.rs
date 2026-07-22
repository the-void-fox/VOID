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

/// Напечатать один байт. Управляющие: `\n` (перевод строки), `\r` (в начало), `0x08` (backspace).
/// Не-ASCII (старший бит) заменяем на `?`: VGA — кодовая страница 437, UTF-8 туда не ложится.
pub fn put_byte(b: u8) {
    unsafe {
        match b {
            b'\n' => newline(),
            b'\r' => COL = 0,
            0x08 => {
                if COL > 0 {
                    COL -= 1;
                    put_cell(ROW, COL, b' ');
                }
            }
            0x20..=0x7e => {
                put_cell(ROW, COL, b);
                COL += 1;
                if COL >= W {
                    newline();
                }
            }
            _ => {
                // UTF-8-хвосты и прочее вне ASCII — одиночным '?', не плодя мусор.
                put_cell(ROW, COL, b'?');
                COL += 1;
                if COL >= W {
                    newline();
                }
            }
        }
    }
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
