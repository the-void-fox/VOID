//! Абстракция растеризатора шрифта: [`Rasterizer`] + типы глифа и метрик.
//!
//! Реальную растеризацию даёт FreeType ([`crate::ft`], за фичей `freetype`).
//! Для тестов и бенчмарков без C-зависимости есть [`StubRasterizer`].

use alloc::vec;
use alloc::vec::Vec;

use ereb_core::CellFlags;

/// Начертание глифа, выбираемое по атрибутам ячейки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum RenderStyle {
    #[default]
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl RenderStyle {
    /// Выбирает начертание по флагам ячейки (учитывает только bold/italic).
    pub fn from_flags(flags: CellFlags) -> Self {
        match (
            flags.contains(CellFlags::BOLD),
            flags.contains(CellFlags::ITALIC),
        ) {
            (true, true) => RenderStyle::BoldItalic,
            (true, false) => RenderStyle::Bold,
            (false, true) => RenderStyle::Italic,
            (false, false) => RenderStyle::Regular,
        }
    }
}

/// Метрики моноширинной ячейки в пикселях.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellMetrics {
    /// Ширина ячейки (advance моноширинного глифа).
    pub width: u32,
    /// Высота строки (ячейки).
    pub height: u32,
    /// Базовая линия — смещение от верха ячейки вниз.
    pub ascent: i32,
    /// Положение подчёркивания — смещение от верха ячейки.
    pub underline_y: u32,
    /// Толщина линии подчёркивания.
    pub underline_thickness: u32,
}

/// Растеризованный глиф: grayscale-покрытие 8 bpp плюс позиционирование.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RasterizedGlyph {
    /// Покрытие, `width × height` байт (8-bpp). Пусто для пробела/.notdef.
    pub bitmap: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Смещение левого края bitmap от пера (может быть отрицательным).
    pub bearing_x: i32,
    /// Смещение верха bitmap вверх от базовой линии.
    pub bearing_y: i32,
    /// Горизонтальный шаг пера после глифа.
    pub advance: f32,
}

impl RasterizedGlyph {
    /// Пустой глиф (нечего рисовать) с заданным advance.
    pub fn blank(advance: f32) -> Self {
        RasterizedGlyph {
            advance,
            ..Default::default()
        }
    }
}

/// Источник растровых глифов фиксированного кегля.
pub trait Rasterizer {
    /// Метрики ячейки для этого шрифта/кегля.
    fn metrics(&self) -> CellMetrics;
    /// Растеризует символ в заданном начертании. Отсутствующие глифы —
    /// [`RasterizedGlyph::blank`] (никогда не паникует).
    fn rasterize(&mut self, ch: char, style: RenderStyle) -> RasterizedGlyph;
}

/// Тестовый растеризатор без шрифтов: любой непробельный символ — сплошной
/// блок на всю ячейку. Позволяет проверять весь пиксельный конвейер без
/// FreeType и без файлов шрифтов.
#[derive(Clone, Copy, Debug)]
pub struct StubRasterizer {
    metrics: CellMetrics,
}

impl StubRasterizer {
    /// Растеризатор с ячейкой `cell_w × cell_h`.
    pub fn new(cell_w: u32, cell_h: u32) -> Self {
        StubRasterizer {
            metrics: CellMetrics {
                width: cell_w,
                height: cell_h,
                ascent: cell_h as i32,
                underline_y: cell_h.saturating_sub(1),
                underline_thickness: 1,
            },
        }
    }
}

impl Default for StubRasterizer {
    fn default() -> Self {
        StubRasterizer::new(8, 16)
    }
}

impl Rasterizer for StubRasterizer {
    fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    fn rasterize(&mut self, ch: char, _style: RenderStyle) -> RasterizedGlyph {
        let w = self.metrics.width;
        let h = self.metrics.height;
        if ch == ' ' {
            return RasterizedGlyph::blank(w as f32);
        }
        // Сплошной блок на всю ячейку: bearing_y = ascent (= h) ⇒ верх глифа
        // приходится ровно на верх ячейки.
        RasterizedGlyph {
            bitmap: vec![0xFF; (w * h) as usize],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: self.metrics.ascent,
            advance: w as f32,
        }
    }
}
