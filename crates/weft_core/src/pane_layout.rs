//! v1.3: Pane split-tree layout (pure logic, no rendering / PTY).
//!
//! A [`SplitTree`] is a binary tree whose leaves are [`PaneId`]s. Internal
//! nodes carry a [`SplitDirection`] and a `ratio` in `(0.0, 1.0)` describing
//! how the parent rect is divided between the first and second child. The
//! tree is the single source of truth for "which panes exist and how are
//! they arranged"; the app layer attaches PTY/Terminal state to each
//! `PaneId` separately.
//!
//! Direction naming mirrors the `Action` enum:
//!
//! | Action             | `SplitDirection` | Visual result            |
//! |--------------------|------------------|--------------------------|
//! | `SplitHorizontal`  | `Horizontal`     | stacked top / bottom     |
//! | `SplitVertical`    | `Vertical`       | side-by-side left / right|
//!
//! `ratio` is the share of the **first** child (top/left). The default 0.5
//! gives an even split. Draggable dividers and per-split ratio persistence
//! are deferred to a later batch — the tree stores the ratio so future
//! resize work has somewhere to write.
//!
//! All operations are infallible apart from "pane not found" / "tree
//! empty" — those return `Err(SplitError)` so callers can decide whether
//! to log, warn, or ignore. The tree never panics on its own.

/// Axis-aligned rectangle in physical pixels: `[x0, y0, x1, y1]`.
pub type Rect = [f32; 4];

/// Monotonic per-tab pane identifier. Unique within a tab; reused across
/// tabs is fine because the app layer keys pane state by `(tab, pane)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u64);

impl PaneId {
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PaneId({})", self.0)
    }
}

/// Direction in which a pane is split. Mirrors the `Action::SplitHorizontal`
/// / `Action::SplitVertical` naming so dispatch is a 1:1 mapping.
///
/// `Horizontal` stacks children top/bottom (the split line is horizontal).
/// `Vertical` places children side-by-side (the split line is vertical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Errors returned by tree mutations. Kept narrow so callers can `match`
/// without a wildcard arm.
//
// `Eq` is intentionally NOT derived: `RatioOutOfRange(f32)` carries an f32,
// which doesn't implement `Eq` (NaN isn't reflexive). `PartialEq` is enough
// for the test suite's `assert_eq!` comparisons.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SplitError {
    /// The tree has no panes (should never happen for a live tab — the
    /// tab is closed when its last pane closes).
    #[error("split tree is empty")]
    Empty,
    /// The given pane id is not a leaf in this tree. Returned by
    /// `split_leaf` and `close_pane` when the caller hands in a stale id
    /// (e.g. the pane was already closed).
    #[error("pane {0} not found in split tree")]
    PaneNotFound(PaneId),
    /// Attempted to split a leaf but the new pane id already exists in the
    /// tree. Indicates a bug in the caller's id allocator.
    #[error("pane {0} already exists in split tree")]
    DuplicatePaneId(PaneId),
    /// `ratio` was outside `(0.0, 1.0)`. Clamping silently would hide a
    /// caller bug, so we reject instead.
    #[error("split ratio {0} out of range (0.0, 1.0)")]
    RatioOutOfRange(f32),
}

// ── Tree node ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Node {
    /// Leaf node — holds a pane id.
    Leaf(PaneId),
    /// Internal split node. `first` is the top/left child, `second` is the
    /// bottom/right child. `ratio` is the first child's share in `(0, 1)`.
    Split {
        dir: SplitDirection,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// True iff this node is a leaf holding `pane`.
    fn is_leaf_with(&self, pane: PaneId) -> bool {
        matches!(self, Node::Leaf(id) if *id == pane)
    }

    /// Collect every leaf PaneId in declaration order (left-to-right,
    /// top-to-bottom depth-first).
    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    /// True iff `pane` is a leaf in this subtree.
    fn contains_leaf(&self, pane: PaneId) -> bool {
        match self {
            Node::Leaf(id) => *id == pane,
            Node::Split { first, second, .. } => {
                first.contains_leaf(pane) || second.contains_leaf(pane)
            }
        }
    }

    /// Recursively compute the rect for every leaf, appending `(pane, rect)`
    /// pairs to `out` in declaration order.
    fn layout_into(&self, rect: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, rect)),
            Node::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let [x0, y0, x1, y1] = rect;
                let (first_rect, second_rect) = match dir {
                    SplitDirection::Horizontal => {
                        // Top/bottom split: first child takes the top share.
                        let h = (y1 - y0).max(0.0);
                        let split_y = y0 + h * ratio;
                        ([x0, y0, x1, split_y], [x0, split_y, x1, y1])
                    }
                    SplitDirection::Vertical => {
                        // Left/right split: first child takes the left share.
                        let w = (x1 - x0).max(0.0);
                        let split_x = x0 + w * ratio;
                        ([x0, y0, split_x, y1], [split_x, y0, x1, y1])
                    }
                };
                first.layout_into(first_rect, out);
                second.layout_into(second_rect, out);
            }
        }
    }
}

// ── SplitTree ──────────────────────────────────────────────────────────

/// Binary tree of pane splits. Owns the layout structure and the currently
/// focused leaf; does NOT own any PTY/Terminal state — the caller attaches
/// that to each [`PaneId`] separately.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitTree {
    root: Option<Node>,
    active: Option<PaneId>,
}

impl SplitTree {
    /// Create a tree with a single root pane. The pane becomes active.
    pub fn new(initial_pane: PaneId) -> Self {
        Self {
            root: Some(Node::Leaf(initial_pane)),
            active: Some(initial_pane),
        }
    }

    /// True iff the tree has no panes.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Number of leaf panes. O(n) — call once per frame, not per pixel.
    pub fn pane_count(&self) -> usize {
        self.panes().len()
    }

    /// Currently focused pane, or `None` when the tree is empty.
    pub fn active(&self) -> Option<PaneId> {
        self.active
    }

    /// All leaf PaneIds in declaration order (depth-first, first child
    /// before second). Stable regardless of focus.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.collect_leaves(&mut out);
        }
        out
    }

    /// True iff `pane` is a leaf in this tree.
    pub fn contains(&self, pane: PaneId) -> bool {
        self.root.as_ref().is_some_and(|r| r.contains_leaf(pane))
    }

    /// Set the active pane. Returns `Err` if the pane isn't a leaf.
    pub fn set_active(&mut self, pane: PaneId) -> Result<(), SplitError> {
        match &self.root {
            None => return Err(SplitError::Empty),
            Some(r) if !r.contains_leaf(pane) => return Err(SplitError::PaneNotFound(pane)),
            _ => {}
        }
        self.active = Some(pane);
        Ok(())
    }

    /// Move focus to the next pane in declaration order (wraps around).
    /// Returns the new active pane, or `None` when the tree is empty.
    pub fn focus_next(&mut self) -> Option<PaneId> {
        self.cycle_active(1)
    }

    /// Move focus to the previous pane in declaration order (wraps around).
    /// Returns the new active pane, or `None` when the tree is empty.
    pub fn focus_prev(&mut self) -> Option<PaneId> {
        self.cycle_active(usize::MAX)
    }

    /// Shared helper for next/prev. `step == 1` advances by one; `step ==
    /// usize::MAX` is interpreted as "go back one" via wrapping arithmetic.
    fn cycle_active(&mut self, step: usize) -> Option<PaneId> {
        let panes = self.panes();
        if panes.is_empty() {
            return None;
        }
        let current = self.active.unwrap_or(panes[0]);
        let idx = panes.iter().position(|p| *p == current).unwrap_or(0);
        let n = panes.len();
        // Wrapping add handles both `+1` (forward) and the `+ (n-1)` needed
        // for a backward step without branching on the step value.
        let back_step = step == usize::MAX;
        let next_idx = if back_step {
            (idx + n - 1) % n
        } else {
            (idx + step) % n
        };
        let next = panes[next_idx];
        self.active = Some(next);
        Some(next)
    }

    /// Split `pane` into two, placing `new_pane` in the `dir` direction
    /// (bottom for `Horizontal`, right for `Vertical`). The original pane
    /// keeps the first-child share; the new pane gets the remainder.
    /// `ratio` is the original pane's share in `(0.0, 1.0)` — pass `0.5`
    /// for an even split. The new pane becomes active.
    ///
    /// Returns the new active pane id on success.
    pub fn split_leaf(
        &mut self,
        pane: PaneId,
        dir: SplitDirection,
        ratio: f32,
        new_pane: PaneId,
    ) -> Result<PaneId, SplitError> {
        if !(0.0..=1.0).contains(&ratio) || ratio == 0.0 || ratio == 1.0 {
            return Err(SplitError::RatioOutOfRange(ratio));
        }
        let root = self.root.as_mut().ok_or(SplitError::Empty)?;
        if root.contains_leaf(new_pane) {
            return Err(SplitError::DuplicatePaneId(new_pane));
        }
        if !root.contains_leaf(pane) {
            return Err(SplitError::PaneNotFound(pane));
        }
        // Walk to the matching leaf and replace it with a Split node. The
        // original pane stays as `first` (top/left), the new pane becomes
        // `second` (bottom/right) — matches the "new pane appears below /
        // to the right of the original" UX contract.
        replace_leaf(root, pane, |old_node| Node::Split {
            dir,
            ratio,
            first: Box::new(old_node),
            second: Box::new(Node::Leaf(new_pane)),
        });
        self.active = Some(new_pane);
        Ok(new_pane)
    }

    /// Close `pane`. If it has a sibling, the sibling replaces its parent
    /// (the tree collapses one level). Closing the last pane empties the
    /// tree. After close, `active` moves to the sibling when present,
    /// otherwise falls back to the previous leaf in declaration order.
    ///
    /// Returns the new active pane (or `None` when the tree is now empty).
    pub fn close_pane(&mut self, pane: PaneId) -> Result<Option<PaneId>, SplitError> {
        let root = self.root.take().ok_or(SplitError::Empty)?;
        if !root.contains_leaf(pane) {
            // Put the root back before returning — close shouldn't mutate
            // state on the error path.
            self.root = Some(root);
            return Err(SplitError::PaneNotFound(pane));
        }

        // Special case: the root itself is the pane being closed.
        if root.is_leaf_with(pane) {
            self.active = None;
            return Ok(None);
        }

        // Otherwise walk the tree, collapsing the parent of the closed
        // leaf into its surviving sibling.
        let mut surviving_sibling: Option<PaneId> = None;
        let new_root = remove_leaf(root, pane, &mut surviving_sibling);

        self.root = Some(new_root);

        // Pick the new active: prefer the surviving sibling of the closed
        // pane (feels natural — focus moves to the pane that absorbed the
        // space), otherwise fall back to the previous leaf in declaration
        // order so the focus doesn't jump wildly when closing nested panes.
        let new_active = if let Some(sib) = surviving_sibling {
            Some(sib)
        } else {
            let panes = self.panes();
            panes.last().copied()
        };
        self.active = new_active;
        Ok(new_active)
    }

    /// Compute the rect for every leaf in `viewport`. Returns
    /// `(pane_id, rect)` pairs in declaration order. Empty when the tree
    /// is empty.
    pub fn layout(&self, viewport: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.layout_into(viewport, &mut out);
        }
        out
    }

    /// Replace `old_pane` with `new_pane` in-place, preserving tree shape.
    /// Used by the restore path to swap a placeholder id for the real
    /// session id once the PTY is spawned. The active pane is updated when
    /// the replaced pane was active.
    pub fn replace_pane(&mut self, old_pane: PaneId, new_pane: PaneId) -> Result<(), SplitError> {
        let root = self.root.as_mut().ok_or(SplitError::Empty)?;
        if root.contains_leaf(new_pane) {
            return Err(SplitError::DuplicatePaneId(new_pane));
        }
        if !replace_leaf_id(root, old_pane, new_pane) {
            return Err(SplitError::PaneNotFound(old_pane));
        }
        if self.active == Some(old_pane) {
            self.active = Some(new_pane);
        }
        Ok(())
    }
}

// ── Free helpers ───────────────────────────────────────────────────────

/// Walk `node` and replace the leaf equal to `target` with the result of
/// `with_leaf(old_leaf_node)`. Panics if `target` isn't present — callers
/// must check `contains_leaf` first.
fn replace_leaf<F>(node: &mut Node, target: PaneId, with_leaf: F)
where
    F: FnOnce(Node) -> Node,
{
    match node {
        Node::Leaf(id) if *id == target => {
            let old = std::mem::replace(node, Node::Leaf(target));
            *node = with_leaf(old);
        }
        Node::Leaf(_) => {}
        Node::Split { first, second, .. } => {
            if first.contains_leaf(target) {
                replace_leaf(first, target, with_leaf);
            } else {
                replace_leaf(second, target, with_leaf);
            }
        }
    }
}

/// Walk `node` and rename any leaf equal to `old_id` to `new_id`. Returns
/// true iff a rename happened.
fn replace_leaf_id(node: &mut Node, old_id: PaneId, new_id: PaneId) -> bool {
    match node {
        Node::Leaf(id) if *id == old_id => {
            *id = new_id;
            true
        }
        Node::Leaf(_) => false,
        Node::Split { first, second, .. } => {
            replace_leaf_id(first, old_id, new_id) || replace_leaf_id(second, old_id, new_id)
        }
    }
}

/// Walk `node`, remove the leaf equal to `target`, and collapse its parent
/// into the surviving sibling. Sets `*surviving_sibling` to the PaneId of
/// the sibling that absorbed the closed pane's slot (if any).
///
/// Returns the (possibly mutated) node. The caller assigns it back to the
/// parent's `first` / `second` slot, or to `self.root` for the top-level
/// call.
fn remove_leaf(node: Node, target: PaneId, surviving_sibling: &mut Option<PaneId>) -> Node {
    match node {
        Node::Leaf(id) if id == target => {
            // Caller handles this via `is_leaf_with` before invoking us;
            // reaching here means a logic bug. Return the leaf unchanged
            // so the tree stays well-formed.
            Node::Leaf(id)
        }
        Node::Leaf(_) => node,
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            // If either child is the target leaf, replace this Split with
            // the other child (collapse one level).
            if first.is_leaf_with(target) {
                *surviving_sibling = leaf_id(&second);
                *second
            } else if second.is_leaf_with(target) {
                *surviving_sibling = leaf_id(&first);
                *first
            } else if first.contains_leaf(target) {
                let new_first = remove_leaf(*first, target, surviving_sibling);
                Node::Split {
                    dir,
                    ratio,
                    first: Box::new(new_first),
                    second,
                }
            } else {
                let new_second = remove_leaf(*second, target, surviving_sibling);
                Node::Split {
                    dir,
                    ratio,
                    first,
                    second: Box::new(new_second),
                }
            }
        }
    }
}

/// Return the PaneId of a leaf node, or `None` for a Split. Used by
/// `remove_leaf` to record which sibling survived a collapse.
fn leaf_id(node: &Node) -> Option<PaneId> {
    match node {
        Node::Leaf(id) => Some(*id),
        Node::Split { .. } => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────-

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const ROOT: Rect = [0.0, 0.0, 100.0, 100.0];

    fn pid(n: u64) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn single_pane_tree_layout_fills_viewport() {
        let tree = SplitTree::new(pid(1));
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.active(), Some(pid(1)));
        let layout = tree.layout(ROOT);
        assert_eq!(layout, vec![(pid(1), ROOT)]);
    }

    #[test]
    fn split_horizontal_stacks_top_bottom() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // New pane (2) is bottom; original (1) is top; new pane is active.
        assert_eq!(tree.active(), Some(pid(2)));
        let layout = tree.layout(ROOT);
        assert_eq!(
            layout,
            vec![
                (pid(1), [0.0, 0.0, 100.0, 50.0]),   // top half
                (pid(2), [0.0, 50.0, 100.0, 100.0]), // bottom half
            ]
        );
    }

    #[test]
    fn split_vertical_places_side_by_side() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        let layout = tree.layout(ROOT);
        assert_eq!(
            layout,
            vec![
                (pid(1), [0.0, 0.0, 50.0, 100.0]),   // left half
                (pid(2), [50.0, 0.0, 100.0, 100.0]), // right half
            ]
        );
    }

    #[test]
    fn ratio_gives_first_child_its_share() {
        let mut tree = SplitTree::new(pid(1));
        // ratio 0.25 → first child (top) gets 25% of the height.
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.25, pid(2))
            .unwrap();
        let layout = tree.layout(ROOT);
        assert_eq!(
            layout,
            vec![
                (pid(1), [0.0, 0.0, 100.0, 25.0]),
                (pid(2), [0.0, 25.0, 100.0, 100.0]),
            ]
        );
    }

    #[test]
    fn ratio_out_of_range_is_rejected() {
        let mut tree = SplitTree::new(pid(1));
        assert_eq!(
            tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.0, pid(2)),
            Err(SplitError::RatioOutOfRange(0.0))
        );
        assert_eq!(
            tree.split_leaf(pid(1), SplitDirection::Horizontal, 1.0, pid(2)),
            Err(SplitError::RatioOutOfRange(1.0))
        );
        assert_eq!(
            tree.split_leaf(pid(1), SplitDirection::Horizontal, -0.1, pid(2)),
            Err(SplitError::RatioOutOfRange(-0.1))
        );
        assert_eq!(
            tree.split_leaf(pid(1), SplitDirection::Horizontal, 1.5, pid(2)),
            Err(SplitError::RatioOutOfRange(1.5))
        );
        // Tree unchanged after the failed splits.
        assert_eq!(tree.pane_count(), 1);
    }

    #[test]
    fn nested_split_keeps_declaration_order() {
        // 1 (root) → split H into 1 (top) + 2 (bottom)
        // 1 (top) → split V into 1 (left) + 3 (right)
        // Declaration order: 1, 3, 2 (depth-first first-child).
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(3))
            .unwrap();
        assert_eq!(tree.panes(), vec![pid(1), pid(3), pid(2)]);
    }

    #[test]
    fn focus_next_cycles_through_panes_in_order() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        // panes() == [1, 2, 3]; active starts at 3 (the newest).
        assert_eq!(tree.active(), Some(pid(3)));
        assert_eq!(tree.focus_next(), Some(pid(1)));
        assert_eq!(tree.focus_next(), Some(pid(2)));
        assert_eq!(tree.focus_next(), Some(pid(3)));
        // Wraps from last back to first.
        assert_eq!(tree.focus_prev(), Some(pid(2)));
        assert_eq!(tree.focus_prev(), Some(pid(1)));
        assert_eq!(tree.focus_prev(), Some(pid(3))); // wraps
    }

    #[test]
    fn set_active_rejects_unknown_pane() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        assert_eq!(
            tree.set_active(pid(99)),
            Err(SplitError::PaneNotFound(pid(99)))
        );
        assert_eq!(tree.set_active(pid(1)), Ok(()));
        assert_eq!(tree.active(), Some(pid(1)));
    }

    #[test]
    fn close_pane_collapses_parent_into_sibling() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // Closing pane 2 (active) → focus moves to sibling 1, tree has
        // one pane again, layout fills the viewport.
        let new_active = tree.close_pane(pid(2)).unwrap();
        assert_eq!(new_active, Some(pid(1)));
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.layout(ROOT), vec![(pid(1), ROOT)]);
    }

    #[test]
    fn close_pane_in_nested_tree_keeps_remaining_structure() {
        // Layout after setup:
        //   root (H)
        //   ├── first: 1 (top)
        //   └── second: Split (H)
        //       ├── first: 2
        //       └── second: 3
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        // Close pane 2 → its sibling (3) should be promoted into the
        // outer Split's second slot.
        let new_active = tree.close_pane(pid(2)).unwrap();
        assert_eq!(new_active, Some(pid(3)));
        assert_eq!(tree.panes(), vec![pid(1), pid(3)]);
        // Layout: top half = pane 1, bottom half = pane 3.
        assert_eq!(
            tree.layout(ROOT),
            vec![
                (pid(1), [0.0, 0.0, 100.0, 50.0]),
                (pid(3), [0.0, 50.0, 100.0, 100.0]),
            ]
        );
    }

    #[test]
    fn close_last_pane_empties_tree() {
        let mut tree = SplitTree::new(pid(1));
        let new_active = tree.close_pane(pid(1)).unwrap();
        assert_eq!(new_active, None);
        assert!(tree.is_empty());
        assert_eq!(tree.active(), None);
        assert!(tree.layout(ROOT).is_empty());
    }

    #[test]
    fn close_unknown_pane_does_not_mutate_tree() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        let snapshot = tree.clone();
        let err = tree.close_pane(pid(99)).unwrap_err();
        assert_eq!(err, SplitError::PaneNotFound(pid(99)));
        assert_eq!(tree, snapshot);
    }

    #[test]
    fn split_rejects_duplicate_pane_id() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        assert_eq!(
            tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(1)),
            Err(SplitError::DuplicatePaneId(pid(1)))
        );
    }

    #[test]
    fn split_unknown_pane_is_error() {
        let mut tree = SplitTree::new(pid(1));
        assert_eq!(
            tree.split_leaf(pid(99), SplitDirection::Horizontal, 0.5, pid(2)),
            Err(SplitError::PaneNotFound(pid(99)))
        );
    }

    #[test]
    fn split_on_empty_tree_is_error() {
        let mut tree = SplitTree::default();
        assert_eq!(
            tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2)),
            Err(SplitError::Empty)
        );
    }

    #[test]
    fn replace_pane_swaps_id_and_preserves_active() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // Active is pane 2. Replace 2 → 99.
        tree.replace_pane(pid(2), pid(99)).unwrap();
        assert_eq!(tree.panes(), vec![pid(1), pid(99)]);
        assert_eq!(tree.active(), Some(pid(99)));
    }

    #[test]
    fn replace_pane_inactive_preserves_active() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // Active is 2; replace 1 (inactive) → 99.
        tree.replace_pane(pid(1), pid(99)).unwrap();
        assert_eq!(tree.panes(), vec![pid(99), pid(2)]);
        assert_eq!(tree.active(), Some(pid(2)));
    }

    #[test]
    fn replace_pane_rejects_existing_target_id() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // Replacing 1 → 2 should fail because 2 already exists.
        assert_eq!(
            tree.replace_pane(pid(1), pid(2)),
            Err(SplitError::DuplicatePaneId(pid(2)))
        );
    }

    #[test]
    fn layout_returns_empty_for_empty_tree() {
        let tree = SplitTree::default();
        assert!(tree.layout(ROOT).is_empty());
    }

    #[test]
    fn contains_and_pane_count_agree() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        let panes: HashSet<PaneId> = tree.panes().into_iter().collect();
        assert_eq!(panes.len(), tree.pane_count());
        for p in &panes {
            assert!(tree.contains(*p));
        }
        assert!(!tree.contains(pid(99)));
    }

    #[test]
    fn focus_on_empty_tree_returns_none() {
        let mut tree = SplitTree::default();
        assert_eq!(tree.focus_next(), None);
        assert_eq!(tree.focus_prev(), None);
    }

    #[test]
    fn deep_nested_layout_sums_to_viewport() {
        // Build a 4-pane grid:
        //   root (V): left half = pane 1, right half = Split (H)
        //     right-top (H first): pane 2
        //     right-bottom (H second): Split (V)
        //       pane 3 (left), pane 4 (right)
        // Viewport 100x100 → pane rects should tile the viewport exactly.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        tree.split_leaf(pid(3), SplitDirection::Vertical, 0.5, pid(4))
            .unwrap();
        let layout = tree.layout(ROOT);
        assert_eq!(layout.len(), 4);
        // Declaration order: 1, 2, 3, 4.
        let ids: Vec<PaneId> = layout.iter().map(|(p, _)| *p).collect();
        assert_eq!(ids, vec![pid(1), pid(2), pid(3), pid(4)]);
        // Pane 1: left half (x ∈ [0,50], y ∈ [0,100]).
        assert_eq!(layout[0].1, [0.0, 0.0, 50.0, 100.0]);
        // Pane 2: right-top (x ∈ [50,100], y ∈ [0,50]).
        assert_eq!(layout[1].1, [50.0, 0.0, 100.0, 50.0]);
        // Pane 3: right-bottom-left (x ∈ [50,75], y ∈ [50,100]).
        assert_eq!(layout[2].1, [50.0, 50.0, 75.0, 100.0]);
        // Pane 4: right-bottom-right (x ∈ [75,100], y ∈ [50,100]).
        assert_eq!(layout[3].1, [75.0, 50.0, 100.0, 100.0]);
    }

    #[test]
    fn pane_id_display_and_order() {
        // Display is useful in trace logs; ordering lets callers sort panes
        // for deterministic iteration.
        assert_eq!(format!("{}", pid(42)), "PaneId(42)");
        assert!(pid(1) < pid(2));
        assert_eq!(pid(7).raw(), 7);
    }
}
