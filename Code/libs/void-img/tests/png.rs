//! PNG: сверка с эталоном по всем разновидностям формата.
//!
//! Смысл теста — независимость. Картинки кодирует крейт `png` от image-rs, он же их и
//! разбирает; наш декодер обязан прийти к тем же пикселям байт в байт. Совпасть по ошибке две
//! независимые реализации не могут, а ошибка в фильтре или в распаковке бит даёт не отказ, а
//! правдоподобно выглядящий мусор — заметить его глазами по снимку в QEMU нельзя.
//!
//! Чересстрочные картинки (Adam7) крейт `png` писать не умеет — их собирает [`adam7`] прямо
//! здесь: заголовок, семь проходов, фильтр «никакой», zlib. Проверяет их всё равно эталон.

use png::{BitDepth, ColorType, Filter, Transformations};
use void_img::{decode, probe, Error, Format};

/// Потолок для тестов: картинки тут крошечные, потолок проверяется отдельно.
const MAX: u64 = 1 << 30;

/// Эталонное разложение в RGBA8: `png` с преобразованиями «развернуть всё» и «ужать 16 бит».
fn reference(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let mut reader = dec.read_info().expect("эталон не читает заголовок");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("нет размера буфера")];
    let info = reader.next_frame(&mut buf).expect("эталон не читает кадр");
    assert_eq!(info.bit_depth, BitDepth::Eight, "STRIP_16 не сработал");
    let src = &buf[..info.buffer_size()];
    let mut px = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    match info.color_type {
        ColorType::Grayscale => src.iter().for_each(|&g| px.extend_from_slice(&[g, g, g, 0xff])),
        ColorType::GrayscaleAlpha => {
            src.chunks_exact(2).for_each(|c| px.extend_from_slice(&[c[0], c[0], c[0], c[1]]))
        }
        ColorType::Rgb => {
            src.chunks_exact(3).for_each(|c| px.extend_from_slice(&[c[0], c[1], c[2], 0xff]))
        }
        ColorType::Rgba => px.extend_from_slice(src),
        ColorType::Indexed => panic!("EXPAND обязан был развернуть палитру"),
    }
    (info.width, info.height, px)
}

/// Псевдослучайные байты: фильтрам нужно, что предсказывать, а константная заливка прячет
/// ошибки в них (у ровного фона все предсказатели дают ноль).
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 24) as u8
        })
        .collect()
}

fn stride(w: u32, ch: usize, depth: u8) -> usize {
    (w as usize * ch * depth as usize).div_ceil(8)
}

struct Case {
    color: ColorType,
    depth: BitDepth,
    palette: bool,
    trns: Option<Vec<u8>>,
}

fn encode(c: &Case, w: u32, h: u32, data: &[u8], filter: Filter) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(c.color);
        enc.set_depth(c.depth);
        enc.set_filter(filter);
        if c.palette {
            // Палитра на всю глубину: тогда любой индекс в шуме заведомо законен.
            let n = 1usize << (c.depth as u8);
            enc.set_palette(noise(n * 3, 7));
        }
        if let Some(t) = &c.trns {
            enc.set_trns(t.clone());
        }
        let mut wr = enc.write_header().expect("эталон не пишет заголовок");
        wr.write_image_data(data).expect("эталон не пишет пиксели");
    }
    out
}

/// Все разновидности формата × все фильтры строк. Размер намеренно нечётный: 23 пикселя при
/// глубине 1/2/4 не делятся на байт, и последний байт строки оказывается заполнен частично —
/// ровно там ошибаются в распаковке бит.
#[test]
fn every_color_type_and_filter() {
    let cases = [
        Case { color: ColorType::Grayscale, depth: BitDepth::One, palette: false, trns: None },
        Case { color: ColorType::Grayscale, depth: BitDepth::Two, palette: false, trns: None },
        Case { color: ColorType::Grayscale, depth: BitDepth::Four, palette: false, trns: None },
        Case { color: ColorType::Grayscale, depth: BitDepth::Eight, palette: false, trns: None },
        Case { color: ColorType::Grayscale, depth: BitDepth::Sixteen, palette: false, trns: None },
        Case { color: ColorType::Rgb, depth: BitDepth::Eight, palette: false, trns: None },
        Case { color: ColorType::Rgb, depth: BitDepth::Sixteen, palette: false, trns: None },
        Case { color: ColorType::GrayscaleAlpha, depth: BitDepth::Eight, palette: false, trns: None },
        Case {
            color: ColorType::GrayscaleAlpha,
            depth: BitDepth::Sixteen,
            palette: false,
            trns: None,
        },
        Case { color: ColorType::Rgba, depth: BitDepth::Eight, palette: false, trns: None },
        Case { color: ColorType::Rgba, depth: BitDepth::Sixteen, palette: false, trns: None },
        Case { color: ColorType::Indexed, depth: BitDepth::One, palette: true, trns: None },
        Case { color: ColorType::Indexed, depth: BitDepth::Two, palette: true, trns: None },
        Case { color: ColorType::Indexed, depth: BitDepth::Four, palette: true, trns: None },
        Case { color: ColorType::Indexed, depth: BitDepth::Eight, palette: true, trns: None },
        // Прозрачность трёх видов: альфа по индексам палитры, прозрачный СЕРЫЙ, прозрачный ЦВЕТ.
        Case {
            color: ColorType::Indexed,
            depth: BitDepth::Four,
            palette: true,
            trns: Some(vec![0, 40, 0xff, 200, 7, 0]),
        },
        Case {
            color: ColorType::Grayscale,
            depth: BitDepth::Eight,
            palette: false,
            trns: Some(vec![0, 0x42]),
        },
        Case {
            color: ColorType::Rgb,
            depth: BitDepth::Eight,
            palette: false,
            trns: Some(vec![0, 0x11, 0, 0x22, 0, 0x33]),
        },
        Case {
            color: ColorType::Grayscale,
            depth: BitDepth::Sixteen,
            palette: false,
            trns: Some(vec![0x12, 0x34]),
        },
    ];
    let filters =
        [Filter::NoFilter, Filter::Sub, Filter::Up, Filter::Avg, Filter::Paeth, Filter::Adaptive];
    let (w, h) = (23u32, 17u32);

    for (n, c) in cases.iter().enumerate() {
        let ch = c.color.samples();
        let data = noise(stride(w, ch, c.depth as u8) * h as usize, n as u32 + 1);
        for f in filters {
            let file = encode(c, w, h, &data, f);
            let (rw, rh, want) = reference(&file);
            let got = decode(&file, MAX).unwrap_or_else(|e| {
                panic!("{:?}/{:?} фильтр {f:?}: {e}", c.color, c.depth);
            });
            assert_eq!((got.w, got.h), (rw, rh), "{:?}/{:?}", c.color, c.depth);
            assert_eq!(
                got.px, want,
                "пиксели разошлись: {:?}/{:?} фильтр {f:?} tRNS={}",
                c.color,
                c.depth,
                c.trns.is_some()
            );
        }
    }
}

/// Собрать ЧЕРЕССТРОЧНЫЙ PNG (Adam7). `samples` — сэмплы построчно, `ch` на пиксель, значения в
/// исходной глубине; проходы упаковываются по правилам формата, фильтр всюду «никакой».
fn adam7(w: u32, h: u32, depth: u8, color: u8, samples: &[u16], palette: Option<Vec<u8>>) -> Vec<u8> {
    const PASS: [(u32, u32, u32, u32); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let ch = match color {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    let mut raw = Vec::new();
    for (x0, y0, dx, dy) in PASS {
        let (pw, ph) = (
            if w > x0 { (w - x0).div_ceil(dx) } else { 0 },
            if h > y0 { (h - y0).div_ceil(dy) } else { 0 },
        );
        if pw == 0 || ph == 0 {
            continue;
        }
        for j in 0..ph {
            raw.push(0u8); // тип фильтра
            let (mut acc, mut bits) = (0u32, 0u32);
            for i in 0..pw {
                let (x, y) = (x0 + i * dx, y0 + j * dy);
                for c in 0..ch {
                    let v = samples[(y as usize * w as usize + x as usize) * ch + c];
                    match depth {
                        16 => raw.extend_from_slice(&v.to_be_bytes()),
                        8 => raw.push(v as u8),
                        d => {
                            acc = (acc << d) | (v as u32 & ((1 << d) - 1));
                            bits += d as u32;
                            if bits == 8 {
                                raw.push(acc as u8);
                                acc = 0;
                                bits = 0;
                            }
                        }
                    }
                }
            }
            if bits > 0 {
                raw.push((acc << (8 - bits)) as u8); // хвост строки добивается до байта
            }
        }
    }

    let mut out = Vec::from(void_img_sig());
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[depth, color, 0, 0, 1]);
    chunk(&mut out, b"IHDR", &ihdr);
    if let Some(p) = palette {
        chunk(&mut out, b"PLTE", &p);
    }
    chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn void_img_sig() -> [u8; 8] {
    [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&out[start..]).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in bytes {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    c ^ 0xffff_ffff
}

/// Adam7 — четыре вида пикселя и три размера. Размеры не случайны: при ширине 3 у половины
/// проходов вовсе нет столбцов, а картинка 1×1 состоит из единственного пикселя первого прохода.
#[test]
fn interlaced() {
    for (w, h) in [(23u32, 17u32), (3, 5), (1, 1), (8, 8)] {
        for (depth, color, pal) in [
            (8u8, 6u8, false),
            (4, 0, false),
            (16, 2, false),
            (2, 3, true),
            (8, 4, false),
        ] {
            let ch = match color {
                0 | 3 => 1,
                2 => 3,
                4 => 2,
                _ => 4,
            };
            let max = if depth == 16 { 0xffffu32 } else { (1u32 << depth) - 1 };
            let n = w as usize * h as usize * ch;
            let samples: Vec<u16> =
                noise(n * 2, w * 31 + h + depth as u32).chunks_exact(2).map(|c| {
                    (u16::from_be_bytes([c[0], c[1]]) as u32 % (max + 1)) as u16
                }).collect();
            let palette = pal.then(|| noise(3 << depth, 5));
            let file = adam7(w, h, depth, color, &samples, palette);
            let (rw, rh, want) = reference(&file);
            let got = decode(&file, MAX).unwrap_or_else(|e| panic!("{w}×{h} тип {color}/{depth}: {e}"));
            assert_eq!((got.w, got.h), (rw, rh));
            assert_eq!(got.px, want, "чересстрочный {w}×{h}, тип {color}, глубина {depth}");
        }
    }
}

/// Заголовок читается без распаковки — и это отдельное обещание: по нему решают, браться ли.
#[test]
fn probe_reads_header_only() {
    let c = Case { color: ColorType::Rgba, depth: BitDepth::Eight, palette: false, trns: None };
    let file = encode(&c, 640, 480, &noise(640 * 480 * 4, 3), Filter::Adaptive);
    let p = probe(&file).unwrap();
    assert_eq!((p.format, p.w, p.h), (Format::Png, 640, 480));
    assert_eq!(p.bytes(), 640 * 480 * 4);
    // Обрезанный файл: заголовок цел, пикселей нет. Размер узнать можно, картинку — нет.
    let head = &file[..40];
    assert_eq!(probe(head).unwrap().w, 640);
    assert!(matches!(decode(head, MAX), Err(Error::Truncated)));
}

/// Отказы: каждый со своей причиной, и ни один не должен превращаться в панику.
#[test]
fn failures_are_honest() {
    let c = Case { color: ColorType::Rgba, depth: BitDepth::Eight, palette: false, trns: None };
    let good = encode(&c, 16, 16, &noise(16 * 16 * 4, 9), Filter::Paeth);

    assert!(matches!(decode(b"not an image at all", MAX), Err(Error::NotImage)));
    assert!(matches!(decode(&good, 16 * 16 - 1), Err(Error::TooBig)));

    // Порча одного байта внутри IDAT. Без сверки CRC это дало бы «картинку» — вот зачем она.
    let mut bad = good.clone();
    let n = bad.len();
    bad[n - 24] ^= 0x40;
    assert!(matches!(decode(&bad, MAX), Err(Error::Checksum)));

    // Глубина 3 бита формату неизвестна.
    let mut bad = good.clone();
    bad[24] = 3;
    let fixed = crc32(&bad[12..29]).to_be_bytes();
    bad[29..33].copy_from_slice(&fixed);
    assert!(matches!(decode(&bad, MAX), Err(Error::Header)));
}

/// Настоящие файлы: снимки экрана из `IMG/` этого же репозитория. Синтетика проверяет ширину
/// охвата, а эти — то, что пишут живые кодировщики (много IDAT, адаптивные фильтры, размер).
#[test]
fn real_screenshots() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../IMG");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("нет корпуса {}, пропускаю", dir.display());
        return;
    };
    let mut seen = 0;
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    files.sort();
    for path in files.iter().take(3) {
        let bytes = std::fs::read(path).unwrap();
        let (rw, rh, want) = reference(&bytes);
        let got = decode(&bytes, 64 << 20).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!((got.w, got.h), (rw, rh), "{}", path.display());
        assert!(got.px == want, "пиксели разошлись: {}", path.display());
        seen += 1;
    }
    assert!(seen > 0, "в корпусе не оказалось ни одного PNG");
}
