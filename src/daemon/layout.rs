//! Binary space partition tree for pane layout.
//!
//! Every pane tiles edge-to-edge and draws its own border, so adjacent panes show
//! touching borders — that is the intended look, not an artifact.
//!
//! Geometry is computed here and nowhere else. The daemon ships finished rects to the
//! client so there is exactly one source of truth for where a pane lives.

use std::collections::HashMap;

use crate::proto::{Dir, PaneId, Rect};

/// Smallest cell rect a pane may occupy: 1 border either side plus useful content.
const MIN_W: u16 = 10;
const MIN_H: u16 = 4;

/// How children of a split are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Side by side — `a` left, `b` right, divided by a vertical line.
    Horizontal,
    /// Stacked — `a` top, `b` bottom, divided by a horizontal line.
    Vertical,
}

impl Axis {
    fn of(dir: Dir) -> Axis {
        if dir.is_horizontal() {
            Axis::Horizontal
        } else {
            Axis::Vertical
        }
    }
}

#[derive(Debug, Clone)]
pub enum Node {
    Leaf(PaneId),
    Split { id: u32, axis: Axis, ratio: f32, a: Box<Node>, b: Box<Node> },
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    root: Option<Node>,
    next_node: u32,
}

/// Result of walking the tree against a concrete area.
#[derive(Debug, Default)]
pub struct Geometry {
    /// Cell rect per pane, including its border.
    pub panes: HashMap<PaneId, Rect>,
    /// Rect covering each split node, needed to convert a resize in cells into a ratio.
    pub splits: HashMap<u32, (Axis, Rect)>,
    /// Panes in left-to-right, top-to-bottom order. Stable ordering for tab bars and
    /// `pane.list`, which would otherwise inherit HashMap iteration order.
    pub order: Vec<PaneId>,
}

impl Layout {
    pub fn new() -> Self {
        Self { root: None, next_node: 0 }
    }

    pub fn single(pane: PaneId) -> Self {
        Self { root: Some(Node::Leaf(pane)), next_node: 0 }
    }

    /// Adopt an externally built tree, reassigning split ids so they are unique and
    /// contiguous. Restore builds trees from disk where ids carry no meaning.
    pub fn from_root(root: Node) -> Self {
        let mut next = 0u32;
        let root = renumber(root, &mut next);
        Self { root: Some(root), next_node: next }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn root(&self) -> Option<&Node> {
        self.root.as_ref()
    }

    /// Panes in stable visual order.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            collect(r, &mut out);
        }
        out
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.panes().contains(&pane)
    }

    /// Split `target`, putting `new_pane` on the `dir` side of it.
    ///
    /// Returns false when the resulting halves would be unusably small — refusing is
    /// better than creating a pane too small to hold a prompt.
    pub fn split(&mut self, target: PaneId, dir: Dir, new_pane: PaneId, area: Rect) -> bool {
        let geo = self.geometry(area);
        let Some(rect) = geo.panes.get(&target) else { return false };
        let axis = Axis::of(dir);
        let fits = match axis {
            Axis::Horizontal => rect.w >= MIN_W * 2,
            Axis::Vertical => rect.h >= MIN_H * 2,
        };
        if !fits {
            return false;
        }

        let id = self.next_node;
        self.next_node += 1;
        // `dir` names where the *new* pane goes, so Left/Up put it in slot `a`.
        let new_first = matches!(dir, Dir::Left | Dir::Up);
        let Some(root) = self.root.take() else { return false };
        self.root = Some(replace_leaf(root, target, |leaf| {
            let (a, b) = if new_first {
                (Box::new(Node::Leaf(new_pane)), Box::new(leaf))
            } else {
                (Box::new(leaf), Box::new(Node::Leaf(new_pane)))
            };
            Node::Split { id, axis, ratio: 0.5, a, b }
        }));
        true
    }

    /// Remove a pane, collapsing its parent split into the surviving sibling.
    pub fn close(&mut self, pane: PaneId) -> bool {
        let Some(root) = self.root.take() else { return false };
        match remove_leaf(root, pane) {
            Removed::Gone => {
                self.root = None;
                true
            }
            Removed::Replaced(node) => {
                self.root = Some(node);
                true
            }
            Removed::Missing(node) => {
                self.root = Some(node);
                false
            }
        }
    }

    /// Move the divider nearest to `pane` in direction `dir` by `cells`.
    ///
    /// `Dir::Right` always moves the relevant divider right, whether the pane sits on the
    /// left or right of it — matching tmux's `resize-pane -R` feel.
    pub fn resize(&mut self, pane: PaneId, dir: Dir, cells: u16, area: Rect) -> bool {
        let geo = self.geometry(area);
        let want = Axis::of(dir);
        let Some(root) = &self.root else { return false };
        let Some(split_id) = nearest_split(root, pane, want) else { return false };
        let Some((_, srect)) = geo.splits.get(&split_id).copied() else { return false };

        let extent = match want {
            Axis::Horizontal => srect.w,
            Axis::Vertical => srect.h,
        };
        if extent == 0 {
            return false;
        }
        let delta = cells as f32 / extent as f32;
        let signed = match dir {
            Dir::Right | Dir::Down => delta,
            Dir::Left | Dir::Up => -delta,
        };

        // Clamp so neither side drops below its minimum.
        let min_frac = match want {
            Axis::Horizontal => MIN_W as f32 / extent as f32,
            Axis::Vertical => MIN_H as f32 / extent as f32,
        };
        if min_frac >= 0.5 {
            return false;
        }

        let Some(root) = self.root.take() else { return false };
        self.root = Some(adjust_ratio(root, split_id, signed, min_frac));
        true
    }

    /// Nearest pane in `dir`, resolved purely by rectangle geometry rather than tree
    /// position. Tree-relative movement surprises people; spatial movement matches what
    /// they see on screen.
    pub fn focus_dir(&self, from: PaneId, dir: Dir, area: Rect) -> Option<PaneId> {
        let geo = self.geometry(area);
        let cur = *geo.panes.get(&from)?;
        let mut best: Option<(PaneId, u16, u16)> = None; // (pane, distance, -overlap)

        for (&pane, &r) in &geo.panes {
            if pane == from {
                continue;
            }
            // Distance along the axis of travel, and overlap across it. A candidate must
            // be strictly on the requested side and share some perpendicular extent.
            let (dist, overlap) = match dir {
                Dir::Right => {
                    if r.x < cur.x + cur.w {
                        continue;
                    }
                    (r.x - (cur.x + cur.w), span_overlap(cur.y, cur.h, r.y, r.h))
                }
                Dir::Left => {
                    if r.x + r.w > cur.x {
                        continue;
                    }
                    (cur.x - (r.x + r.w), span_overlap(cur.y, cur.h, r.y, r.h))
                }
                Dir::Down => {
                    if r.y < cur.y + cur.h {
                        continue;
                    }
                    (r.y - (cur.y + cur.h), span_overlap(cur.x, cur.w, r.x, r.w))
                }
                Dir::Up => {
                    if r.y + r.h > cur.y {
                        continue;
                    }
                    (cur.y - (r.y + r.h), span_overlap(cur.x, cur.w, r.x, r.w))
                }
            };
            if overlap == 0 {
                continue;
            }
            let key = (pane, dist, u16::MAX - overlap);
            match &best {
                None => best = Some(key),
                Some((_, bd, bo)) if (dist, u16::MAX - overlap) < (*bd, *bo) => best = Some(key),
                _ => {}
            }
        }
        best.map(|(p, _, _)| p)
    }

    /// Exchange two panes' positions in the tree.
    pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
        if a == b || !self.contains(a) || !self.contains(b) {
            return false;
        }
        if let Some(root) = self.root.take() {
            self.root = Some(swap_leaves(root, a, b));
        }
        true
    }

    /// Walk the tree, assigning a rect to every pane and split.
    pub fn geometry(&self, area: Rect) -> Geometry {
        let mut geo = Geometry::default();
        if let Some(root) = &self.root {
            walk(root, area, &mut geo);
            collect(root, &mut geo.order);
        }
        geo
    }

    /// How many panes a named preset wants. The caller spawns that many, then calls
    /// [`Layout::preset`].
    pub fn preset_pane_count(name: &str) -> Option<usize> {
        Some(match name {
            "solo" => 1,
            "duo" => 2,
            "trio" | "dev" => 3,
            "quad" => 4,
            _ => return None,
        })
    }

    /// Build a named layout over exactly the panes given.
    pub fn preset(name: &str, panes: &[PaneId]) -> Option<Layout> {
        let want = Self::preset_pane_count(name)?;
        if panes.len() != want {
            return None;
        }
        let mut l = Layout::new();
        let mut id = 0u32;
        let mut node = |axis, ratio, a, b| {
            let n = Node::Split { id, axis, ratio, a: Box::new(a), b: Box::new(b) };
            id += 1;
            n
        };
        let leaf = |i: usize| Node::Leaf(panes[i]);
        let root = match name {
            "solo" => leaf(0),
            "duo" => node(Axis::Horizontal, 0.5, leaf(0), leaf(1)),
            // One tall pane on the left, two stacked on the right.
            "trio" => {
                let right = node(Axis::Vertical, 0.5, leaf(1), leaf(2));
                node(Axis::Horizontal, 0.5, leaf(0), right)
            }
            // A large working pane with a short logs strip beneath, plus a side column.
            "dev" => {
                let left = node(Axis::Vertical, 0.75, leaf(0), leaf(1));
                node(Axis::Horizontal, 0.65, left, leaf(2))
            }
            "quad" => {
                let top = node(Axis::Horizontal, 0.5, leaf(0), leaf(1));
                let bottom = node(Axis::Horizontal, 0.5, leaf(2), leaf(3));
                node(Axis::Vertical, 0.5, top, bottom)
            }
            _ => return None,
        };
        l.root = Some(root);
        l.next_node = id;
        Some(l)
    }
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

fn span_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start + a_len;
    let b_end = b_start + b_len;
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn renumber(n: Node, next: &mut u32) -> Node {
    match n {
        Node::Leaf(p) => Node::Leaf(p),
        Node::Split { axis, ratio, a, b, .. } => {
            let id = *next;
            *next += 1;
            Node::Split {
                id,
                axis,
                ratio,
                a: Box::new(renumber(*a, next)),
                b: Box::new(renumber(*b, next)),
            }
        }
    }
}

fn collect(n: &Node, out: &mut Vec<PaneId>) {
    match n {
        Node::Leaf(p) => out.push(*p),
        Node::Split { a, b, .. } => {
            collect(a, out);
            collect(b, out);
        }
    }
}

fn walk(n: &Node, area: Rect, geo: &mut Geometry) {
    match n {
        Node::Leaf(p) => {
            geo.panes.insert(*p, area);
        }
        Node::Split { id, axis, ratio, a, b } => {
            geo.splits.insert(*id, (*axis, area));
            let (ra, rb) = divide(area, *axis, *ratio);
            walk(a, ra, geo);
            walk(b, rb, geo);
        }
    }
}

fn divide(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    let r = ratio.clamp(0.05, 0.95);
    match axis {
        Axis::Horizontal => {
            let wa = ((area.w as f32 * r).round() as u16).clamp(1, area.w.saturating_sub(1));
            (
                Rect::new(area.x, area.y, wa, area.h),
                Rect::new(area.x + wa, area.y, area.w - wa, area.h),
            )
        }
        Axis::Vertical => {
            let ha = ((area.h as f32 * r).round() as u16).clamp(1, area.h.saturating_sub(1));
            (
                Rect::new(area.x, area.y, area.w, ha),
                Rect::new(area.x, area.y + ha, area.w, area.h - ha),
            )
        }
    }
}

fn replace_leaf(n: Node, target: PaneId, f: impl FnOnce(Node) -> Node + Copy) -> Node {
    match n {
        Node::Leaf(p) if p == target => f(Node::Leaf(p)),
        Node::Leaf(p) => Node::Leaf(p),
        Node::Split { id, axis, ratio, a, b } => Node::Split {
            id,
            axis,
            ratio,
            a: Box::new(replace_leaf(*a, target, f)),
            b: Box::new(replace_leaf(*b, target, f)),
        },
    }
}

enum Removed {
    /// The whole subtree disappeared.
    Gone,
    /// Subtree survives in changed form.
    Replaced(Node),
    /// Target wasn't in here; tree returned untouched.
    Missing(Node),
}

fn remove_leaf(n: Node, target: PaneId) -> Removed {
    match n {
        Node::Leaf(p) if p == target => Removed::Gone,
        Node::Leaf(p) => Removed::Missing(Node::Leaf(p)),
        Node::Split { id, axis, ratio, a, b } => match remove_leaf(*a, target) {
            // Parent collapses into the surviving sibling; the split node vanishes.
            Removed::Gone => Removed::Replaced(*b),
            Removed::Replaced(na) => Removed::Replaced(Node::Split {
                id,
                axis,
                ratio,
                a: Box::new(na),
                b,
            }),
            Removed::Missing(na) => match remove_leaf(*b, target) {
                Removed::Gone => Removed::Replaced(na),
                Removed::Replaced(nb) => Removed::Replaced(Node::Split {
                    id,
                    axis,
                    ratio,
                    a: Box::new(na),
                    b: Box::new(nb),
                }),
                Removed::Missing(nb) => Removed::Missing(Node::Split {
                    id,
                    axis,
                    ratio,
                    a: Box::new(na),
                    b: Box::new(nb),
                }),
            },
        },
    }
}

/// Innermost split above `pane` whose axis matches `want`.
fn nearest_split(n: &Node, pane: PaneId, want: Axis) -> Option<u32> {
    fn find(n: &Node, pane: PaneId, want: Axis, best: Option<u32>) -> Option<u32> {
        match n {
            // `best` holds the deepest matching-axis ancestor seen on the way down.
            Node::Leaf(p) if *p == pane => best,
            Node::Leaf(_) => None,
            Node::Split { id, axis, a, b, .. } => {
                let next = if *axis == want { Some(*id) } else { best };
                find(a, pane, want, next).or_else(|| find(b, pane, want, next))
            }
        }
    }
    find(n, pane, want, None)
}

fn adjust_ratio(n: Node, split_id: u32, delta: f32, min_frac: f32) -> Node {
    match n {
        Node::Leaf(p) => Node::Leaf(p),
        Node::Split { id, axis, ratio, a, b } => {
            let ratio = if id == split_id {
                (ratio + delta).clamp(min_frac, 1.0 - min_frac)
            } else {
                ratio
            };
            Node::Split {
                id,
                axis,
                ratio,
                a: Box::new(adjust_ratio(*a, split_id, delta, min_frac)),
                b: Box::new(adjust_ratio(*b, split_id, delta, min_frac)),
            }
        }
    }
}

fn swap_leaves(n: Node, x: PaneId, y: PaneId) -> Node {
    match n {
        Node::Leaf(p) if p == x => Node::Leaf(y),
        Node::Leaf(p) if p == y => Node::Leaf(x),
        Node::Leaf(p) => Node::Leaf(p),
        Node::Split { id, axis, ratio, a, b } => Node::Split {
            id,
            axis,
            ratio,
            a: Box::new(swap_leaves(*a, x, y)),
            b: Box::new(swap_leaves(*b, x, y)),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0, y: 0, w: 120, h: 40 };

    fn quad() -> Layout {
        // 1|2 over 3|4
        let mut l = Layout::single(1);
        assert!(l.split(1, Dir::Down, 3, AREA));
        assert!(l.split(1, Dir::Right, 2, AREA));
        assert!(l.split(3, Dir::Right, 4, AREA));
        l
    }

    #[test]
    fn split_places_new_pane_on_requested_side() {
        let mut l = Layout::single(1);
        assert!(l.split(1, Dir::Right, 2, AREA));
        let geo = l.geometry(AREA);
        assert!(geo.panes[&1].x < geo.panes[&2].x, "new pane should be to the right");

        let mut l = Layout::single(1);
        assert!(l.split(1, Dir::Left, 2, AREA));
        let geo = l.geometry(AREA);
        assert!(geo.panes[&2].x < geo.panes[&1].x, "new pane should be to the left");
    }

    #[test]
    fn split_refuses_when_halves_would_be_too_small() {
        let tiny = Rect::new(0, 0, MIN_W * 2 - 1, 40);
        let mut l = Layout::single(1);
        assert!(!l.split(1, Dir::Right, 2, tiny));
        assert_eq!(l.panes().len(), 1, "a refused split must not mutate the tree");
    }

    #[test]
    fn panes_tile_the_area_without_gaps_or_overlap() {
        let l = quad();
        let geo = l.geometry(AREA);
        assert_eq!(geo.panes.len(), 4);

        let covered: u32 = geo.panes.values().map(|r| r.w as u32 * r.h as u32).sum();
        assert_eq!(covered, AREA.w as u32 * AREA.h as u32, "tiling must be exact");

        let rects: Vec<_> = geo.panes.values().copied().collect();
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let ox = span_overlap(a.x, a.w, b.x, b.w);
                let oy = span_overlap(a.y, a.h, b.y, b.h);
                assert!(ox == 0 || oy == 0, "panes {a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn close_collapses_parent_into_sibling() {
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        assert!(l.close(2));
        assert_eq!(l.panes(), vec![1]);
        // The surviving pane must reclaim the entire area, not keep half of it.
        assert_eq!(l.geometry(AREA).panes[&1], AREA);
    }

    #[test]
    fn close_last_pane_empties_the_layout() {
        let mut l = Layout::single(1);
        assert!(l.close(1));
        assert!(l.is_empty());
        assert!(l.geometry(AREA).panes.is_empty());
    }

    #[test]
    fn close_unknown_pane_is_a_noop() {
        let mut l = quad();
        assert!(!l.close(99));
        assert_eq!(l.panes().len(), 4, "tree must survive a miss intact");
    }

    #[test]
    fn closing_every_pane_leaves_no_orphans() {
        let mut l = quad();
        for p in [2, 4, 3] {
            assert!(l.close(p));
        }
        assert_eq!(l.panes(), vec![1]);
        assert_eq!(l.geometry(AREA).panes[&1], AREA);
    }

    #[test]
    fn focus_dir_is_symmetric_across_a_split() {
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        assert_eq!(l.focus_dir(1, Dir::Right, AREA), Some(2));
        assert_eq!(l.focus_dir(2, Dir::Left, AREA), Some(1));
        assert_eq!(l.focus_dir(1, Dir::Left, AREA), None, "nothing to the left of the leftmost");
        assert_eq!(l.focus_dir(1, Dir::Up, AREA), None);
    }

    #[test]
    fn focus_dir_navigates_a_quad_grid() {
        let l = quad();
        assert_eq!(l.focus_dir(1, Dir::Right, AREA), Some(2));
        assert_eq!(l.focus_dir(1, Dir::Down, AREA), Some(3));
        assert_eq!(l.focus_dir(4, Dir::Up, AREA), Some(2));
        assert_eq!(l.focus_dir(4, Dir::Left, AREA), Some(3));
        assert_eq!(l.focus_dir(2, Dir::Right, AREA), None);
    }

    #[test]
    fn focus_dir_ignores_panes_with_no_perpendicular_overlap() {
        // 1 spans the full height on the left; 2 over 3 on the right.
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        l.split(2, Dir::Down, 3, AREA);
        // From 3 (lower right), nothing lies to the right.
        assert_eq!(l.focus_dir(3, Dir::Right, AREA), None);
        // Both right-hand panes see 1 to their left because 1 spans both.
        assert_eq!(l.focus_dir(2, Dir::Left, AREA), Some(1));
        assert_eq!(l.focus_dir(3, Dir::Left, AREA), Some(1));
    }

    #[test]
    fn resize_moves_divider_and_keeps_ratio_in_bounds() {
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        let before = l.geometry(AREA).panes[&1].w;
        assert!(l.resize(1, Dir::Right, 10, AREA));
        let after = l.geometry(AREA).panes[&1].w;
        assert!(after > before, "Dir::Right should widen the left pane");

        // Hammer the divider well past either end; it must clamp, not invert.
        for _ in 0..200 {
            l.resize(1, Dir::Left, 20, AREA);
        }
        let geo = l.geometry(AREA);
        assert!(geo.panes[&1].w >= 1 && geo.panes[&2].w >= 1);
        assert_eq!(geo.panes[&1].w + geo.panes[&2].w, AREA.w);
    }

    #[test]
    fn resize_from_either_side_moves_the_same_divider_the_same_way() {
        let mut l1 = Layout::single(1);
        l1.split(1, Dir::Right, 2, AREA);
        let mut l2 = l1.clone();

        l1.resize(1, Dir::Right, 8, AREA);
        l2.resize(2, Dir::Right, 8, AREA);
        assert_eq!(
            l1.geometry(AREA).panes[&1].w,
            l2.geometry(AREA).panes[&1].w,
            "Dir::Right means 'divider moves right' regardless of which side is focused"
        );
    }

    #[test]
    fn resize_needs_a_matching_axis_ancestor() {
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        // Only a horizontal split exists, so a vertical resize has no divider to move.
        assert!(!l.resize(1, Dir::Down, 5, AREA));
    }

    #[test]
    fn resize_picks_the_innermost_matching_split() {
        // Outer horizontal split, with another horizontal split inside its right half.
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        l.split(2, Dir::Right, 3, AREA);
        let before = l.geometry(AREA);
        l.resize(2, Dir::Right, 6, AREA);
        let after = l.geometry(AREA);
        assert_eq!(before.panes[&1].w, after.panes[&1].w, "outer divider must not move");
        assert!(after.panes[&2].w > before.panes[&2].w);
    }

    #[test]
    fn swap_exchanges_positions() {
        let mut l = Layout::single(1);
        l.split(1, Dir::Right, 2, AREA);
        let before = l.geometry(AREA);
        assert!(l.swap(1, 2));
        let after = l.geometry(AREA);
        assert_eq!(before.panes[&1], after.panes[&2]);
        assert_eq!(before.panes[&2], after.panes[&1]);
    }

    #[test]
    fn swap_rejects_unknown_or_identical_panes() {
        let mut l = quad();
        assert!(!l.swap(1, 1));
        assert!(!l.swap(1, 99));
    }

    #[test]
    fn presets_produce_the_promised_pane_count() {
        for name in ["solo", "duo", "trio", "dev", "quad"] {
            let n = Layout::preset_pane_count(name).unwrap();
            let panes: Vec<PaneId> = (1..=n as u32).collect();
            let l = Layout::preset(name, &panes).expect(name);
            assert_eq!(l.panes().len(), n, "{name}");

            // Every preset must tile its area exactly, same invariant as manual splits.
            let geo = l.geometry(AREA);
            let covered: u32 = geo.panes.values().map(|r| r.w as u32 * r.h as u32).sum();
            assert_eq!(covered, AREA.w as u32 * AREA.h as u32, "{name} leaves gaps");
        }
        assert!(Layout::preset("nope", &[1]).is_none());
        assert!(Layout::preset("duo", &[1]).is_none(), "wrong pane count must be rejected");
    }

    #[test]
    fn split_ids_stay_unique_so_resize_targets_one_divider() {
        let l = quad();
        let geo = l.geometry(AREA);
        assert_eq!(geo.splits.len(), 3, "quad has exactly three dividers");
    }

    #[test]
    fn geometry_order_is_stable_left_to_right_top_to_bottom() {
        let l = quad();
        assert_eq!(l.geometry(AREA).order, vec![1, 2, 3, 4]);
    }
}
