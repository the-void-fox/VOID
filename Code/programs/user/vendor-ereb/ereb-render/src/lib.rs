//! # ereb-render
//!
//! Превращение одногридного состояния [`ereb_core::Grid`] в пиксели.
//! Не знает ни про ANSI, ни про окна — только про ячейки, глифы и буферы.
//!
//! ## Конвейер
//!
//! 1. [`Rasterizer`] растеризует символы в grayscale-глифы; [`GlyphCache`] их кэширует.
//! 2. [`GridRenderer`] рисует грид в [`Surface`] (фон → глиф цветом fg → подчёркивание).
//! 3. [`Compositor`] накладывает оверлеи (курсор, выделения, графику) с z-index и
//!    blend mode — см. `obsidian/02-architecture/adr/0004-compositor-layers.md`.
//!
//! Реальная растеризация — FreeType ([`FtFont`], за фичей `freetype`). Ядро
//! рендера — чистый Rust и тестируется через [`StubRasterizer`] без C-зависимости.
//!
//! ```
//! use ereb_core::Grid;
//! use ereb_render::{GlyphCache, GridRenderer, Palette, StubRasterizer, Surface};
//!
//! let mut cache = GlyphCache::new(StubRasterizer::new(8, 16));
//! let renderer = GridRenderer::new(Palette::default(), cache.metrics());
//!
//! let mut grid = Grid::new(4, 2);
//! vte::Parser::new().advance(&mut grid, b"hi");
//!
//! let (w, h) = renderer.pixel_size(grid.cols(), grid.rows());
//! let mut surface = Surface::new(w, h, Palette::default().background);
//! renderer.paint(&grid, &mut cache, &mut surface);
//! assert_eq!(surface.width(), 32);
//! ```

// См. `std` в Cargo.toml: ядро рендера alloc-only, чтобы крейт ехал на VOID ([ADR 0005]).
//
// [ADR 0005]: ../../../obsidian/02-architecture/adr/0005-void-target.md
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod cache;
mod color;
mod compositor;
mod grid;
mod surface;

pub mod font;

#[cfg(feature = "freetype")]
mod ft;
#[cfg(feature = "ttf")]
mod ttf;

pub use cache::GlyphCache;
pub use color::{Palette, Rgb};
pub use compositor::{Compositor, Layer};
pub use font::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle, StubRasterizer};
pub use grid::{CURSOR_Z, GridRenderer, cursor_layer};
pub use surface::{BYTES_PER_PIXEL, BlendMode, Rect, Surface};

#[cfg(feature = "freetype")]
pub use ft::FtFont;
#[cfg(feature = "ttf")]
pub use ttf::{TtfError, TtfFont};

#[cfg(test)]
mod tests {
    use super::*;
    use ereb_core::Grid;

    fn render_bytes(
        cols: usize,
        rows: usize,
        bytes: &[u8],
    ) -> (Surface, GlyphCache<StubRasterizer>) {
        let mut cache = GlyphCache::new(StubRasterizer::new(8, 16));
        let renderer = GridRenderer::new(Palette::default(), cache.metrics());
        let mut grid = Grid::new(cols, rows);
        vte::Parser::new().advance(&mut grid, bytes);
        let (w, h) = renderer.pixel_size(cols, rows);
        let mut surface = Surface::new(w, h, Palette::default().background);
        renderer.paint(&grid, &mut cache, &mut surface);
        (surface, cache)
    }

    // --- palette -------------------------------------------------------------

    #[test]
    fn palette_resolves_color_variants() {
        use ereb_core::{Color, NamedColor};
        let p = Palette::default();
        assert_eq!(p.resolve(Color::Default, p.foreground), p.foreground);
        assert_eq!(
            p.resolve(Color::Named(NamedColor::Red), p.foreground),
            p.ansi[1]
        );
        assert_eq!(
            p.resolve(Color::Rgb(1, 2, 3), p.foreground),
            Rgb::new(1, 2, 3)
        );
        // 256-палитра: индекс 0..15 == ansi; куб и серый по формуле xterm.
        assert_eq!(p.resolve(Color::Indexed(1), p.foreground), p.ansi[1]);
        assert_eq!(p.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(p.indexed(231), Rgb::new(255, 255, 255));
        assert_eq!(p.indexed(232), Rgb::new(8, 8, 8));
    }

    // --- surface primitives --------------------------------------------------

    #[test]
    fn fill_rect_clips_to_bounds() {
        let mut s = Surface::new(4, 4, Rgb::new(0, 0, 0));
        s.fill_rect(
            Rect {
                x: 2,
                y: 2,
                w: 10,
                h: 10,
            },
            Rgb::new(255, 0, 0),
            BlendMode::Replace,
        );
        assert_eq!(s.pixel(3, 3), Rgb::new(255, 0, 0));
        assert_eq!(s.pixel(1, 1), Rgb::new(0, 0, 0));
    }

    #[test]
    fn alpha_blend_half_coverage() {
        let mut s = Surface::new(1, 1, Rgb::new(0, 0, 0));
        let mask = [128u8];
        s.blit_mask(
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            &mask,
            Rgb::new(255, 255, 255),
            BlendMode::AlphaBlend,
        );
        // ~50% между чёрным и белым.
        let px = s.pixel(0, 0);
        assert!((px.r as i32 - 128).abs() <= 1, "got {px:?}");
    }

    #[test]
    fn invert_blend_inverts_destination() {
        let mut s = Surface::new(1, 1, Rgb::new(10, 20, 30));
        s.fill_rect(
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            Rgb::default(),
            BlendMode::Invert,
        );
        assert_eq!(s.pixel(0, 0), Rgb::new(245, 235, 225));
    }

    // --- damage (paint_rows) -------------------------------------------------

    #[test]
    fn paint_rows_matches_full_repaint() {
        let mut cache = GlyphCache::new(StubRasterizer::new(8, 16));
        let renderer = GridRenderer::new(Palette::default(), cache.metrics());
        let mut grid = Grid::new(4, 2);
        let mut parser = vte::Parser::new();
        parser.advance(&mut grid, b"ab\r\ncd");
        let (w, h) = renderer.pixel_size(4, 2);
        let mut s = Surface::new(w, h, Palette::default().background);
        renderer.paint(&grid, &mut cache, &mut s);

        // Меняем только строку 0 (CUP home + "XY"), затем damage-перерисовка её.
        parser.advance(&mut grid, b"\x1b[HXY");
        renderer.paint_rows(&grid, &mut cache, &mut s, &[0]);

        // Эталон: полный рендер итогового грида — должен совпасть до пикселя.
        let mut t = Surface::new(w, h, Palette::default().background);
        renderer.paint(&grid, &mut cache, &mut t);
        assert_eq!(
            s.data(),
            t.data(),
            "damage-перерисовка строки ≠ полному кадру"
        );

        // Индексы вне грида — без паники и без изменений.
        renderer.paint_rows(&grid, &mut cache, &mut s, &[99]);
        assert_eq!(s.data(), t.data());
    }

    #[test]
    fn paint_cells_matches_grid_paint() {
        let mut cache = GlyphCache::new(StubRasterizer::new(8, 16));
        let renderer = GridRenderer::new(Palette::default(), cache.metrics());
        let mut grid = Grid::new(4, 2);
        vte::Parser::new().advance(&mut grid, b"\x1b[31mab\x1b[0m\r\ncd");
        let (w, h) = renderer.pixel_size(4, 2);

        // Эталон — путь грида.
        let mut g = Surface::new(w, h, Palette::default().background);
        renderer.paint(&grid, &mut cache, &mut g);

        // Плоский грид ячеек из того же грида → paint_cells должен совпасть.
        let cells: Vec<_> = (0..2)
            .flat_map(|row| (0..4).map(move |col| (col, row)))
            .map(|(col, row)| grid.view_cell(col, row, 0))
            .collect();
        let mut c = Surface::new(w, h, Palette::default().background);
        renderer.paint_cells(&cells, 4, 2, &mut cache, &mut c);
        assert_eq!(c.data(), g.data(), "paint_cells ≠ пути грида");

        // Damage-вариант: красим обе строки (плюс индекс 9 вне грида — пропуск
        // без паники) в свежую поверхность → совпадает с полным кадром.
        let mut d = Surface::new(w, h, Palette::default().background);
        renderer.paint_cells_rows(&cells, 4, 2, &mut cache, &mut d, &[0, 1, 9]);
        assert_eq!(d.data(), g.data());
    }

    // --- compositor ----------------------------------------------------------

    #[test]
    fn compositor_orders_by_z() {
        let mut s = Surface::new(1, 1, Rgb::new(0, 0, 0));
        let mut c = Compositor::new();
        // Добавлены в «неправильном» порядке — z должен победить.
        c.push(Layer::Rect {
            z: 10,
            rect: Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            color: Rgb::new(0, 255, 0),
            blend: BlendMode::Replace,
        });
        c.push(Layer::Rect {
            z: 0,
            rect: Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            color: Rgb::new(255, 0, 0),
            blend: BlendMode::Replace,
        });
        c.render(&mut s);
        assert_eq!(s.pixel(0, 0), Rgb::new(0, 255, 0));
    }

    // --- grid rendering ------------------------------------------------------

    #[test]
    fn glyph_cell_is_foreground_blank_cell_is_background() {
        let p = Palette::default();
        let (s, _) = render_bytes(3, 1, b"A ");
        // 'A' в ячейке 0 (stub = сплошной блок) — пиксели = fg по умолчанию.
        assert_eq!(s.pixel(0, 0), p.foreground);
        // Пустая ячейка 1 — фон.
        assert_eq!(s.pixel(8, 0), p.background);
    }

    #[test]
    fn sgr_red_foreground_paints_red_glyph() {
        let p = Palette::default();
        let (s, _) = render_bytes(2, 1, b"\x1b[31mX");
        assert_eq!(s.pixel(0, 0), p.ansi[1]);
    }

    #[test]
    fn reverse_video_swaps_fg_and_bg() {
        let p = Palette::default();
        // Реверс на пустой ячейке: фон становится цветом текста (fg).
        // Грид шире одной ячейки, чтобы печать пробела не вызвала автоскролл.
        let (s, _) = render_bytes(2, 1, b"\x1b[7m ");
        assert_eq!(s.pixel(0, 0), p.foreground);
    }

    #[test]
    fn underline_draws_on_bottom_row() {
        let p = Palette::default();
        // Подчёркивание на пустой ячейке: нижняя строка пикселей = fg.
        let (s, _) = render_bytes(2, 1, b"\x1b[4m ");
        assert_eq!(s.pixel(0, 15), p.foreground);
        assert_eq!(s.pixel(0, 0), p.background);
    }

    #[test]
    fn cache_reuses_glyphs() {
        // Грид с запасом по ширине, чтобы три 'A' не вызвали автоскролл.
        let (_, cache) = render_bytes(4, 1, b"AAA");
        // Три одинаковых 'A' → один уникальный глиф в кэше.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cursor_layer_inverts_cell() {
        let p = Palette::default();
        let mut cache = GlyphCache::new(StubRasterizer::new(8, 16));
        let renderer = GridRenderer::new(p, cache.metrics());
        let grid = Grid::new(2, 1); // курсор в (0,0), видим по умолчанию
        let (w, h) = renderer.pixel_size(2, 1);
        let mut surface = Surface::new(w, h, p.background);
        renderer.paint(&grid, &mut cache, &mut surface);

        let mut compositor = Compositor::new();
        compositor.push(cursor_layer(&grid, cache.metrics()).unwrap());
        compositor.render(&mut surface);

        // Фон чёрный → инверсия = белый под курсором; соседняя ячейка не тронута.
        assert_eq!(surface.pixel(0, 0), Rgb::new(255, 255, 255));
        assert_eq!(surface.pixel(8, 0), p.background);
    }

    #[test]
    fn renders_full_grid() {
        // Перф-дымовой тест: большой грид целиком за один кадр.
        let (s, _) = render_bytes(200, 50, &vec![b'x'; 200 * 50]);
        assert_eq!((s.width(), s.height()), (200 * 8, 50 * 16));
    }
}
