//! `wall` — обои (Веха 139): картинка из store на ФОНОВОМ СЛОЕ композитора.
//!
//! Первый клиент слоя. Всё, что он делает, — просит поверхность [`win::Layer::WALLPAPER`] во
//! весь экран, разбирает картинку и заполняет ею кадр. Ни фокуса, ни клавиш, ни места в ленте у
//! него нет: слой на то и слой.
//!
//! ## Почему это отдельная программа, а не поле в композиторе
//!
//! Соблазн был обратный: в `wm` уже есть фреймбуфер и цвет стола, дорисовать туда картинку —
//! десять строк. Но тогда композитор обязан уметь читать store, разбирать PNG и JPEG и
//! масштабировать — три чужих умения в программе, где лежат буферы ВСЕХ окон. Битый файл обоев
//! ронял бы весь экран. Здесь он роняет только обои: `wm` заметит смерть клиента, уберёт
//! поверхность, и на экране останется цвет стола.
//!
//! Второй довод сильнее первого: следом идёт БАР, и он тоже не часть композитора. Написать обои
//! спецслучаем внутри `wm` значило бы выбросить эту работу через веху.
//!
//! ## «Заполнить с обрезкой»
//!
//! Экран — не рамка для фотографии: полосы цвета стола по краям выглядят поломкой. Поэтому
//! [`void_img::Image::cover`] — занять весь экран, сохранив пропорции, лишнее обрезать поровну
//! с краёв. Растянуть без пропорций нельзя (люди на снимке станут толще), вписать целиком —
//! те самые полосы.
//!
//! Запуск: `wall <корень store | content-id>`. Обычно его запускает сам `wm`, прочитав в
//! конфиге поколения строку `desktop wallpaper <имя>`.
#![no_std]
#![no_main]

extern crate alloc;

use void_user as sys;
use void_user::win::{self, Event, Window};

#[path = "../obj.rs"]
mod obj;

/// Куча ЛЕНИВАЯ (`SYS_MAP` только резервирует диапазон), поэтому запас ничего не стоит, пока в
/// него не пишут. Пик здесь — распакованная картинка целиком плюс кадр экрана: снимок с
/// телефона это 64 МиБ, и меньшая арена означала бы «обои бывают только маленькие».
#[global_allocator]
static ALLOC: sys::heap::Heap<{ 128 * 1024 * 1024 }> = sys::heap::Heap::new();

/// Потолок площади — тот же, что у `img`: 17 Мпикс это снимок 4624×3468 и запас в арене.
const MAX_PIXELS: u64 = 17_000_000;

#[no_mangle]
pub extern "C" fn _start(_a0: usize, _a1: usize) -> ! {
    // Нулевым словом argv идёт имя программы, наш аргумент — первый после него.
    let mut abuf = [0u8; 256];
    let n = sys::args(&mut abuf);
    let spec = abuf[..n].split(|&b| b == 0).filter(|s| !s.is_empty()).nth(1).unwrap_or(&[]);
    if spec.is_empty() {
        say("wall: wall <корень store | content-id>\n");
        sys::exit(2);
    }

    // Размер экрана спрашиваем у КОМПОЗИТОРА, а не у железа: права `mmio:fb` у обоев нет и быть
    // не должно — программа, которая может писать во фреймбуфер напрямую, никакому композитору
    // не подчиняется.
    let Some((sw, sh)) = win::screen() else {
        say("wall: композитора нет (WM в окружении) — обоям не на чем лежать\n");
        sys::exit(1);
    };

    let Some(store) = sys::cap_named("STORE") else {
        say("wall: нет права на store (STORE в окружении)\n");
        sys::exit(1);
    };
    let bytes = match obj::read(store, spec) {
        Ok(b) => b,
        Err(e) => {
            say(&alloc::format!("wall: {e}\n"));
            sys::exit(1);
        }
    };

    let Some(full) = render(&bytes, sw, sh) else { sys::exit(1) };

    let Some(mut surf) = Window::layer(win::Layer::WALLPAPER, sw, sh, "обои") else {
        say("wall: композитор не дал поверхность слоя\n");
        sys::exit(1);
    };
    surf.pixels().copy_from_slice(&full.px);
    surf.damage(0, 0, sw, sh);
    drop(full);

    // Дальше только спим. Уйти нельзя: буфер кадра — НАШИ страницы, и композитор смотрит в них
    // ровно пока мы живы. Выход отсюда — это «обоев больше нет», а не «обои показаны».
    loop {
        match surf.next_event() {
            // Поверхности назначили другой размер. Считаем ЗАНОВО ОТ ФАЙЛА, а не от готового
            // кадра: тот уже обрезан под прежний экран, и обрезать обрезанное значит терять
            // края дважды. Файл ради этого и держим — он в сотни раз меньше пикселей.
            Some(Event::Resize { w, h }) if (w, h) != (surf.width, surf.height) => {
                let Some(next) = render(&bytes, w, h) else { continue };
                if !surf.resize_buf(w, h) {
                    continue;
                }
                surf.pixels().copy_from_slice(&next.px);
                surf.damage(0, 0, w, h);
            }
            Some(Event::Close) => {
                surf.destroy();
                sys::exit(0);
            }
            _ => {}
        }
    }
}

/// Файл → кадр `w × h`, заполненный с обрезкой. `None` — уже сказано, из-за чего.
fn render(bytes: &[u8], w: u16, h: u16) -> Option<void_img::Image> {
    let t0 = sys::now() as u64;
    let img = match void_img::decode(bytes, MAX_PIXELS) {
        Ok(i) => i,
        Err(e) => {
            say(&alloc::format!("wall: {e}\n"));
            return None;
        }
    };
    let (iw, ih) = (img.w, img.h);
    let out = match img.cover(w as u32, h as u32) {
        Ok(c) => c,
        Err(e) => {
            say(&alloc::format!("wall: {e}\n"));
            return None;
        }
    };
    let ms = sys::ticks_to_ns(sys::now() as u64 - t0) / 1_000_000;
    say(&alloc::format!("wall: {iw}×{ih} → {w}×{h} с обрезкой, {ms} мс\n"));
    Some(out)
}

fn say(s: &str) {
    sys::write(s.as_bytes());
}
