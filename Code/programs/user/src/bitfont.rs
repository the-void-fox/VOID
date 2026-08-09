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
//! заводить. Файл ядра — только данные (`static [u8; 4096]`), ничего кроме них он не тянет.

use ereb_render::{CellMetrics, RasterizedGlyph, Rasterizer, RenderStyle};

// Путь отсчитывается от каталога файла, ПОДКЛЮЧИВШЕГО этот модуль (`src/bin/../`), — таково
// правило вложенных `#[path]`. Подключат из другого места — сборка сломается здесь, с указанием
// на эту строку; это лучше, чем копия таблицы, которая сломается молча и через полгода.
#[path = "../../../kernel/src/arch/x86_64/font.rs"]
mod table;

/// Ширина и высота глифа в таблице.
const W: u32 = 8;
const H: u32 = 16;

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
        let code = map_cp866(ch);
        if code == b' ' {
            return RasterizedGlyph::default();
        }
        let rows = &table::CP866_8X16[code as usize * H as usize..][..H as usize];
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

/// Unicode → байт CP866. Копия правила из консоли ядра (`arch/x86_64/vga.rs::map_cp866`): здесь
/// оно короткое, а тащить ради него ещё один файл ядра — хуже.
fn map_cp866(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        'А'..='Я' => 0x80 + (c as u32 - 'А' as u32) as u8,
        'а'..='п' => 0xA0 + (c as u32 - 'а' as u32) as u8,
        'р'..='я' => 0xE0 + (c as u32 - 'р' as u32) as u8,
        'Ё' => 0xF0,
        'ё' => 0xF1,
        '═' => 0xCD, '║' => 0xBA, '╔' => 0xC9, '╗' => 0xBB, '╚' => 0xC8, '╝' => 0xBC,
        '╟' => 0xC7, '╢' => 0xB6, '╠' => 0xCC, '╣' => 0xB9, '╦' => 0xCB, '╩' => 0xCA, '╬' => 0xCE,
        '─' => 0xC4, '│' => 0xB3, '┌' => 0xDA, '┐' => 0xBF, '└' => 0xC0, '┘' => 0xD9,
        '├' => 0xC3, '┤' => 0xB4, '┬' => 0xC2, '┴' => 0xC1, '┼' => 0xC5,
        '█' => 0xDB, '░' => 0xB0, '▒' => 0xB1, '▓' => 0xB2, '•' => 0x07, '°' => 0xF8,
        '—' | '–' => 0xC4,
        '·' => 0xFA,
        '«' => 0xAE, '»' => 0xAF,
        '→' => 0x1A, '←' => 0x1B, '↑' => 0x18, '↓' => 0x19,
        '…' => b'.',
        '×' => b'x',
        '≈' => b'~',
        '№' => b'#',
        _ => b'?',
    }
}
