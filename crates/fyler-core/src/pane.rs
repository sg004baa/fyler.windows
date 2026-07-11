//! 複数paneの識別子・レイアウト・操作。
//!
//! エディタ実装やGUI座標系に依存しない純粋な状態として、appとGUIの正典に使う。

use std::fmt;

/// アプリケーションセッション内でpaneを一意に識別するID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(u64);

impl PaneId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 二分木の子を並べる方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    /// 上下に並べる。
    Horizontal,
    /// 左右に並べる。
    Vertical,
}

/// 幾何的に隣接するpaneを探す方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

/// ユーザーが要求できるpane操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    SplitHorizontal,
    SplitVertical,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    FocusNext,
    FocusPrevious,
    Close,
}

/// paneの二分レイアウト。
#[derive(Debug, Clone, PartialEq)]
pub enum PaneLayout {
    Leaf(PaneId),
    Split {
        direction: SplitDirection,
        /// `first`へ割り当てる比率。現在はsplit時に0.5を使う。
        ratio: f32,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    pub const fn leaf(id: PaneId) -> Self {
        Self::Leaf(id)
    }

    /// `target` leafを二分し、後半側へ`new_id`を追加した新しいlayoutを返す。
    /// targetが存在しない場合は`None`。
    pub fn split(&self, target: PaneId, direction: SplitDirection, new_id: PaneId) -> Option<Self> {
        if self.contains(new_id) {
            return None;
        }
        match self {
            Self::Leaf(id) if *id == target => Some(Self::Split {
                direction,
                ratio: 0.5,
                first: Box::new(Self::Leaf(*id)),
                second: Box::new(Self::Leaf(new_id)),
            }),
            Self::Leaf(_) => None,
            Self::Split {
                direction: own_direction,
                ratio,
                first,
                second,
            } => {
                if let Some(first) = first.split(target, direction, new_id) {
                    Some(Self::Split {
                        direction: *own_direction,
                        ratio: *ratio,
                        first: Box::new(first),
                        second: second.clone(),
                    })
                } else {
                    second
                        .split(target, direction, new_id)
                        .map(|second| Self::Split {
                            direction: *own_direction,
                            ratio: *ratio,
                            first: first.clone(),
                            second: Box::new(second),
                        })
                }
            }
        }
    }

    /// `id` leafを除去して一人子を繰り上げる。最後のleafや未知IDは`None`。
    pub fn close(&self, id: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(_) => None,
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => match (&**first, &**second) {
                (Self::Leaf(first_id), _) if *first_id == id => Some((**second).clone()),
                (_, Self::Leaf(second_id)) if *second_id == id => Some((**first).clone()),
                _ => {
                    if first.contains(id) {
                        first.close(id).map(|first| Self::Split {
                            direction: *direction,
                            ratio: *ratio,
                            first: Box::new(first),
                            second: second.clone(),
                        })
                    } else if second.contains(id) {
                        second.close(id).map(|second| Self::Split {
                            direction: *direction,
                            ratio: *ratio,
                            first: first.clone(),
                            second: Box::new(second),
                        })
                    } else {
                        None
                    }
                }
            },
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// `id`と同じ分割nodeに属するsibling側の最寄りleafを返す。
    pub fn sibling_leaf(&self, id: PaneId) -> Option<PaneId> {
        match self {
            Self::Leaf(_) => None,
            Self::Split { first, second, .. } => {
                if matches!(&**first, Self::Leaf(first_id) if *first_id == id) {
                    second.leaves().first().copied()
                } else if matches!(&**second, Self::Leaf(second_id) if *second_id == id) {
                    first.leaves().last().copied()
                } else if first.contains(id) {
                    first.sibling_leaf(id)
                } else if second.contains(id) {
                    second.sibling_leaf(id)
                } else {
                    None
                }
            }
        }
    }

    /// 描画順(上→下、左→右)でleafを列挙する。
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    fn collect_leaves(&self, leaves: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => leaves.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_leaves(leaves);
                second.collect_leaves(leaves);
            }
        }
    }

    /// 分割木を単位矩形へ投影し、指定方向で最も近いleafを返す。
    pub fn focus_neighbor(&self, active: PaneId, direction: FocusDirection) -> Option<PaneId> {
        let mut rects = Vec::new();
        self.collect_rects(Rect::UNIT, &mut rects);
        let (_, active_rect) = rects.iter().find(|(id, _)| *id == active)?;
        let active_rect = *active_rect;
        let (ax, ay) = active_rect.center();

        rects
            .into_iter()
            .filter(|(id, _)| *id != active)
            .filter_map(|(id, rect)| {
                let (x, y) = rect.center();
                let (overlaps, primary, secondary) = match direction {
                    FocusDirection::Left if x < ax => (
                        ranges_overlap(active_rect.top, active_rect.bottom, rect.top, rect.bottom),
                        ax - x,
                        (ay - y).abs(),
                    ),
                    FocusDirection::Right if x > ax => (
                        ranges_overlap(active_rect.top, active_rect.bottom, rect.top, rect.bottom),
                        x - ax,
                        (ay - y).abs(),
                    ),
                    FocusDirection::Up if y < ay => (
                        ranges_overlap(active_rect.left, active_rect.right, rect.left, rect.right),
                        ay - y,
                        (ax - x).abs(),
                    ),
                    FocusDirection::Down if y > ay => (
                        ranges_overlap(active_rect.left, active_rect.right, rect.left, rect.right),
                        y - ay,
                        (ax - x).abs(),
                    ),
                    _ => return None,
                };
                Some((id, !overlaps, primary, secondary))
            })
            .min_by(|left, right| {
                left.1
                    .cmp(&right.1)
                    .then_with(|| left.2.total_cmp(&right.2))
                    .then_with(|| left.3.total_cmp(&right.3))
                    .then_with(|| left.0.cmp(&right.0))
            })
            .map(|(id, _, _, _)| id)
    }

    fn collect_rects(&self, rect: Rect, output: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Leaf(id) => output.push((*id, rect)),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let ratio = ratio.clamp(0.0, 1.0);
                let (first_rect, second_rect) = rect.split(*direction, ratio);
                first.collect_rects(first_rect, output);
                second.collect_rects(second_rect, output);
            }
        }
    }
}

fn ranges_overlap(a_start: f32, a_end: f32, b_start: f32, b_end: f32) -> bool {
    a_start < b_end && b_start < a_end
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Rect {
    const UNIT: Self = Self {
        left: 0.0,
        top: 0.0,
        right: 1.0,
        bottom: 1.0,
    };

    fn center(self) -> (f32, f32) {
        (
            (self.left + self.right) / 2.0,
            (self.top + self.bottom) / 2.0,
        )
    }

    fn split(self, direction: SplitDirection, ratio: f32) -> (Self, Self) {
        match direction {
            SplitDirection::Horizontal => {
                let middle = self.top + (self.bottom - self.top) * ratio;
                (
                    Self {
                        bottom: middle,
                        ..self
                    },
                    Self {
                        top: middle,
                        ..self
                    },
                )
            }
            SplitDirection::Vertical => {
                let middle = self.left + (self.right - self.left) * ratio;
                (
                    Self {
                        right: middle,
                        ..self
                    },
                    Self {
                        left: middle,
                        ..self
                    },
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u64) -> PaneId {
        PaneId::new(value)
    }

    #[test]
    fn split_replaces_only_target_leaf_and_preserves_order() {
        let layout = PaneLayout::leaf(id(1))
            .split(id(1), SplitDirection::Vertical, id(2))
            .unwrap()
            .split(id(1), SplitDirection::Horizontal, id(3))
            .unwrap();

        assert_eq!(layout.leaves(), vec![id(1), id(3), id(2)]);
        assert_eq!(layout.split(id(99), SplitDirection::Vertical, id(4)), None);
    }

    #[test]
    fn close_collapses_parent_and_rejects_last_or_unknown_leaf() {
        let layout = PaneLayout::leaf(id(1))
            .split(id(1), SplitDirection::Vertical, id(2))
            .unwrap()
            .split(id(2), SplitDirection::Horizontal, id(3))
            .unwrap();

        assert_eq!(layout.close(id(2)).unwrap().leaves(), vec![id(1), id(3)]);
        assert_eq!(layout.close(id(99)), None);
        assert_eq!(PaneLayout::leaf(id(1)).close(id(1)), None);
        assert_eq!(layout.sibling_leaf(id(2)), Some(id(3)));
        assert_eq!(layout.sibling_leaf(id(3)), Some(id(2)));
    }

    #[test]
    fn focus_uses_split_geometry() {
        let layout = PaneLayout::leaf(id(1))
            .split(id(1), SplitDirection::Vertical, id(2))
            .unwrap()
            .split(id(2), SplitDirection::Horizontal, id(3))
            .unwrap();

        assert_eq!(
            layout.focus_neighbor(id(1), FocusDirection::Right),
            Some(id(2))
        );
        assert_eq!(
            layout.focus_neighbor(id(2), FocusDirection::Down),
            Some(id(3))
        );
        assert_eq!(
            layout.focus_neighbor(id(3), FocusDirection::Left),
            Some(id(1))
        );
        assert_eq!(layout.focus_neighbor(id(1), FocusDirection::Left), None);
    }
}
