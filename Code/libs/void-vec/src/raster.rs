//! Растеризатор `.vg`: контуры → сглаженные пиксели RGBA8888.
//!
//! ## Как считается сглаживание
//!
//! Не надвыборкой. Знак VOID ([`logo`](../../programs/user/src/logo.rs)) считается сеткой 4×4 на
//! пиксель, и это честно, но там фигур шесть и рисуются они раз в загрузку. Иконка перерисовывается
//! каждый кадр, и надвыборка 4×4 стоила бы шестнадцати проверок на пиксель.
//!
//! Здесь другой приём — НАКОПЛЕНИЕ ПЛОЩАДИ (его же используют растеризаторы шрифтов). Каждый
//! отрезок контура добавляет в буфер ровно ту долю пикселя, которую он отсекает, со знаком по
//! направлению обхода. Затем строка проходится бегущей суммой, и сумма в пикселе — это и есть
//! число обходов, дробное на краю фигуры. Цена — один проход по отрезкам и один по строке, а
//! точность выше, чем у сетки 4×4: доля площади считается ТОЧНО, а не пробами.
//!
//! Отсюда же бесплатно берутся оба правила заливки: по ненулевому обходу это `min(|сумма|, 1)`,
//! по чётности — та же сумма, сложенная пилой с периодом 2.
//!
//! ## Почему полосами
//!
//! Буфер накопления — это `f32` на пиксель, и на полный экран 1280×800 он весил бы 4 МиБ, чего
//! в куче процесса нет. Но строки НЕЗАВИСИМЫ: сумма каждой начинается с нуля (у замкнутого
//! контура вклады в любой строке в сумме дают ноль). Значит картинку можно резать на полосы и
//! держать буфер на полосу, проходя контур по разу на полосу. Иконке хватает одной полосы;
//! большой фигуре — десятка проходов, что дешевле, чем мегабайты.
//!
//! ## Отсечение
//!
//! По вертикали — границами цикла строк: вклад за пределами полосы просто не считается, а `x`
//! на входе в полосу берётся из уравнения отрезка, а не «как получилось».
//!
//! По горизонтали — прижатием `x` к краям. Это не грубость: у накопления вклад отрезка это
//! ОБХОД, и если увести его на нулевой столбец, всё, что правее, останется залитым правильно.
//! Обрезать геометрию по-настоящему пришлось бы делением отрезков, а результат тот же.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Seg, Vg};

/// Куда вписать вьюбокс: прямоугольник назначения в пикселях холста.
///
/// Вписывание с сохранением пропорций и по центру. Растягивать иконку под чужие пропорции
/// нельзя — она рисовалась в своих, и растянутая читается как ошибка вёрстки.
#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Fit {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Fit {
        Fit { x, y, w, h }
    }

    /// Квадрат со стороной `s` — обычный случай иконки.
    pub fn square(x: i32, y: i32, s: u32) -> Fit {
        Fit { x, y, w: s, h: s }
    }
}

/// Прямоугольник отсечения в пикселях назначения: `[x0, x1) × [y0, y1)`.
#[derive(Clone, Copy, Debug)]
pub struct Clip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Clip {
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Clip {
        Clip { x0, y0, x1, y1 }
    }
}

/// Холст: чужой буфер RGBA8888 и его размеры. Рисование идёт ПОВЕРХ (смешивание по альфе) —
/// иконка ложится на уже нарисованную подложку.
///
/// Это удобный случай общего [`rasterize`], а не единственный способ рисовать: тулкиту системы
/// нужен СВОЙ вывод пикселя (у него своё отсечение и своя формула смешивания, общая для всех
/// виджетов), и дублировать её здесь значило бы завести вторую, которая однажды разойдётся.
pub struct Canvas<'a> {
    px: &'a mut [u8],
    stride: usize,
    rows: usize,
}

/// Сколько ячеек буфера накопления держим на полосу. 8192 `f32` — 32 КиБ: помещается в кучу
/// процесса и не заставляет ходить по контуру лишний раз даже для иконки в пол-экрана.
const BAND_CELLS: usize = 8192;

impl<'a> Canvas<'a> {
    /// `stride` — пикселей в строке буфера, `rows` — строк. `None`, если байт меньше, чем
    /// обещают размеры: рисовать в такой буфер значило бы читать чужую память.
    pub fn new(px: &'a mut [u8], stride: usize, rows: usize) -> Option<Canvas<'a>> {
        if px.len() < stride.checked_mul(rows)?.checked_mul(4)? {
            return None;
        }
        Some(Canvas { px, stride, rows })
    }

    /// Нарисовать фигуру, вписав её вьюбокс в `fit`.
    pub fn draw(&mut self, vg: &Vg, fit: Fit, tint: Option<[u8; 4]>) {
        let (stride, rows) = (self.stride, self.rows);
        let clip = Clip::new(0, 0, stride as i32, rows as i32);
        let px = &mut *self.px;
        rasterize(vg, fit, clip, tint, &mut |x, y, c, cov| {
            let a = (c[3] as u32 * cov.min(256)) / 256;
            if a == 0 {
                return;
            }
            let o = ((y as usize * stride) + x as usize) * 4;
            for i in 0..3 {
                px[o + i] = blend(px[o + i], c[i], a);
            }
            // Альфа холста растёт, но не падает: иконка на непрозрачной подложке обязана
            // оставить её непрозрачной, иначе композитор увидит дырку.
            px[o + 3] = blend(px[o + 3], 255, a);
        });
    }
}

/// Растеризовать фигуру, отдавая КАЖДЫЙ затронутый пиксель наружу: `(x, y, цвет, покрытие)`,
/// где покрытие — доли 256. Смешивание — забота вызывающего.
///
/// `tint` заменяет цвет ВСЕХ фигур файла. Иконки системы одноцветные, а цвет им назначает тема
/// (обычный, приглушённый, под курсором) — держать по файлу на состояние было бы расточительно
/// и разъезжалось бы с темой.
///
/// Пиксели приходят слева направо и сверху вниз В ПРЕДЕЛАХ ОДНОЙ ФИГУРЫ, а фигуры — в порядке
/// файла. Значит один и тот же пиксель может прийти несколько раз (фигура поверх фигуры), и
/// вызывающий обязан именно СМЕШИВАТЬ, а не присваивать.
pub fn rasterize(
    vg: &Vg,
    fit: Fit,
    clip: Clip,
    tint: Option<[u8; 4]>,
    sink: &mut dyn FnMut(i32, i32, [u8; 4], u32),
) {
    let (vw, vh) = vg.size();
    if fit.w == 0 || fit.h == 0 {
        return;
    }
    // Масштаб по узкой стороне + центрирование: пропорции вьюбокса сохраняются.
    let sx = fit.w as f32 / vw as f32;
    let sy = fit.h as f32 / vh as f32;
    let s = if sx < sy { sx } else { sy };
    let ox = fit.x as f32 + (fit.w as f32 - vw as f32 * s) * 0.5;
    let oy = fit.y as f32 + (fit.h as f32 - vh as f32 * s) * 0.5;

    // Прямоугольник, который вообще может быть затронут, — пересечение `fit` с отсечением.
    // Всё дальнейшее считается в ЕГО координатах.
    let rx0 = fit.x.max(clip.x0);
    let ry0 = fit.y.max(clip.y0);
    let rx1 = (fit.x + fit.w as i32).min(clip.x1);
    let ry1 = (fit.y + fit.h as i32).min(clip.y1);
    if rx1 <= rx0 || ry1 <= ry0 {
        return;
    }
    let (rw, rh) = ((rx1 - rx0) as usize, (ry1 - ry0) as usize);

    // +2 столбца: вклад отрезка ложится в два соседних пикселя, и у правого края второй
    // должен куда-то попасть — иначе он затёк бы в начало следующей строки.
    let aw = rw + 2;
    let band = (BAND_CELLS / aw).max(1).min(rh);
    let mut acc = vec![0f32; aw * band];

    let mut y = 0usize;
    while y < rh {
        let bh = band.min(rh - y);
        for sh in vg.shapes() {
            let color = tint.unwrap_or(sh.rgba);
            if color[3] == 0 {
                continue;
            }
            acc[..aw * bh].fill(0.0);
            let mut f = Flatten {
                acc: &mut acc,
                aw,
                bh,
                // Перенос из вьюбокса сразу в координаты ПОЛОСЫ: так во внутреннем цикле
                // не остаётся ни одного лишнего вычитания.
                ox: ox - rx0 as f32,
                oy: oy - ry0 as f32 - y as f32,
                s,
                rw,
                start: (0.0, 0.0),
                cur: (0.0, 0.0),
                open: false,
            };
            for seg in sh.segs() {
                f.seg(seg);
            }
            f.finish();

            // Бегущая сумма по строке полосы: сумма в пикселе и есть число обходов.
            for row in 0..bh {
                let mut sum = 0.0f32;
                let line = row * aw;
                let dy = ry0 + y as i32 + row as i32;
                for col in 0..rw {
                    sum += acc[line + col];
                    let cov = coverage(sum, sh.evenodd());
                    // Порог в половину уровня: ниже него пиксель всё равно не изменится, а
                    // вызовов наружу экономит много — фигура редко покрывает всю ширину.
                    if cov < 1.0 / 512.0 {
                        continue;
                    }
                    let c = (cov * 256.0) as u32;
                    sink(rx0 + col as i32, dy, color, if c > 256 { 256 } else { c });
                }
            }
        }
        y += bh;
    }
}

#[inline]
fn blend(dst: u8, src: u8, a: u32) -> u8 {
    ((dst as u32 * (255 - a) + src as u32 * a) / 255) as u8
}

/// Сумма обходов → покрытие 0..1.
#[inline]
fn coverage(sum: f32, evenodd: bool) -> f32 {
    let v = fabs(sum);
    if !evenodd {
        return if v > 1.0 { 1.0 } else { v };
    }
    // Пила с периодом 2: 0→0, 1→1, 2→0, 3→1. Ровно правило чётности, только с дробным краем.
    let t = v - 2.0 * floorf(v * 0.5);
    if t > 1.0 {
        2.0 - t
    } else {
        t
    }
}

// В no_std нет ни `floor`, ни `abs` у f32 (они живут в std), а тянуть libm ради двух строк
// не за что. Диапазон здесь пиксельный, за i32 не выходит.
#[inline]
fn fabs(x: f32) -> f32 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}
#[inline]
fn floorf(x: f32) -> f32 {
    let t = x as i32 as f32;
    if x < t {
        t - 1.0
    } else {
        t
    }
}
#[inline]
fn ceilf(x: f32) -> f32 {
    -floorf(-x)
}

// ── Разложение контура на отрезки и накопление ──────────────────────────────────────────────

struct Flatten<'a> {
    acc: &'a mut Vec<f32>,
    aw: usize,
    bh: usize,
    ox: f32,
    oy: f32,
    s: f32,
    rw: usize,
    start: (f32, f32),
    cur: (f32, f32),
    open: bool,
}

impl Flatten<'_> {
    /// Точка вьюбокса → точка полосы.
    #[inline]
    fn dev(&self, x: f32, y: f32) -> (f32, f32) {
        (self.ox + x * self.s, self.oy + y * self.s)
    }

    fn seg(&mut self, seg: Seg) {
        match seg {
            Seg::Move(x, y) => {
                // Незакрытый подконтур закрывается сам: заливка не бывает открытой, и
                // оставить его открытым значит потерять один обход и вывернуть фигуру.
                self.finish();
                self.cur = self.dev(x, y);
                self.start = self.cur;
                self.open = true;
            }
            Seg::Line(x, y) => {
                let p = self.dev(x, y);
                self.edge(self.cur, p);
                self.cur = p;
            }
            Seg::Quad(cx, cy, x, y) => {
                let c = self.dev(cx, cy);
                let p = self.dev(x, y);
                let a = self.cur;
                let n = self.steps(dist(a, c) + dist(c, p));
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let m = 1.0 - t;
                    let q = (
                        m * m * a.0 + 2.0 * m * t * c.0 + t * t * p.0,
                        m * m * a.1 + 2.0 * m * t * c.1 + t * t * p.1,
                    );
                    self.edge(self.cur, q);
                    self.cur = q;
                }
            }
            Seg::Cubic(ax, ay, bx, by, x, y) => {
                let c1 = self.dev(ax, ay);
                let c2 = self.dev(bx, by);
                let p = self.dev(x, y);
                let a = self.cur;
                let n = self.steps(dist(a, c1) + dist(c1, c2) + dist(c2, p));
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let m = 1.0 - t;
                    let (m2, t2) = (m * m, t * t);
                    let q = (
                        m2 * m * a.0 + 3.0 * m2 * t * c1.0 + 3.0 * m * t2 * c2.0 + t2 * t * p.0,
                        m2 * m * a.1 + 3.0 * m2 * t * c1.1 + 3.0 * m * t2 * c2.1 + t2 * t * p.1,
                    );
                    self.edge(self.cur, q);
                    self.cur = q;
                }
            }
            Seg::Close => self.finish(),
        }
    }

    /// Число отрезков на кривую — по длине её управляющей ломаной В ПИКСЕЛЯХ. Кривая, занявшая
    /// три пикселя, не заслуживает двенадцати отрезков, а занявшая пол-экрана — заслуживает.
    /// Длина берётся манхэттенская: корня в no_std нет, а завышение на четверть тут безобидно.
    #[inline]
    fn steps(&self, len: f32) -> usize {
        let n = (len * 0.4) as usize;
        n.clamp(2, 48)
    }

    fn finish(&mut self) {
        if self.open {
            let (s, c) = (self.start, self.cur);
            if s != c {
                self.edge(c, s);
            }
            self.cur = s;
            self.open = false;
        }
    }

    /// Вклад отрезка в буфер накопления. Это и есть весь растеризатор.
    fn edge(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if p0.1 == p1.1 {
            // Горизонтальный отрезок не пересекает ни одной строки — обход он не меняет.
            return;
        }
        let (dir, mut p0, p1) = if p0.1 < p1.1 { (1.0f32, p0, p1) } else { (-1.0f32, p1, p0) };
        if p1.1 <= 0.0 || p0.1 >= self.bh as f32 {
            return;
        }
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        // Вход в полосу сверху: `x` берём из уравнения отрезка, а не с его конца.
        if p0.1 < 0.0 {
            p0.0 -= p0.1 * dxdy;
            p0.1 = 0.0;
        }
        let mut x = p0.0;
        let y_end = {
            let e = ceilf(p1.1) as i32;
            (e.max(0) as usize).min(self.bh)
        };
        let y_beg = (floorf(p0.1) as i32).max(0) as usize;
        let xmax = self.rw as f32;

        for row in y_beg..y_end {
            let rowf = row as f32;
            let dy = fmin(rowf + 1.0, p1.1) - fmax(rowf, p0.1);
            if dy <= 0.0 {
                x += dxdy * dy;
                continue;
            }
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            // Прижатие к краям — то самое горизонтальное отсечение (см. заголовок файла).
            let (a, b) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let x0 = fmax(0.0, fmin(a, xmax));
            let x1 = fmax(0.0, fmin(b, xmax));
            let line = row * self.aw;

            let x0f = floorf(x0);
            let x0i = x0f as usize;
            let x1c = ceilf(x1);
            let x1i = x1c as usize;

            if x1i <= x0i + 1 {
                // Отрезок не выходит за один пиксель: делим вклад между ним и соседом по
                // положению СЕРЕДИНЫ — этого достаточно, площадь тут линейна.
                let xm = 0.5 * (x0 + x1) - x0f;
                self.acc[line + x0i] += d * (1.0 - xm);
                self.acc[line + x0i + 1] += d * xm;
            } else {
                // Отрезок пересекает несколько пикселей: у крайних вклад — треугольник,
                // у средних — ровная доля.
                let inv = 1.0 / (x1 - x0);
                let fx0 = x0 - x0f;
                let a0 = 0.5 * inv * (1.0 - fx0) * (1.0 - fx0);
                let fx1 = x1 - x1c + 1.0;
                let am = 0.5 * inv * fx1 * fx1;
                self.acc[line + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.acc[line + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = inv * (1.5 - fx0);
                    self.acc[line + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.acc[line + xi] += d * inv;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * inv;
                    self.acc[line + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.acc[line + x1i] += d * am;
            }
            x = xnext;
        }
    }
}

#[inline]
fn fmin(a: f32, b: f32) -> f32 {
    if a < b {
        a
    } else {
        b
    }
}
#[inline]
fn fmax(a: f32, b: f32) -> f32 {
    if a > b {
        a
    } else {
        b
    }
}
#[inline]
fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    fabs(a.0 - b.0) + fabs(a.1 - b.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Writer;

    fn rgba(w: usize, h: usize) -> Vec<u8> {
        vec![0u8; w * h * 4]
    }

    /// Квадрат по границам пикселей: заливка обязана быть ровно 255 внутри и 0 снаружи —
    /// сглаживание не должно «подмазывать» края, которые и так на месте.
    #[test]
    fn crisp_square() {
        let mut w = Writer::new();
        w.shape([255, 255, 255, 255], false);
        w.move_to(2.0, 2.0);
        w.line_to(6.0, 2.0);
        w.line_to(6.0, 6.0);
        w.line_to(2.0, 6.0);
        w.close();
        let bytes = w.finish(8, 8).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(8, 8);
        Canvas::new(&mut px, 8, 8).unwrap().draw(&vg, Fit::square(0, 0, 8), None);
        for y in 0..8 {
            for x in 0..8 {
                let v = px[(y * 8 + x) * 4];
                let inside = (2..6).contains(&x) && (2..6).contains(&y);
                assert_eq!(v, if inside { 255 } else { 0 }, "пиксель {x},{y}");
            }
        }
    }

    /// Половина пикселя — половина яркости. Проверяет, что край считается площадью, а не
    /// округлением до ближайшего пикселя.
    #[test]
    fn half_pixel_is_half_bright() {
        let mut w = Writer::new();
        w.shape([255, 255, 255, 255], false);
        w.move_to(0.0, 0.0);
        w.line_to(0.5, 0.0);
        w.line_to(0.5, 1.0);
        w.line_to(0.0, 1.0);
        w.close();
        let bytes = w.finish(1, 1).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(1, 1);
        Canvas::new(&mut px, 1, 1).unwrap().draw(&vg, Fit::square(0, 0, 1), None);
        assert!((px[0] as i32 - 128).abs() <= 2, "получилось {}", px[0]);
    }

    /// Дырка в фигуре: внешний контур по часовой, внутренний против — при ненулевом обходе
    /// середина обязана остаться пустой.
    #[test]
    fn nonzero_hole() {
        let mut w = Writer::new();
        w.shape([255, 255, 255, 255], false);
        for (a, b, rev) in [(0.0f32, 8.0f32, false), (3.0, 5.0, true)] {
            w.move_to(a, a);
            if rev {
                w.line_to(a, b);
                w.line_to(b, b);
                w.line_to(b, a);
            } else {
                w.line_to(b, a);
                w.line_to(b, b);
                w.line_to(a, b);
            }
            w.close();
        }
        let bytes = w.finish(8, 8).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(8, 8);
        Canvas::new(&mut px, 8, 8).unwrap().draw(&vg, Fit::square(0, 0, 8), None);
        assert_eq!(px[(1 * 8 + 1) * 4], 255, "стенка");
        assert_eq!(px[(4 * 8 + 4) * 4], 0, "дырка");
    }

    /// Тот же контур без разворота внутреннего: по ненулевому обходу дырки нет, по чётности —
    /// есть. Правило заливки должно решать, а не порядок точек.
    #[test]
    fn evenodd_makes_hole() {
        let mk = |eo: bool| {
            let mut w = Writer::new();
            w.shape([255, 255, 255, 255], eo);
            for (a, b) in [(0.0f32, 8.0f32), (3.0, 5.0)] {
                w.move_to(a, a);
                w.line_to(b, a);
                w.line_to(b, b);
                w.line_to(a, b);
                w.close();
            }
            w.finish(8, 8).unwrap()
        };
        for (eo, want) in [(false, 255u8), (true, 0u8)] {
            let bytes = mk(eo);
            let vg = Vg::parse(&bytes).unwrap();
            let mut px = rgba(8, 8);
            Canvas::new(&mut px, 8, 8).unwrap().draw(&vg, Fit::square(0, 0, 8), None);
            assert_eq!(px[(4 * 8 + 4) * 4], want, "evenodd={eo}");
        }
    }

    /// Полосы не должны быть видны: рисуем высокую фигуру, буфер накопления заведомо меньше её.
    /// Раньше здесь ловилась бы утечка суммы из строки в строку.
    #[test]
    fn bands_leave_no_seam() {
        let mut w = Writer::new();
        w.shape([255, 255, 255, 255], false);
        w.move_to(0.0, 0.0);
        w.line_to(600.0, 0.0);
        w.line_to(600.0, 600.0);
        w.line_to(0.0, 600.0);
        w.close();
        let bytes = w.finish(600, 600).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(600, 600);
        Canvas::new(&mut px, 600, 600).unwrap().draw(&vg, Fit::square(0, 0, 600), None);
        for y in 0..600 {
            assert_eq!(px[(y * 600 + 300) * 4], 255, "строка {y}");
        }
    }

    /// Фигура за краем холста не должна ни падать, ни рисоваться не туда.
    #[test]
    fn clips_outside_canvas() {
        let mut w = Writer::new();
        w.shape([255, 255, 255, 255], false);
        w.move_to(0.0, 0.0);
        w.line_to(10.0, 0.0);
        w.line_to(10.0, 10.0);
        w.line_to(0.0, 10.0);
        w.close();
        let bytes = w.finish(10, 10).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(8, 8);
        let mut c = Canvas::new(&mut px, 8, 8).unwrap();
        c.draw(&vg, Fit::square(-4, -4, 10), None);
        c.draw(&vg, Fit::square(6, 6, 10), None);
        assert_eq!(px[0], 255, "левый верх покрыт");
        assert_eq!(px[(7 * 8 + 7) * 4], 255, "правый низ покрыт");
    }

    /// Цвет темы должен побеждать цвет из файла.
    #[test]
    fn tint_wins() {
        let mut w = Writer::new();
        w.shape([255, 0, 0, 255], false);
        w.move_to(0.0, 0.0);
        w.line_to(4.0, 0.0);
        w.line_to(4.0, 4.0);
        w.line_to(0.0, 4.0);
        w.close();
        let bytes = w.finish(4, 4).unwrap();
        let vg = Vg::parse(&bytes).unwrap();

        let mut px = rgba(4, 4);
        Canvas::new(&mut px, 4, 4).unwrap().draw(&vg, Fit::square(0, 0, 4), Some([0, 0x8c, 0, 255]));
        assert_eq!(&px[0..4], &[0, 0x8c, 0, 255]);
    }
}
