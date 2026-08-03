//! Направленная навигация фокуса между панелями (hjkl ↔ Left/Down/Up/Right).
//!
//! Геометрический поиск: от прямоугольника текущей панели ищем ближайшую в
//! заданном направлении. Предпочитаем панель, ближайшую по краю в направлении
//! движения; при равенстве — с наименьшим смещением центра по перпендикуляру.


use crate::layout::{Area, PaneId, PaneRect};

/// Направление перемещения фокуса.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Ближайшая панель от `from` в направлении `dir` среди `rects`.
///
/// `None`, если `from` не найдена или в эту сторону панелей нет.
pub fn neighbor(rects: &[PaneRect], from: PaneId, dir: Direction) -> Option<PaneId> {
    let cur = rects.iter().find(|r| r.id == from)?.area;

    let mut best: Option<((i32, i32), PaneId)> = None;
    for r in rects {
        if r.id == from {
            continue;
        }
        let a = r.area;
        // (в нужной полуплоскости?, расстояние по направлению, смещение по перпендикуляру)
        let (in_dir, primary, secondary) = match dir {
            Direction::Right => (
                a.col >= cur.col + cur.cols,
                a.col as i32 - (cur.col + cur.cols) as i32,
                perp(center_y(cur), center_y(a)),
            ),
            Direction::Left => (
                a.col + a.cols <= cur.col,
                cur.col as i32 - (a.col + a.cols) as i32,
                perp(center_y(cur), center_y(a)),
            ),
            Direction::Down => (
                a.row >= cur.row + cur.rows,
                a.row as i32 - (cur.row + cur.rows) as i32,
                perp(center_x(cur), center_x(a)),
            ),
            Direction::Up => (
                a.row + a.rows <= cur.row,
                cur.row as i32 - (a.row + a.rows) as i32,
                perp(center_x(cur), center_x(a)),
            ),
        };
        if !in_dir {
            continue;
        }
        let key = (primary.max(0), secondary);
        if best.as_ref().is_none_or(|(bk, _)| key < *bk) {
            best = Some((key, r.id));
        }
    }
    best.map(|(_, id)| id)
}

/// Центр по X в полуединицах (×2, чтобы избежать дробей).
fn center_x(a: Area) -> i32 {
    a.col as i32 * 2 + a.cols as i32
}

/// Центр по Y в полуединицах.
fn center_y(a: Area) -> i32 {
    a.row as i32 * 2 + a.rows as i32
}

/// Модуль разности центров (перпендикулярное смещение).
fn perp(c1: i32, c2: i32) -> i32 {
    (c1 - c2).abs()
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use super::*;
    use crate::layout::{SplitDirection, SplitTree};

    /// Раскладка из трёх панелей: слева 0 на всю высоту, справа сверху 1, снизу 2.
    fn three_panes() -> Vec<PaneRect> {
        let tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(SplitTree::leaf(PaneId(1))),
                second: Box::new(SplitTree::leaf(PaneId(2))),
            }),
        };
        tree.layout(
            Area {
                col: 0,
                row: 0,
                cols: 80,
                rows: 24,
            },
            1,
        )
    }

    #[test]
    fn right_from_left_pane() {
        let r = three_panes();
        // Справа от 0 — верхняя правая (1), её центр ближе к центру 0.
        assert_eq!(neighbor(&r, PaneId(0), Direction::Right), Some(PaneId(1)));
    }

    #[test]
    fn left_returns_to_pane_zero() {
        let r = three_panes();
        assert_eq!(neighbor(&r, PaneId(1), Direction::Left), Some(PaneId(0)));
        assert_eq!(neighbor(&r, PaneId(2), Direction::Left), Some(PaneId(0)));
    }

    #[test]
    fn down_and_up_between_stacked() {
        let r = three_panes();
        assert_eq!(neighbor(&r, PaneId(1), Direction::Down), Some(PaneId(2)));
        assert_eq!(neighbor(&r, PaneId(2), Direction::Up), Some(PaneId(1)));
    }

    #[test]
    fn no_neighbor_off_edge() {
        let r = three_panes();
        assert_eq!(neighbor(&r, PaneId(0), Direction::Left), None);
        assert_eq!(neighbor(&r, PaneId(0), Direction::Up), None);
        assert_eq!(neighbor(&r, PaneId(1), Direction::Up), None);
    }

    #[test]
    fn unknown_pane_is_none() {
        let r = three_panes();
        assert_eq!(neighbor(&r, PaneId(99), Direction::Right), None);
    }
}
