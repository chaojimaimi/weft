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

/// v1.3.3: Spatial direction for `focus_in_direction`. Each variant is the
/// side of the active pane to look for a neighbour (e.g. `Up` means "move
/// focus to a pane whose bottom edge is above the active pane's top edge").
///
/// Mirrors the `Action::FocusPaneUp` / `FocusPaneDown` / `FocusPaneLeft` /
/// `FocusPaneRight` variants 1:1, so dispatch is a plain translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDirection {
    Up,
    Down,
    Left,
    Right,
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
///
/// `zoomed_pane` (v1.3.3) is the pane id currently shown full-viewport,
/// collapsing all siblings. When `Some`, `panes`/`layout`/`contains` etc.
/// behave as if the tree had a single leaf, but the underlying tree shape
/// is preserved so toggling zoom off restores the exact prior layout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitTree {
    root: Option<Node>,
    active: Option<PaneId>,
    zoomed_pane: Option<PaneId>,
}

impl SplitTree {
    /// Create a tree with a single root pane. The pane becomes active.
    pub fn new(initial_pane: PaneId) -> Self {
        Self {
            root: Some(Node::Leaf(initial_pane)),
            active: Some(initial_pane),
            zoomed_pane: None,
        }
    }

    /// True iff the tree has no panes.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Number of leaf panes in the underlying tree, ignoring zoom. O(n) —
    /// call once per frame, not per pixel.
    ///
    /// v1.3.3: This is a **structural** query (callers ask "how many real
    /// panes exist in this tab?"), so it walks the real tree even when
    /// zoomed — `panes()` would return 1 while zoomed, fooling any caller
    /// that uses this to decide "is this a single-pane tab?".
    pub fn pane_count(&self) -> usize {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.collect_leaves(&mut out);
        }
        out.len()
    }

    /// Currently focused pane, or `None` when the tree is empty.
    pub fn active(&self) -> Option<PaneId> {
        self.active
    }

    /// All leaf PaneIds in declaration order (depth-first, first child
    /// before second). Stable regardless of focus.
    ///
    /// v1.3.3: When zoomed, returns only `[zoomed_pane]` so renderers and
    /// hit-testing naturally collapse to a single pane without each caller
    /// needing to repeat the zoom check.
    pub fn panes(&self) -> Vec<PaneId> {
        if let Some(zoomed) = self.zoomed_pane {
            return vec![zoomed];
        }
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.collect_leaves(&mut out);
        }
        out
    }

    /// True iff `pane` is a leaf in this tree.
    ///
    /// v1.3.3: When zoomed, only the zoomed pane counts as "present" for
    /// hit-testing and validation; siblings are still in the tree but
    /// logically hidden.
    pub fn contains(&self, pane: PaneId) -> bool {
        if let Some(zoomed) = self.zoomed_pane {
            return zoomed == pane;
        }
        self.root.as_ref().is_some_and(|r| r.contains_leaf(pane))
    }

    /// Set the active pane. Returns `Err` if the pane isn't a leaf.
    ///
    /// v1.3.3: While zoomed, focus shifts are no-ops (the active pane can't
    /// change without un-zooming first). We return `Ok(())` rather than an
    /// error because callers like mouse hit-testing treat this as "nothing
    /// to do" rather than a real failure.
    pub fn set_active(&mut self, pane: PaneId) -> Result<(), SplitError> {
        match &self.root {
            None => return Err(SplitError::Empty),
            Some(r) if !r.contains_leaf(pane) => return Err(SplitError::PaneNotFound(pane)),
            _ => {}
        }
        if self.zoomed_pane.is_some() {
            return Ok(());
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
    ///
    /// v1.3.3: No-op when zoomed — `panes()` already returns a single-pane
    /// vec, so there's nothing to cycle through.
    fn cycle_active(&mut self, step: usize) -> Option<PaneId> {
        if self.zoomed_pane.is_some() {
            return self.zoomed_pane;
        }
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
        // v1.3.3: Exit zoom so the freshly-split layout is visible. The
        // new pane becomes active (and visible) regardless of whether the
        // previously-zoomed pane was the one split.
        self.zoomed_pane = None;
        self.active = Some(new_pane);
        Ok(new_pane)
    }

    /// Close `pane`. If it has a sibling, the sibling replaces its parent
    /// (the tree collapses one level). Closing the last pane empties the
    /// tree. After close, `active` moves to the sibling when present,
    /// otherwise falls back to the previous leaf in declaration order.
    ///
    /// v1.3.3: If the closed pane was the zoomed pane, zoom clears (the
    /// surviving sibling becomes both active and visible). Closing a
    /// hidden pane while zoomed is allowed — the tree updates, but the
    /// zoomed pane stays zoomed unless it was the one closed.
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
            self.zoomed_pane = None;
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
        // v1.3.3: If we just closed the zoomed pane, the zoom no longer
        // points at a valid leaf — clear it. The new active pane (computed
        // above) takes over the full viewport on the next layout pass.
        if self.zoomed_pane == Some(pane) {
            self.zoomed_pane = None;
        } else if self.zoomed_pane.is_some() {
            // Hidden pane was closed while zoomed. The `new_active` above
            // may point at a now-hidden sibling (e.g. via the
            // `surviving_sibling` path), which would put `active` and
            // `zoomed_pane` in disagreement. Pin `active` to the zoomed
            // pane so they stay consistent — the user is still looking at
            // the zoomed pane, so it must also be the focused one.
            self.active = self.zoomed_pane;
            return Ok(self.zoomed_pane);
        }
        Ok(new_active)
    }

    /// Compute the rect for every leaf in `viewport`. Returns
    /// `(pane_id, rect)` pairs in declaration order. Empty when the tree
    /// is empty.
    ///
    /// v1.3.3: When zoomed, returns `[(zoomed_pane, viewport)]` — the
    /// underlying tree shape is preserved (so toggling zoom off restores
    /// the exact prior layout) but the renderer sees only the zoomed pane
    /// occupying the full viewport.
    pub fn layout(&self, viewport: Rect) -> Vec<(PaneId, Rect)> {
        if let Some(zoomed) = self.zoomed_pane {
            return vec![(zoomed, viewport)];
        }
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
    ///
    /// v1.3.3: Also updates `zoomed_pane` if it pointed at the replaced
    /// id, so a restored session stays zoomed across the id swap.
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
        if self.zoomed_pane == Some(old_pane) {
            self.zoomed_pane = Some(new_pane);
        }
        Ok(())
    }

    /// v1.3.2: Set the ratio of the split whose two child subtrees contain
    /// `first` and `second` respectively.
    ///
    /// Using **both** pane ids (instead of just one) uniquely identifies the
    /// correct split even in nested layouts — e.g. a 3-pane tree where pane 1
    /// appears in both an inner and an outer split. The divider the user
    /// grabbed separates `first` (top/left) from `second` (bottom/right), so
    /// only the split that has one in each child is the target.
    ///
    /// `new_ratio` is the first-child (top/left) share, clamped to
    /// `[RATIO_MIN, RATIO_MAX]` = `[0.1, 0.9]`. Returns `Ok(false)` if the
    /// root is a single leaf; `Ok(true)` if a split's ratio was updated.
    pub fn set_ratio_for_pair(
        &mut self,
        first: PaneId,
        second: PaneId,
        new_ratio: f32,
    ) -> Result<bool, SplitError> {
        const RATIO_MIN: f32 = 0.1;
        const RATIO_MAX: f32 = 0.9;
        if !new_ratio.is_finite() {
            return Err(SplitError::RatioOutOfRange(new_ratio));
        }
        // v1.3.3: When zoomed there's only one visible pane — no divider
        // to drag. Return Ok(false) so the drag loop releases without
        // erroring, matching the "single-leaf" code path below.
        if self.zoomed_pane.is_some() {
            return Ok(false);
        }
        let clamped = new_ratio.clamp(RATIO_MIN, RATIO_MAX);
        let root = self.root.as_mut().ok_or(SplitError::Empty)?;
        if !root.contains_leaf(first) {
            return Err(SplitError::PaneNotFound(first));
        }
        if !root.contains_leaf(second) {
            return Err(SplitError::PaneNotFound(second));
        }
        if matches!(root, Node::Leaf(_)) {
            return Ok(false);
        }
        Ok(set_split_ratio_for_pair(root, first, second, clamped))
    }

    // ── v1.3.3: Pane zoom ────────────────────────────────────────────────

    /// Currently zoomed pane (shown full-viewport), or `None` when the
    /// tree is laid out normally.
    pub fn zoomed_pane(&self) -> Option<PaneId> {
        self.zoomed_pane
    }

    /// True iff a pane is currently zoomed (full-viewport).
    pub fn is_zoomed(&self) -> bool {
        self.zoomed_pane.is_some()
    }

    /// Toggle zoom on the active pane. When zooming in, the active pane
    /// expands to fill the entire viewport (siblings are hidden but kept
    /// in the tree). When zooming out, the prior layout is restored.
    ///
    /// Returns the pane id that is now zoomed (`Some` when zooming in),
    /// or `None` when zooming out / when the tree is empty.
    ///
    /// Zooming into a single-pane tree is a no-op — there's nothing to
    /// hide, and we want the toggle to be a true inverse (calling zoom
    /// again should restore, not stay zoomed).
    pub fn toggle_zoom(&mut self) -> Option<PaneId> {
        if let Some(_zoomed) = self.zoomed_pane.take() {
            // Was zoomed → un-zoom. Layout returns to normal on the next
            // call to `layout()`.
            return None;
        }
        // Need at least 2 panes for zoom to be meaningful.
        let panes = self.panes();
        if panes.len() < 2 {
            return None;
        }
        let active = self.active?;
        self.zoomed_pane = Some(active);
        Some(active)
    }

    // ── v1.3.3: Direction-aware focus ───────────────────────────────────

    /// Move focus to the nearest pane in the given direction, based on
    /// spatial layout (not declaration order). Returns the newly focused
    /// pane id, or `None` if no neighbour exists in that direction.
    ///
    /// Algorithm: compute every pane's rect via `layout(viewport)`, find
    /// the active pane's rect, then for the requested axis pick the
    /// candidate whose edge is "just past" the active pane in that
    /// direction and whose span overlaps the active pane on the cross
    /// axis. Among overlapping candidates, the one with the smallest
    /// gap wins (nearest neighbour semantics, matches tmux/iTerm2 UX).
    ///
    /// No-op when zoomed (only one visible pane).
    pub fn focus_in_direction(&mut self, dir: FocusDirection, viewport: Rect) -> Option<PaneId> {
        if self.zoomed_pane.is_some() {
            return None;
        }
        let layouts = self.layout(viewport);
        if layouts.len() < 2 {
            return None;
        }
        let active = self.active?;
        let active_rect = layouts
            .iter()
            .find(|(id, _)| *id == active)
            .map(|(_, r)| *r)?;
        let best = nearest_pane_in_direction(active, active_rect, dir, &layouts);
        if let Some(id) = best {
            self.active = Some(id);
        }
        best
    }
}

/// v1.3.3: Pick the nearest pane to `active`'s rect in the given direction.
///
/// "Nearest" = smallest positive gap along the direction axis, with the
/// additional requirement that the candidate overlaps the active pane on
/// the cross axis (so focus doesn't leap across a corner to a diagonally
/// placed pane — matches tmux's behaviour where you need a real edge
/// adjacency, not just a corner touch).
fn nearest_pane_in_direction(
    active: PaneId,
    active_rect: Rect,
    dir: FocusDirection,
    layouts: &[(PaneId, Rect)],
) -> Option<PaneId> {
    let [ax0, ay0, ax1, ay1] = active_rect;
    // Cross-axis overlap test: the candidate must share at least this
    // much of the cross axis with the active pane. A small epsilon avoids
    // floating-point corner-touch false positives.
    const OVERLAP_EPS: f32 = 0.5;

    let mut best: Option<(PaneId, f32)> = None;
    for &(id, rect) in layouts {
        if id == active {
            continue;
        }
        let [cx0, cy0, cx1, cy1] = rect;
        let (gap, overlap_ok) = match dir {
            // Candidate is "Up" of active: its bottom edge is at or above
            // active's top edge. Gap = active_top - candidate_bottom.
            FocusDirection::Up => {
                let gap = ay0 - cy1;
                let overlap = (ax1.min(cx1) - ax0.max(cx0)).max(0.0);
                (gap, overlap > OVERLAP_EPS)
            }
            // Candidate is "Down": its top edge is at or below active's
            // bottom edge. Gap = candidate_top - active_bottom.
            FocusDirection::Down => {
                let gap = cy0 - ay1;
                let overlap = (ax1.min(cx1) - ax0.max(cx0)).max(0.0);
                (gap, overlap > OVERLAP_EPS)
            }
            // Candidate is "Left": its right edge is at or left of active's
            // left edge. Gap = active_left - candidate_right.
            FocusDirection::Left => {
                let gap = ax0 - cx1;
                let overlap = (ay1.min(cy1) - ay0.max(cy0)).max(0.0);
                (gap, overlap > OVERLAP_EPS)
            }
            // Candidate is "Right": its left edge is at or right of active's
            // right edge. Gap = candidate_left - active_right.
            FocusDirection::Right => {
                let gap = cx0 - ax1;
                let overlap = (ay1.min(cy1) - ay0.max(cy0)).max(0.0);
                (gap, overlap > OVERLAP_EPS)
            }
        };
        if !overlap_ok || gap < -OVERLAP_EPS {
            continue;
        }
        let gap = gap.max(0.0);
        match best {
            None => best = Some((id, gap)),
            Some((_, bg)) if gap < bg => best = Some((id, gap)),
            _ => {}
        }
    }
    best.map(|(id, _)| id)
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

/// v1.3.2: Walk `node`, find the unique `Split` where one child subtree
/// contains `a` and the other contains `b`, and set its `ratio` to
/// `new_ratio`. Returns true iff a split was updated.
///
/// Using both pane ids uniquely identifies the correct split even in nested
/// layouts — e.g. a 3-pane tree where pane 1 is a leaf in an inner split but
/// also a descendant of the outer split. Only the split that separates `a`
/// from `b` (one in each child) is the target.
fn set_split_ratio_for_pair(node: &mut Node, a: PaneId, b: PaneId, new_ratio: f32) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Split {
            ratio,
            first,
            second,
            ..
        } => {
            let a_in_first = first.contains_leaf(a);
            let b_in_first = first.contains_leaf(b);
            // a in first & b in second, or vice versa → THIS is the split.
            if (a_in_first && second.contains_leaf(b)) || (b_in_first && second.contains_leaf(a)) {
                *ratio = new_ratio;
                true
            } else if a_in_first || b_in_first {
                set_split_ratio_for_pair(first, a, b, new_ratio)
            } else {
                set_split_ratio_for_pair(second, a, b, new_ratio)
            }
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

    // ── v1.3.2: set_ratio_for_pair ───────────────────────────────────────

    #[test]
    fn set_ratio_on_vertical_split_updates_divider() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        assert!(tree.set_ratio_for_pair(pid(1), pid(2), 0.25).unwrap());
        let vp = [0.0, 0.0, 100.0, 100.0];
        let layouts = tree.layout(vp);
        let first = layouts.iter().find(|(id, _)| *id == pid(1)).unwrap().1;
        // first child (left) gets 25% width = 25px
        assert!((first[2] - 25.0).abs() < 0.01, "first.x1 = {}", first[2]);
    }

    #[test]
    fn set_ratio_on_horizontal_split_updates_divider() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        assert!(tree.set_ratio_for_pair(pid(1), pid(2), 0.25).unwrap());
        let vp = [0.0, 0.0, 100.0, 100.0];
        let layouts = tree.layout(vp);
        let first = layouts.iter().find(|(id, _)| *id == pid(1)).unwrap().1;
        // first child (top) gets 25% height = 25px
        assert!((first[3] - 25.0).abs() < 0.01, "first.y1 = {}", first[3]);
    }

    #[test]
    fn set_ratio_clamps_to_0_1_0_9() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        assert!(tree.set_ratio_for_pair(pid(1), pid(2), 0.0).unwrap());
        let vp = [0.0, 0.0, 100.0, 100.0];
        let first = tree
            .layout(vp)
            .iter()
            .find(|(id, _)| *id == pid(1))
            .unwrap()
            .1;
        // 0.0 → clamped to 0.1 → 10px
        assert!((first[2] - 10.0).abs() < 0.01);
        assert!(tree.set_ratio_for_pair(pid(1), pid(2), 1.0).unwrap());
        let first = tree
            .layout(vp)
            .iter()
            .find(|(id, _)| *id == pid(1))
            .unwrap()
            .1;
        // 1.0 → clamped to 0.9 → 90px
        assert!((first[2] - 90.0).abs() < 0.01);
    }

    #[test]
    fn set_ratio_on_nested_split_targets_inner_split() {
        // Root: Horizontal(1, 2), then split 1 into Vertical(1, 3).
        // The inner Vertical divider separates 1↔3.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(3))
            .unwrap();
        // set_ratio_for_pair(1, 3) targets the inner Vertical split.
        assert!(tree.set_ratio_for_pair(pid(1), pid(3), 0.8).unwrap());
        let vp = [0.0, 0.0, 100.0, 100.0];
        let layouts = tree.layout(vp);
        let pane1 = layouts.iter().find(|(id, _)| *id == pid(1)).unwrap().1;
        let pane2 = layouts.iter().find(|(id, _)| *id == pid(2)).unwrap().1;
        // Outer Horizontal ratio is still 0.5 → pane 2 starts at y=50.
        assert!((pane2[1] - 50.0).abs() < 0.01, "outer ratio unchanged");
        // Inner Vertical ratio is 0.8 → pane 1 width = 80% of top band.
        assert!((pane1[2] - 80.0).abs() < 0.01, "inner ratio updated");
    }

    #[test]
    fn set_ratio_on_nested_split_targets_outer_split() {
        // Same tree: Root: Horizontal(1, 2), then split 1 into Vertical(1, 3).
        // The outer Horizontal divider separates {1,3}↔2.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(3))
            .unwrap();
        // set_ratio_for_pair(1, 2) targets the outer Horizontal split
        // (1 is in first subtree, 2 is in second).
        assert!(tree.set_ratio_for_pair(pid(1), pid(2), 0.3).unwrap());
        let vp = [0.0, 0.0, 100.0, 100.0];
        let pane2 = tree
            .layout(vp)
            .iter()
            .find(|(id, _)| *id == pid(2))
            .unwrap()
            .1;
        // Outer Horizontal ratio is now 0.3 → pane 2 (bottom) starts at y=30.
        assert!(
            (pane2[1] - 30.0).abs() < 0.01,
            "outer ratio updated, pane2.y0 = {}",
            pane2[1]
        );
    }

    #[test]
    fn set_ratio_on_root_leaf_returns_false() {
        let mut tree = SplitTree::new(pid(1));
        // Single pane — no pair to pass. Use itself as both.
        assert_eq!(tree.set_ratio_for_pair(pid(1), pid(1), 0.5), Ok(false));
    }

    #[test]
    fn set_ratio_unknown_pane_is_error() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        let snapshot = tree.clone();
        assert_eq!(
            tree.set_ratio_for_pair(pid(99), pid(2), 0.5),
            Err(SplitError::PaneNotFound(pid(99)))
        );
        assert_eq!(tree, snapshot);
    }

    #[test]
    fn set_ratio_on_empty_tree_is_error() {
        let mut tree = SplitTree::default();
        assert_eq!(
            tree.set_ratio_for_pair(pid(1), pid(2), 0.5),
            Err(SplitError::Empty)
        );
    }

    #[test]
    fn set_ratio_does_not_change_active() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.set_active(pid(2)).unwrap();
        assert_eq!(tree.active(), Some(pid(2)));
        tree.set_ratio_for_pair(pid(1), pid(2), 0.3).unwrap();
        assert_eq!(tree.active(), Some(pid(2)));
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

    // ── v1.3.3: Pane zoom ───────────────────────────────────────────────

    #[test]
    fn toggle_zoom_on_active_pane_collapses_layout() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        // active is pane 2 (the new pane). Zoom it.
        assert_eq!(tree.toggle_zoom(), Some(pid(2)));
        assert!(tree.is_zoomed());
        assert_eq!(tree.zoomed_pane(), Some(pid(2)));
        // Layout returns only the zoomed pane filling the viewport.
        assert_eq!(tree.layout(ROOT), vec![(pid(2), ROOT)]);
        assert_eq!(tree.panes(), vec![pid(2)]);
    }

    #[test]
    fn toggle_zoom_again_restores_layout() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom(); // zoom in
        assert_eq!(tree.toggle_zoom(), None); // zoom out
        assert!(!tree.is_zoomed());
        // Both panes are back, in the original 50/50 split.
        assert_eq!(
            tree.layout(ROOT),
            vec![
                (pid(1), [0.0, 0.0, 50.0, 100.0]),
                (pid(2), [50.0, 0.0, 100.0, 100.0]),
            ]
        );
    }

    #[test]
    fn zoom_on_single_pane_tree_is_noop() {
        let mut tree = SplitTree::new(pid(1));
        // Nothing to hide — zoom is a no-op so the toggle stays invertible.
        assert_eq!(tree.toggle_zoom(), None);
        assert!(!tree.is_zoomed());
    }

    #[test]
    fn zoom_on_empty_tree_is_noop() {
        let mut tree = SplitTree::default();
        assert_eq!(tree.toggle_zoom(), None);
        assert!(!tree.is_zoomed());
    }

    #[test]
    fn zoomed_tree_contains_only_returns_true_for_zoomed_pane() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        // Pane 1 is still in the underlying tree but logically hidden.
        assert!(tree.contains(pid(2)));
        assert!(!tree.contains(pid(1)));
    }

    #[test]
    fn zoomed_focus_next_prev_are_noops() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        // Active is pane 3. Zoom.
        tree.toggle_zoom();
        assert_eq!(tree.active(), Some(pid(3)));
        // Both focus ops return the zoomed pane (no cycling).
        assert_eq!(tree.focus_next(), Some(pid(3)));
        assert_eq!(tree.focus_prev(), Some(pid(3)));
        assert_eq!(tree.active(), Some(pid(3)));
    }

    #[test]
    fn zoomed_set_active_is_silent_noop() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        // set_active on a hidden pane returns Ok (not Err) but does not
        // change focus — mouse clicks on hidden panes shouldn't error.
        tree.set_active(pid(1)).unwrap();
        assert_eq!(tree.active(), Some(pid(2)));
    }

    #[test]
    fn split_leaf_exits_zoom() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        assert!(tree.is_zoomed());
        // Splitting the zoomed pane forces un-zoom so the new layout shows.
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        assert!(!tree.is_zoomed());
        assert_eq!(tree.panes().len(), 3);
    }

    #[test]
    fn close_zoomed_pane_clears_zoom() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom(); // zoom pane 2
                            // Close pane 2 → zoom must clear, surviving sibling becomes active.
        let new_active = tree.close_pane(pid(2)).unwrap();
        assert_eq!(new_active, Some(pid(1)));
        assert!(!tree.is_zoomed());
        assert_eq!(tree.layout(ROOT), vec![(pid(1), ROOT)]);
    }

    #[test]
    fn close_hidden_pane_while_zoomed_keeps_zoom() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom(); // zoom pane 2; pane 1 hidden but still in tree
                            // Closing a hidden pane updates the tree shape
                            // but the zoomed pane (2) is unaffected.
        tree.close_pane(pid(1)).unwrap();
        assert!(tree.is_zoomed());
        assert_eq!(tree.zoomed_pane(), Some(pid(2)));
        // v1.3.3 P2 fix: active must stay pinned to the zoomed pane so
        // the two never disagree while zoomed.
        assert_eq!(tree.active(), Some(pid(2)));
        // After un-zoom, the layout is a single-pane tree.
        tree.toggle_zoom();
        assert_eq!(tree.layout(ROOT), vec![(pid(2), ROOT)]);
    }

    #[test]
    fn pane_count_reflects_real_tree_while_zoomed() {
        // v1.3.3 P2 fix: pane_count is structural, not rendering-related.
        // It must report the underlying leaf count even when zoomed.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        assert_eq!(tree.pane_count(), 3);
        tree.toggle_zoom();
        // panes() collapses to 1 for rendering, but pane_count stays 3.
        assert_eq!(tree.panes().len(), 1);
        assert_eq!(tree.pane_count(), 3);
    }

    #[test]
    fn replace_pane_propagates_zoom_id() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        // Restore path swaps pane 2 → 99. Zoom should follow.
        tree.replace_pane(pid(2), pid(99)).unwrap();
        assert_eq!(tree.zoomed_pane(), Some(pid(99)));
        assert!(tree.is_zoomed());
        assert_eq!(tree.layout(ROOT), vec![(pid(99), ROOT)]);
    }

    #[test]
    fn set_ratio_for_pair_is_noop_while_zoomed() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        // No divider visible — drag should be a silent no-op.
        assert_eq!(tree.set_ratio_for_pair(pid(1), pid(2), 0.3), Ok(false));
        // After un-zoom the ratio is still 0.5.
        tree.toggle_zoom();
        let first = tree
            .layout(ROOT)
            .iter()
            .find(|(id, _)| *id == pid(1))
            .unwrap()
            .1;
        assert!((first[2] - 50.0).abs() < 0.01);
    }

    // ── v1.3.3: Direction-aware focus ───────────────────────────────────

    #[test]
    fn focus_left_in_vertical_split_moves_to_left_pane() {
        // Vertical split: pane 1 (left), pane 2 (right). Active = 2.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        assert_eq!(tree.active(), Some(pid(2)));
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Left, ROOT),
            Some(pid(1))
        );
        assert_eq!(tree.active(), Some(pid(1)));
    }

    #[test]
    fn focus_right_in_vertical_split_moves_to_right_pane() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.set_active(pid(1)).unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Right, ROOT),
            Some(pid(2))
        );
    }

    #[test]
    fn focus_up_in_horizontal_split_moves_to_top_pane() {
        // Horizontal split: pane 1 (top), pane 2 (bottom). Active = 2.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Up, ROOT),
            Some(pid(1))
        );
    }

    #[test]
    fn focus_down_in_horizontal_split_moves_to_bottom_pane() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        tree.set_active(pid(1)).unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Down, ROOT),
            Some(pid(2))
        );
    }

    #[test]
    fn focus_in_direction_no_neighbour_returns_none() {
        // Single pane — no neighbour in any direction.
        let mut tree = SplitTree::new(pid(1));
        assert_eq!(tree.focus_in_direction(FocusDirection::Up, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Down, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Left, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Right, ROOT), None);
    }

    #[test]
    fn focus_left_at_left_edge_returns_none() {
        // Vertical split: pane 1 (left), pane 2 (right). Active = 1.
        // Already at the left edge — no pane further left.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.set_active(pid(1)).unwrap();
        assert_eq!(tree.focus_in_direction(FocusDirection::Left, ROOT), None);
        // Active unchanged.
        assert_eq!(tree.active(), Some(pid(1)));
    }

    #[test]
    fn focus_direction_in_2x2_grid_finds_correct_neighbour() {
        // Build a 2x2 grid:
        //   root (H): top = pane 1, bottom = pane 4
        //   top (V): pane 1 (left), pane 2 (right)
        //   bottom (V): pane 3 (left), pane 4 (right)
        //
        // Layout (100x100):
        //   pane 1: [0,0,50,50]    pane 2: [50,0,100,50]
        //   pane 3: [0,50,50,100]  pane 4: [50,50,100,100]
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(4))
            .unwrap();
        // Active is now pane 4 (bottom). Split top (pane 1) into V(1, 2).
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        // Split bottom (pane 4) into V(3, 4)? Careful: pane 4 is currently
        // a leaf, so splitting it makes pane 3 (left) + pane 4 (right)? No —
        // split_leaf keeps the original as first, new as second. So we get
        // V(4, 3). Instead, split pane 1 first to get the top row, then
        // split pane 4. To get V(3, 4) we'd need to insert pane 3 as the
        // "original" — but the API only adds the new pane as second child.
        //
        // For test purposes the exact id placement doesn't matter as long
        // as we know the geometry. Let's just split pane 4 → V(4, 3): pane
        // 4 (left), pane 3 (right) in the bottom row. Final layout:
        //   pane 1: [0,0,50,50]    pane 2: [50,0,100,50]
        //   pane 4: [0,50,50,100]  pane 3: [50,50,100,100]
        tree.split_leaf(pid(4), SplitDirection::Vertical, 0.5, pid(3))
            .unwrap();
        // Active is now pane 3 (last new pane). Set focus to pane 1 (top-left).
        tree.set_active(pid(1)).unwrap();
        // From pane 1 (top-left): Right → pane 2, Down → pane 4.
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Right, ROOT),
            Some(pid(2))
        );
        tree.set_active(pid(1)).unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Down, ROOT),
            Some(pid(4))
        );
        // From pane 1: Left/Up should be no-ops (edge of grid).
        tree.set_active(pid(1)).unwrap();
        assert_eq!(tree.focus_in_direction(FocusDirection::Left, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Up, ROOT), None);
    }

    #[test]
    fn focus_direction_in_grid_wraps_via_multiple_splits() {
        // Same grid as above. From pane 2 (top-right):
        //   Left → pane 1, Down → pane 3.
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(4))
            .unwrap();
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.split_leaf(pid(4), SplitDirection::Vertical, 0.5, pid(3))
            .unwrap();
        tree.set_active(pid(2)).unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Left, ROOT),
            Some(pid(1))
        );
        tree.set_active(pid(2)).unwrap();
        assert_eq!(
            tree.focus_in_direction(FocusDirection::Down, ROOT),
            Some(pid(3))
        );
        // From pane 2: Right/Up should be no-ops.
        tree.set_active(pid(2)).unwrap();
        assert_eq!(tree.focus_in_direction(FocusDirection::Right, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Up, ROOT), None);
    }

    #[test]
    fn focus_direction_picks_nearest_when_multiple_candidates() {
        // Three panes stacked vertically (root H, then split bottom again):
        //   pane 1: [0,0,100,33.33]
        //   pane 2: [0,33.33,100,66.66]
        //   pane 3: [0,66.66,100,100]
        // From pane 1, "Down" → nearest is pane 2 (not pane 3).
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Horizontal, 0.5, pid(2))
            .unwrap();
        // Now split pane 2 (bottom) again to get a 3-pane stack. The
        // outer ratio stays 0.5 so pane 1 keeps the top half; pane 2's
        // half is then split 50/50 into panes 2 (middle) and 3 (bottom).
        tree.split_leaf(pid(2), SplitDirection::Horizontal, 0.5, pid(3))
            .unwrap();
        tree.set_active(pid(1)).unwrap();
        let down_target = tree.focus_in_direction(FocusDirection::Down, ROOT);
        assert_eq!(down_target, Some(pid(2)));
    }

    #[test]
    fn focus_direction_no_op_when_zoomed() {
        let mut tree = SplitTree::new(pid(1));
        tree.split_leaf(pid(1), SplitDirection::Vertical, 0.5, pid(2))
            .unwrap();
        tree.toggle_zoom();
        // Zoomed → direction focus is a no-op (only one visible pane).
        assert_eq!(tree.focus_in_direction(FocusDirection::Left, ROOT), None);
        assert_eq!(tree.focus_in_direction(FocusDirection::Right, ROOT), None);
    }

    #[test]
    fn focus_direction_on_empty_tree_returns_none() {
        let mut tree = SplitTree::default();
        assert_eq!(tree.focus_in_direction(FocusDirection::Up, ROOT), None);
    }
}
