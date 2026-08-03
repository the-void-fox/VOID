//! Тайлинг панелей: дерево разбиений [`SplitTree`] и его раскладка в
//! прямоугольники [`Area`].
//!
//! Чистая геометрия в координатах **ячеек грида** (без пикселей, PTY и рендера),
//! поэтому легко тестируется. Каждый лист дерева — панель ([`PaneId`]); узлы
//! делят прямоугольник на две части по доле `ratio`, оставляя `border` ячеек на
//! разделитель между соседями. Пиксели/рендер/PTY навешиваются поверх.

/// Идентификатор панели — стабильный ключ в коллекции панелей мультиплексора.
use alloc::boxed::Box;
#[allow(unused_imports)]
use core_maths::CoreFloat;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(pub usize);

/// Ось разбиения (терминология Zellij/i3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitDirection {
    /// Делим по ширине: `first` слева, `second` справа (вертикальный разделитель).
    Vertical,
    /// Делим по высоте: `first` сверху, `second` снизу (горизонтальный разделитель).
    Horizontal,
}

/// Дерево тайлинга: лист — панель, узел — разбиение на две части.
#[derive(Clone, PartialEq, Debug)]
pub enum SplitTree {
    /// Панель.
    Leaf(PaneId),
    /// Разбиение прямоугольника на `first`/`second`.
    Split {
        /// Ось разбиения.
        direction: SplitDirection,
        /// Доля `first` от полезного места (после вычета разделителя), 0.0..=1.0.
        ratio: f32,
        /// Левая/верхняя часть.
        first: Box<SplitTree>,
        /// Правая/нижняя часть.
        second: Box<SplitTree>,
    },
}

/// Прямоугольник в координатах ячеек грида.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Area {
    /// Левый столбец (включительно).
    pub col: u16,
    /// Верхняя строка (включительно).
    pub row: u16,
    /// Ширина в ячейках.
    pub cols: u16,
    /// Высота в ячейках.
    pub rows: u16,
}

/// Раскладка одной панели: её id и занятый прямоугольник.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PaneRect {
    /// Какая панель.
    pub id: PaneId,
    /// Где она.
    pub area: Area,
}

impl SplitTree {
    /// Дерево из одной панели.
    pub fn leaf(id: PaneId) -> Self {
        SplitTree::Leaf(id)
    }

    /// Раскладывает дерево по прямоугольнику `area`, резервируя `border` ячеек на
    /// разделитель между соседями. Возвращает прямоугольник каждой панели.
    pub fn layout(&self, area: Area, border: u16) -> Vec<PaneRect> {
        let mut out = Vec::new();
        self.layout_into(area, border, &mut out);
        out
    }

    fn layout_into(&self, area: Area, border: u16, out: &mut Vec<PaneRect>) {
        match self {
            SplitTree::Leaf(id) => out.push(PaneRect { id: *id, area }),
            SplitTree::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_area(area, *direction, *ratio, border);
                first.layout_into(a, border, out);
                second.layout_into(b, border, out);
            }
        }
    }

    /// Все панели дерева слева направо / сверху вниз (порядок обхода).
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_ids(&mut out);
        out
    }

    fn collect_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            SplitTree::Leaf(id) => out.push(*id),
            SplitTree::Split { first, second, .. } => {
                first.collect_ids(out);
                second.collect_ids(out);
            }
        }
    }

    /// Разбивает лист `target` на него же (`first`) и новую панель `new`
    /// (`second`) по оси `direction`/доле `ratio`. Возвращает `false`, если
    /// `target` не найден.
    pub fn split(
        &mut self,
        target: PaneId,
        direction: SplitDirection,
        ratio: f32,
        new: PaneId,
    ) -> bool {
        match self {
            SplitTree::Leaf(id) if *id == target => {
                *self = SplitTree::Split {
                    direction,
                    ratio,
                    first: Box::new(SplitTree::Leaf(target)),
                    second: Box::new(SplitTree::Leaf(new)),
                };
                true
            }
            SplitTree::Leaf(_) => false,
            SplitTree::Split { first, second, .. } => {
                first.split(target, direction, ratio, new)
                    || second.split(target, direction, ratio, new)
            }
        }
    }

    /// Удаляет панель `target`: её родительский узел схлопывается в оставшегося
    /// соседа. Корневой одиночный лист удалить нельзя (вернёт `false`).
    pub fn close(&mut self, target: PaneId) -> bool {
        // Является ли прямой ребёнок листом-`target`? (true=first, false=second)
        let which = match self {
            SplitTree::Leaf(_) => return false,
            SplitTree::Split { first, second, .. } => {
                if matches!(**first, SplitTree::Leaf(id) if id == target) {
                    Some(true)
                } else if matches!(**second, SplitTree::Leaf(id) if id == target) {
                    Some(false)
                } else {
                    None
                }
            }
        };

        match which {
            // Прямой ребёнок — закрываемая панель: заменяем узел на соседа.
            Some(first_is_target) => {
                let old = core::mem::replace(self, SplitTree::Leaf(target));
                if let SplitTree::Split { first, second, .. } = old {
                    *self = if first_is_target { *second } else { *first };
                }
                true
            }
            // Иначе ищем глубже.
            None => match self {
                SplitTree::Split { first, second, .. } => {
                    first.close(target) || second.close(target)
                }
                SplitTree::Leaf(_) => false,
            },
        }
    }
}

/// Делит прямоугольник надвое по оси/доле, оставляя `border` ячеек между частями.
///
/// `ratio` насыщается к 0..=1; первая часть не может превысить полезное место,
/// размеры не уходят в минус (`saturating`), так что слишком маленький
/// прямоугольник даёт вырожденные (нулевые), но не паникующие части.
fn split_area(area: Area, dir: SplitDirection, ratio: f32, border: u16) -> (Area, Area) {
    let ratio = ratio.clamp(0.0, 1.0);
    match dir {
        SplitDirection::Vertical => {
            let usable = area.cols.saturating_sub(border);
            let first_w = scale(usable, ratio);
            let second_w = usable - first_w;
            let first = Area {
                cols: first_w,
                ..area
            };
            let second = Area {
                col: area.col + first_w + border,
                cols: second_w,
                ..area
            };
            (first, second)
        }
        SplitDirection::Horizontal => {
            let usable = area.rows.saturating_sub(border);
            let first_h = scale(usable, ratio);
            let second_h = usable - first_h;
            let first = Area {
                rows: first_h,
                ..area
            };
            let second = Area {
                row: area.row + first_h + border,
                rows: second_h,
                ..area
            };
            (first, second)
        }
    }
}

/// `total * ratio`, округлённое и ограниченное сверху `total`.
fn scale(total: u16, ratio: f32) -> u16 {
    ((total as f32 * ratio).round() as u16).min(total)
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;

    const FULL: Area = Area {
        col: 0,
        row: 0,
        cols: 80,
        rows: 24,
    };

    #[test]
    fn single_leaf_fills_area() {
        let tree = SplitTree::leaf(PaneId(0));
        let rects = tree.layout(FULL, 1);
        assert_eq!(rects, vec![PaneRect {
            id: PaneId(0),
            area: FULL
        }]);
    }

    #[test]
    fn vertical_split_halves_width_with_divider() {
        let tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        let rects = tree.layout(FULL, 1);
        // usable = 79; first = round(39.5) = 40; second = 39; разделитель в col 40.
        assert_eq!(rects[0].area, Area {
            col: 0,
            row: 0,
            cols: 40,
            rows: 24
        });
        assert_eq!(rects[1].area, Area {
            col: 41,
            row: 0,
            cols: 39,
            rows: 24
        });
    }

    #[test]
    fn horizontal_split_divides_height() {
        let tree = SplitTree::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        let rects = tree.layout(FULL, 1);
        // usable = 23; first = round(11.5) = 12; second = 11; разделитель в row 12.
        assert_eq!(rects[0].area, Area {
            col: 0,
            row: 0,
            cols: 80,
            rows: 12
        });
        assert_eq!(rects[1].area, Area {
            col: 0,
            row: 13,
            cols: 80,
            rows: 11
        });
    }

    #[test]
    fn ratio_biases_first() {
        let tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.7,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        let rects = tree.layout(FULL, 0);
        // border 0: usable = 80; first = round(56.0) = 56; second = 24.
        assert_eq!(rects[0].area.cols, 56);
        assert_eq!(rects[1].area.cols, 24);
        assert_eq!(rects[1].area.col, 56);
    }

    #[test]
    fn nested_split_tiles_three_panes() {
        // Вертикально: слева панель 0, справа — горизонтальное разбиение 1/2.
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
        let rects = tree.layout(FULL, 1);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].id, PaneId(0));
        assert_eq!(rects[1].id, PaneId(1));
        assert_eq!(rects[2].id, PaneId(2));
        // Правая колонка начинается за разделителем, делится по высоте.
        assert_eq!(rects[1].area.col, 41);
        assert_eq!(rects[2].area.col, 41);
        assert!(rects[1].area.row < rects[2].area.row);
        // Панели не налезают друг на друга по высоте (разделитель между ними).
        assert!(rects[1].area.row + rects[1].area.rows < rects[2].area.row);
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        // Ширина меньше разделителя — части вырождаются в ноль, но без паники.
        let rects = tree.layout(
            Area {
                col: 0,
                row: 0,
                cols: 1,
                rows: 1,
            },
            2,
        );
        assert_eq!(rects[0].area.cols, 0);
        assert_eq!(rects[1].area.cols, 0);
    }

    #[test]
    fn pane_ids_in_traversal_order() {
        let tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(7))),
            second: Box::new(SplitTree::leaf(PaneId(3))),
        };
        assert_eq!(tree.pane_ids(), vec![PaneId(7), PaneId(3)]);
    }

    #[test]
    fn split_leaf_creates_node() {
        let mut tree = SplitTree::leaf(PaneId(0));
        assert!(tree.split(PaneId(0), SplitDirection::Vertical, 0.5, PaneId(1)));
        assert_eq!(tree, SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        });
    }

    #[test]
    fn split_unknown_target_is_noop() {
        let mut tree = SplitTree::leaf(PaneId(0));
        assert!(!tree.split(PaneId(9), SplitDirection::Vertical, 0.5, PaneId(1)));
        assert_eq!(tree, SplitTree::leaf(PaneId(0)));
    }

    #[test]
    fn split_nested_target() {
        let mut tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        // Разбиваем правую панель по высоте.
        assert!(tree.split(PaneId(1), SplitDirection::Horizontal, 0.5, PaneId(2)));
        assert_eq!(tree.pane_ids(), vec![PaneId(0), PaneId(1), PaneId(2)]);
    }

    #[test]
    fn close_collapses_parent_into_sibling() {
        let mut tree = SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(1))),
        };
        assert!(tree.close(PaneId(0)));
        assert_eq!(tree, SplitTree::leaf(PaneId(1)));
    }

    #[test]
    fn close_root_leaf_fails() {
        let mut tree = SplitTree::leaf(PaneId(0));
        assert!(!tree.close(PaneId(0)));
        assert_eq!(tree, SplitTree::leaf(PaneId(0)));
    }

    #[test]
    fn close_nested_keeps_other_panes() {
        // (0 | (1 / 2)) → закрываем 1 → (0 | 2)
        let mut tree = SplitTree::Split {
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
        assert!(tree.close(PaneId(1)));
        assert_eq!(tree.pane_ids(), vec![PaneId(0), PaneId(2)]);
        // Правая ветка схлопнулась в лист 2.
        assert_eq!(tree, SplitTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(SplitTree::leaf(PaneId(0))),
            second: Box::new(SplitTree::leaf(PaneId(2))),
        });
    }
}
