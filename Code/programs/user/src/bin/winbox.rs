//! `winbox` — простейший клиент композитора (Веха 117).
//!
//! Просит окно, рисует в свою память, кладёт пиксели объектом в store и говорит «готово».
//! По клику меняет цвет и перерисовывается — то есть проверяет обе стороны протокола: и
//! доставку кадра, и доставку событий.
//!
//! Приложение НЕ знает ни про фреймбуфер, ни про своё положение на экране, ни про то, что его
//! таскают мышью. Оно знает только свои пиксели и свои события — так и должно быть.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
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

    let store = sys::cap_named("STORE").unwrap_or_else(|| sys::start_cap(1));
    let (w, h) = (360u16, 220u16);
    let Some(window) = Window::create(w, h, name) else {
        sys::write_console("[winbox] композитора нет (WM в окружении) — окно не открыть\n".as_bytes());
        sys::exit(1);
    };

    let mut shade = 0usize;
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    draw(&mut pixels, w, h, shade);
    window.present(store, &pixels);

    // Живём событиями: пока их нет, спим в `SYS_CALL` внутри `next_event` — процессор не тратим.
    loop {
        match window.next_event() {
            Some(Event::Button { down: true, .. }) => {
                shade = (shade + 1) % PALETTE.len();
                draw(&mut pixels, w, h, shade);
                window.present(store, &pixels);
            }
            Some(Event::Key(k)) => {
                // Ctrl-C или 'q' — уйти. Окно исчезнет вместе с процессом.
                if k == 3 || k == b'q' {
                    sys::exit(0);
                }
            }
            Some(Event::Close) => sys::exit(0),
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
