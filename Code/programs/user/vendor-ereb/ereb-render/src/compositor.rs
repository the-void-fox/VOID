//! [`Compositor`] — сборка финального кадра из слоёв с z-index и blend mode.
//!
//! См. [[ADR-0004]](../../../obsidian/02-architecture/adr/0004-compositor-layers.md):
//! `ereb-core` остаётся одногридным, а слои (курсор, выделения, графика, popup'ы
//! плагинов) живут здесь, в рендере. Базовый грид рисуется прямо в поверхность
//! ([`crate::grid::GridRenderer`]) как слой z=0; всё, что выше, проходит через
//! compositor.

use alloc::vec::Vec;

use crate::color::Rgb;
use crate::surface::{BlendMode, Rect, Surface};

/// Один слой композитора — конкретный примитив отрисовки с z-index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layer {
    /// Сплошной прямоугольник.
    Rect {
        z: i32,
        rect: Rect,
        color: Rgb,
        blend: BlendMode,
    },
    /// Grayscale-маска (глиф) — например, курсор-«bar», combining marks, popup-текст.
    Mask {
        z: i32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        bitmap: Vec<u8>,
        color: Rgb,
        blend: BlendMode,
    },
    /// Сырые RGBA-пиксели (Kitty graphics, превью).
    Pixels {
        z: i32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        blend: BlendMode,
    },
}

impl Layer {
    /// Z-index слоя (порядок наложения; больше — выше).
    pub fn z(&self) -> i32 {
        match self {
            Layer::Rect { z, .. } | Layer::Mask { z, .. } | Layer::Pixels { z, .. } => *z,
        }
    }

    fn composite_into(&self, surface: &mut Surface) {
        match self {
            Layer::Rect {
                rect, color, blend, ..
            } => surface.fill_rect(*rect, *color, *blend),
            Layer::Mask {
                x,
                y,
                width,
                height,
                bitmap,
                color,
                blend,
                ..
            } => surface.blit_mask(
                Rect {
                    x: *x,
                    y: *y,
                    w: *width,
                    h: *height,
                },
                bitmap,
                *color,
                *blend,
            ),
            Layer::Pixels {
                x,
                y,
                width,
                height,
                rgba,
                blend,
                ..
            } => surface.blit_rgba(
                Rect {
                    x: *x,
                    y: *y,
                    w: *width,
                    h: *height,
                },
                rgba,
                *blend,
            ),
        }
    }
}

/// Набор слоёв-оверлеев, накладываемых поверх базовой поверхности.
#[derive(Clone, Debug, Default)]
pub struct Compositor {
    layers: Vec<Layer>,
}

impl Compositor {
    pub fn new() -> Self {
        Compositor::default()
    }

    /// Убирает все слои (вызывается в начале каждого кадра).
    pub fn clear(&mut self) {
        self.layers.clear();
    }

    /// Добавляет слой.
    pub fn push(&mut self, layer: Layer) {
        self.layers.push(layer);
    }

    /// Число слоёв.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Пуст ли набор слоёв.
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Накладывает слои на поверхность в порядке возрастания z (стабильно —
    /// при равных z сохраняется порядок добавления).
    pub fn render(&self, surface: &mut Surface) {
        let mut order: Vec<&Layer> = self.layers.iter().collect();
        order.sort_by_key(|layer| layer.z());
        for layer in order {
            layer.composite_into(surface);
        }
    }
}
