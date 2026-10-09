//! Per-tab session state (v0.9 H1, v1.3 multi-pane).
//!
//! A `Tab` owns one or more [`Pane`]s arranged by a [`SplitTree`]. Each pane
//! carries its own PTY, VT `Terminal`, input/selection handlers, IME preedit,
//! and block-scroll state. The tab-level concerns (split tree, active pane
//! id, stable tab `session_id` for tab-bar / context-menu ownership) live
//! here.
//!
//! `Tab` implements `Deref<Target=Pane>` / `DerefMut` so existing call sites
//! that read `tab.pty` / `tab.input_handler` / … keep compiling unchanged:
//! the field access auto-dereferences to the active pane. The terminal is
//! reached through the `Pane` accessors (`with_terminal` / `lock_terminal` —
//! T10 P1). New code that needs to operate on a non-active pane should go
//! through `tab.pane(id)` / `tab.pane_mut(id)`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use weft_core::pane_layout::{PaneId, PaneTree, SplitDirection, SplitError, SplitTree};
use weft_core::persistence::{PaneTreeSnapshot, SnapshotPaneNode, TabSnapshot};
use weft_core::pty::PtyError;

use crate::pane::Pane;
use crate::AppEvent;
#[cfg(test)]
use crate::AppMsg;

/// v1.3.4: Geometry inputs for `Tab::split_active_pane`. Bundles the
/// renderer's current content rect + cell size into a single value so the
/// split method stays under clippy's `too_many_arguments` threshold and
/// the call site reads naturally. Built by the dispatch layer from
/// `App::terminal_layout()`; zeroed values mean "renderer not ready" and
/// the split method falls back to the active pane's current size.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PaneSplitGeometry {
    /// Content-area rect in physical pixels `[x0, y0, x1, y1]`.
    pub content_rect: weft_core::pane_layout::Rect,
    /// Cell width in physical pixels.
    pub cell_w: f32,
    /// Cell height in physical pixels.
    pub cell_h: f32,
}

mod lifecycle;
mod pane_pump;
mod primary_history;
mod resize;
mod scroll;
mod tui_scroll;

pub(crate) use primary_history::PrimaryHistoryRefresh;
pub(crate) use scroll::BlockScrollAnchor;
pub(crate) use tui_scroll::PendingTuiScroll;
pub use tui_scroll::TuiScrollResolution;

/// v1.10.26 (D-1) + v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): the two most recent
/// alt-screen flips from one source pane — the burst-storm signature for
/// `burst_locked_cols`. FIX_background_pane_pump §2.5: stored per source
/// pane (`Tab::alt_flip_history` map), so one pane's record can never be
/// reset by another pane's flip.
///
/// A real alt TUI (vim/less) enters with ONE flip and goes quiet; an omp
/// repaint feedback loop toggles DEC 1049 every ~130ms. The freeze needs the
/// *two-flip* signature: two flips from the same pane inside the debounce
/// window, the most recent still fresh — so an isolated single flip is never
/// frozen and a storm is pinned at the current grid size until quiet, then
/// the live kind converges once. `count` records how many real flips are
/// stored (0..=2); a value of 1 (a lone flip) never forms a record no matter
/// how fresh it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AltFlipHistory {
    /// Pane that produced these flips. The lock is scoped to this pane:
    /// pane A's storm cannot widen pane B's real TUI target (v1.10.19
    /// "A 面板风暴误伤 B 面板").
    pub(crate) src_pane: PaneId,
    /// The older of the two most recent flips (`count == 1` → not meaningful).
    pub(crate) older: std::time::Instant,
    /// The most recent flip.
    pub(crate) newer: std::time::Instant,
    /// Real flips recorded, capped at 2. `< 2` → no storm signature.
    pub(crate) count: u8,
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A tab owns one or more panes. The tab-level `session_id` is the externally
/// visible identifier (tab bar, context menu, autosave); each pane also has
/// its own `pane_session_id` for PTY / trace correlation.
///
/// The active pane is the one that receives keyboard input and renders the
/// cursor. `Deref` / `DerefMut` forward to it so the existing single-pane
/// call sites (`tab.pty`, `tab.input_handler`, …) keep working without
/// change; the terminal goes through the `Pane` lock accessors (T10 P1).
pub struct Tab {
    /// Stable tab-level identifier. Stays constant for the tab's lifetime
    /// regardless of how many panes are open or which is active.
    pub session_id: u64,
    /// Layout tree of pane splits. Owns the structure (direction + ratio at
    /// each internal node, `PaneId` at each leaf) but not the pane state —
    /// the state lives in `panes`.
    #[allow(dead_code)] // v1.3 Batch 5+: read by split_tree() for multi-pane layout
    split_tree: SplitTree,
    /// Per-pane state keyed by `PaneId`. Every leaf in `split_tree` has an
    /// entry here; closing a pane removes both the leaf and the entry.
    panes: HashMap<PaneId, Pane>,
    /// Currently focused pane. Keyboard input, IME, and Deref forward here.
    /// Always points at a leaf present in `split_tree` / `panes`.
    active_pane: PaneId,
    /// v1.10.4: set when any pane enters/exits alt-screen (DEC 1049).
    /// The redraw loop checks this to trigger a geometry recompute so PTY
    /// cols switch between full-width (TUI) and gutter-subtracted (BlockView
    /// shell output). Consumed only when the tab is active; a background
    /// tab's flag self-heals on activation because
    /// `resize_all_panes_for_rect` re-reads the screen mode live.
    ///
    /// v1.10.19: consumption is debounced via the flip history /
    /// `alt_rescale_last_taken` (see `tab/resize.rs`) so a burst of toggles
    /// coalesces into one recompute instead of one per flip. v1.10.26 (D-2):
    /// armed off the u64 flip-counter diff, so even-count batches (h→l
    /// net-zero) still refresh the window.
    ///
    /// FIX_background_pane_pump §2.4 (2): "any pane" is literal now —
    /// background panes are pumped and consumed too, so their flips arm
    /// this flag. The single merged value is deliberate: one main thread
    /// consumes all panes in order (no lost-write race), and the resize
    /// commit itself walks every pane (`resize_all_panes_for_rect`), so one
    /// armed recompute serves all sources.
    pending_alt_rescale: bool,
    /// FIX_background_pane_pump §2.5 (was a single `Option<AltFlipHistory>`
    /// slot): per-pane flip history of the last alt-screen toggles that
    /// armed `pending_alt_rescale` — the last two flip instants of THAT
    /// pane. Keyed by `PaneId`; absent until the pane's first flip. Drives
    /// both the `take_pending_alt_rescale` debounce freshness and the
    /// `burst_locked_cols` storm freeze (see `tab/resize.rs`). Per-pane
    /// storage is the point: a single slot let pane B's isolated flip
    /// discard pane A's accumulated record, un-freezing A's storm early
    /// (the v1.10.19 "A 面板风暴误伤 B 面板" mirror regression).
    alt_flip_history: HashMap<PaneId, AltFlipHistory>,
    /// v1.10.19: time the pending rescale was last consumed by
    /// `take_pending_alt_rescale`. A fresh toggle inside the debounce window
    /// after a consumed recompute marks a burst (the SIGWINCH feedback loop
    /// toggles every ~130ms); the recompute then holds until the toggles go
    /// quiet so the burst coalesces into one.
    alt_rescale_last_taken: Option<std::time::Instant>,
    /// v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): armed after a
    /// committed PTY resize; the first following PTY output logs the
    /// post-resize repaint latency (stage 3/4 of the RESIZE_PROBE chain) and
    /// disarms itself, so it fires once per resize instead of per byte.
    resize_output_probe: Option<std::time::Instant>,
}

/// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): immutable draw inputs for one
/// non-active pane, derived under the [`Tab::active_pane_views`] invariants.
/// The rect is NOT carried here — layout is the caller's concern; pair
/// `pane_id` against `split_tree().layout(content_rect)` at the call site.
pub(crate) struct BackgroundPaneView {
    /// Pane the view came from — the pairing key for the caller's rects.
    pub pane_id: PaneId,
    /// Owned terminal lock guard (T10 P1). Background panes' terminals are
    /// only read during draw; the guard is `lock_arc`-owned, so the view
    /// carries no borrow of the `Tab`.
    pub terminal: crate::pane::TerminalGuard,
    /// Block-scroll snapshot (`offset_value + fraction`) taken at derivation
    /// time, so the draw path needs no second borrow of the pane.
    pub block_scroll: f32,
    /// v1.4.1: pane-scoped namespace for the styled-line vertex cache.
    pub pane_session_id: u64,
}

/// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): the split-borrow view of a
/// tab's panes for one draw frame — mutable access to the active pane plus
/// shared reads of every background pane, replacing the raw `*const
/// Terminal` pointer bridge in redraw_controller.
pub(crate) struct PaneViewSet<'a> {
    pub active: &'a mut Pane,
    pub backgrounds: Vec<BackgroundPaneView>,
}

impl std::ops::Deref for Tab {
    type Target = Pane;
    fn deref(&self) -> &Self::Target {
        self.panes
            .get(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl std::ops::DerefMut for Tab {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.panes
            .get_mut(&self.active_pane)
            .expect("active_pane always points at a present pane")
    }
}

impl Tab {
    /// Borrow the active pane.
    pub(crate) fn active(&self) -> &Pane {
        self
    }

    /// Mutably borrow the active pane.
    pub(crate) fn active_mut(&mut self) -> &mut Pane {
        self
    }

    /// v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): split-borrow the pane map
    /// for one draw frame — `&mut` on the active pane plus shared reads of
    /// every background pane, so callers never need a raw-pointer bridge.
    ///
    /// CALLER ORDER CONTRACT: `split_tree().layout(content_rect)` (and any
    /// other read of the tab) must complete BEFORE this call — the returned
    /// views hold `&mut Tab` for their whole lifetime, so the read-only
    /// layout walk has to finish first. See redraw_controller::run_redraw.
    ///
    /// WHY SAFE CODE, not the plan-reviewed raw-pointer sketch: the locked
    /// form (`map_ptr → get_mut(active)` then `(*map_ptr).iter()`) is UB
    /// under Stacked Borrows in BOTH orders — the second reborrow through
    /// the raw pointer pops the first one's tag, and Miri (A9, nightly
    /// 2026-09-18) rejects it: "trying to retag ... Unique permission ...
    /// tag does not exist in the borrow stack" at the return of this fn.
    /// `iter_mut` derives the same shape soundly: std guarantees the yielded
    /// `&mut Pane`s are pairwise disjoint, so keeping the active entry's
    /// `&mut` and shrinking every other element to a shared `&Terminal`
    /// needs no unsafe — the borrow checker now enforces what the five
    /// invariants could only document.
    pub(crate) fn active_pane_views(&mut self) -> PaneViewSet<'_> {
        let active_id = self.active_pane_id();
        let mut active: Option<&mut Pane> = None;
        let mut backgrounds: Vec<BackgroundPaneView> = Vec::new();
        for (id, pane) in self.panes.iter_mut() {
            if *id == active_id {
                active = Some(pane);
            } else if let Some(terminal) = pane.lock_terminal() {
                // Same snapshot the raw-pointer bridge took: offset + the
                // transient trackpad fraction, read once per frame.
                backgrounds.push(BackgroundPaneView {
                    pane_id: *id,
                    terminal,
                    block_scroll: pane.block_scroll_anchor.offset_value() as f32
                        + pane.block_scroll_fraction,
                    pane_session_id: pane.pane_session_id,
                });
            }
        }
        PaneViewSet {
            active: active.expect("active_pane always points at a present pane"),
            backgrounds,
        }
    }

    /// Borrow a specific pane by id. Returns `None` if the pane is not in
    /// this tab (closed or never created).
    pub(crate) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    /// Mutably borrow a specific pane by id.
    pub(crate) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    /// v1.12.2 B3-2 (PLAN_S2_render): clear the dirty-row flags of every
    /// non-active pane after the frame consumed them. Mirrors the active
    /// pane's post-draw `clear_all_dirty` (redraw_controller): background
    /// grids are never cleared otherwise, so their dirty set only grows and
    /// the per-pane incremental row cache would keep rebuilding long-idle
    /// rows.
    pub(crate) fn clear_background_grid_dirty(&mut self, active_id: PaneId) {
        for (id, pane) in self.panes.iter_mut() {
            if *id == active_id {
                continue;
            }
            pane.with_terminal(|t| t.grid_mut().clear_all_dirty());
        }
    }

    /// v1.5.0: Mutable iterator over all panes. Used by `apply_config` to
    /// reseed palette / scrollback on every pane in every tab when a
    /// profile switch or config reload fires.
    pub(crate) fn panes_mut(&mut self) -> impl Iterator<Item = &mut Pane> {
        self.panes.values_mut()
    }

    pub(crate) fn panes(&self) -> impl Iterator<Item = (PaneId, &Pane)> {
        self.panes.iter().map(|(id, pane)| (*id, pane))
    }

    /// Id of the currently focused pane.
    pub(crate) fn active_pane_id(&self) -> PaneId {
        self.active_pane
    }

    /// Read-only view of the split tree (for layout / hit-testing).
    pub(crate) fn split_tree(&self) -> &SplitTree {
        &self.split_tree
    }

    /// Number of panes in this tab. O(n) — walks the tree.
    ///
    /// v1.3: the tab-bar and a future pane-close affordance will read this.
    /// Currently only exercised by tests; the allow keeps the gate clean
    /// until the UI consumer lands.
    #[allow(dead_code)]
    pub(crate) fn pane_count(&self) -> usize {
        self.split_tree.pane_count()
    }

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

    /// Create a new tab with a PTY + Terminal pair at the given size.
    ///
    /// `cwd` — if `Some(path)`, the shell starts in that directory (via
    /// `chdir` in the child process before exec). Pass `None` to inherit
    /// the weft process's cwd. Using `chdir` instead of sending a `cd`
    /// command keeps the initial tab clean — no `cd` appears in the
    /// terminal, shell history, or block tracker.
    pub fn new(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
        cwd: Option<&str>,
    ) -> Self {
        let pane = Pane::spawn(rows, cols, scrollback_lines, proxy, cwd);
        Self::from_single_pane(pane)
    }

    /// PTY-less test constructor — terminal/pty stay None. Production code
    /// goes through `Tab::new` (a real PTY spawn); v1.12.23 audit batch 2
    /// narrowed this to `#[cfg(test)]` since tests are its only callers
    /// (the only way to build a Tab without spawning a PTY).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::from_single_pane(Pane::empty())
    }

    /// Build a tab from a single starting pane. The pane becomes the root
    /// of the split tree and the active pane. Used by `new` / `empty` and
    /// by tests that construct a pane manually.
    fn from_single_pane(pane: Pane) -> Self {
        let pane_id = PaneId(pane.pane_session_id);
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);
        Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            split_tree: SplitTree::new(pane_id),
            panes,
            active_pane: pane_id,
            pending_alt_rescale: false,
            alt_flip_history: HashMap::new(),
            alt_rescale_last_taken: None,
            resize_output_probe: None,
        }
    }

    /// Test-only constructor: build a tab wrapping a single pre-built pane.
    /// Used by snapshot / scroll tests that need a Terminal without spawning
    /// a PTY.
    #[cfg(test)]
    pub(crate) fn with_single_pane(pane: Pane) -> Self {
        Self::from_single_pane(pane)
    }

    /// Bump and return the next per-tab input-event sequence number.
    ///
    /// Currently called at the keyboard-encode chokepoint only. IME commit,
    /// mouse gestures, and paste are not yet wired — extending coverage to
    /// those sources is a follow-up if cross-source trace correlation is
    /// needed. The counter is per-process monotonic (the static allocator
    /// never resets), so two tabs never share a sequence number — useful for
    /// cross-tab log correlation during free testing.
    pub(crate) fn next_input_seq(&mut self) -> u64 {
        self.active_mut().next_input_seq()
    }

    /// Current input-event sequence (last value returned by `next_input_seq`).
    /// Read from trace points that observe an Effect but did not themselves
    /// bump the counter (e.g. `drain_effects`).
    pub(crate) fn input_seq(&self) -> u64 {
        self.active().input_seq
    }

    /// Grid dimensions of the active pane for the current split layout.
    /// Unlike the full content-area dimensions, this remains equal to the
    /// active Terminal grid after a split and is safe for redraw convergence.
    pub(crate) fn active_pane_dimensions_for_rect(
        &self,
        content_rect: weft_core::pane_layout::Rect,
        cell_w: f32,
        cell_h: f32,
    ) -> Option<(usize, usize)> {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let rect = self
            .split_tree
            .layout(content_rect)
            .into_iter()
            .find_map(|(id, rect)| (id == self.active_pane).then_some(rect))?;
        let [x0, y0, x1, y1] = rect;
        let pane_width = (x1 - x0).max(0.0);
        let pane_height = (y1 - y0).max(0.0);
        // v1.10.19/25 Batch 2: cols track the pane's terminal cols *kind* and
        // must not swing with transient DEC 1049 toggles (see
        // `Terminal::tui_cols_kind`; Content for primary TUIs, Full on alt).
        // v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): `burst_locked_cols` freezes
        // rows+cols at the pane's current grid size while a toggle storm is
        // fresh — desired == current → the shared drift check queues nothing
        // mid-storm, so the app gets a single post-quiet redraw (tab/resize.rs).
        let (rows, cols) =
            self.burst_locked_cols(self.active_pane, pane_width, pane_height, cell_w, cell_h);
        (rows > 0 && cols > 0).then_some((rows, cols))
    }

    /// v1.3 Batch 6: Resize every pane's terminal according to its split-tree-
    /// computed rect. `content_rect` is the full content area (chrome already
    /// subtracted); `cell_w` / `cell_h` are physical-pixel cell dimensions.
    /// Each pane's (rows, cols) is derived from its rect size ÷ cell size.
    /// Returns `true` if the active pane was resized.
    ///
    /// For single-pane tabs this is equivalent to the pane-level
    /// `resize_terminal_and_queue` — the split tree returns one rect equal to
    /// `content_rect`.
    pub(crate) fn resize_all_panes_for_rect(
        &mut self,
        content_rect: weft_core::pane_layout::Rect,
        cell_w: f32,
        cell_h: f32,
    ) -> bool {
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return false;
        }
        let layouts = self.split_tree.layout(content_rect);
        let active = self.active_pane;
        let mut active_resized = false;
        for (pane_id, rect) in layouts {
            let [x0, y0, x1, y1] = rect;
            let pane_width = (x1 - x0).max(0.0);
            let pane_height = (y1 - y0).max(0.0);
            // v1.10.19/25 Batch 2: cols track the pane's terminal cols kind and
            // must not swing with transient DEC 1049 toggles — the 99↔102 flip
            // re-queues a PTY resize each cycle, feeding the SIGWINCH loop.
            // v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): `burst_locked_cols` freezes
            // rows+cols at the current grid size during a toggle storm
            // (identically to `active_pane_dimensions_for_rect`), so the
            // queued PTY resize is a no-op mid-storm.
            let (rows, cols) =
                self.burst_locked_cols(pane_id, pane_width, pane_height, cell_w, cell_h);
            if rows == 0 || cols == 0 {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                if pane.resize_terminal_and_queue(rows, cols) && pane_id == active {
                    active_resized = true;
                }
            }
        }
        active_resized
    }

    /// v1.3 Batch 6: Find the pane whose split-tree-computed rect contains
    /// the physical-pixel point `(x, y)`. Used by mouse hit-testing to route
    /// clicks / scrolls to the correct pane. Returns `None` if the point is
    /// outside all panes (e.g. in the padding or chrome).
    pub(crate) fn pane_hit_test(
        &self,
        x: f32,
        y: f32,
        content_rect: weft_core::pane_layout::Rect,
    ) -> Option<PaneId> {
        let layouts = self.split_tree.layout(content_rect);
        layouts.into_iter().find_map(|(id, rect)| {
            let [x0, y0, x1, y1] = rect;
            (x >= x0 && x < x1 && y >= y0 && y < y1).then_some(id)
        })
    }

    /// v1.11.15 (FIX A, PLAN_v11115 §1.2): whether the active pane's PTY
    /// reader observed this session's mouse-disable sequence (or its exit).
    /// While true, no hover/wheel bytes may be written to this pane — the
    /// shell behind the in-flight disable may already be in cooked mode.
    /// Per-pane by design: only THIS session's sends are suppressed.
    pub fn mouse_suppressed(&self) -> bool {
        weft_core::input::is_suppressed(&self.mouse_suppress)
    }

    /// v1.11.15 (FIX D, PLAN_v11115 §4): copy the active pane's negotiated
    /// input modes into `input_handler` before mouse-event encoding. One
    /// method closes three sync gaps (Release path, TerminalOwner move
    /// shortcut, non-active owner tabs) — it reads the TARGET tab.
    /// Borrow-split precedent: mouse_controller.rs's sync.
    pub fn sync_mouse_modes(&mut self) {
        // T10: mode flags read under ONE short guard ending at the `drop`
        // below; the `input_handler` writes afterwards are lock-free pane
        // fields (reads extracted first). Re-entrancy (D9 rule 5): this
        // method locks — never call it inside another guard scope.
        let Some(terminal) = self.lock_terminal() else {
            return;
        };
        let mouse_protocol = terminal.mouse_protocol();
        let sgr_mouse = terminal.sgr_mouse();
        let app_cursor_keys = terminal.app_cursor_keys();
        drop(terminal);
        self.input_handler.app_cursor_keys = app_cursor_keys;
        self.input_handler.mouse_protocol = mouse_protocol;
        self.input_handler.sgr_mouse = sgr_mouse;
    }

    /// Deliver one PTY-native interrupt without discarding command output.
    ///
    /// Ctrl+C is always one ETX byte, regardless of whether the foreground app
    /// uses the alternate screen, primary-screen cursor addressing, raw mode,
    /// SSH, or a conventional shell command. Output is intentionally retained:
    /// interactive tools write confirmation/resume tails after the first or
    /// second Ctrl+C, and a post-write `tcflush` can race away the ETX itself.
    pub fn interrupt_pty(&mut self) -> bool {
        self.active_mut().interrupt_pty()
    }

    /// Write input initiated by the user to this tab's PTY.
    ///
    /// A first Ctrl+C may freeze the current primary-screen transcript while
    /// an interactive program decides whether to exit. Any later user input
    /// means the program continued, so that frozen transcript is stale and
    /// must not be reused by a future interrupt. Keeping this cancellation at
    /// the PTY boundary covers keyboard, paste, mouse, wheel, workflow, and
    /// delayed TUI-scroll input uniformly.
    ///
    /// v1.11.15 (FIX E): returns the bytes the PTY actually accepted
    /// (`Pty::write_sync_reported`) so a partial write is observable;
    /// existing `is_ok()` / `let _ =` callers compile unchanged, and the
    /// paste path consumes the count for a truncation toast.
    pub fn write_user_input(&mut self, data: &[u8]) -> weft_core::pty::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        // Genuine PTY input is the semantic boundary for returning to the
        // live view — EXCEPT while a command is executing. During
        // CommandExecuting, keystrokes are input TO the running program (a
        // primary-screen TUI like openclaw, or an interactive prompt), not a
        // request to leave history browsing. Snapping here would clear
        // `primary_history_view`, and when the program's redraw then crosses
        // the TUI detection threshold, `show_block_view()` flips to the live
        // grid mid-interaction — the openclaw "jump to top" symptom. The
        // redraw_controller's follow logic already snaps on fresh output when
        // appropriate, so this snap is redundant during execution anyway.
        // [P2 TOCTOU 登记·接受一帧滞后] executing=probe 短锁快照，与分支动
        // 作之间 worker 可翻转 phase：最坏单键入走错分支、下一键自纠；PTY
        // 写两分支均无条件执行，无输入丢失/数据损坏。
        let executing = self
            .with_terminal(|t| {
                t.block_tracker().phase() == weft_core::blocks::ShellPhase::CommandExecuting
            })
            .unwrap_or(false);
        if !executing {
            self.snap_to_bottom();
        } else if self
            .with_terminal(|t| t.is_alt_screen_active())
            .unwrap_or(false)
        {
            // Round 4 review (LOW-1): inside an alternate-screen child
            // (vim/less) the BlockView is not rendered — a snapshot bypass
            // window has no consumer. Keep the keystroke from arming a
            // window that would only rescan the frozen primary document.
            //
            // v1.10.12: any keystroke also exits the alt-screen history peek
            // (the input is meant for the TUI, so return to its live grid and
            // reset the browse position to the tail for the next peek).
            self.snap_to_bottom();
        } else {
            // v1.10.4: EVERY keystroke during execution drives the running
            // program's repaint. A screen-owned primary-screen TUI (openclaw)
            // has suspended print capture, so its live block updates only via
            // snapshot refresh — arm the rate-limit bypass window on every
            // keystroke, whether or not the user is browsing history. Without
            // this, a keypress right after the user scrolled back to the tail
            // (history browsing off) is gated by the 50ms snapshot rate limit
            // and the selection change lags a blink (the persistent flicker).
            // The redraw's split pty batches (cursor-move header, then
            // content) each get an unthrottled snapshot inside the window.
            self.primary_history_refresh.arm_force();
            // v1.10.6: also refresh the snapshot NOW (synchronously) so the
            // precise cursor line is available for the next paint frame —
            // even if no PTY output arrives (e.g. IME preedit before commit,
            // or a TUI that redraws on the next vsync). Without this the
            // cursor_snapshot_line stays None until the force window is
            // consumed, and the caret/preedit fall back to the imprecise
            // formula (which lands on the wrong row).
            self.with_terminal(|terminal| {
                if terminal.show_block_view() {
                    terminal.snapshot_primary_screen_output_for_caret();
                }
            });
        }
        let pane = self.active_mut();
        pane.with_terminal(|t| t.cancel_primary_screen_interrupt_capture());
        // T10 P2 (D4): through the shared write half — this lock serializes
        // against the parse worker's VT replies at the fd boundary.
        let writer = pane.writer.as_ref().ok_or_else(|| {
            PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "tab has no PTY",
            ))
        })?;
        writer.write_sync_reported(data)
    }

    /// Directory inherited by a newly-created sibling tab. Prefer live OSC 7
    /// state, retaining a restored cwd until the shell reports one.
    pub fn launch_cwd(&self) -> Option<String> {
        self.with_terminal(|t| t.cwd().map(str::to_owned))
            .flatten()
            .or_else(|| self.restored_cwd_fallback().map(str::to_owned))
    }

    /// v1.12.28 (P1-02 ②): serialize one leaf pane for the split-tree
    /// snapshot. A live leaf carries its live terminal state (same field
    /// semantics as the single-pane path). A PTY-dead leaf keeps its tree
    /// position with the single-pane PTY-dead semantics (real
    /// `restored_snapshot` first, then a minimal shell from `restored_cwd`).
    /// Documented degradation: a dead leaf reopens as an empty
    /// pending-activation pane.
    fn pane_leaf_snapshot(&self, pane_id: PaneId) -> SnapshotPaneNode {
        let Some(pane) = self.panes.get(&pane_id) else {
            return SnapshotPaneNode::Pane {
                cwd: None,
                editor_buffer: String::new(),
                block_ids: Vec::new(),
            };
        };
        // T10 P1: the live serialization runs inside the guard scope and only
        // the owned node escapes; the PTY-dead fallbacks below are unchanged.
        let live = pane.with_terminal(|terminal| SnapshotPaneNode::Pane {
            cwd: terminal
                .cwd()
                .map(str::to_owned)
                .or_else(|| pane.restored_cwd_fallback().map(str::to_owned)),
            editor_buffer: TabSnapshot::encode_editor_buffer(&terminal.editor().buffer),
            block_ids: terminal.block_tracker().lineage_block_ids(),
        });
        if let Some(node) = live {
            return node;
        }
        if let Some(saved) = pane.restored_snapshot.as_ref() {
            return SnapshotPaneNode::Pane {
                cwd: saved.cwd.clone(),
                editor_buffer: saved.editor_buffer.clone(),
                block_ids: saved.block_ids.clone(),
            };
        }
        SnapshotPaneNode::Pane {
            cwd: pane.restored_cwd.clone(),
            editor_buffer: String::new(),
            block_ids: Vec::new(),
        }
    }

    /// v1.0 H4: Serialize this tab's UI state to a [`TabSnapshot`] for
    /// SQLite persistence. A restored tab whose PTY failed keeps its loaded
    /// snapshot; only a fresh empty tab with no recovery state returns `None`.
    ///
    /// The PTY itself is NOT serialized (impossible to revive). On
    /// restore, the tab shows the saved editor draft + block history;
    /// the user presses Enter to spawn a fresh shell in the saved cwd.
    ///
    /// v1.12.28 (P1-02 ②): multi-pane tabs additionally persist the split
    /// tree (`panes`) — see the branch below.
    pub fn to_snapshot(&self, position: usize, active: bool) -> Option<TabSnapshot> {
        // T10 P1 (D9 rule 2): the terminal state is read under ONE short
        // guard; the multi-pane fold below re-locks per leaf
        // (`pane_leaf_snapshot`), so no guard may be held across it.
        let live = self.with_terminal(|terminal| {
            let shell_phase = match terminal.block_tracker().phase() {
                weft_core::blocks::ShellPhase::NotIntegrated => "NotIntegrated",
                weft_core::blocks::ShellPhase::AtPrompt => "AtPrompt",
                weft_core::blocks::ShellPhase::CommandExecuting => "CommandExecuting",
            };
            (
                terminal.cwd().map(str::to_owned),
                weft_core::persistence::TabSnapshot::encode_editor_buffer(
                    &terminal.editor().buffer,
                ),
                shell_phase,
                terminal.block_tracker().lineage_block_ids(),
            )
        });
        let Some((live_cwd, editor_buffer, shell_phase, block_ids)) = live else {
            // PTY-dead pane: keep the recovery state serializable. A fresh
            // empty tab with no recovery state still returns `None`
            // (unchanged); a pane with the real attached snapshot or only
            // the workspace cwd fallback (v1.10.24 B1) stays serializable —
            // for the fallback-only case we synthesize the same minimal
            // snapshot the old stub carried.
            let mut snapshot = match self.restored_snapshot.clone() {
                Some(snapshot) => snapshot,
                None => {
                    let cwd = self.restored_cwd.clone()?;
                    TabSnapshot {
                        position: 0,
                        active: false,
                        cwd: Some(cwd),
                        block_scroll_offset: 0,
                        editor_buffer: String::new(),
                        shell_phase: "AtPrompt".to_string(),
                        block_ids: Vec::new(),
                        panes: None,
                    }
                }
            };
            if snapshot.cwd.is_none() {
                snapshot.cwd = self.restored_cwd.clone();
            }
            snapshot.position = position;
            snapshot.active = active;
            snapshot.block_scroll_offset = self.block_scroll();
            return Some(snapshot);
        };
        let cwd = live_cwd.or_else(|| self.restored_cwd_fallback().map(str::to_owned));
        // v1.12.28 (P1-02 ②): multi-pane branch FIRST. `pane_count()` is the
        // structural accessor (zoom-safe) — `split_tree().panes()` collapses
        // to `[zoomed]` and `Tab::panes()` iterates a HashMap, so neither may
        // gate this branch. `export_tree` ignores zoom, so the snapshot
        // stores the STRUCTURE tree and a zoomed tab restores un-zoomed
        // (documented). The active leaf's state also fills the top-level
        // legacy fields so single-pane readers stay consistent.
        if self.pane_count() > 1 {
            let active_id = self.active_pane_id();
            let mut index = 0usize;
            let mut active_leaf = 0usize;
            let tree = self
                .split_tree
                .export_tree(|pane_id| (pane_id, self.pane_leaf_snapshot(pane_id)))
                .map(|exported| {
                    snapshot_node_from_tree(exported, active_id, &mut index, &mut active_leaf)
                });
            let panes = tree.map(|tree| PaneTreeSnapshot { tree, active_leaf });
            return Some(TabSnapshot {
                position,
                active,
                cwd,
                block_scroll_offset: self.block_scroll(),
                editor_buffer,
                shell_phase: shell_phase.to_string(),
                block_ids,
                panes,
            });
        }
        // v1.7.6: persist per-tab block ownership so each tab can restore
        // only its own history on next launch (per-tab isolation).
        // v1.12.24 (N-3): use the full lineage (session-produced UNION
        // loaded-from-previous-restore) — the v1.7.6 session-only design
        // dropped the previous generation's recall on every restart-restore
        // cycle, so each ↑ history shrunk by one generation per restart.
        // (cwd / editor_buffer / shell_phase / block_ids were captured under
        // the single short guard at the top of this function.)
        // v1.12.28 (P1-02 ②): single-pane tabs never write the `panes`
        // field — the current path is unchanged apart from the new field.
        Some(TabSnapshot {
            position,
            active,
            cwd,
            block_scroll_offset: self.block_scroll(),
            editor_buffer,
            shell_phase: shell_phase.to_string(),
            block_ids,
            panes: None,
        })
    }

    /// v1.0 H4: Restore the editor draft + block-scroll offset from a
    /// [`TabSnapshot`]. Called after [`Tab::new`] to apply the saved
    /// state. The PTY + terminal are already initialized by `new`;
    /// this just rehydrates the editor buffer and scroll offset.
    ///
    /// Returns `false` if the snapshot's editor buffer JSON was invalid
    /// (the tab stays usable with an empty editor — same as a fresh tab).
    pub fn restore_from_snapshot(&mut self, snap: &TabSnapshot) -> bool {
        self.restored_snapshot = Some(snap.clone());
        self.set_block_scroll(snap.block_scroll_offset);
        match weft_core::persistence::TabSnapshot::decode_editor_buffer(&snap.editor_buffer) {
            Some(buf) => self
                .with_terminal(|terminal| terminal.editor_mut().buffer = buf)
                .is_some(),
            None => {
                tracing::warn!("failed to deserialize editor buffer; using empty");
                false
            }
        }
    }
}

/// v1.12.28 (P1-02 ②): DFS-fold the exported pane tree into a
/// [`SnapshotPaneNode`], recording the DFS index (first child before
/// second — the `export_tree` / `pane_id_at_index` leaf order) of
/// `active_id` into `active_leaf`. `index` counts leaves as they are
/// folded; the caller passes a shared cursor so nested `Split`s keep
/// numbering monotonically.
fn snapshot_node_from_tree(
    tree: PaneTree<(PaneId, SnapshotPaneNode)>,
    active_id: PaneId,
    index: &mut usize,
    active_leaf: &mut usize,
) -> SnapshotPaneNode {
    match tree {
        PaneTree::Leaf((pane_id, node)) => {
            if pane_id == active_id {
                *active_leaf = *index;
            }
            *index += 1;
            node
        }
        PaneTree::Split {
            direction,
            ratio,
            first,
            second,
        } => SnapshotPaneNode::Split {
            direction,
            ratio,
            first: Box::new(snapshot_node_from_tree(
                *first,
                active_id,
                index,
                active_leaf,
            )),
            second: Box::new(snapshot_node_from_tree(
                *second,
                active_id,
                index,
                active_leaf,
            )),
        },
    }
}

#[cfg(test)]
#[path = "tab/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tab/snapshot_tests.rs"]
mod snapshot_tests;

#[cfg(test)]
#[path = "tab/tui_render_mode_tests.rs"]
mod tui_render_mode_tests;
