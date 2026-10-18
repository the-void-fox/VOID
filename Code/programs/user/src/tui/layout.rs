//! Прямоугольники и их деление — то же, чем в оконном тулките занимается `ui::Rect`.
//!
//! Раскладка здесь ОДНОМЕРНАЯ и за один шаг: прямоугольник делится на полосы сверху вниз или
//! слева направо, каждая со своей мерой. Двумерных сеток, вложенных пропорций и прочего нет —
//! и не потому, что лень: из полос складывается всё, что нужно терминалу, а каждая следующая
//! степень свободы в раскладке оплачивается тем, что её надо понимать, читая чужой код.

use alloc::vec::Vec;

/// Прямоугольник в ЯЧЕЙКАХ, не в пикселях.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    pub const fn right(&self) -> u16 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> u16 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Отступить внутрь со всех сторон. Пустой прямоугольник, если отступать уже некуда, —
    /// рисование по нему просто ничего не сделает, и это лучше паники на узком окне.
    pub fn shrink(&self, dx: u16, dy: u16) -> Rect {
        Rect {
            x: self.x + dx,
            y: self.y + dy,
            w: self.w.saturating_sub(dx * 2),
            h: self.h.saturating_sub(dy * 2),
        }
    }

    /// Отрезать `n` строк сверху и отдать ОБЕ части: отрезанное и остаток.
    ///
    /// Парой, а не одной: строка состояния и то, что над ней, нужны обе, и считать вторую
    /// вычитанием — это та самая вторая мера, которая однажды разойдётся с первой.
    pub fn cut_top(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.h);
        (Rect { h: n, ..*self }, Rect { y: self.y + n, h: self.h - n, ..*self })
    }

    /// То же снизу.
    pub fn cut_bottom(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.h);
        (Rect { y: self.bottom() - n, h: n, ..*self }, Rect { h: self.h - n, ..*self })
    }

    /// То же слева.
    pub fn cut_left(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.w);
        (Rect { w: n, ..*self }, Rect { x: self.x + n, w: self.w - n, ..*self })
    }

    /// То же справа.
    pub fn cut_right(&self, n: u16) -> (Rect, Rect) {
        let n = n.min(self.w);
        (Rect { x: self.right() - n, w: n, ..*self }, Rect { w: self.w - n, ..*self })
    }
}

/// Мера полосы при делении.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    /// Ровно столько ячеек.
    Len(u16),
    /// Доля от целого, в процентах.
    Pct(u16),
    /// Всё, что осталось, поровну между такими полосами.
    Fill,
}

/// Разделить прямоугольник на полосы ПО ВЕРТИКАЛИ (сверху вниз).
pub fn rows(r: Rect, parts: &[Part]) -> Vec<Rect> {
    let sizes = share(r.h, parts);
    let mut out = Vec::with_capacity(parts.len());
    let mut y = r.y;
    for s in sizes {
        out.push(Rect { x: r.x, y, w: r.w, h: s });
        y += s;
    }
    out
}

/// Разделить ПО ГОРИЗОНТАЛИ (слева направо).
pub fn cols(r: Rect, parts: &[Part]) -> Vec<Rect> {
    let sizes = share(r.w, parts);
    let mut out = Vec::with_capacity(parts.len());
    let mut x = r.x;
    for s in sizes {
        out.push(Rect { x, y: r.y, w: s, h: r.h });
        x += s;
    }
    out
}

/// Раздать `total` ячеек по мерам.
///
/// Порядок раздачи — сперва точные меры и проценты, остаток поровну между `Fill`. Недостача
/// режет ПОСЛЕДНИЕ полосы, а не первые: на узком окне человеку нужнее начало — заголовок,
/// первая колонка, — а не хвост.
fn share(total: u16, parts: &[Part]) -> Vec<u16> {
    let mut out = alloc::vec![0u16; parts.len()];
    let mut left = total;
    let mut fills = 0;
    for (i, p) in parts.iter().enumerate() {
        let want = match p {
            Part::Len(n) => *n,
            Part::Pct(p) => (total as u32 * *p as u32 / 100) as u16,
            Part::Fill => {
                fills += 1;
                continue;
            }
        };
        let got = want.min(left);
        out[i] = got;
        left -= got;
    }
    if fills > 0 {
        let each = left / fills;
        // Лишние ячейки от деления уходят ПЕРВОЙ растяжимой полосе — иначе экран на одну колонку
        // уже, чем окно, и под правым краем остаётся неубранный столбец прошлого кадра.
        let mut extra = left % fills;
        for (i, p) in parts.iter().enumerate() {
            if matches!(p, Part::Fill) {
                out[i] = each + if extra > 0 { 1 } else { 0 };
                extra = extra.saturating_sub(1);
            }
        }
    }
    out
}
