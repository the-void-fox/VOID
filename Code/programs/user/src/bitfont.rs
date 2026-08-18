//! Запасной растеризатор: битмапный шрифт 8×16 в раскладке CP866 (Веха 114).
//!
//! Зачем он нужен. До этой вехи `term` носил внутри себя TTF на 2.6 МБ (`include_bytes!`) — срез
//! угла, который уже отомстил: посев такого бинаря в store падал по куче ядра на riscv, из-за
//! чего терминал пришлось сделать x86-only. Настоящий шрифт теперь приходит **файлом** — из
//! пакета nixpkgs или из любого пути, названного конфигом, — а это значит, что его может и НЕ
//! БЫТЬ: свежая система, пустой профиль, опечатка в пути.
//!
//! «Нет шрифта — нет терминала» здесь недопустимо: терминал это то, чем систему чинят. Поэтому
//! в бинаре остаётся минимальный шрифт, которого хватает, чтобы прочитать сообщение об ошибке и
//! набрать команду, — 4 КиБ вместо 2.6 МБ.
//!
//! Таблица глифов **не копируется**: она подключается по пути прямо из ядра
//! (`arch/x86_64/font.rs`), где живёт с Вехи 41 для VGA-текста. Копия неизбежно разошлась бы с
//! оригиналом, а один и тот же шрифт в двух местах системы — ровно то, чего в VOID стараются не
//! заводить. Само подключение с Вехи 140 живёт в [`void_user::glyph`] — тот же шрифт понадобился
//! бару, а он не терминал и растеризатора ereb не заводит.

use ereb_render::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle};

// Веха 140 — сама таблица и отображение Unicode→CP866 переехали в `void_user::glyph`: тот же
// шрифт понадобился бару, а панель, тянущая ради шестнадцати байт строк глифа весь ereb, — это
// не переиспользование. Здесь остаётся ровно то, что делает из шрифта РАСТЕРИЗАТОР терминала.
use void_user::glyph;

/// Ширина и высота глифа в таблице.
const W: u32 = glyph::W;
const H: u32 = glyph::H;

/// Битмапный шрифт с целочисленным увеличением. Дробного кегля у растровых шрифтов не бывает:
/// вместо мыла — честные крупные пиксели.
pub struct BitmapFont {
    scale: u32,
}

impl BitmapFont {
    /// Подобрать увеличение под желаемый кегль (высоту ячейки в пикселях).
    pub fn new(font_px: u32) -> BitmapFont {
        BitmapFont { scale: (font_px / H).max(1) }
    }

    /// Кегль, который получился на самом деле, — его стоит сказать вслух: человек просил 18,
    /// а получил 16, и знать об этом лучше сразу.
    pub fn effective_px(&self) -> u32 {
        self.scale * H
    }
}

impl Rasterizer for BitmapFont {
    fn metrics(&self) -> CellMetrics {
        CellMetrics {
            width: W * self.scale,
            height: H * self.scale,
            // Базовая линия у шрифта 8×16 — на 12-й строке (под ней хвосты «р», «у», «g»).
            ascent: (12 * self.scale) as i32,
            underline_y: 14 * self.scale,
            underline_thickness: self.scale,
        }
    }

    fn rasterize(&mut self, ch: char, style: RenderStyle) -> RasterizedGlyph {
        if glyph::map_cp866(ch) == b' ' {
            // Пустой глиф, но с НАСТОЯЩИМ шагом пера (Веха 145). Здесь стоял
            // `RasterizedGlyph::default()`, у которого `advance == 0`, — и пробелы пропадали
            // из строки вовсе: «раскладка клавиатуры» превращалась в «раскладкаклавиатуры».
            // Терминал этого не видел, потому что рисует по СЕТКЕ и шаг берёт из метрик; первым
            // на грабли наступило меню, у которого текст пропорциональный.
            return RasterizedGlyph::blank((W * self.scale) as f32);
        }
        let rows = glyph::rows(ch);
        let bold = matches!(style, RenderStyle::Bold | RenderStyle::BoldItalic);
        let (w, h) = (W * self.scale, H * self.scale);
        let mut bitmap = alloc::vec![0u8; (w * h) as usize];
        for (y, row) in rows.iter().enumerate() {
            // Жирность у растрового шрифта делается размазыванием на пиксель вправо — тем же
            // приёмом, что и у текстовых консолей: своего начертания в таблице нет.
            let bits = if bold { *row | (*row >> 1) } else { *row };
            for x in 0..W {
                if bits & (0x80 >> x) == 0 {
                    continue;
                }
                for sy in 0..self.scale {
                    for sx in 0..self.scale {
                        let px = x * self.scale + sx;
                        let py = y as u32 * self.scale + sy;
                        bitmap[(py * w + px) as usize] = 0xFF;
                    }
                }
            }
        }
        RasterizedGlyph {
            bitmap,
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: (12 * self.scale) as i32,
            advance: w as f32,
        }
    }
}
