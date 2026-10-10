//! Tab pane-lifecycle methods (split / focus / close / zoom), moved
//! verbatim out of `tab.rs` (v1.13.8 S3 zero-behavior file-budget split;
//! `impl Tab` cross-file block per the tab/scroll.rs precedent).

use super::{PaneSplitGeometry, Tab};
use crate::pane::Pane;
use crate::AppEvent;
use weft_core::pane_layout::{PaneId, SplitDirection, SplitError};

impl Tab {
    // ── v1.3: Pane lifecycle (split / focus / close) ──────────────────

    /// Split the active pane in `direction`. The new pane inherits the
    /// active pane's cwd (live OSC 7 if available, else the restored
    /// snapshot cwd, else `None` to inherit the weft process cwd).
    ///
    /// v1.3.4: The new pane is forked with the **post-split** `(rows, cols)`
    /// derived from `preview_split_leaf_rect(content_rect, …)` ÷ cell size,
    /// so the shell prompt lands at the correct row from the very first
    /// frame — previously the new pane started at the active pane's
    /// full-viewport size and the renderer clipped it to the half-rect,
    /// leaving the prompt stranded at the bottom edge.
    ///
    /// `geo` carries the renderer's current content rect + cell size. When
    /// the renderer isn't ready yet (early boot) it may be zeroed, in which
    /// case the new pane falls back to the active pane's current
    /// `(rows, cols)` — `recompute_layout()` corrects it on the next redraw.
    ///
    /// The new pane becomes active. Returns its id on success.
    pub(crate) fn split_active_pane(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
        geo: PaneSplitGeometry,
    ) -> Result<PaneId, SplitError> {
        let cwd = self.launch_cwd();
        let active_id = self.active_pane;
        // Try to compute the new pane's intended (rows, cols) from the
        // post-split rect. Falls back to the active pane's current size
        // when the layout isn't usable (zero cell size, etc.).
        let new_pane_dims = if geo.cell_w > 0.0 && geo.cell_h > 0.0 {
            // `preview_split_leaf_rect` only needs an id that isn't already
            // a leaf — it uses it solely for the duplicate check. The real
            // pane id is assigned by `Pane::spawn` below. PaneId(0) is
            // safe because the per-process session counter starts at 1
            // (see `NEXT_PANE_SESSION` in pane.rs).
            const PREVIEW_PLACEHOLDER: PaneId = PaneId(0);
            match self.split_tree.preview_split_leaf_rect(
                active_id,
                direction,
                ratio,
                PREVIEW_PLACEHOLDER,
                geo.content_rect,
            ) {
                Ok(rect) => {
                    let [x0, y0, x1, y1] = rect;
                    let w = (x1 - x0).max(0.0);
                    let h = (y1 - y0).max(0.0);
                    let cols = crate::layout::terminal_content_cols(w, geo.cell_w);
                    let rows = (h / geo.cell_h).floor() as usize;
                    if rows > 0 && cols > 0 {
                        Some((rows, cols))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::debug!(?e, "preview_split_leaf_rect fell back to active size");
                    None
                }
            }
        } else {
            None
        };
        let (rows, cols) = new_pane_dims.unwrap_or_else(|| {
            self.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols))
                .unwrap_or((24, 80))
        });
        let new_pane = Pane::spawn(rows, cols, scrollback_lines, proxy, cwd.as_deref());
        self.split_active_pane_inner(direction, ratio, new_pane)
    }

    /// Test-only split that injects a no-PTY pane (built via
    /// `Pane::with_terminal_only`). Mirrors `split_active_pane` minus the
    /// PTY spawn so unit tests can exercise the tree / HashMap plumbing
    /// without a real shell.
    #[cfg(test)]
    pub(crate) fn split_active_pane_test(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        scrollback_lines: usize,
    ) -> Result<PaneId, SplitError> {
        let new_pane = Pane::with_terminal_only(scrollback_lines);
        self.split_active_pane_inner(direction, ratio, new_pane)
    }

    /// Shared core of `split_active_pane` / `split_active_pane_test`:
    /// insert a pre-built pane as the second child of the active leaf.
    /// Splitting is infallible once the caller hands us a pane — the only
    /// error paths are ratio-out-of-range and a stale active id, both of
    /// which indicate caller bugs.
    fn split_active_pane_inner(
        &mut self,
        direction: SplitDirection,
        ratio: f32,
        new_pane: Pane,
    ) -> Result<PaneId, SplitError> {
        let new_pane_id = PaneId(new_pane.pane_session_id);
        self.split_tree
            .split_leaf(self.active_pane, direction, ratio, new_pane_id)?;
        self.panes.insert(new_pane_id, new_pane);
        self.active_pane = new_pane_id;
        Ok(new_pane_id)
    }

    /// v1.6.2: Split a specific pane (identified by `leaf`) by inserting a
    /// pre-built `new_pane` as its second child. Used by workspace restore
    /// to rebuild a saved split tree — the pane being split may not be the
    /// currently active pane (e.g. when the saved tree has a Split node
    /// whose first child is itself a Split, so the active pane has moved
    /// into the first child's subtree by the time we need to split the
    /// root leaf to attach the second child).
    ///
    /// Unlike `split_active_pane_inner`, this does NOT change the active
    /// pane. The caller controls focus separately.
    pub(crate) fn split_pane_with_pane(
        &mut self,
        leaf: PaneId,
        direction: SplitDirection,
        ratio: f32,
        new_pane: Pane,
    ) -> Result<PaneId, SplitError> {
        let new_pane_id = PaneId(new_pane.pane_session_id);
        self.split_tree
            .split_leaf(leaf, direction, ratio, new_pane_id)?;
        self.panes.insert(new_pane_id, new_pane);
        Ok(new_pane_id)
    }

    /// Cycle focus to the next pane in declaration order (wraps around).
    /// Returns the new active pane id, or `None` if the tab has no panes
    /// (should never happen for a live tab — the tab is closed when its
    /// last pane closes). No-op (returns the same id) when the tab has
    /// exactly one pane.
    pub(crate) fn focus_next_pane(&mut self) -> Option<PaneId> {
        self.split_tree.focus_next()
    }

    /// v1.3 Batch 6: Set the active pane by id. Used by mouse hit-testing
    /// to focus the pane under the cursor on click. Returns `Err` if the
    /// pane id is not a leaf in the split tree (stale id — shouldn't happen
    /// for a live tab).
    pub(crate) fn set_active_pane(&mut self, id: PaneId) -> Result<(), SplitError> {
        self.split_tree.set_active(id)?;
        self.active_pane = id;
        Ok(())
    }

    /// Cycle focus to the previous pane in declaration order (wraps).
    pub(crate) fn focus_prev_pane(&mut self) -> Option<PaneId> {
        self.split_tree.focus_prev()
    }

    /// Close the active pane. Returns `Ok(true)` if the tab is now empty
    /// (caller should close the tab via `SessionManager::close_active`);
    /// returns `Ok(false)` if the tab still has panes — in that case
    /// focus moves to the surviving sibling of the closed pane (or the
    /// previous leaf in declaration order when the closed pane was the
    /// root).
    ///
    /// Returns `Err` only if the active pane id is stale (should never
    /// happen — `active_pane` is always kept in sync with the tree).
    pub(crate) fn close_active_pane(&mut self) -> Result<bool, SplitError> {
        let closing = self.active_pane;
        // v1.13.6 T10 P2 (D2): a PTY-backed pane closes ASYNCHRONOUSLY —
        // drop its PTY (`begin_close`); the worker finishes the backlog and
        // reports `PtyExited`, whose arm runs the finish-parity (force-settle
        // + drain) and tree shrink via the SAME `close_exited_pane` a natural
        // shell exit uses. A pane without a PTY (spawn failure, test panes —
        // or a SECOND close after the first already dropped it) closes
        // synchronously as before.
        if self
            .panes
            .get(&closing)
            .is_some_and(|pane| pane.pty.is_some())
        {
            if let Some(pane) = self.panes.get_mut(&closing) {
                pane.begin_close_teardown();
            }
            return Ok(false);
        }
        let new_active = self.split_tree.close_pane(closing)?;
        // Update `active_pane` BEFORE removing from `panes` so the invariant
        // ("active_pane always points at a live pane in `panes`") is never
        // violated — Deref would panic if it ran between the remove and the
        // reassignment.
        if let Some(id) = new_active {
            self.active_pane = id;
        }
        self.panes.remove(&closing);
        // FIX_background_pane_pump §2.5: the per-pane storm record dies with
        // the pane — a stale entry would keep freezing layout for a pane
        // that no longer exists and would grow the map over tab lifetime.
        self.alt_flip_history.remove(&closing);
        Ok(new_active.is_none())
    }

    /// v1.3.2: Adjust the ratio of the split that separates `first` and
    /// `second`. Mirrors `SplitTree::set_ratio_for_pair` — clamps to
    /// [0.1, 0.9], returns `Ok(false)` if the tree is a single leaf.
    pub(crate) fn set_pane_ratio(
        &mut self,
        first: weft_core::pane_layout::PaneId,
        second: weft_core::pane_layout::PaneId,
        new_ratio: f32,
    ) -> Result<bool, weft_core::pane_layout::SplitError> {
        self.split_tree.set_ratio_for_pair(first, second, new_ratio)
    }

    /// v1.3.3: Toggle pane zoom on the active pane. When zooming in, the
    /// active pane fills the viewport and siblings are hidden (but kept
    /// in the tree). When zooming out, the prior layout is restored.
    /// Returns `Some(pane_id)` when now zoomed, `None` when un-zoomed.
    ///
    /// `active_pane` is updated when zoom changes so callers that read it
    /// (e.g. for cursor drawing) stay in sync.
    pub(crate) fn toggle_pane_zoom(&mut self) -> Option<weft_core::pane_layout::PaneId> {
        let zoomed = self.split_tree.toggle_zoom();
        // Keep `active_pane` in lockstep with the tree's notion of focus.
        // When zooming in, the active pane becomes the zoomed pane (which
        // it already was — `toggle_zoom` zooms the active). When zooming
        // out, the active stays where the tree left it (unchanged).
        if let Some(id) = zoomed {
            self.active_pane = id;
        } else if let Some(active) = self.split_tree.active() {
            self.active_pane = active;
        }
        zoomed
    }

    /// v1.3.3: Move focus to the nearest pane in `dir`, based on spatial
    /// layout of `content_rect`. Returns the newly focused pane id, or
    /// `None` if no neighbour exists in that direction.
    pub(crate) fn focus_direction_pane(
        &mut self,
        dir: weft_core::pane_layout::FocusDirection,
        content_rect: weft_core::pane_layout::Rect,
    ) -> Option<weft_core::pane_layout::PaneId> {
        let new_active = self.split_tree.focus_in_direction(dir, content_rect)?;
        self.active_pane = new_active;
        Some(new_active)
    }

    /// v1.3.3: True iff the tab is currently in pane-zoom mode (one pane
    /// shown full-viewport, siblings hidden).
    ///
    /// `split_tree().layout()` already collapses to a single-pane vec when
    /// zoomed, so the redraw pipeline gets the right geometry without an
    /// explicit branch; v1.12.23 audit batch 2 doc fix — this is read by the
    /// zoom toggle action (app/action.rs) to skip the toggle when already
    /// zoomed, not only by a future status-bar UX.
    pub(crate) fn is_zoomed(&self) -> bool {
        self.split_tree.is_zoomed()
    }
}
