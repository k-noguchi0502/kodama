//! Pane layout: a binary tree of splits whose leaves are panes.

pub type PaneId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children side by side (a vertical divider).
    Horizontal,
    /// Children stacked (a horizontal divider).
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    /// Splits along `axis`, leaving `gap` between the halves.
    fn split(&self, axis: Axis, ratio: f32, gap: f32) -> (Rect, Rect, Rect) {
        match axis {
            Axis::Horizontal => {
                let avail = (self.w - gap).max(0.0);
                let a = (avail * ratio).round();
                (
                    Rect { w: a, ..*self },
                    Rect {
                        x: self.x + a,
                        w: gap.min(self.w),
                        ..*self
                    },
                    Rect {
                        x: self.x + a + gap,
                        w: avail - a,
                        ..*self
                    },
                )
            }
            Axis::Vertical => {
                let avail = (self.h - gap).max(0.0);
                let a = (avail * ratio).round();
                (
                    Rect { h: a, ..*self },
                    Rect {
                        y: self.y + a,
                        h: gap.min(self.h),
                        ..*self
                    },
                    Rect {
                        y: self.y + a + gap,
                        h: avail - a,
                        ..*self
                    },
                )
            }
        }
    }
}

enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// Share of the space given to `first`.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }
}

/// A divider between two subtrees, addressed by the path to its split node.
#[derive(Clone, Debug, PartialEq)]
pub struct Divider {
    /// The gap between the two children (the drag handle).
    pub rect: Rect,
    /// The area of the split node, to turn pointer positions into ratios.
    pub parent: Rect,
    pub axis: Axis,
    /// From the root: `false` takes `first`, `true` takes `second`.
    pub path: Vec<bool>,
}

const MIN_RATIO: f32 = 0.05;

/// A layout without the window's live state, so it can be saved and rebuilt.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Leaf(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Shape>,
        second: Box<Shape>,
    },
}

pub struct Layout {
    root: Node,
}

impl Layout {
    pub fn new(id: PaneId) -> Self {
        Self {
            root: Node::Leaf(id),
        }
    }

    /// Pane ids in reading order (left to right, top to bottom).
    pub fn ids(&self) -> Vec<PaneId> {
        fn walk(node: &Node, out: &mut Vec<PaneId>) {
            match node {
                Node::Leaf(id) => out.push(*id),
                Node::Split { first, second, .. } => {
                    walk(first, out);
                    walk(second, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Replaces pane `target` with a split holding it and `new` (after it).
    pub fn split(&mut self, target: PaneId, new: PaneId, axis: Axis) -> bool {
        fn walk(node: &mut Node, target: PaneId, new: PaneId, axis: Axis) -> bool {
            match node {
                Node::Leaf(id) if *id == target => {
                    *node = Node::Split {
                        axis,
                        ratio: 0.5,
                        first: Box::new(Node::Leaf(target)),
                        second: Box::new(Node::Leaf(new)),
                    };
                    true
                }
                Node::Leaf(_) => false,
                Node::Split { first, second, .. } => {
                    walk(first, target, new, axis) || walk(second, target, new, axis)
                }
            }
        }
        walk(&mut self.root, target, new, axis)
    }

    /// Removes a pane; its sibling takes over the parent's space. The last
    /// pane cannot be removed.
    pub fn remove(&mut self, id: PaneId) -> bool {
        fn walk(node: &mut Node, id: PaneId) -> bool {
            let Node::Split { first, second, .. } = node else {
                return false;
            };
            let keep = match (&**first, &**second) {
                (Node::Leaf(a), _) if *a == id => Some(false),
                (_, Node::Leaf(b)) if *b == id => Some(true),
                _ => None,
            };
            match keep {
                Some(take_first) => {
                    let placeholder = Box::new(Node::Leaf(0));
                    let survivor = if take_first {
                        std::mem::replace(first, placeholder)
                    } else {
                        std::mem::replace(second, placeholder)
                    };
                    *node = *survivor;
                    true
                }
                None => walk(first, id) || walk(second, id),
            }
        }
        walk(&mut self.root, id)
    }

    pub fn rects(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        fn walk(node: &Node, area: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
            match node {
                Node::Leaf(id) => out.push((*id, area)),
                Node::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => {
                    let (a, _, b) = area.split(*axis, *ratio, gap);
                    walk(first, a, gap, out);
                    walk(second, b, gap, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, area, gap, &mut out);
        out
    }

    pub fn dividers(&self, area: Rect, gap: f32) -> Vec<Divider> {
        fn walk(node: &Node, area: Rect, gap: f32, path: &mut Vec<bool>, out: &mut Vec<Divider>) {
            if let Node::Split {
                axis,
                ratio,
                first,
                second,
            } = node
            {
                let (a, rect, b) = area.split(*axis, *ratio, gap);
                out.push(Divider {
                    rect,
                    parent: area,
                    axis: *axis,
                    path: path.clone(),
                });
                path.push(false);
                walk(first, a, gap, path, out);
                path.pop();
                path.push(true);
                walk(second, b, gap, path, out);
                path.pop();
            }
        }
        let mut out = Vec::new();
        walk(&self.root, area, gap, &mut Vec::new(), &mut out);
        out
    }

    pub fn set_ratio(&mut self, path: &[bool], ratio: f32) {
        let mut node = &mut self.root;
        for &second_child in path {
            match node {
                Node::Split { first, second, .. } => {
                    node = if second_child { second } else { first };
                }
                Node::Leaf(_) => return,
            }
        }
        if let Node::Split { ratio: r, .. } = node {
            *r = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        }
    }

    /// Moves the nearest divider of the matching orientation around pane
    /// `id` towards `dir` by `delta` (a fraction of the split's size).
    pub fn resize(&mut self, id: PaneId, dir: Direction, delta: f32) {
        fn walk(node: &mut Node, id: PaneId, axis: Axis, delta: f32) -> bool {
            let Node::Split {
                axis: a,
                ratio,
                first,
                second,
            } = node
            else {
                return false;
            };
            let in_first = first.contains(id);
            if !in_first && !second.contains(id) {
                return false;
            }
            let child = if in_first { first } else { second };
            if walk(child, id, axis, delta) {
                return true;
            }
            if *a == axis {
                *ratio = (*ratio + delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                return true;
            }
            false
        }
        let sign = match dir {
            Direction::Left | Direction::Up => -1.0,
            Direction::Right | Direction::Down => 1.0,
        };
        walk(&mut self.root, id, dir.axis(), sign * delta);
    }

    /// The tree of splits, with the live pane id in each leaf.
    pub fn shape(&self) -> Shape {
        fn walk(node: &Node) -> Shape {
            match node {
                Node::Leaf(id) => Shape::Leaf(*id),
                Node::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => Shape::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(walk(first)),
                    second: Box::new(walk(second)),
                },
            }
        }
        walk(&self.root)
    }

    /// A layout with the same splits as `shape`.
    pub fn from_shape(shape: Shape) -> Self {
        fn walk(shape: Shape) -> Node {
            match shape {
                Shape::Leaf(id) => Node::Leaf(id),
                Shape::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => Node::Split {
                    axis,
                    ratio,
                    first: Box::new(walk(*first)),
                    second: Box::new(walk(*second)),
                },
            }
        }
        Self { root: walk(shape) }
    }

    /// Exchanges the positions of two panes.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        fn walk(node: &mut Node, a: PaneId, b: PaneId) {
            match node {
                Node::Leaf(id) if *id == a => *id = b,
                Node::Leaf(id) if *id == b => *id = a,
                Node::Leaf(_) => {}
                Node::Split { first, second, .. } => {
                    walk(first, a, b);
                    walk(second, a, b);
                }
            }
        }
        walk(&mut self.root, a, b);
    }

    /// The pane adjacent to `id` in `dir`: of the panes beyond its edge that
    /// overlap it on the other axis, the closest, then the most overlapping.
    pub fn neighbor(&self, id: PaneId, dir: Direction, area: Rect, gap: f32) -> Option<PaneId> {
        let rects = self.rects(area, gap);
        let (_, from) = *rects.iter().find(|(p, _)| *p == id)?;
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.0);
        rects
            .iter()
            .filter(|(p, _)| *p != id)
            .filter_map(|(p, r)| {
                let (distance, shared) = match dir {
                    Direction::Left => (
                        from.x - (r.x + r.w),
                        overlap(from.y, from.y + from.h, r.y, r.y + r.h),
                    ),
                    Direction::Right => (
                        r.x - (from.x + from.w),
                        overlap(from.y, from.y + from.h, r.y, r.y + r.h),
                    ),
                    Direction::Up => (
                        from.y - (r.y + r.h),
                        overlap(from.x, from.x + from.w, r.x, r.x + r.w),
                    ),
                    Direction::Down => (
                        r.y - (from.y + from.h),
                        overlap(from.x, from.x + from.w, r.x, r.x + r.w),
                    ),
                };
                (distance >= -0.5 && shared > 0.0).then_some((*p, distance, shared))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
            .map(|(p, _, _)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 50.0,
    };

    #[test]
    fn split_and_remove() {
        let mut l = Layout::new(1);
        assert!(l.split(1, 2, Axis::Horizontal));
        assert!(l.split(2, 3, Axis::Vertical));
        assert_eq!(l.ids(), vec![1, 2, 3]);
        let rects = l.rects(AREA, 2.0);
        assert_eq!(
            rects[0].1,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 49.0,
                h: 50.0
            }
        );
        assert_eq!(
            rects[1].1,
            Rect {
                x: 51.0,
                y: 0.0,
                w: 49.0,
                h: 24.0
            }
        );
        assert_eq!(
            rects[2].1,
            Rect {
                x: 51.0,
                y: 26.0,
                w: 49.0,
                h: 24.0
            }
        );

        assert!(l.remove(2));
        assert_eq!(l.ids(), vec![1, 3]);
        assert_eq!(
            l.rects(AREA, 2.0)[1].1,
            Rect {
                x: 51.0,
                y: 0.0,
                w: 49.0,
                h: 50.0
            }
        );
        assert!(l.remove(1));
        assert_eq!(l.ids(), vec![3]);
        assert!(!l.remove(3));
    }

    #[test]
    fn neighbours_follow_geometry() {
        let mut l = Layout::new(1);
        l.split(1, 2, Axis::Horizontal);
        l.split(2, 3, Axis::Vertical);
        assert_eq!(l.neighbor(1, Direction::Right, AREA, 2.0), Some(2));
        assert_eq!(l.neighbor(3, Direction::Left, AREA, 2.0), Some(1));
        assert_eq!(l.neighbor(2, Direction::Down, AREA, 2.0), Some(3));
        assert_eq!(l.neighbor(3, Direction::Up, AREA, 2.0), Some(2));
        assert_eq!(l.neighbor(1, Direction::Left, AREA, 2.0), None);
    }

    #[test]
    fn swapping_exchanges_places() {
        let mut l = Layout::new(1);
        l.split(1, 2, Axis::Horizontal);
        l.split(2, 3, Axis::Vertical);
        l.swap(1, 3);
        assert_eq!(l.ids(), vec![3, 2, 1]);
        assert_eq!(l.neighbor(3, Direction::Right, AREA, 2.0), Some(2));
    }

    #[test]
    fn resize_moves_the_matching_divider() {
        let mut l = Layout::new(1);
        l.split(1, 2, Axis::Horizontal);
        l.split(2, 3, Axis::Vertical);
        // Pane 3 sits in a vertical split; a horizontal resize reaches the root.
        l.resize(3, Direction::Left, 0.1);
        assert_eq!(l.rects(AREA, 0.0)[0].1.w, 40.0);
        l.resize(3, Direction::Up, 0.1);
        assert_eq!(l.rects(AREA, 0.0)[1].1.h, 20.0);

        let dividers = l.dividers(AREA, 2.0);
        assert_eq!(dividers.len(), 2);
        assert_eq!(dividers[1].path, vec![true]);
        l.set_ratio(&dividers[1].path, 0.5);
        assert_eq!(l.rects(AREA, 0.0)[1].1.h, 25.0);
    }

    #[test]
    fn shape_roundtrips_the_splits() {
        let mut l = Layout::new(1);
        l.split(1, 2, Axis::Horizontal);
        l.split(2, 3, Axis::Vertical);
        l.set_ratio(&[], 0.25);
        let again = Layout::from_shape(l.shape());
        assert_eq!(again.ids(), vec![1, 2, 3]);
        assert_eq!(again.rects(AREA, 0.0)[0].1.w, l.rects(AREA, 0.0)[0].1.w);
    }
}
