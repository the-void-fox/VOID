//! Холст тулкита: RGBA-кадр, смешивание и скруглённые прямоугольники со сглаживанием.
//!
//! ## Почему альфа настоящая, а не «цвет фона под панелью»
//!
//! Остров со скруглёнными углами обязан показывать то, что под ним, — иначе уголки придётся
//! закрашивать цветом обоев, а обои у человека свои. Поэтому холст пишет **прямую альфу** в
//! четвёртый байт пикселя, а накладывает поверхность композитор ([[layers]]): клиент объявляет
//! `Layer::alpha`, и `wm` смешивает её кадр по каждому пикселю вместо копирования строк.
//!
//! Прямая (не предумноженная) альфа выбрана потому, что кадр читает ещё и человек глазами через
//! `void-img`, и потому, что композитор и так умеет `blend(dst, src, cover)` — предумножение
//! потребовало бы второго пути смешивания ради одной панели.
//!
//! ## Сглаживание — то же, что у окон
//!
//! Скругление считается знаковым расстоянием до прямоугольника со скруглёнными углами, ровно как
//! в композиторе (`rrect_sd` в `bin/wm.rs`): целочисленно, в 1/256 пикселя, без единого `sqrt`
//! на пиксель середины. Две разные формулы дали бы два разных скругления — у окна и у панели, —
//! и система выглядела бы собранной из двух систем.

use super::Rect;

/// Цвет с прямой альфой. `a = 0` — пиксель не пишется вовсе.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const CLEAR: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r, g, b, a }
    }

    /// Непрозрачный цвет из `0xRRGGBB` — так он пишется в конфиге и так же читается глазами.
    pub const fn hex(v: u32) -> Rgba {
        Rgba { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 0xff }
    }

    pub const fn with_a(self, a: u8) -> Rgba {
        Rgba { r: self.r, g: self.g, b: self.b, a }
    }

    /// Разобрать `#rrggbb` или `#rrggbbaa` из конфига. `None` — не цвет.
    pub fn parse(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#').unwrap_or(s);
        if h.len() != 6 && h.len() != 8 {
            return None;
        }
        let mut v = [0u8; 4];
        v[3] = 0xff;
        for (i, b) in v.iter_mut().enumerate().take(h.len() / 2) {
            let byte = h.get(i * 2..i * 2 + 2)?;
            *b = u8::from_str_radix(byte, 16).ok()?;
        }
        Some(Rgba { r: v[0], g: v[1], b: v[2], a: v[3] })
    }

    /// Смешать с другим цветом: `t` в долях 256. Нужно состояниям «наведён» и «нажат» —
    /// отдельных цветов в теме на них нет намеренно, иначе тема разрослась бы втрое.
    pub fn mix(self, o: Rgba, t: u32) -> Rgba {
        let t = t.min(256);
        let m = |a: u8, b: u8| ((a as u32 * (256 - t) + b as u32 * t) / 256) as u8;
        Rgba { r: m(self.r, o.r), g: m(self.g, o.g), b: m(self.b, o.b), a: m(self.a, o.a) }
    }
}

/// Кадр клиента: RGBA8888 по строкам — те самые страницы, в которые смотрит композитор.
pub struct Canvas<'a> {
    px: &'a mut [u8],
    pub w: i32,
    pub h: i32,
    /// Веха 145 — за этот прямоугольник не пишется ни один пиксель.
    clip: Rect,
}

impl<'a> Canvas<'a> {
    pub fn new(px: &'a mut [u8], w: i32, h: i32) -> Canvas<'a> {
        Canvas { px, w, h, clip: Rect::new(0, 0, w, h) }
    }

    /// Веха 145 — ограничить рисование прямоугольником. Понадобилось ВЫЕЗЖАЮЩЕЙ карточке: она
    /// начинает движение выше своего места и обязана быть срезанной краем панели, а не наехать
    /// на её острова. Проверкой в [`Canvas::blend`], а не отдельным путём: клип, о котором можно
    /// забыть в одном виджете из десяти, хуже отсутствующего.
    pub fn set_clip(&mut self, r: Rect) {
        self.clip = r.intersect(Rect::new(0, 0, self.w, self.h));
    }

    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Весь холст — ПРОЗРАЧНЫМ. Не чёрным: чёрный это цвет, и на обоях он виден полосой.
    pub fn clear(&mut self) {
        self.px.fill(0);
    }

    /// Один пиксель: `cov` — покрытие в долях 256 (сглаживание), альфа берётся из цвета.
    ///
    /// Смешивание по прямой альфе честное, с делением: `out_a = sa + da(1-sa)`, цвет — среднее
    /// по вкладам. Без деления получилось бы «поверх непрозрачного» верно, а «поверх
    /// полупрозрачного» — темнее, чем надо, и щели у скруглений выдали бы это сразу.
    #[inline]
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba, cov: u32) {
        if cov == 0 || c.a == 0 || !self.clip.contains(x, y) {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        if i + 3 >= self.px.len() {
            return;
        }
        let sa = c.a as u32 * cov.min(256) / 256;
        if sa == 0 {
            return;
        }
        if sa >= 255 {
            self.px[i] = c.r;
            self.px[i + 1] = c.g;
            self.px[i + 2] = c.b;
            self.px[i + 3] = 0xff;
            return;
        }
        let da = self.px[i + 3] as u32;
        let k = da * (255 - sa) / 255; // вклад того, что уже нарисовано
        let oa = sa + k;
        if oa == 0 {
            return;
        }
        let mix = |s: u8, d: u8| ((s as u32 * sa + d as u32 * k) / oa) as u8;
        self.px[i] = mix(c.r, self.px[i]);
        self.px[i + 1] = mix(c.g, self.px[i + 1]);
        self.px[i + 2] = mix(c.b, self.px[i + 2]);
        self.px[i + 3] = oa as u8;
    }

    /// Стереть кусок В ПРОЗРАЧНОСТЬ. Не «залить фоном»: под панелью обои, и любой цвет здесь
    /// был бы враньём о том, что там на самом деле.
    pub fn erase(&mut self, r: Rect) {
        let r = r.intersect(self.clip);
        for y in r.y..r.bottom() {
            let a = ((y * self.w + r.x) * 4) as usize;
            let b = ((y * self.w + r.right()) * 4) as usize;
            if b <= self.px.len() && a < b {
                self.px[a..b].fill(0);
            }
        }
    }

    /// Прямоугольник без скруглений.
    pub fn fill(&mut self, r: Rect, c: Rgba) {
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                self.blend(x, y, c, 256);
            }
        }
    }

    /// Скруглённый прямоугольник со сглаженным краем.
    pub fn rrect(&mut self, r: Rect, rad: i32, fill: Rgba) {
        self.rrect_bordered(r, rad, 0, fill, Rgba::CLEAR);
    }

    /// Скруглённый прямоугольник с рамкой толщиной `t` изнутри.
    ///
    /// Рамка рисуется КОЛЬЦОМ (покрытие внешней формы минус внутренней), а не «сначала вся
    /// форма цветом рамки, сверху заливка». Со сплошными цветами разницы нет, с
    /// полупрозрачными — есть: два наложения дали бы в середине альфу больше заказанной, и
    /// остров получился бы плотнее, чем просили.
    pub fn rrect_bordered(&mut self, r: Rect, rad: i32, t: i32, fill: Rgba, border: Rgba) {
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        let rad = rad.clamp(0, r.w.min(r.h) / 2);
        // Обходим только видимую часть: карточка, срезанная клипом наполовину, не должна стоить
        // как целая — она рисуется каждый кадр движения.
        let vis = r.intersect(self.clip);
        for y in vis.y..vis.bottom() {
            // Строка вдали от углов заливается без единого корня: покрытие там известно заранее.
            let near_y = (y - r.y) < rad || (r.y + r.h - 1 - y) < rad;
            for x in vis.x..vis.right() {
                let near_x = (x - r.x) < rad || (r.x + r.w - 1 - x) < rad;
                let (out, inn) = if near_y && near_x {
                    let d = rrect_sd(x, y, r, rad);
                    ((128 - d).clamp(0, 256) as u32, (128 - (d + t * 256)).clamp(0, 256) as u32)
                } else {
                    let edge = (x - r.x) < t
                        || (r.x + r.w - 1 - x) < t
                        || (y - r.y) < t
                        || (r.y + r.h - 1 - y) < t;
                    (256, if edge { 0 } else { 256 })
                };
                if out == 0 {
                    continue;
                }
                if out > inn {
                    self.blend(x, y, border, out - inn);
                }
                if inn > 0 {
                    self.blend(x, y, fill, inn);
                }
            }
        }
    }

    /// Серая маска глифа (или иконки) цветом `c`. Нужна тексту: растеризатор отдаёт покрытие.
    pub fn mask(&mut self, x: i32, y: i32, w: i32, h: i32, mask: &[u8], c: Rgba) {
        for row in 0..h {
            for col in 0..w {
                let v = mask[(row * w + col) as usize];
                if v != 0 {
                    self.blend(x + col, y + row, c, v as u32 + 1);
                }
            }
        }
    }
}

/// Знаковое расстояние до скруглённого прямоугольника в 1/256 пикселя (внутри — отрицательное).
///
/// Копия формулы композитора: одинаковое скругление у окна и у панели важнее, чем экономия
/// десяти строк на общем модуле, — общий модуль пришлось бы делать зависимостью `wm` от тулкита,
/// то есть класть в композитор чужой код (ровно то, чего [[layers]] избегает).
fn rrect_sd(px: i32, py: i32, r: Rect, rad: i32) -> i32 {
    let ax = (2 * px - (2 * r.x + r.w - 1)).abs();
    let ay = (2 * py - (2 * r.y + r.h - 1)).abs();
    let qx = ax - (r.w - 1 - 2 * rad);
    let qy = ay - (r.h - 1 - 2 * rad);
    let (mx, my) = (qx.max(0), qy.max(0));
    let len = isqrt(mx * mx + my * my);
    let inside = qx.max(qy).min(0);
    (len + inside - 2 * rad) * 128
}

/// Целочисленный квадратный корень (тот же алгоритм, что в композиторе): плавающей точки в
/// рисовании панели нет вовсе — она стоила бы дороже самого пикселя.
fn isqrt(v: i32) -> i32 {
    if v <= 0 {
        return 0;
    }
    let mut r = 0i32;
    let mut bit = 1i32 << 30;
    let mut rem = v;
    while bit > rem {
        bit >>= 2;
    }
    while bit != 0 {
        if rem >= r + bit {
            rem -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    r
}
