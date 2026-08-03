//! [`Surface`] — RGBA-фреймбуфер и низкоуровневые операции рисования.
//!
//! Формат пикселя — **RGBA8888**, 4 байта на пиксель, порядок `R,G,B,A`,
//! row-major. Финальный буфер всегда непрозрачен (`A = 255`); альфа-канал в
//! источниках используется только как покрытие при блендинге. Свизл под формат
//! конкретного бэкенда (`XRGB8888` для wl_shm/DRM) — забота бэкенда.

use alloc::vec;
use alloc::vec::Vec;

use crate::color::Rgb;

/// Байт на пиксель (RGBA8888).
pub const BYTES_PER_PIXEL: usize = 4;

/// Прямоугольник в пиксельных координатах (левый верхний угол может быть < 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Способ наложения источника на приёмник.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BlendMode {
    /// Жёсткая запись: `dst = src` (покрытие игнорируется).
    #[default]
    Replace,
    /// `dst = src·a + dst·(1−a)`, где `a` — покрытие. Для текста и полупрозрачного.
    AlphaBlend,
    /// `dst = (255−dst)`, модулированное покрытием. Для блочного курсора/выделения.
    Invert,
}

/// `src·a + dst·(1−a)` для одного канала, `a` в `0..=255`.
#[inline]
fn over(dst: u8, src: u8, a: u16) -> u8 {
    ((src as u16 * a + dst as u16 * (255 - a) + 127) / 255) as u8
}

/// RGBA-фреймбуфер фиксированного размера.
pub struct Surface {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Surface {
    /// Создаёт поверхность `width × height`, залитую цветом `bg`.
    pub fn new(width: u32, height: u32, bg: Rgb) -> Self {
        let mut surface = Surface {
            width,
            height,
            data: vec![0u8; width as usize * height as usize * BYTES_PER_PIXEL],
        };
        surface.clear(bg);
        surface
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Сырой RGBA-буфер (для передачи в SHM/DRM).
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Гарантирует размер `width × height`, **не** очищая содержимое — вызывающий
    /// обязан сам перезалить все пиксели (например, `GridRenderer::paint`, который
    /// начинается с [`clear`](Self::clear)). При совпадении размера — no-op.
    pub fn ensure_size(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.data
            .resize(width as usize * height as usize * BYTES_PER_PIXEL, 0);
    }

    /// Меняет размер поверхности (с перезаливкой `bg`), если он отличается.
    pub fn resize(&mut self, width: u32, height: u32, bg: Rgb) {
        if self.width == width && self.height == height {
            self.clear(bg);
            return;
        }
        self.width = width;
        self.height = height;
        self.data
            .resize(width as usize * height as usize * BYTES_PER_PIXEL, 0);
        self.clear(bg);
    }

    /// Заливает всю поверхность сплошным цветом.
    pub fn clear(&mut self, color: Rgb) {
        for px in self.data.chunks_exact_mut(BYTES_PER_PIXEL) {
            px[0] = color.r;
            px[1] = color.g;
            px[2] = color.b;
            px[3] = 255;
        }
    }

    /// Цвет пикселя `(x, y)` (для тестов/отладки). За пределами — чёрный.
    pub fn pixel(&self, x: u32, y: u32) -> Rgb {
        if x >= self.width || y >= self.height {
            return Rgb::default();
        }
        let i = (y as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL;
        Rgb::new(self.data[i], self.data[i + 1], self.data[i + 2])
    }

    /// Наносит один пиксель с заданным покрытием и режимом наложения.
    #[inline]
    fn put(&mut self, x: i32, y: i32, color: Rgb, coverage: u8, blend: BlendMode) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        if coverage == 0 && blend != BlendMode::Replace {
            return;
        }
        let i = (y as usize * self.width as usize + x as usize) * BYTES_PER_PIXEL;
        let px = &mut self.data[i..i + BYTES_PER_PIXEL];
        let (sr, sg, sb) = match blend {
            BlendMode::Invert => (255 - px[0], 255 - px[1], 255 - px[2]),
            _ => (color.r, color.g, color.b),
        };
        if blend == BlendMode::Replace {
            px[0] = sr;
            px[1] = sg;
            px[2] = sb;
        } else {
            let a = coverage as u16;
            px[0] = over(px[0], sr, a);
            px[1] = over(px[1], sg, a);
            px[2] = over(px[2], sb, a);
        }
        px[3] = 255;
    }

    /// Заливает прямоугольник (с обрезкой по границам поверхности).
    pub fn fill_rect(&mut self, rect: Rect, color: Rgb, blend: BlendMode) {
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = (rect.x + rect.w as i32).min(self.width as i32);
        let y1 = (rect.y + rect.h as i32).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // Быстрый путь `Replace`: пишем по строкам без побайтовой проверки границ.
        if blend == BlendMode::Replace {
            let w = self.width as usize;
            let row_px = (x1 - x0) as usize;
            for y in y0..y1 {
                let base = (y as usize * w + x0 as usize) * BYTES_PER_PIXEL;
                for px in self.data[base..base + row_px * BYTES_PER_PIXEL]
                    .chunks_exact_mut(BYTES_PER_PIXEL)
                {
                    px[0] = color.r;
                    px[1] = color.g;
                    px[2] = color.b;
                    px[3] = 255;
                }
            }
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                self.put(x, y, color, 255, blend);
            }
        }
    }

    /// Наносит grayscale-маску (покрытие) цветом `color` в прямоугольник `dst`.
    ///
    /// Так рисуются глифы: 8-bpp bitmap от растеризатора + цвет переднего плана.
    pub fn blit_mask(&mut self, dst: Rect, mask: &[u8], color: Rgb, blend: BlendMode) {
        debug_assert!(mask.len() >= (dst.w * dst.h) as usize);
        // Клипуем приёмник один раз и идём по строкам без побайтовой проверки.
        let x0 = dst.x.max(0);
        let y0 = dst.y.max(0);
        let x1 = (dst.x + dst.w as i32).min(self.width as i32);
        let y1 = (dst.y + dst.h as i32).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let w = self.width as usize;
        let stride = dst.w as usize;
        for y in y0..y1 {
            let mask_row = (y - dst.y) as usize * stride;
            let px_row = y as usize * w;
            for x in x0..x1 {
                let coverage = mask[mask_row + (x - dst.x) as usize];
                if coverage == 0 {
                    continue;
                }
                let i = (px_row + x as usize) * BYTES_PER_PIXEL;
                let px = &mut self.data[i..i + BYTES_PER_PIXEL];
                let (sr, sg, sb) = match blend {
                    BlendMode::Invert => (255 - px[0], 255 - px[1], 255 - px[2]),
                    _ => (color.r, color.g, color.b),
                };
                if blend == BlendMode::Replace {
                    px[0] = sr;
                    px[1] = sg;
                    px[2] = sb;
                } else {
                    let a = coverage as u16;
                    px[0] = over(px[0], sr, a);
                    px[1] = over(px[1], sg, a);
                    px[2] = over(px[2], sb, a);
                }
                px[3] = 255;
            }
        }
    }

    /// Наносит сырые RGBA-пиксели в прямоугольник `dst` (Kitty graphics и т.п.).
    pub fn blit_rgba(&mut self, dst: Rect, rgba: &[u8], blend: BlendMode) {
        debug_assert!(rgba.len() >= (dst.w * dst.h) as usize * BYTES_PER_PIXEL);
        let x0 = dst.x.max(0);
        let y0 = dst.y.max(0);
        let x1 = (dst.x + dst.w as i32).min(self.width as i32);
        let y1 = (dst.y + dst.h as i32).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let w = self.width as usize;
        let stride = dst.w as usize;
        // Быстрый путь `Replace`: копируем строки целиком (источники непрозрачны).
        if blend == BlendMode::Replace {
            let row_px = (x1 - x0) as usize;
            let bytes = row_px * BYTES_PER_PIXEL;
            for y in y0..y1 {
                let sy = (y - dst.y) as usize;
                let sx = (x0 - dst.x) as usize;
                let si = (sy * stride + sx) * BYTES_PER_PIXEL;
                let di = (y as usize * w + x0 as usize) * BYTES_PER_PIXEL;
                self.data[di..di + bytes].copy_from_slice(&rgba[si..si + bytes]);
            }
            return;
        }
        // Прочие режимы блендим по альфе источника.
        for y in y0..y1 {
            let src_row = (y - dst.y) as usize * stride;
            for x in x0..x1 {
                let i = (src_row + (x - dst.x) as usize) * BYTES_PER_PIXEL;
                let color = Rgb::new(rgba[i], rgba[i + 1], rgba[i + 2]);
                self.put(x, y, color, rgba[i + 3], blend);
            }
        }
    }
}
