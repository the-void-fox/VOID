//! `winbox` — простейший клиент композитора (Веха 117).
//!
//! Просит окно, рисует ПРЯМО В ОБЩИЙ БУФЕР КАДРА и говорит «готово». По клику меняет цвет и
//! перерисовывается — то есть проверяет обе стороны протокола: и доставку кадра, и доставку
//! событий.
//!
//! Веха 129 — пикселей никуда не «отдают»: буфер, выданный при создании окна, это те же
//! страницы, в которые смотрит композитор ([[shm]]). До неё кадр ехал объектом store, и по
//! замеру платили не за пиксели, а за сам факт объекта.
//!
//! Приложение НЕ знает ни про фреймбуфер, ни про своё положение на экране, ни про то, что его
//! таскают мышью. Оно знает только свои пиксели и свои события — так и должно быть.
#![no_std]
#![no_main]

extern crate alloc;

use void_user as sys;
use void_user::win::{Event, Window};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 4 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Палитра клетчатого фона — чтобы на снимке было видно и границы окна, и что содержимое
/// действительно от КЛИЕНТА, а не нарисовано композитором.
const PALETTE: [(u8, u8, u8); 4] =
    [(0x1f, 0x6f, 0xeb), (0x23, 0x8b, 0x5a), (0xa3, 0x71, 0xf7), (0xdb, 0x61, 0x2a)];

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Своё имя — из argv: два одинаковых клиента должны различаться в заголовке.
    let mut abuf = [0u8; 128];
    let n = sys::args(&mut abuf);
    let name = abuf[..n]
        .split(|&b| b == 0)
        .next()
        .and_then(|s| core::str::from_utf8(s).ok())
        .unwrap_or("winbox");

    let (w, h) = (360u16, 220u16);
    let Some(mut window) = Window::create(w, h, name) else {
        sys::write_console("[winbox] композитора нет (WM в окружении) — окно не открыть\n".as_bytes());
        sys::exit(1);
    };

    let mut shade = 0usize;
    draw(window.pixels(), w, h, shade);
    window.damage(0, 0, w, h);

    // Живём событиями: пока их нет, спим в `SYS_CALL` внутри `next_event` — процессор не тратим.
    loop {
        match window.next_event() {
            Some(Event::Button { down: true, .. }) => {
                shade = (shade + 1) % PALETTE.len();
                draw(window.pixels(), w, h, shade);
                window.damage(0, 0, w, h);
            }
            // Веха 127: клавиша приезжает целиком. Смотрим на КЛАВИШУ (`sym`), а не на
            // напечатанный символ, — так `q` останется выходом и в другой раскладке.
            Some(Event::Key { sym, mods, .. }) => {
                // Ctrl-C или 'q' — уйти. Прощаемся с композитором, а не просто умираем:
                // окно должно исчезнуть в тот же миг, а не когда нас заметят мёртвыми.
                if (sym == b'c' as u16 && mods & 2 != 0) || sym == b'q' as u16 {
                    window.destroy();
                    sys::exit(0);
                }
            }
            Some(Event::Close) => {
                window.destroy();
                sys::exit(0);
            }
            _ => {}
        }
    }
}

/// Клетка выбранного цвета плюс рамка: видно и содержимое, и его края.
fn draw(px: &mut [u8], w: u16, h: u16, shade: usize) {
    let (r, g, b) = PALETTE[shade];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let cell = ((x / 20) + (y / 20)) % 2 == 0;
            let edge = x < 2 || y < 2 || x + 2 >= w as usize || y + 2 >= h as usize;
            let c = if edge {
                (0xff, 0xff, 0xff)
            } else if cell {
                (r, g, b)
            } else {
                (r / 2, g / 2, b / 2)
            };
            let p = (y * w as usize + x) * 4;
            px[p] = c.0;
            px[p + 1] = c.1;
            px[p + 2] = c.2;
            px[p + 3] = 0xff;
        }
    }
}
