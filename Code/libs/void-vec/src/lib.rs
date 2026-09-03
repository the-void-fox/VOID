//! Формат `.vg` — векторная фигура VOID — и его чтение и запись.
//!
//! ## Что в нём есть и чего нет
//!
//! Есть: вьюбокс, список фигур, у каждой сплошной цвет RGBA, правило заливки и поток команд
//! `move/line/quad/cubic/close`. Всё.
//!
//! Нет: обводок (конвертер превращает штрих в контур ещё на хосте), градиентов, обтравки,
//! фильтров, текста, трансформов и групп. Не «пока нет» — их незачем: иконки системы, которые
//! мы рисуем, пользуются только заливкой, а каждая лишняя возможность формата это код, который
//! в системе придётся держать вечно.
//!
//! ## Раскладка
//!
//! ```text
//! заголовок, 12 байт
//!   0..4   b"VG1\0"
//!   4..6   ширина вьюбокса, u16
//!   6..8   высота вьюбокса, u16
//!   8..10  число фигур, u16
//!   10..12 ноль (запас на выравнивание)
//!
//! фигура, 8 байт заголовка + поток
//!   0..4   r, g, b, a
//!   4      флаги: бит 0 — заливка по чётности (иначе по ненулевому обходу)
//!   5      ноль
//!   6..8   длина потока команд в байтах, u16
//!   8..    поток
//!
//! команда: байт кода + точки подряд, точка = x: i16, y: i16
//!   0 M — 1 точка   1 L — 1 точка   2 Q — 2 точки   3 C — 3 точки   4 Z — нет точек
//! ```
//!
//! **Координаты — в 1/16 единицы вьюбокса.** Не в пикселях: `.vg` не знает, каким размером его
//! нарисуют, и знать не должен — иконка бара живёт и в 21 пиксель, и в 48 на экране повыше.
//! Шестнадцатых хватает: на иконке в 24 единицы это 1/16 пикселя при рисовании один-к-одному,
//! а сглаживание всё равно считает точнее, чем видит глаз. Знаковый i16 даёт вьюбокс до 2048
//! единиц и позволяет точкам выходить за его край — у обводок, превращённых в контур, так бывает.
//!
//! Иконка целиком — сотни две байт, поэтому она просто лежит в бинаре по `include_bytes!`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

mod raster;
pub use raster::{rasterize, Canvas, Clip, Fit};

/// Магия файла. Версия в ней же: разойдётся формат — старые файлы просто не прочитаются, а не
/// прочитаются НЕВЕРНО.
pub const MAGIC: [u8; 4] = *b"VG1\0";

/// Единиц вьюбокса в одной единице координаты.
pub const SCALE: f32 = 16.0;

/// Флаг фигуры: заливка по чётности пересечений (SVG `fill-rule="evenodd"`).
pub const F_EVENODD: u8 = 1;

/// Коды команд потока.
pub const OP_MOVE: u8 = 0;
pub const OP_LINE: u8 = 1;
pub const OP_QUAD: u8 = 2;
pub const OP_CUBIC: u8 = 3;
pub const OP_CLOSE: u8 = 4;

/// Сколько точек несёт команда. `None` — код неизвестен, дальше поток читать нельзя.
const fn arity(op: u8) -> Option<usize> {
    match op {
        OP_MOVE | OP_LINE => Some(1),
        OP_QUAD => Some(2),
        OP_CUBIC => Some(3),
        OP_CLOSE => Some(0),
        _ => None,
    }
}

// ── Чтение ──────────────────────────────────────────────────────────────────────────────────

/// Разобранный `.vg`. Ничего не копирует: держит ссылку на байты и ходит по ним на месте —
/// иконка живёт в `.rodata` бинаря, и копировать её в кучу ради рисования незачем.
#[derive(Clone, Copy)]
pub struct Vg<'a> {
    w: u16,
    h: u16,
    n: u16,
    body: &'a [u8],
}

/// Одна фигура: цвет, правило и поток команд.
#[derive(Clone, Copy)]
pub struct Shape<'a> {
    pub rgba: [u8; 4],
    pub flags: u8,
    ops: &'a [u8],
}

/// Команда потока, уже с точками в единицах ВЬЮБОКСА (деление на [`SCALE`] сделано).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

impl<'a> Vg<'a> {
    /// Разбор заголовка. `None` — не наш файл или он оборван; частично разобранного `.vg` не
    /// бывает: иконку либо видно целиком, либо не видно вовсе.
    pub fn parse(data: &'a [u8]) -> Option<Vg<'a>> {
        if data.len() < 12 || data[0..4] != MAGIC {
            return None;
        }
        let w = u16::from_le_bytes([data[4], data[5]]);
        let h = u16::from_le_bytes([data[6], data[7]]);
        let n = u16::from_le_bytes([data[8], data[9]]);
        if w == 0 || h == 0 {
            return None;
        }
        Some(Vg { w, h, n, body: &data[12..] })
    }

    /// Вьюбокс: в этих единицах заданы все координаты.
    pub fn size(&self) -> (u16, u16) {
        (self.w, self.h)
    }

    pub fn shape_count(&self) -> u16 {
        self.n
    }

    /// Фигуры по порядку. Порядок — это порядок рисования: следующая ложится поверх предыдущей.
    pub fn shapes(&self) -> Shapes<'a> {
        Shapes { rest: self.body, left: self.n }
    }
}

pub struct Shapes<'a> {
    rest: &'a [u8],
    left: u16,
}

impl<'a> Iterator for Shapes<'a> {
    type Item = Shape<'a>;

    fn next(&mut self) -> Option<Shape<'a>> {
        if self.left == 0 || self.rest.len() < 8 {
            return None;
        }
        let len = u16::from_le_bytes([self.rest[6], self.rest[7]]) as usize;
        if self.rest.len() < 8 + len {
            // Файл оборван. Дальше не идём и молча заканчиваем: нарисуется то, что успело
            // прочитаться. Отказ здесь ничем не лучше — иконку всё равно уже не показать.
            self.left = 0;
            return None;
        }
        let s = Shape {
            rgba: [self.rest[0], self.rest[1], self.rest[2], self.rest[3]],
            flags: self.rest[4],
            ops: &self.rest[8..8 + len],
        };
        self.rest = &self.rest[8 + len..];
        self.left -= 1;
        Some(s)
    }
}

impl<'a> Shape<'a> {
    /// Заливка по чётности, а не по ненулевому обходу.
    pub fn evenodd(&self) -> bool {
        self.flags & F_EVENODD != 0
    }

    /// Команды контура по порядку.
    pub fn segs(&self) -> Segs<'a> {
        Segs { rest: self.ops }
    }
}

pub struct Segs<'a> {
    rest: &'a [u8],
}

impl<'a> Segs<'a> {
    fn pt(&self, i: usize) -> (f32, f32) {
        let o = 1 + i * 4;
        let x = i16::from_le_bytes([self.rest[o], self.rest[o + 1]]);
        let y = i16::from_le_bytes([self.rest[o + 2], self.rest[o + 3]]);
        (x as f32 / SCALE, y as f32 / SCALE)
    }
}

impl<'a> Iterator for Segs<'a> {
    type Item = Seg;

    fn next(&mut self) -> Option<Seg> {
        let op = *self.rest.first()?;
        let k = arity(op)?;
        if self.rest.len() < 1 + k * 4 {
            return None;
        }
        let s = match op {
            OP_MOVE => {
                let p = self.pt(0);
                Seg::Move(p.0, p.1)
            }
            OP_LINE => {
                let p = self.pt(0);
                Seg::Line(p.0, p.1)
            }
            OP_QUAD => {
                let (c, p) = (self.pt(0), self.pt(1));
                Seg::Quad(c.0, c.1, p.0, p.1)
            }
            OP_CUBIC => {
                let (a, b, p) = (self.pt(0), self.pt(1), self.pt(2));
                Seg::Cubic(a.0, a.1, b.0, b.1, p.0, p.1)
            }
            _ => Seg::Close,
        };
        self.rest = &self.rest[1 + k * 4..];
        Some(s)
    }
}

// ── Запись ──────────────────────────────────────────────────────────────────────────────────
//
// Писатель живёт ЗДЕСЬ, а не в конвертере, хотя пользуется им только конвертер. Причина одна:
// формат должен иметь единственное описание. Разъедься читатель с писателем — иконки перестали
// бы рисоваться молча, а так они не соберутся.

/// Сборка `.vg` по фигурам.
#[derive(Default)]
pub struct Writer {
    shapes: Vec<(([u8; 4], u8), Vec<u8>)>,
    cur: Option<(([u8; 4], u8), Vec<u8>)>,
}

/// Координата вьюбокса → i16 в 1/16. С насыщением: точка, улетевшая за пределы i16, это уже
/// не иконка, а ошибка выше по течению — но обрезать её лучше, чем свернуть в мусор.
fn q(v: f32) -> [u8; 2] {
    let t = v * SCALE;
    let t = if t < i16::MIN as f32 {
        i16::MIN
    } else if t > i16::MAX as f32 {
        i16::MAX
    } else {
        // Округление к ближайшему: `as i16` рубит к нулю, и контур уезжал бы на пол-шестнадцатой
        // в сторону начала координат.
        (t + if t < 0.0 { -0.5 } else { 0.5 }) as i16
    };
    t.to_le_bytes()
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    /// Начать фигуру. Предыдущая, если была, закрывается.
    pub fn shape(&mut self, rgba: [u8; 4], evenodd: bool) {
        self.flush();
        self.cur = Some(((rgba, if evenodd { F_EVENODD } else { 0 }), Vec::new()));
    }

    fn push(&mut self, op: u8, pts: &[(f32, f32)]) {
        let Some((_, s)) = self.cur.as_mut() else { return };
        s.push(op);
        for &(x, y) in pts {
            s.extend_from_slice(&q(x));
            s.extend_from_slice(&q(y));
        }
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.push(OP_MOVE, &[(x, y)]);
    }
    pub fn line_to(&mut self, x: f32, y: f32) {
        self.push(OP_LINE, &[(x, y)]);
    }
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.push(OP_QUAD, &[(cx, cy), (x, y)]);
    }
    pub fn cubic_to(&mut self, ax: f32, ay: f32, bx: f32, by: f32, x: f32, y: f32) {
        self.push(OP_CUBIC, &[(ax, ay), (bx, by), (x, y)]);
    }
    pub fn close(&mut self) {
        self.push(OP_CLOSE, &[]);
    }

    fn flush(&mut self) {
        if let Some(s) = self.cur.take() {
            // Фигура без единой команды — это чаще всего пустая группа из SVG. Молча выкидываем:
            // в файле она заняла бы восемь байт и ничего не нарисовала.
            if !s.1.is_empty() {
                self.shapes.push(s);
            }
        }
    }

    /// Готовые байты файла. `None` — фигур не набралось или поток одной из них не влез в u16
    /// (65 КБ на контур — это уже не иконка, и молча обрезать его нельзя).
    pub fn finish(mut self, w: u16, h: u16) -> Option<Vec<u8>> {
        self.flush();
        if self.shapes.is_empty() || self.shapes.len() > u16::MAX as usize {
            return None;
        }
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&(self.shapes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        for ((rgba, flags), ops) in &self.shapes {
            if ops.len() > u16::MAX as usize {
                return None;
            }
            out.extend_from_slice(rgba);
            out.push(*flags);
            out.push(0);
            out.extend_from_slice(&(ops.len() as u16).to_le_bytes());
            out.extend_from_slice(ops);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Квадрат 8×8 внутри вьюбокса 16×16.
    fn square() -> Vec<u8> {
        let mut w = Writer::new();
        w.shape([255, 0, 0, 255], false);
        w.move_to(4.0, 4.0);
        w.line_to(12.0, 4.0);
        w.line_to(12.0, 12.0);
        w.line_to(4.0, 12.0);
        w.close();
        w.finish(16, 16).unwrap()
    }

    #[test]
    fn roundtrip() {
        let bytes = square();
        let vg = Vg::parse(&bytes).unwrap();
        assert_eq!(vg.size(), (16, 16));
        assert_eq!(vg.shape_count(), 1);
        let s = vg.shapes().next().unwrap();
        assert_eq!(s.rgba, [255, 0, 0, 255]);
        assert!(!s.evenodd());
        let segs: Vec<Seg> = s.segs().collect();
        assert_eq!(
            segs,
            [
                Seg::Move(4.0, 4.0),
                Seg::Line(12.0, 4.0),
                Seg::Line(12.0, 12.0),
                Seg::Line(4.0, 12.0),
                Seg::Close,
            ]
        );
    }

    #[test]
    fn rejects_foreign_and_short() {
        assert!(Vg::parse(b"").is_none());
        assert!(Vg::parse(b"PNG\0not really a file").is_none());
        let bytes = square();
        assert!(Vg::parse(&bytes[..11]).is_none());
    }

    /// Дробная координата укладывается в шестнадцатые и возвращается той же.
    #[test]
    fn sixteenths_survive() {
        let mut w = Writer::new();
        w.shape([1, 2, 3, 4], true);
        w.move_to(0.0625, -0.125);
        w.line_to(1.5, 2.25);
        let bytes = w.finish(4, 4).unwrap();
        let vg = Vg::parse(&bytes).unwrap();
        let s = vg.shapes().next().unwrap();
        assert!(s.evenodd());
        let segs: Vec<Seg> = s.segs().collect();
        assert_eq!(segs, [Seg::Move(0.0625, -0.125), Seg::Line(1.5, 2.25)]);
    }

    /// Пустая фигура в файл не попадает.
    #[test]
    fn empty_shape_dropped() {
        let mut w = Writer::new();
        w.shape([0, 0, 0, 255], false);
        w.shape([0, 0, 0, 255], false);
        w.move_to(0.0, 0.0);
        w.line_to(1.0, 1.0);
        let vg_bytes = w.finish(2, 2).unwrap();
        assert_eq!(Vg::parse(&vg_bytes).unwrap().shape_count(), 1);
    }
}
