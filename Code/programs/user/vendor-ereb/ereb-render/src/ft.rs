//! FreeType-backed [`Rasterizer`] (за фичей `freetype`).
//!
//! Один моноширинный face на кегль. Bold/italic на MVP отображаются как regular
//! (несколько начертаний — задача Этапа 17); ключ кэша уже учитывает стиль,
//! поэтому добавить отдельные faces позже — тривиально.

use std::path::Path;

use freetype::face::LoadFlag;
use freetype::{Face, Library};

use crate::font::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle};

/// Шрифт, растеризующий глифы через FreeType.
pub struct FtFont {
    // Library должен жить не меньше, чем Face.
    _library: Library,
    face: Face,
    metrics: CellMetrics,
}

impl FtFont {
    /// Загружает шрифт из файла и фиксирует кегль `pixel_height` (px).
    pub fn new(path: impl AsRef<Path>, pixel_height: u32) -> Result<Self, freetype::Error> {
        let library = Library::init()?;
        let face = library.new_face(path.as_ref(), 0)?;
        face.set_pixel_sizes(0, pixel_height)?;
        let metrics = compute_metrics(&face, pixel_height);
        Ok(FtFont {
            _library: library,
            face,
            metrics,
        })
    }
}

/// Считает метрики ячейки из face после установки кегля.
fn compute_metrics(face: &Face, pixel_height: u32) -> CellMetrics {
    // size_metrics в формате 26.6 fixed-point — сдвигаем на 6 для пикселей.
    let sm = face.size_metrics();
    let ascent = sm
        .map(|m| (m.ascender >> 6) as i32)
        .unwrap_or(pixel_height as i32);
    let height = sm
        .map(|m| (m.height >> 6) as u32)
        .filter(|&h| h > 0)
        .unwrap_or(pixel_height)
        .max(1);
    let width = monospace_width(face)
        .or_else(|| sm.map(|m| (m.max_advance >> 6) as u32))
        .filter(|&w| w > 0)
        .unwrap_or(pixel_height / 2)
        .max(1);

    let thickness = (pixel_height / 16).max(1);
    let underline_y = (ascent as u32 + thickness).min(height.saturating_sub(thickness));

    CellMetrics {
        width,
        height,
        ascent,
        underline_y,
        underline_thickness: thickness,
    }
}

/// Ширина моноширинной ячейки = advance репрезентативного глифа.
fn monospace_width(face: &Face) -> Option<u32> {
    face.load_char('M' as usize, LoadFlag::DEFAULT).ok()?;
    let advance = face.glyph().advance().x >> 6;
    (advance > 0).then_some(advance as u32)
}

impl Rasterizer for FtFont {
    fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    fn rasterize(&mut self, ch: char, _style: RenderStyle) -> RasterizedGlyph {
        if self.face.load_char(ch as usize, LoadFlag::RENDER).is_err() {
            return RasterizedGlyph::blank(self.metrics.width as f32);
        }
        let slot = self.face.glyph();
        let bitmap = slot.bitmap();
        let width = bitmap.width().max(0) as u32;
        let rows = bitmap.rows().max(0) as u32;
        let pitch = bitmap.pitch();
        let buffer = bitmap.buffer();

        // FreeType хранит строки с шагом `pitch` (может быть > width и/или
        // отрицательным для bottom-up). Перепаковываем в плотный width×rows.
        let mut packed = vec![0u8; (width * rows) as usize];
        if width > 0 && rows > 0 {
            let pitch_abs = pitch.unsigned_abs() as usize;
            let w = width as usize;
            for row in 0..rows as usize {
                let src_row = if pitch >= 0 {
                    row
                } else {
                    rows as usize - 1 - row
                };
                let src = src_row * pitch_abs;
                let dst = row * w;
                packed[dst..dst + w].copy_from_slice(&buffer[src..src + w]);
            }
        }

        RasterizedGlyph {
            bitmap: packed,
            width,
            height: rows,
            bearing_x: slot.bitmap_left(),
            bearing_y: slot.bitmap_top(),
            advance: (slot.advance().x >> 6) as f32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Интеграционный тест с реальным шрифтом. По умолчанию `#[ignore]` —
    /// запускать с путём к шрифту:
    /// `EREB_TEST_FONT=/path/to/Mono.ttf cargo test -p ereb-render \
    ///     --features freetype -- --ignored`
    #[test]
    #[ignore = "требует EREB_TEST_FONT с путём к моноширинному шрифту"]
    fn rasterizes_real_glyph() {
        let path = std::env::var("EREB_TEST_FONT").expect("set EREB_TEST_FONT");
        let mut font = FtFont::new(&path, 16).expect("load font");

        let m = font.metrics();
        assert!(m.width > 0 && m.height > 0, "metrics: {m:?}");

        let glyph = font.rasterize('A', RenderStyle::Regular);
        assert_eq!(glyph.bitmap.len(), (glyph.width * glyph.height) as usize);
        assert!(
            glyph.bitmap.iter().any(|&c| c > 0),
            "'A' должна иметь покрытие"
        );

        // Пробел — пустой глиф.
        assert!(font.rasterize(' ', RenderStyle::Regular).bitmap.is_empty());
    }
}
