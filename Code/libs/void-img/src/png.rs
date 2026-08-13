//! PNG — контейнер свой, inflate чужой.
//!
//! Формат простой и заморожен: подпись, дальше цепочка чанков `длина|тип|данные|CRC`. Всё, что
//! нам нужно, лежит в четырёх из них — `IHDR` (размеры и вид пикселя), `PLTE` (палитра),
//! `tRNS` (прозрачность) и `IDAT` (сжатые строки). Остальные (гамма, цветовой профиль, тексты,
//! кадры APNG) пропускаются молча: критические мы обязаны понимать, но незнакомых критических
//! в живых PNG не бывает, а вспомогательные на пиксели не влияют.
//!
//! Три места, где PNG сложнее, чем кажется, и где ошибка даёт не отказ, а кривую картинку:
//!
//! 1. **Фильтры строк.** Каждая строка предсказывается по СОСЕДЯМ и хранится разницей. Снимать
//!    фильтр можно только вперёд и только по месту: предсказание смотрит на уже восстановленную
//!    строку выше и на уже восстановленные пиксели слева.
//! 2. **Упаковка бит.** При глубине 1/2/4 в байте лежит несколько пикселей, старшими битами
//!    вперёд, и строка добивается до целого байта.
//! 3. **Adam7.** Чересстрочная картинка приезжает СЕМЬЮ отдельными картинками со своими
//!    размерами и своей фильтрацией, которые надо разложить по решётке. В вебе так почти никто
//!    не пишет, но «почти» — это и есть тот файл, на котором обои окажутся мусором.

use alloc::vec::Vec;

use crate::{Error, Image};

/// Подпись: `\x89PNG\r\n\x1a\n`. Байты подобраны так, чтобы файл, прошедший через канал с
/// переводом строк или через 7-битную передачу, перестал быть валидным сразу, а не в пикселях.
pub const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Заголовок `IHDR` — всё, что определяет раскладку пикселей.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub w: u32,
    pub h: u32,
    /// Бит на СЭМПЛ (не на пиксель): 1, 2, 4, 8 или 16.
    pub depth: u8,
    /// 0 — серый, 2 — RGB, 3 — палитра, 4 — серый+альфа, 6 — RGBA.
    pub color: u8,
    /// 0 — построчно, 1 — Adam7.
    pub interlace: u8,
}

impl Info {
    /// Сэмплов на пиксель.
    fn channels(&self) -> usize {
        match self.color {
            0 | 3 => 1,
            2 => 3,
            4 => 2,
            _ => 4,
        }
    }

    /// Байт на пиксель, ОКРУГЛЁННЫЙ ВВЕРХ — ровно на столько фильтры Sub/Paeth отступают влево.
    /// При глубине меньше 8 это 1: соседом считается предыдущий БАЙТ, а не предыдущий пиксель.
    fn bpp(&self) -> usize {
        (self.channels() * self.depth as usize).div_ceil(8)
    }

    /// Байт в строке шириной `w` пикселей (без байта фильтра).
    fn stride(&self, w: u32) -> usize {
        (w as usize * self.channels() * self.depth as usize).div_ceil(8)
    }
}

/// Adam7: (x0, y0, шаг по x, шаг по y) для каждого из семи проходов.
const PASS: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Таблица CRC-32/ISO-HDLC считается на этапе компиляции — 1 КиБ в `.rodata` вместо кода
/// инициализации и мьютекса вокруг него.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in bytes {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

/// Прочитать `IHDR`. Отдельно от [`decode`], потому что размер часто нужен раньше решения
/// распаковывать: 16-мегапиксельный снимок с телефона лучше отвергнуть по заголовку.
pub fn info(bytes: &[u8]) -> Result<Info, Error> {
    if !bytes.starts_with(&SIG) {
        return Err(Error::NotImage);
    }
    // подпись(8) + длина(4) + тип(4) + IHDR(13) + CRC(4)
    if bytes.len() < 33 {
        return Err(Error::Truncated);
    }
    if be32(&bytes[8..]) != 13 || &bytes[12..16] != b"IHDR" {
        return Err(Error::Header);
    }
    let d = &bytes[16..29];
    let info = Info { w: be32(d), h: be32(&d[4..]), depth: d[8], color: d[9], interlace: d[12] };
    if info.w == 0 || info.h == 0 || info.w > i32::MAX as u32 || info.h > i32::MAX as u32 {
        return Err(Error::Header);
    }
    // Метод сжатия и метод фильтрации в стандарте ровно по одному, и других не будет: место
    // под них оставлено в 1996-м и с тех пор не понадобилось.
    if d[10] != 0 || d[11] != 0 {
        return Err(Error::Unsupported("иной метод сжатия/фильтрации"));
    }
    if info.interlace > 1 {
        return Err(Error::Unsupported("неизвестная схема чересстрочности"));
    }
    // Глубина разрешена не любая: у цвета и альфы нет упакованных бит, у палитры нет 16.
    let ok = match info.color {
        0 => matches!(info.depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(info.depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(info.depth, 8 | 16),
        _ => return Err(Error::Header),
    };
    if !ok {
        return Err(Error::Header);
    }
    Ok(info)
}

/// Размеры одного прохода Adam7 в пикселях. Ноль по любой стороне значит «этого прохода в файле
/// нет вовсе» — для узких картинок обычное дело.
fn pass_size(info: &Info, p: usize) -> (u32, u32) {
    let (x0, y0, dx, dy) = PASS[p];
    let w = if info.w > x0 { (info.w - x0).div_ceil(dx) } else { 0 };
    let h = if info.h > y0 { (info.h - y0).div_ceil(dy) } else { 0 };
    (w, h)
}

/// Распаковать PNG в RGBA8888.
///
/// 16-битные сэмплы ужимаются до 8 бит взятием старшего байта. Это потеря, и она осознанная:
/// экран у нас 8-битный, а честное преобразование `v * 255 / 65535` отличается от `v >> 8`
/// меньше чем на единицу младшего разряда.
pub fn decode(bytes: &[u8], max_pixels: u64) -> Result<Image, Error> {
    let hdr = info(bytes)?;
    if hdr.w as u64 * hdr.h as u64 > max_pixels {
        return Err(Error::TooBig);
    }

    let mut pal: Vec<[u8; 4]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut z: Vec<u8> = Vec::new();
    let mut seen_idat = false;

    let mut p = 8;
    loop {
        if p + 12 > bytes.len() {
            return Err(Error::Truncated);
        }
        let len = be32(&bytes[p..]) as usize;
        if p + 12 + len > bytes.len() {
            return Err(Error::Truncated);
        }
        let ty: [u8; 4] = bytes[p + 4..p + 8].try_into().unwrap();
        let data = &bytes[p + 8..p + 8 + len];
        // CRC считаем ВСЕГДА. Он покрывает тип и данные (не длину) и стоит на порядок дешевле
        // самой распаковки; молча декодировать битый файл — худший из возможных исходов, потому
        // что результат выглядит как картинка.
        if crc32(&bytes[p + 4..p + 8 + len]) != be32(&bytes[p + 8 + len..]) {
            return Err(Error::Checksum);
        }
        match &ty {
            b"IHDR" => {}
            b"PLTE" => {
                if len % 3 != 0 || len > 256 * 3 {
                    return Err(Error::Corrupt("палитра не кратна трём"));
                }
                pal.try_reserve_exact(len / 3).map_err(|_| Error::NoMemory)?;
                for c in data.chunks_exact(3) {
                    pal.push([c[0], c[1], c[2], 0xff]);
                }
            }
            b"tRNS" => {
                trns.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
                trns.extend_from_slice(data);
            }
            b"IDAT" => {
                seen_idat = true;
                z.try_reserve(len).map_err(|_| Error::NoMemory)?;
                z.extend_from_slice(data);
            }
            // Кадры APNG (`fcTL`/`fdAT`) сюда не попадают: `IDAT` в APNG — первый кадр, и он
            // же обычная картинка для тех, кто анимации не знает. Ровно так мы себя и ведём.
            b"IEND" => break,
            _ => {}
        }
        p += 12 + len;
    }
    if !seen_idat {
        return Err(Error::Corrupt("нет ни одного IDAT"));
    }

    // Прозрачность приезжает отдельным чанком и означает разное в зависимости от вида пикселя:
    // для палитры это альфа по индексам, для серого и RGB — ОДИН прозрачный цвет, заданный в
    // исходной глубине (сравнивать надо сырые сэмплы, до ужимания до 8 бит).
    let mut key: Option<[u16; 3]> = None;
    match hdr.color {
        3 => {
            if pal.is_empty() {
                return Err(Error::Corrupt("палитровый PNG без PLTE"));
            }
            for (i, &a) in trns.iter().enumerate() {
                if let Some(e) = pal.get_mut(i) {
                    e[3] = a;
                }
            }
        }
        0 if trns.len() >= 2 => {
            let v = u16::from_be_bytes([trns[0], trns[1]]);
            key = Some([v, v, v]);
        }
        2 if trns.len() >= 6 => {
            key = Some([
                u16::from_be_bytes([trns[0], trns[1]]),
                u16::from_be_bytes([trns[2], trns[3]]),
                u16::from_be_bytes([trns[4], trns[5]]),
            ]);
        }
        _ => {}
    }

    // Сколько байт обязано получиться из inflate — считаем ТОЧНО и передаём как потолок.
    // Это и защита от «архивной бомбы» (сжатый мусор, разворачивающийся в гигабайты), и
    // проверка целостности: несовпадение длины значит, что файл не тот, за кого себя выдаёт.
    let raw_len = if hdr.interlace == 0 {
        hdr.h as usize * (1 + hdr.stride(hdr.w))
    } else {
        (0..7)
            .map(|i| {
                let (pw, ph) = pass_size(&hdr, i);
                if pw == 0 || ph == 0 {
                    0
                } else {
                    ph as usize * (1 + hdr.stride(pw))
                }
            })
            .sum()
    };
    let mut raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&z, raw_len)
        .map_err(|_| Error::Deflate)?;
    if raw.len() != raw_len {
        return Err(Error::Corrupt("строк меньше, чем обещал заголовок"));
    }
    drop(z);

    let mut img = Image::new(hdr.w, hdr.h)?;
    if hdr.interlace == 0 {
        pass(&hdr, &pal, key, &mut raw, hdr.w, hdr.h, (0, 0, 1, 1), &mut img)?;
    } else {
        let mut off = 0;
        for i in 0..7 {
            let (pw, ph) = pass_size(&hdr, i);
            if pw == 0 || ph == 0 {
                continue;
            }
            let n = ph as usize * (1 + hdr.stride(pw));
            let (x0, y0, dx, dy) = PASS[i];
            pass(&hdr, &pal, key, &mut raw[off..off + n], pw, ph, (x0, y0, dx, dy), &mut img)?;
            off += n;
        }
    }
    Ok(img)
}

/// Один проход (для нечересстрочной картинки он единственный): снять фильтры и разложить
/// пиксели по решётке `x0 + i*dx`, `y0 + j*dy`.
fn pass(
    hdr: &Info,
    pal: &[[u8; 4]],
    key: Option<[u16; 3]>,
    raw: &mut [u8],
    pw: u32,
    ph: u32,
    grid: (u32, u32, u32, u32),
    img: &mut Image,
) -> Result<(), Error> {
    let stride = hdr.stride(pw);
    unfilter(raw, stride, hdr.bpp(), ph as usize)?;
    let (x0, y0, dx, dy) = grid;
    for j in 0..ph {
        let row = &raw[j as usize * (stride + 1) + 1..][..stride];
        expand(hdr, pal, key, row, img, y0 + j * dy, x0, dx, pw)?;
    }
    Ok(())
}

/// Предсказатель Paeth: из трёх соседей берётся тот, к которому ближе их линейная комбинация.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Снять фильтры строк ПРЯМО В БУФЕРЕ.
///
/// Каждая строка занимает `1 + stride` байт: тип фильтра и данные. Восстановление опирается на
/// уже восстановленных соседей — левого (`a`, на `bpp` байт назад), верхнего (`b`) и
/// верхне-левого (`c`), — поэтому идти можно только сверху вниз и слева направо, и никакой
/// копии не нужно. Тип фильтра разбирается ОДИН раз на строку, а не на байт: внутренний цикл
/// тут исполняется миллионы раз (для 1920×1080 RGBA — восемь миллионов).
fn unfilter(raw: &mut [u8], stride: usize, bpp: usize, rows: usize) -> Result<(), Error> {
    for y in 0..rows {
        let start = y * (stride + 1);
        let ft = raw[start];
        let o = start + 1;
        let up = o.wrapping_sub(stride + 1);
        match ft {
            // None — данные уже готовы.
            0 => {}
            // Sub: слева. Первые bpp байт остаются как есть — слева ничего нет.
            1 => {
                for i in bpp..stride {
                    raw[o + i] = raw[o + i].wrapping_add(raw[o + i - bpp]);
                }
            }
            // Up: сверху. У первой строки верх — нули, то есть делать нечего.
            2 => {
                if y > 0 {
                    for i in 0..stride {
                        raw[o + i] = raw[o + i].wrapping_add(raw[up + i]);
                    }
                }
            }
            // Average: полусумма левого и верхнего, БЕЗ переполнения (складывать надо в u16).
            3 => {
                for i in 0..stride {
                    let a = if i >= bpp { raw[o + i - bpp] as u16 } else { 0 };
                    let b = if y > 0 { raw[up + i] as u16 } else { 0 };
                    raw[o + i] = raw[o + i].wrapping_add(((a + b) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= bpp { raw[o + i - bpp] } else { 0 };
                    let b = if y > 0 { raw[up + i] } else { 0 };
                    let c = if y > 0 && i >= bpp { raw[up + i - bpp] } else { 0 };
                    raw[o + i] = raw[o + i].wrapping_add(paeth(a, b, c));
                }
            }
            _ => return Err(Error::Corrupt("неизвестный фильтр строки")),
        }
    }
    Ok(())
}

/// Сэмпл номер `idx` в строке. При глубине меньше 8 сэмплы упакованы в байт СТАРШИМИ БИТАМИ
/// ВПЕРЁД — порядок, обратный привычному «младший бит первый», и перепутать его легко.
#[inline]
fn sample(row: &[u8], depth: u8, idx: usize) -> u16 {
    match depth {
        8 => row[idx] as u16,
        16 => u16::from_be_bytes([row[idx * 2], row[idx * 2 + 1]]),
        d => {
            let per = 8 / d as usize;
            let shift = 8 - d as usize * (idx % per + 1);
            ((row[idx / per] >> shift) & ((1 << d) - 1)) as u16
        }
    }
}

/// Сырой сэмпл → байт. Для 1/2/4 бит это не сдвиг, а растяжение на весь диапазон: белое в
/// однобитной картинке должно стать 255, а не 1.
#[inline]
fn scale(v: u16, depth: u8) -> u8 {
    match depth {
        16 => (v >> 8) as u8,
        8 => v as u8,
        4 => (v * 17) as u8,
        2 => (v * 85) as u8,
        _ => {
            if v != 0 {
                0xff
            } else {
                0
            }
        }
    }
}

/// Разложить одну строку прохода в кадр RGBA.
fn expand(
    hdr: &Info,
    pal: &[[u8; 4]],
    key: Option<[u16; 3]>,
    row: &[u8],
    img: &mut Image,
    y: u32,
    x0: u32,
    dx: u32,
    count: u32,
) -> Result<(), Error> {
    let base = y as usize * img.w as usize * 4;
    // Быстрый путь ровно для того случая, которым записан почти каждый снимок экрана: RGBA8
    // без чересстрочности. Строка уже в нужном виде — остаётся скопировать.
    if hdr.color == 6 && hdr.depth == 8 && dx == 1 && key.is_none() {
        let n = count as usize * 4;
        img.px[base + x0 as usize * 4..][..n].copy_from_slice(&row[..n]);
        return Ok(());
    }
    let ch = hdr.channels();
    for i in 0..count as usize {
        let d = base + (x0 as usize + i * dx as usize) * 4;
        let px: [u8; 4] = match hdr.color {
            0 => {
                let v = sample(row, hdr.depth, i);
                let g = scale(v, hdr.depth);
                [g, g, g, if key == Some([v, v, v]) { 0 } else { 0xff }]
            }
            2 => {
                let (r, g, b) = (
                    sample(row, hdr.depth, i * ch),
                    sample(row, hdr.depth, i * ch + 1),
                    sample(row, hdr.depth, i * ch + 2),
                );
                let a = if key == Some([r, g, b]) { 0 } else { 0xff };
                [scale(r, hdr.depth), scale(g, hdr.depth), scale(b, hdr.depth), a]
            }
            3 => {
                let idx = sample(row, hdr.depth, i) as usize;
                *pal.get(idx).ok_or(Error::Corrupt("индекс вне палитры"))?
            }
            4 => {
                let g = scale(sample(row, hdr.depth, i * ch), hdr.depth);
                [g, g, g, scale(sample(row, hdr.depth, i * ch + 1), hdr.depth)]
            }
            _ => [
                scale(sample(row, hdr.depth, i * ch), hdr.depth),
                scale(sample(row, hdr.depth, i * ch + 1), hdr.depth),
                scale(sample(row, hdr.depth, i * ch + 2), hdr.depth),
                scale(sample(row, hdr.depth, i * ch + 3), hdr.depth),
            ],
        };
        img.px[d..d + 4].copy_from_slice(&px);
    }
    Ok(())
}
