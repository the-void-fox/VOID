//! Сверка растеризаторов: FreeType против чистого Rust ([ADR 0006]).
//!
//! Рисует один и тот же текст обоими и выкладывает результат кадрами PPM плюс числовую
//! сводку по метрикам и покрытию. Нужен именно ГЛАЗАМИ: расхождение в позиции полей R/G/B
//! или в bearing тесты пройдут, а картинка — нет (на VOID так и вышло, см. Веху 96).
//!
//! ```text
//! cargo run -p ereb-render --features "freetype ttf" --example rasterizer_compare -- \
//!     /path/to/Mono.ttf [кегль] [каталог-вывода]
//! ```
//!
//! [ADR 0006]: ../../../obsidian/02-architecture/adr/0006-pure-rust-rasterizer.md

use std::io::Write;

use ereb_core::Grid;
use ereb_render::{
    FtFont, GlyphCache, GridRenderer, Palette, Rasterizer, RenderStyle, Surface, TtfFont,
};

/// Что рисуем: латиница, кириллица, псевдографика и глифы Nerd Font из Private Use Area —
/// ровно то, ради чего всё затевалось.
const SAMPLE: &[&str] = &[
    "ABCDEFGHIJKLM abcdefghijklm 0123456789",
    "Кириллица: съешь ещё этих мягких булок",
    "рамки: ┌─┬─┐ ├─┼─┤ └─┴─┘ │ █ ▓ ▒ ░",
    "Nerd Font: \u{e0b0}\u{e0b2} \u{f07b}\u{f15b}\u{f121} \u{f09b}\u{e795}\u{f0e7}",
    "код: fn main() { let x = 1 + 2; } // <= != ->",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("нужен путь к шрифту")?;
    let px: u32 = args.next().unwrap_or_else(|| "18".into()).parse()?;
    let outdir = args.next().unwrap_or_else(|| ".".into());

    let mut ft = FtFont::new(&path, px)?;
    let mut ttf = TtfFont::new(&path, px)?;

    println!("шрифт: {path}, кегль {px} px\n");
    let (mft, mttf) = (ft.metrics(), ttf.metrics());
    println!("метрики ячейки          FreeType   чистый Rust   расхождение");
    let rows: [(&str, i64, i64); 5] = [
        ("ширина", mft.width as i64, mttf.width as i64),
        ("высота", mft.height as i64, mttf.height as i64),
        ("базовая линия", mft.ascent as i64, mttf.ascent as i64),
        ("подчёркивание Y", mft.underline_y as i64, mttf.underline_y as i64),
        (
            "толщина линии",
            mft.underline_thickness as i64,
            mttf.underline_thickness as i64,
        ),
    ];
    for (name, a, b) in rows {
        println!("  {name:<20} {a:>8} {b:>13} {:>13}", b - a);
    }

    // Поглифовая сводка: совпадает ли размер пятна и насколько разошлось покрытие.
    println!("\nглифы (размер bitmap, bearing, средняя |Δ| покрытия там, где размеры совпали):");
    let mut worst: Vec<(f64, char)> = Vec::new();
    let mut missing_ft = 0usize;
    let mut missing_ttf = 0usize;
    for ch in SAMPLE.iter().flat_map(|s| s.chars()).filter(|c| *c != ' ') {
        let a = ft.rasterize(ch, RenderStyle::Regular);
        let b = ttf.rasterize(ch, RenderStyle::Regular);
        if a.bitmap.is_empty() && !b.bitmap.is_empty() {
            missing_ft += 1;
        }
        if b.bitmap.is_empty() && !a.bitmap.is_empty() {
            missing_ttf += 1;
        }
        if a.width == b.width && a.height == b.height && !a.bitmap.is_empty() {
            let diff: u64 = a
                .bitmap
                .iter()
                .zip(&b.bitmap)
                .map(|(x, y)| x.abs_diff(*y) as u64)
                .sum();
            worst.push((diff as f64 / a.bitmap.len() as f64, ch));
        } else if !a.bitmap.is_empty() || !b.bitmap.is_empty() {
            println!(
                "  {ch:?}: размеры РАЗНЫЕ — ft {}×{} bearing({},{}) · rust {}×{} bearing({},{})",
                a.width, a.height, a.bearing_x, a.bearing_y, b.width, b.height, b.bearing_x, b.bearing_y
            );
        }
    }
    worst.sort_by(|x, y| y.0.total_cmp(&x.0));
    let n = worst.len();
    let mean = worst.iter().map(|(d, _)| d).sum::<f64>() / n.max(1) as f64;
    println!("  совпали по размеру: {n} глифов, средняя |Δ| покрытия {mean:.1} из 255");
    println!("  худшие: {:?}", &worst[..worst.len().min(5)]);
    if missing_ft + missing_ttf > 0 {
        println!("  ПУСТЫХ: у FreeType {missing_ft}, у чистого Rust {missing_ttf}");
    }

    // Кэш глифов забирает растеризатор себе, поэтому шрифты уходят сюда по значению — это
    // последнее их использование.
    for (name, surface) in [("ft", paint(ft)?), ("rust", paint(ttf)?)] {
        let file = format!("{outdir}/raster-{name}.ppm");
        write_ppm(&file, &surface)?;
        println!("кадр → {file}");
    }
    Ok(())
}

/// Нарисовать [`SAMPLE`] обычным конвейером рендера (грид → пиксели).
fn paint<R: Rasterizer>(font: R) -> Result<Surface, Box<dyn std::error::Error>> {
    let mut cache = GlyphCache::new(font);
    let renderer = GridRenderer::new(Palette::default(), cache.metrics());
    let (cols, rows) = (SAMPLE.iter().map(|s| s.chars().count()).max().unwrap_or(1) + 2, SAMPLE.len() + 2);
    let mut grid = Grid::new(cols, rows);
    let mut parser = vte::Parser::new();
    for line in SAMPLE {
        parser.advance(&mut grid, format!("{line}\r\n").as_bytes());
    }
    let (w, h) = renderer.pixel_size(cols, rows);
    let mut surface = Surface::new(w, h, Palette::default().background);
    renderer.paint(&grid, &mut cache, &mut surface);
    Ok(surface)
}

/// PPM (P6) — без внешних крейтов: сравнивать всё равно глазами, а PNG потребовал бы zlib.
fn write_ppm(path: &str, s: &Surface) -> std::io::Result<()> {
    let (w, h) = (s.width(), s.height());
    let mut out = Vec::with_capacity((w * h * 3) as usize + 32);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in s.data().chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    std::fs::File::create(path)?.write_all(&out)
}
