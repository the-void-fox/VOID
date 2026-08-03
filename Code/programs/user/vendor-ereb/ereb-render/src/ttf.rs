//! [`Rasterizer`] на чистом Rust (за фичей `ttf`) — [ADR 0006].
//!
//! Зеркало [`crate::ft`], только без C: разбор шрифта, метрики и контуры даёт `ttf-parser`,
//! контуры в покрытие переводит `ab_glyph_rasterizer` (оба — под зонтиком `ab_glyph`).
//!
//! Зачем, если FreeType работает: у **VOID** нет libc, и FreeType там не соберётся в принципе,
//! а глифы Nerd Font — единственная причина, по которой ereb туда едет ([ADR 0005]). Значит
//! выбор был между «портировать libc ради одного шрифта» и «растеризовать самим». Побочно это
//! убирает C-зависимость и на Linux — то, чего просят принципы проекта.
//!
//! **Чего здесь нет и не будет: хинтинга.** FreeType с автохинтером на мелких кеглях
//! заметно чётче. Это принятая цена, а не недоделка.
//!
//! Начертания (bold/italic) пока отображаются как regular — ровно как в [`crate::ft`];
//! отдельные faces появятся вместе с ними.
//!
//! [ADR 0005]: ../../../obsidian/02-architecture/adr/0005-void-target.md
//! [ADR 0006]: ../../../obsidian/02-architecture/adr/0006-pure-rust-rasterizer.md

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
// `f32::round` — метод std; в no_std его добавляет это трейт-расширение (см. Cargo.toml).
#[allow(unused_imports)]
use core_maths::CoreFloat;

use crate::font::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle};

/// Ошибка загрузки шрифта.
#[derive(Debug)]
pub enum TtfError {
    /// Файл не читается. Только под `std`: в VOID файлов нет, шрифт приходит байтами.
    #[cfg(feature = "std")]
    Io(std::io::Error),
    /// Данные не разбираются как шрифт.
    Invalid,
}

impl fmt::Display for TtfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "std")]
            TtfError::Io(e) => write!(f, "не читается файл шрифта: {e}"),
            TtfError::Invalid => write!(f, "данные не разбираются как шрифт"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TtfError {}

#[cfg(feature = "std")]
impl From<std::io::Error> for TtfError {
    fn from(e: std::io::Error) -> Self {
        TtfError::Io(e)
    }
}

/// Шрифт, растеризующий глифы без C-кода.
pub struct TtfFont {
    font: FontVec,
    /// Масштаб в терминах `ab_glyph` — **не равен кеглю в пикселях**, см. [`px_scale_for`].
    scale: PxScale,
    metrics: CellMetrics,
}

/// Перевести кегль «высота em-квадрата в пикселях» (семантика `FT_Set_Pixel_Sizes(0, N)`)
/// в [`PxScale`] крейта `ab_glyph`.
///
/// Ловушка, на которой сверка с FreeType и поймала расхождение: у `ab_glyph` `PxScale` меряется
/// **в высоте строки** (ascender − descender + line_gap), а не в em. У FiraCode это отношение
/// ≈ 1.18, поэтому дословная передача кегля давала глифы на ~15 % мельче, чем у FreeType, —
/// систематически и во всём наборе. Тесты этого не видят: сами по себе глифы корректны.
fn px_scale_for(font: &FontVec, pixel_height: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(1.0);
    PxScale::from(pixel_height * font.height_unscaled() / upem)
}

impl TtfFont {
    /// Загружает шрифт из файла и фиксирует кегль `pixel_height` (px).
    #[cfg(feature = "std")]
    pub fn new(path: impl AsRef<std::path::Path>, pixel_height: u32) -> Result<Self, TtfError> {
        Self::from_vec(std::fs::read(path)?, pixel_height)
    }

    /// То же из уже прочитанных байт. Отдельный конструктор нужен не для удобства: **в VOID
    /// файловых путей нет**, шрифт приезжает объектом из стора — и туда поедет именно этот вход.
    pub fn from_vec(data: Vec<u8>, pixel_height: u32) -> Result<Self, TtfError> {
        let font = FontVec::try_from_vec(data).map_err(|_| TtfError::Invalid)?;
        let scale = px_scale_for(&font, pixel_height.max(1) as f32);
        let metrics = compute_metrics(&font, scale, pixel_height);
        Ok(TtfFont {
            font,
            scale,
            metrics,
        })
    }
}

/// Метрики ячейки. Считаются по тем же величинам, что берёт FreeType из `size_metrics`,
/// иначе два растеризатора разъехались бы по геометрии грида, а не только по виду глифов:
/// `ascent` — от верха ячейки до базовой линии, `height` — шаг строки (ascender − descender +
/// line_gap), ширина — advance репрезентативного глифа.
fn compute_metrics(font: &FontVec, scale: PxScale, pixel_height: u32) -> CellMetrics {
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent().round() as i32;
    let height = scaled.height().round().max(1.0) as u32;
    let width = monospace_width(&scaled)
        .filter(|&w| w > 0)
        .unwrap_or(pixel_height / 2)
        .max(1);

    // Толщина и положение подчёркивания — как в ft.rs: у шрифта эти метрики есть не всегда,
    // а расхождение здесь сразу видно глазом на подчёркнутом тексте.
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

/// Ширина моноширинной ячейки = advance репрезентативного глифа (тот же 'M', что в `ft.rs`).
fn monospace_width<F: Font>(scaled: &impl ScaleFont<F>) -> Option<u32> {
    let advance = scaled.h_advance(scaled.glyph_id('M'));
    (advance > 0.0).then_some(advance.round() as u32)
}

impl Rasterizer for TtfFont {
    fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    fn rasterize(&mut self, ch: char, _style: RenderStyle) -> RasterizedGlyph {
        let scaled = self.font.as_scaled(self.scale);
        let id = scaled.glyph_id(ch);
        let advance = scaled.h_advance(id);

        // Нет контура — это НЕ ошибка: пробел и .notdef приходят сюда штатно. FreeType в том же
        // случае отдаёт пустой bitmap, поэтому и мы отдаём пустой глиф с верным advance.
        let Some(outlined) = self.font.outline_glyph(id.with_scale(self.scale)) else {
            return RasterizedGlyph::blank(advance);
        };

        // px_bounds — в пикселях относительно пера, ось Y вниз, базовая линия = 0. Значит верх
        // глифа лежит в отрицательном Y, и `bearing_y` FreeType (вверх от базовой линии) — это
        // ровно `-min.y`.
        let bounds = outlined.px_bounds();
        let width = bounds.width() as u32;
        let height = bounds.height() as u32;
        let mut bitmap = vec![0u8; (width * height) as usize];
        outlined.draw(|x, y, coverage| {
            if x < width && y < height {
                bitmap[(y * width + x) as usize] = (coverage * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
            }
        });

        RasterizedGlyph {
            bitmap,
            width,
            height,
            bearing_x: bounds.min.x as i32,
            bearing_y: -bounds.min.y as i32,
            advance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Интеграционный тест с реальным шрифтом — парный к тесту в `ft.rs`:
    /// `EREB_TEST_FONT=/path/to/Mono.ttf cargo test -p ereb-render --features ttf -- --ignored`
    #[test]
    #[ignore = "требует EREB_TEST_FONT с путём к моноширинному шрифту"]
    fn rasterizes_real_glyph() {
        let path = std::env::var("EREB_TEST_FONT").expect("set EREB_TEST_FONT");
        let mut font = TtfFont::new(&path, 16).expect("load font");

        let m = font.metrics();
        assert!(m.width > 0 && m.height > 0, "metrics: {m:?}");

        let glyph = font.rasterize('A', RenderStyle::Regular);
        assert_eq!(glyph.bitmap.len(), (glyph.width * glyph.height) as usize);
        assert!(
            glyph.bitmap.iter().any(|&c| c > 0),
            "'A' должна иметь покрытие"
        );

        // Пробел — пустой глиф, но advance у него настоящий (иначе поедет весь грид).
        let space = font.rasterize(' ', RenderStyle::Regular);
        assert!(space.bitmap.is_empty());
        assert!(space.advance > 0.0, "у пробела обязан быть advance");
    }

    /// Битые данные не должны паниковать — только ошибка.
    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            TtfFont::from_vec(vec![0u8; 64], 16),
            Err(TtfError::Invalid)
        ));
    }
}
