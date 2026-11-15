//! `icat` — показать картинку В ТЕРМИНАЛЕ (Веха 230.5, протокол Kitty).
//!
//! ```text
//! icat /снимок-2026-10-18-180000.png        вписать в ширину окна
//! icat картинка.png 30                      шириной в 30 знакомест
//! ```
//!
//! ## Зачем отдельная программа, если есть `img`
//!
//! `img` открывает ОКНО — он про композитор. Здесь картинка нужна внутри ленты терминала, рядом
//! с командой, которая её сделала: так смотрят скриншот после `install`, миниатюру после
//! `fetch`, картинку из пакета. Это разные задачи, и у них разные хозяева экрана.
//!
//! ## Почему протокол Kitty
//!
//! Выбор владельца (14.10.2026): «китиграфикпротокол нам нужен». Довод тот же, что у нас всегда
//! с чужими соглашениями: его понимают `chafa`, `timg`, `kitten icat` — и если они однажды
//! приедут пакетами, картинки у них заработают без нашего участия. Своё изобретение понимали бы
//! только мы.
//!
//! ## Кто что делает
//!
//! Декодирует PNG/JPEG и УМЕНЬШАЕТ картинку эта программа, а не терминал: по проводу между нами
//! лежит IPC, и слать четыре мегабайта пикселей, из которых на экране останется шестьсот
//! килобайт, — это платить за работу, которую всё равно выбросят. Терминал подгоняет присланное
//! под знакоместа точно (он один знает свой шрифт), но подгонять ему уже почти нечего.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use base64ct::Encoding;
use void_user as sys;
use void_user::{posix as px, stdio};

#[global_allocator]
static ALLOC: sys::heap::Heap<{ 96 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок площади — тот же, что у `img`: снимок современного телефона и запас под него.
const MAX_PIXELS: u64 = 17_000_000;

/// Сколько знакомест занимает картинка по умолчанию: почти вся ширина окна, но не впритык —
/// рамка окна и поля читаются глазом как часть картинки, если её прижать к краю.
const MARGIN_COLS: u16 = 2;

/// Размер знакоместа, от которого считается масштаб. Настоящий знает терминал (у него шрифт), и
/// он же подгонит присланное точно; нам он нужен лишь затем, чтобы не слать лишние пиксели.
const CELL: (u32, u32) = (8, 16);

/// Кусок данных в одной APC-последовательности. 4096 — то, что рекомендует само описание
/// протокола: куски больше неудобны чужим терминалам, меньше — лишние накладные расходы.
const CHUNK: usize = 4096;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    let argv = sys::argv::Argv::take();
    let Some(path) = argv.str(0).map(String::from) else {
        sys::write("icat: показать картинку в терминале\n\n".as_bytes());
        sys::write("  icat <файл> [ширина в знакоместах]\n".as_bytes());
        sys::write("  icat test [ширина]    — клетчатый квадрат: умеет ли терминал картинки\n".as_bytes());
        sys::exit(2);
    };
    let width = argv.str(1).and_then(|s| s.parse::<u16>().ok());

    // ПРОБА: картинка, которой не нужен файл. Нужна ровно затем, зачем `beep` звуку — ответить
    // на вопрос «а вообще работает?» одной командой, без поиска подходящего файла на машине,
    // где его может и не быть вовсе.
    if path == "test" {
        let (w, h) = (256u32, 128u32);
        let mut px = alloc::vec![0u8; w as usize * h as usize * 4];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                // Клетка 16×16 плюс наклонный градиент: по клетке видно геометрию и масштаб, по
                // градиенту — что цвета доехали, а не превратились в плашку.
                let cell = ((x / 16) + (y / 16)) % 2 == 0;
                let g = (x * 255 / w as usize) as u8;
                px[i] = if cell { g } else { 255 - g };
                px[i + 1] = (y * 255 / h as usize) as u8;
                px[i + 2] = if cell { 220 } else { 40 };
                px[i + 3] = 255;
            }
        }
        let cols = width.unwrap_or(32).clamp(1, 200);
        let rows = (cols as u32 * h * CELL.0 / (w * CELL.1)).max(1) as u16;
        send(&px, w, h, cols, rows);
        sys::exit(0);
    }

    let Some(ep) = sys::cap_named("POSIXFS").or_else(|| Some(sys::start_cap(0))) else {
        fail("icat: нет файловой персоналии");
    };
    let mut pbuf = [0u8; 512];
    let n = px::resolve(path.as_bytes(), &mut pbuf);
    let Some(bytes) = read_file(ep, &pbuf[..n]) else {
        fail(&format!("icat: не прочитать {path}"));
    };

    let img = match void_img::decode(&bytes, MAX_PIXELS) {
        Ok(i) => i,
        Err(e) => fail(&format!("icat: {e}")),
    };
    drop(bytes);

    // Сколько знакомест отдать картинке. Окно знает терминал — спрашиваем его; нет ответа
    // (нас запустили не из терминала) — берём безопасные 80×25.
    let (cols, rows) = stdio::win_size().unwrap_or((80, 25));
    let by_width = width.unwrap_or(cols.saturating_sub(MARGIN_COLS)).clamp(1, cols);
    // Пропорции считаем В ПИКСЕЛЯХ: знакоместо вдвое выше своей ширины, и картинка, вписанная
    // «по числу клеток», вышла бы вытянутой вдвое.
    let w_px = by_width as u32 * CELL.0;
    let h_px = (w_px as u64 * img.h as u64 / img.w.max(1) as u64) as u32;
    // Не выше окна: в терминале картинка живёт в ленте, и та её часть, что уехала за верх,
    // прокруткой не вернётся (мы храним знаки, а не пиксели).
    let cap_limit = (rows.saturating_sub(2)).max(1) as u32 * CELL.1;
    let (w_px, h_px) = if h_px > cap_limit {
        ((cap_limit as u64 * img.w as u64 / img.h.max(1) as u64) as u32, cap_limit)
    } else {
        (w_px, h_px)
    };
    let (w_px, h_px) = (w_px.max(1), h_px.max(1));
    let cols_out = w_px.div_ceil(CELL.0).max(1) as u16;
    let rows_out = h_px.div_ceil(CELL.1).max(1) as u16;

    let small = shrink(&img, w_px, h_px);
    send(&small, w_px, h_px, cols_out, rows_out);
    sys::exit(0);
}

/// Уменьшить ближайшим соседом. Картинка в терминале — иллюстрация в тексте, и гладкость ей
/// нужна меньше, чем скорость: вчетверо больше кода ради сглаживания того, что человек видит
/// в ленте между строк, не окупается.
fn shrink(img: &void_img::Image, w: u32, h: u32) -> Vec<u8> {
    if (w, h) == (img.w, img.h) {
        return img.px.clone();
    }
    let mut out = alloc::vec![0u8; w as usize * h as usize * 4];
    for y in 0..h as usize {
        let sy = (y as u64 * img.h as u64 / h as u64).min(img.h as u64 - 1) as usize;
        for x in 0..w as usize {
            let sx = (x as u64 * img.w as u64 / w as u64).min(img.w as u64 - 1) as usize;
            let si = (sy * img.w as usize + sx) * 4;
            let di = (y * w as usize + x) * 4;
            out[di..di + 4].copy_from_slice(&img.px[si..si + 4]);
        }
    }
    out
}

/// Отправить картинку терминалу: `ESC _ G <ключи> ; <данные base64> ESC \`, кусками.
fn send(px: &[u8], w: u32, h: u32, cols: u16, rows: u16) {
    let b64 = base64ct::Base64::encode_string(px);
    let total = b64.len().div_ceil(CHUNK).max(1);
    for (i, chunk) in b64.as_bytes().chunks(CHUNK).enumerate() {
        let more = if i + 1 < total { 1 } else { 0 };
        let keys = if i == 0 {
            // `a=T` — принять и показать; `f=32` — пиксели RGBA как есть; `s`/`v` — их размер;
            // `c`/`r` — во сколько знакомест вписать.
            format!("a=T,f=32,s={w},v={h},c={cols},r={rows},m={more}")
        } else {
            format!("m={more}")
        };
        let mut packet = Vec::with_capacity(chunk.len() + keys.len() + 8);
        packet.extend_from_slice(b"\x1b_G");
        packet.extend_from_slice(keys.as_bytes());
        packet.push(b';');
        packet.extend_from_slice(chunk);
        packet.extend_from_slice(b"\x1b\\");
        sys::write(&packet);
    }
}

/// Прочитать файл целиком. `None` — не открылся или оборвался на середине: показать половину
/// картинки хуже, чем честно отказать.
fn read_file(ep: usize, path: &[u8]) -> Option<Vec<u8>> {
    let fd = px::open(ep, path, 0);
    if fd == usize::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = px::read(ep, fd, &mut buf);
        if n == usize::MAX {
            px::close(ep, fd);
            return None;
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    px::close(ep, fd);
    (!out.is_empty()).then_some(out)
}

fn fail(msg: &str) -> ! {
    sys::write(msg.as_bytes());
    sys::write(b"\n");
    sys::exit(1);
}
