//! [`GlyphCache`] — кэш растеризованных глифов поверх [`Rasterizer`].

use alloc::collections::BTreeMap;

use crate::font::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle};

/// Кэширует результаты растеризации по ключу `(символ, начертание)`.
///
/// Растеризатор дёргается лениво — один раз на уникальный глиф; дальше отдаётся
/// ссылка из кэша.
pub struct GlyphCache<R: Rasterizer> {
    rasterizer: R,
    metrics: CellMetrics,
    cache: BTreeMap<(char, RenderStyle), RasterizedGlyph>,
}

impl<R: Rasterizer> GlyphCache<R> {
    /// Оборачивает растеризатор, считав его метрики один раз.
    pub fn new(rasterizer: R) -> Self {
        let metrics = rasterizer.metrics();
        GlyphCache {
            rasterizer,
            metrics,
            cache: BTreeMap::new(),
        }
    }

    /// Метрики ячейки шрифта.
    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Глиф для `(ch, style)`; при первом запросе растеризует и кэширует.
    pub fn get(&mut self, ch: char, style: RenderStyle) -> &RasterizedGlyph {
        let rasterizer = &mut self.rasterizer;
        self.cache
            .entry((ch, style))
            .or_insert_with(|| rasterizer.rasterize(ch, style))
    }

    /// Число закэшированных глифов (для тестов/диагностики).
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Пуст ли кэш.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}
