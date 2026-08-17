//! Текст тулкита: настоящий шрифт (TTF), запасной 8×16 и строка ПРОПОРЦИОНАЛЬНО (Веха 144).
//!
//! ## Почему не растровый шрифт панели, как было до этой вехи
//!
//! Скруглённым островам нужен и приличный текст: рядом со сглаженным углом ступенчатый глиф
//! 8×16 выдаёт себя мгновенно. Растеризатор у системы уже есть — `ereb-render` с чистым
//! Rust-контуром (терминал живёт на нём с Вехи 97), — и второй заводить не за что.
//!
//! ## Но запасной шрифт остаётся, и это не перестраховка
//!
//! Шрифт в VOID приходит **пакетом**, а не лежит в бинаре (Веха 114: TTF на 2.6 МБ в образе уже
//! однажды сломал посев в store). Значит на свежей системе его нет вовсе, и панель обязана
//! подняться без него — иначе первый же экран новой установки оказался бы пустым. Встроенные
//! 8×16 из таблицы ядра стоят четыре килобайта и читаются.
//!
//! Расплата за пропорциональность: ширину строки нельзя посчитать умножением. Поэтому [`Font`]
//! хранит кэш глифов, а измерение строки — такой же вызов с `&mut`, как и рисование.

use ereb_render::{CellMetrics, GlyphCache, RasterizedGlyph, Rasterizer, RenderStyle, TtfFont};

use super::bitfont::BitmapFont;
use super::paint::{Canvas, Rgba};

/// Откуда берутся глифы. Два источника вместо одного — см. шапку модуля.
enum Face {
    Ttf(TtfFont),
    Bitmap(BitmapFont),
}

impl Rasterizer for Face {
    fn metrics(&self) -> CellMetrics {
        match self {
            Face::Ttf(f) => f.metrics(),
            Face::Bitmap(f) => f.metrics(),
        }
    }
    fn rasterize(&mut self, ch: char, style: RenderStyle) -> RasterizedGlyph {
        match self {
            Face::Ttf(f) => f.rasterize(ch, style),
            Face::Bitmap(f) => f.rasterize(ch, style),
        }
    }
}

/// Шрифт тулкита: растеризатор плюс кэш глифов.
pub struct Font {
    cache: GlyphCache<Face>,
    ttf: bool,
}

impl Font {
    /// Взять шрифт: названный конфигом файл, иначе встроенный 8×16.
    ///
    /// О неудаче говорится ВСЛУХ и с причиной — молчаливый откат на запасной шрифт оставил бы
    /// человека гадать между опечаткой в имени, не тем пакетом и не тем файлом внутри пакета.
    pub fn load(name: Option<&str>, px: u32) -> Font {
        if let Some(name) = name {
            match super::font::find(name) {
                Some(bytes) => match TtfFont::from_vec(bytes, px) {
                    Ok(f) => {
                        return Font { cache: GlyphCache::new(Face::Ttf(f)), ttf: true };
                    }
                    Err(_) => say(&alloc::format!("ui: {} — не разбирается как шрифт", name)),
                },
                None => {
                    say(&alloc::format!("ui: шрифт {} не найден (ни путь, ни пакет)", name));
                    super::font::list();
                }
            }
        }
        let f = BitmapFont::new(px);
        Font { cache: GlyphCache::new(Face::Bitmap(f)), ttf: false }
    }

    /// Настоящий ли это шрифт. Нужно там, где выбор символа зависит от набора: у встроенного
    /// 8×16 (CP866) нет ни многоточия, ни стрелок — вместо них он рисует «?».
    pub fn ttf(&self) -> bool {
        self.ttf
    }

    /// Шаг строки в пикселях.
    pub fn line_h(&self) -> i32 {
        self.cache.metrics().height as i32
    }

    /// От верха строки до базовой линии.
    pub fn ascent(&self) -> i32 {
        self.cache.metrics().ascent
    }

    /// Ширина строки в пикселях. С кэшем это дёшево, но всё равно `&mut`: первый вызов
    /// растеризует то, чего в кэше ещё нет.
    pub fn width(&mut self, s: &str) -> i32 {
        let mut pen = 0.0f32;
        for ch in s.chars() {
            pen += self.cache.get(ch, RenderStyle::Regular).advance;
        }
        (pen + 0.5) as i32
    }

    /// Нарисовать строку от точки `x` по БАЗОВОЙ ЛИНИИ `base`. Возвращает конец пера.
    pub fn draw(&mut self, c: &mut Canvas, x: i32, base: i32, s: &str, col: Rgba) -> i32 {
        let mut pen = x as f32;
        for ch in s.chars() {
            let g = self.cache.get(ch, RenderStyle::Regular);
            if g.width > 0 && g.height > 0 && !g.bitmap.is_empty() {
                c.mask(
                    (pen + 0.5) as i32 + g.bearing_x,
                    base - g.bearing_y,
                    g.width as i32,
                    g.height as i32,
                    &g.bitmap,
                    col,
                );
            }
            pen += g.advance;
        }
        (pen + 0.5) as i32
    }

    /// То же, но не шире `max`: хвост, который не влез, заменяется многоточием.
    ///
    /// Режем ПО СИМВОЛАМ, а не по байтам: заголовки в системе русские, и строка, разрезанная
    /// посреди буквы, стала бы мусором (на этих граблях панель уже стояла — Веха 143.2).
    pub fn draw_clip(
        &mut self,
        c: &mut Canvas,
        x: i32,
        base: i32,
        s: &str,
        col: Rgba,
        max: i32,
    ) -> i32 {
        if max <= 0 {
            return x;
        }
        if self.width(s) <= max {
            return self.draw(c, x, base, s, col);
        }
        let tail = if self.ttf { "…" } else { "..." };
        let room = max - self.width(tail);
        let mut pen = 0.0f32;
        let mut end = 0;
        for (i, ch) in s.char_indices() {
            let w = self.cache.get(ch, RenderStyle::Regular).advance;
            if (pen + w + 0.5) as i32 > room {
                break;
            }
            pen += w;
            end = i + ch.len_utf8();
        }
        let cut = self.draw(c, x, base, &s[..end], col);
        self.draw(c, cut, base, tail, col)
    }
}

fn say(s: &str) {
    void_user::write_console(s.as_bytes());
    void_user::write_console(b"\n");
}
