//! `img` — показать картинку из store (Веха 138).
//!
//! Первый потребитель [`void_img`]: берёт объект store (корень или content-id), разбирает PNG
//! или JPEG в RGBA, вписывает в окно и показывает. Обоями это ещё не является — обои станут
//! клиентом СЛОЯ, когда слои появятся в протоколе; здесь проверяется ровно то, ради чего
//! декодеры и написаны: настоящий файл превращается в настоящие пиксели на экране VOID.
//!
//! Запуск: `img <корень|content-id>`. Картинка кладётся в store мостом с хоста
//! (`void-store-import <образ> put файл.png f/etc/wall.png`) или скачивается из vvsh
//! (`\fetch("http://…/x.jpg", "wall")`) — во втором случае объект приезжает блобом из кусков,
//! и собирает его обратно [`obj::read`] (общий с обоями).
#![no_std]
#![no_main]

extern crate alloc;

use void_user as sys;
use void_user::win::{Event, Window};

#[path = "../obj.rs"]
mod obj;

/// Арена под пиксели. Куча процесса ЛЕНИВАЯ (`SYS_MAP` резервирует диапазон, страницы приходят
/// по обращению), поэтому «попросить с запасом» ничего не стоит: снимок 4624×3468 разворачивается
/// в 64 МиБ RGBA, и вместе с исходным потоком это единственный настоящий расход программы.
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 128 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок площади: 17 Мпикс — это ровно снимок современного телефона (4624×3468) и запас в
/// арене под него. Больше — честный отказ по заголовку, а не смерть в аллокаторе на середине.
const MAX_PIXELS: u64 = 17_000_000;

/// Окно не больше этого: показать 4624 пикселя в ширину некуда, а вписывать надо ДО того, как
/// картинка окажется в буфере окна (композитор масштабировать не умеет и не должен).
const WIN_MAX: (u32, u32) = (1000, 700);

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Нулевым словом в argv идёт ИМЯ программы (так его читает `winbox`, чтобы подписать окно);
    // наш аргумент — первый после него.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf);
    let spec = abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).nth(1).unwrap_or(&[]);
    if spec.is_empty() {
        sys::write("img: img <корень store | content-id>\n".as_bytes());
        sys::exit(2);
    }

    let Some(store) = sys::cap_named("STORE") else {
        sys::write("img: нет права на store (STORE в окружении)\n".as_bytes());
        sys::exit(1);
    };
    let bytes = match obj::read(store, spec) {
        Ok(b) => b,
        Err(e) => {
            say(&alloc::format!("img: {e}\n"));
            sys::exit(1);
        }
    };

    // Сначала заголовок: размеры и формат известны до того, как выделен хоть байт под пиксели.
    let probe = match void_img::probe(&bytes) {
        Ok(p) => p,
        Err(e) => {
            say(&alloc::format!("img: {e}\n"));
            sys::exit(1);
        }
    };
    say(&alloc::format!(
        "img: {} {}×{} ({} КиБ в store, {} КиБ в пикселях)\n",
        match probe.format {
            void_img::Format::Png => "PNG",
            void_img::Format::Jpeg => "JPEG",
        },
        probe.w,
        probe.h,
        bytes.len() / 1024,
        probe.bytes() / 1024,
    ));

    let t0 = sys::now() as u64;
    let img = match void_img::decode(&bytes, MAX_PIXELS) {
        Ok(i) => i,
        Err(e) => {
            say(&alloc::format!("img: {e}\n"));
            sys::exit(1);
        }
    };
    let ms = sys::ticks_to_ns(sys::now() as u64 - t0) / 1_000_000;
    let mpx = probe.w as u64 * probe.h as u64 / 1000;
    say(&alloc::format!(
        "img: разобрано за {} мс ({} тыс. пикселей/с)\n",
        ms,
        if ms > 0 { mpx * 1000 / ms } else { 0 }
    ));
    drop(bytes);

    // Вписать целиком, сохраняя пропорции: обрезать чужую фотографию мы не вправе, а окно с
    // соотношением сторон картинки честнее полей внутри окна.
    let (ww, wh) = fit(img.w, img.h, WIN_MAX.0, WIN_MAX.1);
    let shown = if (ww, wh) == (img.w, img.h) {
        img
    } else {
        match img.scaled(ww, wh) {
            Ok(s) => s,
            Err(e) => {
                say(&alloc::format!("img: масштабирование: {e}\n"));
                sys::exit(1);
            }
        }
    };

    let Some(mut window) = Window::create(ww as u16, wh as u16, "img") else {
        sys::write("img: композитора нет (WM в окружении) — окно не открыть\n".as_bytes());
        sys::exit(1);
    };
    // Буфер окна — те же страницы, в которые смотрит композитор ([[shm]], Веха 129), и лежат
    // там RGBA8888 в том же порядке, что отдаёт декодер. Копия ровно одна.
    window.pixels().copy_from_slice(&shown.px);
    window.damage(0, 0, ww as u16, wh as u16);

    loop {
        match window.next_event() {
            Some(Event::Key { sym, mods, .. }) => {
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

fn say(s: &str) {
    sys::write(s.as_bytes());
}

/// Наибольший размер с сохранением пропорций, влезающий в `mw × mh`. Увеличивать не будем:
/// растянутая на весь экран иконка — не то, что просят, показывая картинку.
fn fit(w: u32, h: u32, mw: u32, mh: u32) -> (u32, u32) {
    if w <= mw && h <= mh {
        return (w, h);
    }
    // Считаем в u64: 4624 × 1000 переполнит u32 при следующем умножении на высоту.
    let (w64, h64) = (w as u64, h as u64);
    if w64 * mh as u64 > h64 * mw as u64 {
        (mw, ((h64 * mw as u64 / w64) as u32).max(1))
    } else {
        (((w64 * mh as u64 / h64) as u32).max(1), mh)
    }
}
