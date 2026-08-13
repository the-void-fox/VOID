//! void-img — картинка из байтов в пиксели (Веха 138).
//!
//! Наружу торчат три вещи: узнать формат и размер БЕЗ распаковки ([`probe`]), распаковать
//! ([`decode`]), уменьшить готовое ([`Image::scaled`]). Результат всегда один и тот же —
//! RGBA8888 подряд, потому что ровно так лежит буфер окна композитора и кадр самого композитора:
//! промежуточных представлений в системе нет и заводить их незачем.
//!
//! ## Кто здесь свой, а кто чужой и почему
//!
//! | часть | откуда | довод |
//! |---|---|---|
//! | контейнер PNG (чанки, фильтры, палитра, Adam7) | свой, `png.rs` | формат заморожен с 2003 года и весь помещается в голову; крейт `png` от image-rs насквозь на `std::io` (40 мест) и тянет `flate2` — это была бы вилка, которую пришлось бы вечно догонять |
//! | inflate | `miniz_oxide` | самая мясистая часть PNG и самая проверенная в экосистеме; писать своё — заводить свои же ошибки в чужом хорошо решённом месте |
//! | JPEG целиком | `zune-jpeg` | baseline написать можно, ПРОГРЕССИВНЫЙ — это отдельный проект; у нас в корпусе (`IMG/`) половина фотографий именно прогрессивная |
//!
//! Это ровно та же линия, что с сетью и распаковкой пакетов: TCP не пишем (ADR 0008), xz и zstd
//! взяли чужие, а вот всё, что определяет ПОВЕДЕНИЕ системы, пишем сами.
//!
//! ## Потолок — обязательный аргумент, а не настройка
//!
//! [`decode`] требует `max_pixels` явно. Причина не в аккуратности: у программы VOID куча —
//! статическая арена известного размера ([`heap::Heap`](../void_user/heap/index.html)), и снимок с
//! телефона (4624×3468 = 16 Мпикс, 64 МиБ RGBA) не «замедлит» программу, а положит её в
//! аллокаторе. Потолок превращает это в честный отказ [`Error::TooBig`] ДО первой аллокации.
//! Свои буферы мы берём через `try_reserve`, поэтому нехватка кучи тоже отказ, а не паника, —
//! но у чужих декодеров внутри обычный `Vec`, и единственная защита от них — не звать их на
//! картинке, которая заведомо не влезет.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

pub mod jpeg;
pub mod png;

/// Распакованная картинка: `w × h` пикселей RGBA8888 подряд, ровно `w * h * 4` байта.
///
/// Альфа НЕ предумножена: PNG её так и хранит, JPEG её не имеет вовсе (там всегда 255).
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// Что за файл нам дали.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
}

/// Формат и размеры, вычитанные из заголовка — без распаковки и почти без памяти.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub format: Format,
    pub w: u32,
    pub h: u32,
}

impl Probe {
    /// Сколько байт займёт результат распаковки. Считается в `u64`: `w * h * 4` для картинки в
    /// 4 Гпикс переполнило бы `usize` на 32 бите, и потолок бы не сработал именно там, где он
    /// нужнее всего.
    pub fn bytes(&self) -> u64 {
        self.w as u64 * self.h as u64 * 4
    }
}

/// Отказы декодера. Каждый — про причину, а не про место: «не наш формат» и «формат наш, но
/// файл битый» это разные новости для того, кто зовёт.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Подпись не PNG и не JPEG.
    NotImage,
    /// Данные кончились раньше, чем формат.
    Truncated,
    /// Заголовок сам себе противоречит (нулевой размер, невозможная глубина).
    Header,
    /// Формат наш, но эта его разновидность не поддержана. Строка говорит какая.
    Unsupported(&'static str),
    /// Не сошлась контрольная сумма (CRC чанка PNG).
    Checksum,
    /// Поток inflate не разворачивается.
    Deflate,
    /// Картинка больше разрешённого потолка.
    TooBig,
    /// Куча не дала памяти. Честный отказ вместо паники в аллокаторе.
    NoMemory,
    /// Формат наш, разновидность наша, но содержимое не сходится.
    Corrupt(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::NotImage => f.write_str("не картинка (ни PNG, ни JPEG)"),
            Error::Truncated => f.write_str("файл обрывается"),
            Error::Header => f.write_str("заголовок противоречив"),
            Error::Unsupported(s) => write!(f, "не поддержано: {s}"),
            Error::Checksum => f.write_str("не сошлась контрольная сумма"),
            Error::Deflate => f.write_str("поток сжатия не разворачивается"),
            Error::TooBig => f.write_str("картинка больше потолка"),
            Error::NoMemory => f.write_str("не хватило кучи"),
            Error::Corrupt(s) => write!(f, "битые данные: {s}"),
        }
    }
}

impl Image {
    /// Пустой кадр нужного размера. Память берётся `try_reserve`: отказ кучи должен возвращаться
    /// вызывающему, а не разворачиваться в `handle_alloc_error` где-то в недрах.
    pub fn new(w: u32, h: u32) -> Result<Image, Error> {
        let n = w as usize * h as usize * 4;
        let mut px = Vec::new();
        px.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        px.resize(n, 0);
        Ok(Image { w, h, px })
    }

    /// Уменьшить (или увеличить) до точного размера `w × h`, БЕЗ сохранения пропорций —
    /// пропорции считает тот, кто зовёт: обоям нужно «заполнить с обрезкой», просмотрщику —
    /// «вписать целиком», и политика у них разная.
    ///
    /// Каждый пиксель приёмника — среднее по прямоугольнику источника, который в него попал.
    /// Это не resampler с ядром (без Ланцоша и даже без билинейной интерполяции при увеличении),
    /// но при уменьшении в разы именно усреднение отличает читаемую картинку от рвани: взять
    /// каждый N-й пиксель фотографии — значит выбросить 90 % данных и получить муар.
    pub fn scaled(&self, w: u32, h: u32) -> Result<Image, Error> {
        if w == 0 || h == 0 || self.w == 0 || self.h == 0 {
            return Err(Error::Header);
        }
        let mut out = Image::new(w, h)?;
        for y in 0..h as usize {
            // Границы блока-источника: [sy0, sy1). Верхняя не меньше нижней+1 — при увеличении
            // блок вырождается в один пиксель, и это ровно то, что нужно (ближайший сосед).
            let sy0 = y * self.h as usize / h as usize;
            let sy1 = (((y + 1) * self.h as usize).div_ceil(h as usize)).max(sy0 + 1).min(self.h as usize);
            for x in 0..w as usize {
                let sx0 = x * self.w as usize / w as usize;
                let sx1 =
                    (((x + 1) * self.w as usize).div_ceil(w as usize)).max(sx0 + 1).min(self.w as usize);
                let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
                for sy in sy0..sy1 {
                    let row = sy * self.w as usize * 4;
                    for sx in sx0..sx1 {
                        let p = row + sx * 4;
                        r += self.px[p] as u32;
                        g += self.px[p + 1] as u32;
                        b += self.px[p + 2] as u32;
                        a += self.px[p + 3] as u32;
                    }
                }
                let n = ((sy1 - sy0) * (sx1 - sx0)) as u32;
                let d = (y * w as usize + x) * 4;
                out.px[d] = (r / n) as u8;
                out.px[d + 1] = (g / n) as u8;
                out.px[d + 2] = (b / n) as u8;
                out.px[d + 3] = (a / n) as u8;
            }
        }
        Ok(out)
    }
}

/// Формат по подписи. PNG — восемь байт, придуманных так, чтобы порча при передаче через
/// текстовый канал была заметна; JPEG — маркер SOI и начало следующего маркера.
pub fn format(bytes: &[u8]) -> Option<Format> {
    if bytes.starts_with(&png::SIG) {
        Some(Format::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(Format::Jpeg)
    } else {
        None
    }
}

/// Формат и размеры по заголовку. Дешёвая проверка перед распаковкой: узнать, влезет ли,
/// и решить, до чего уменьшать, — не заплатив за пиксели.
pub fn probe(bytes: &[u8]) -> Result<Probe, Error> {
    match format(bytes).ok_or(Error::NotImage)? {
        Format::Png => {
            let i = png::info(bytes)?;
            Ok(Probe { format: Format::Png, w: i.w, h: i.h })
        }
        Format::Jpeg => {
            let (w, h) = jpeg::info(bytes)?;
            Ok(Probe { format: Format::Jpeg, w, h })
        }
    }
}

/// Распаковать в RGBA8888. `max_pixels` — потолок ПЛОЩАДИ (см. рассуждение в шапке модуля):
/// проверяется по заголовку, до того как выделен хоть байт под пиксели.
pub fn decode(bytes: &[u8], max_pixels: u64) -> Result<Image, Error> {
    match format(bytes).ok_or(Error::NotImage)? {
        Format::Png => png::decode(bytes, max_pixels),
        Format::Jpeg => jpeg::decode(bytes, max_pixels),
    }
}
