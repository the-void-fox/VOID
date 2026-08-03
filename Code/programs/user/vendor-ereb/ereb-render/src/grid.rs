//! [`GridRenderer`] — отрисовка одногридного состояния [`ereb_core::Grid`] в
//! [`Surface`] плюс построение слоя курсора для compositor'а.


use ereb_core::{Cell, CellFlags, Grid};

use crate::cache::GlyphCache;
use crate::color::{Palette, Rgb};
use crate::compositor::Layer;
use crate::font::{CellMetrics, Rasterizer, RenderStyle};
use crate::surface::{BlendMode, Rect, Surface};

/// Z-index слоя курсора (см. таблицу слоёв в rendering.md).
pub const CURSOR_Z: i32 = 30;

/// Рисует грид в поверхность: фон ячеек → глифы → подчёркивание.
///
/// Курсор и прочие оверлеи рисует не сам, а через compositor — см.
/// [`cursor_layer`].
pub struct GridRenderer {
    palette: Palette,
    metrics: CellMetrics,
}

impl GridRenderer {
    pub fn new(palette: Palette, metrics: CellMetrics) -> Self {
        GridRenderer { palette, metrics }
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Размер поверхности (в пикселях) для грида `cols × rows`.
    pub fn pixel_size(&self, cols: usize, rows: usize) -> (u32, u32) {
        (
            cols as u32 * self.metrics.width,
            rows as u32 * self.metrics.height,
        )
    }

    /// Рисует весь грид в `surface`, используя `cache` для глифов.
    pub fn paint<R: Rasterizer>(
        &self,
        grid: &Grid,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
    ) {
        self.paint_view(grid, cache, surface, 0);
    }

    /// Как [`paint`](Self::paint), но рисует вьюпорт, сдвинутый на `scroll` строк
    /// вверх в scrollback (`scroll == 0` — живой экран). Перерисовывает весь кадр
    /// (очистка + все строки) — для ресайза, прокрутки, первого кадра.
    pub fn paint_view<R: Rasterizer>(
        &self,
        grid: &Grid,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
        scroll: usize,
    ) {
        surface.clear(self.palette.background);
        for row in 0..grid.rows() {
            self.paint_row_cells(grid, cache, surface, row, scroll);
        }
    }

    /// Damage-путь: перерисовывает только строки `rows` живого экрана (каждую
    /// сперва очищает фоном, затем красит ячейки). Прочие строки сохраняют
    /// пиксели прошлого кадра — вызывающий обязан держать `surface` персистентным
    /// (без `clear`/`resize` между кадрами). Индексы вне грида игнорируются.
    pub fn paint_rows<R: Rasterizer>(
        &self,
        grid: &Grid,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
        rows: &[usize],
    ) {
        let m = self.metrics;
        let w = surface.width();
        for &row in rows {
            if row >= grid.rows() {
                continue;
            }
            surface.fill_rect(
                Rect {
                    x: 0,
                    y: (row as u32 * m.height) as i32,
                    w,
                    h: m.height,
                },
                self.palette.background,
                BlendMode::Replace,
            );
            self.paint_row_cells(grid, cache, surface, row, 0);
        }
    }

    /// Красит ячейки одной строки `row` грида (без очистки строки).
    fn paint_row_cells<R: Rasterizer>(
        &self,
        grid: &Grid,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
        row: usize,
        scroll: usize,
    ) {
        for col in 0..grid.cols() {
            let cell = grid.view_cell(col, row, scroll);
            self.draw_cell(cache, surface, col, row, &cell);
        }
    }

    /// Рисует **плоский грид ячеек** `cols × rows` (row-major) целиком: очистка +
    /// все строки. Так клиент сессии рисует [`ereb_ipc::ScreenUpdate`] (готовый
    /// кадр от сервера: панели + рамки + статус-бар уже ячейками).
    pub fn paint_cells<R: Rasterizer>(
        &self,
        cells: &[Cell],
        cols: usize,
        rows: usize,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
    ) {
        surface.clear(self.palette.background);
        for row in 0..rows {
            self.paint_cell_row(cells, cols, row, cache, surface);
        }
    }

    /// Damage-путь для плоского грида: перекрашивает только строки `dirty_rows`
    /// (каждую — очистка фоном + ячейки). Прочие сохраняют прошлый кадр (см.
    /// [`paint_rows`](Self::paint_rows)).
    pub fn paint_cells_rows<R: Rasterizer>(
        &self,
        cells: &[Cell],
        cols: usize,
        rows: usize,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
        dirty_rows: &[usize],
    ) {
        let m = self.metrics;
        let w = surface.width();
        for &row in dirty_rows {
            if row >= rows {
                continue;
            }
            surface.fill_rect(
                Rect {
                    x: 0,
                    y: (row as u32 * m.height) as i32,
                    w,
                    h: m.height,
                },
                self.palette.background,
                BlendMode::Replace,
            );
            self.paint_cell_row(cells, cols, row, cache, surface);
        }
    }

    /// Красит одну строку плоского грида (без очистки).
    fn paint_cell_row<R: Rasterizer>(
        &self,
        cells: &[Cell],
        cols: usize,
        row: usize,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
    ) {
        for col in 0..cols {
            if let Some(cell) = cells.get(row * cols + col) {
                self.draw_cell(cache, surface, col, row, cell);
            }
        }
    }

    /// Рисует одну ячейку `(col, row)`: фон (если ≠ дефолт) → глиф → подчёркивание.
    /// Строку/ячейку перед этим должен очистить вызывающий.
    fn draw_cell<R: Rasterizer>(
        &self,
        cache: &mut GlyphCache<R>,
        surface: &mut Surface,
        col: usize,
        row: usize,
        cell: &Cell,
    ) {
        let m = self.metrics;
        let mut fg = self.palette.resolve(cell.fg, self.palette.foreground);
        let mut bg = self.palette.resolve(cell.bg, self.palette.background);
        if cell.flags.contains(CellFlags::REVERSE) {
            core::mem::swap(&mut fg, &mut bg);
        }

        let x = (col as u32 * m.width) as i32;
        let y = (row as u32 * m.height) as i32;

        if bg != self.palette.background {
            surface.fill_rect(
                Rect {
                    x,
                    y,
                    w: m.width,
                    h: m.height,
                },
                bg,
                BlendMode::Replace,
            );
        }

        if cell.flags.contains(CellFlags::HIDDEN) {
            return;
        }

        if cell.ch != ' ' {
            let style = RenderStyle::from_flags(cell.flags);
            let glyph = cache.get(cell.ch, style);
            let dst = Rect {
                x: x + glyph.bearing_x,
                y: y + m.ascent - glyph.bearing_y,
                w: glyph.width,
                h: glyph.height,
            };
            surface.blit_mask(dst, &glyph.bitmap, fg, BlendMode::AlphaBlend);
        }

        if cell.flags.contains(CellFlags::UNDERLINE) {
            surface.fill_rect(
                Rect {
                    x,
                    y: y + m.underline_y as i32,
                    w: m.width,
                    h: m.underline_thickness,
                },
                fg,
                BlendMode::Replace,
            );
        }
    }
}

/// Слой блочного курсора (инверсия ячейки) для compositor'а, либо `None`,
/// если курсор скрыт.
///
/// На MVP — только стиль Block; underline/bar-стили придут позже.
pub fn cursor_layer(grid: &Grid, metrics: CellMetrics) -> Option<Layer> {
    let cursor = grid.cursor();
    if !cursor.visible {
        return None;
    }
    Some(Layer::Rect {
        z: CURSOR_Z,
        rect: Rect {
            x: (cursor.col as u32 * metrics.width) as i32,
            y: (cursor.row as u32 * metrics.height) as i32,
            w: metrics.width,
            h: metrics.height,
        },
        // Цвет для Invert не используется.
        color: Rgb::default(),
        blend: BlendMode::Invert,
    })
}
