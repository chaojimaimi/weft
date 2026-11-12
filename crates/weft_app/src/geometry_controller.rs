//! Shared layout and hit-testing controller.

use super::*;

mod targets;

// v1.20.3 WP1 1B: hit-test / pixel→content mapping family moved to a
// #[path] submodule (line-count debt; same extraction shape as targets.rs).
// The #[path] is relative to src/ because geometry_controller.rs is a
// non-mod.rs file.
#[path = "geometry_controller/hit_tests.rs"]
mod hit_tests;

/// Action triggered by clicking a button in the find popup. Produced by
/// `App::find_button_at` (this controller — the hit-test lives here) from
/// the find scene's hit-test rects.
///
/// v1.11.13 (PLAN_v11113 §M5): moved here from main.rs — the enum belongs
/// to the controller that produces and consumes it.
pub(crate) enum FindButtonAction {
    /// Click the ".*" toggle — flip regex mode (visual only).
    ToggleRegex,
    /// Click the "Aa" toggle — flip case-sensitive search.
    ToggleCase,
    /// Click the "↓" button — jump to next match.
    Next,
    /// Click the "↑" button — jump to previous match.
    Prev,
}

impl App {
    pub(super) fn active_scrollbar_layout(
        &self,
    ) -> Option<crate::scrollbar_component::ScrollbarLayout> {
        let renderer = self.renderer.as_ref()?;
        let ctx = renderer.layout_ctx?;
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient reads as None.
        let tab = self.sessions.active()?;
        let terminal = tab.lock_terminal()?;
        if !terminal.show_block_view() {
            return None;
        }
        // Step 3: read cached scroll metrics from the last draw() instead of
        // re-running block_content_metrics_with_cache (O(n)) on every mouse
        // move. The cache is written at the end of each draw() call; mouse
        // events read the previous frame's metrics (1-frame lag is
        // imperceptible for scrollbar hit-testing).
        let (total, visible, max_scroll) = renderer.cached_scroll_metrics.get()?;
        crate::scrollbar_component::scrollbar_layout(
            &ctx,
            total,
            visible,
            max_scroll,
            tab.block_scroll(),
        )
    }

    /// Build the one shared terminal layout used by PTY sizing, rendering and
    /// pointer hit-testing.
    pub(super) fn terminal_layout(&self) -> Option<TerminalLayout> {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return None;
        };
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let layout = terminal_layout_for_renderer(renderer, window.inner_size(), chrome_left);
        Some(layout)
    }

    /// Compute grid dimensions; returns `(0, 0)` until layout is ready.
    pub(super) fn grid_dims(&self) -> (usize, usize) {
        self.terminal_layout()
            .map(TerminalLayout::dimensions)
            .unwrap_or((0, 0))
    }

    pub(super) fn terminal_content_contains(&self, x: f64, y: f64) -> bool {
        self.terminal_layout()
            .is_some_and(|layout| layout.contains_content(x, y))
    }

    /// Recompute grid rows/cols from the current window + cell dimensions and
    /// resize the terminal / queue a PTY SIGWINCH. Used after a font or padding
    /// change (cell size or usable area changes) and on window resize.
    pub(super) fn recompute_layout(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        // v1.14.2 (A3/PLAN §2): lightweight path — when the window size
        // already matches the renderer viewport, skip `renderer.resize`
        // (no set_drawable_size, no force_full_grid_redraw, no nested
        // request_redraw). This is the drag/tween hot path (A2's 8ms
        // cadence): chrome_left moves, the size does not. Failure safety
        // rides the existing per-frame fingerprints — active grid
        // prev_grid_origin (paint/grid.rs), background grid entry
        // origin/dims, BgBlockFingerprint.rect — so a sidebar-driven
        // origin_x change still full-redraws exactly once, then resumes
        // incrementally. A REAL window resize still passes (size differs).
        if (size.width as f32, size.height as f32) != renderer.viewport {
            renderer.resize(window, size);
        }
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let base_layout = terminal_layout_for_renderer(renderer, size, chrome_left);
        if base_layout.rows == 0 || base_layout.cols == 0 {
            return;
        }
        // v0.9 W5: resize every tab's terminal so non-active tabs also pick
        // up the new chrome_left (sidebar open/close shifts the grid). Only
        // the active tab sends a PTY resize immediately; background tabs get
        // their PTY resize on activation (refresh_grid_for_active_tab).
        //
        // v1.3 Batch 6: resize ALL panes per tab according to their split-tree
        // rects. For single-pane tabs this is equivalent to the old
        // `resize_terminal_and_queue` call (the split tree returns one rect
        // equal to the content area).
        let active = self.sessions.active_idx();
        let header_rows = renderer.block_header_rows();
        let layout_ctx = base_layout.layout_ctx();
        let content_rect: weft_core::pane_layout::Rect = [
            base_layout.content.left as f32,
            base_layout.content.top as f32,
            base_layout.content.right as f32,
            base_layout.content.bottom as f32,
        ];
        let cell_w = base_layout.cell_width as f32;
        let cell_h = base_layout.cell_height as f32;
        // WP1.5 F2 (PLAN_v1180_WP15_G4_RECONCILE_CACHE): feed the block
        // layout cache to the per-tab reconcile memo below — a cache-backed
        // compute is O(blocks) stale scalar reads instead of the O(document)
        // uncached scan, and the memo key carries `total_misses()` so a
        // rebuild after the final cols invalidates stale memos. Borrow shape
        // (WP1.5 review P1-1, same as window_event/resized.rs): `renderer`
        // above is a `&mut` binding held across this loop, so the RefCell
        // borrow MUST come from that binding. R2: try_borrow, never panic —
        // a live draw-side borrow degrades to the uncached direct path
        // (correctness first), but the degradation MUST be audible: a silent
        // fallback would resurrect the O(document) stall with no signal.
        let block_cache = match renderer.block_layout_cache.try_borrow() {
            Ok(cache) => Some(cache),
            Err(_) => {
                tracing::warn!("block reconcile cache unavailable");
                None
            }
        };
        for (i, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
            if tab.has_terminal() {
                // F7-1: KEPT for every tab — the PTY+Grid resize queue still
                // enqueues for background panes, and a real resize still
                // clears their content-anchored selections (the selection
                // contract is resize-scoped, not metrics-scoped).
                let resized = tab.resize_all_panes_for_rect(content_rect, cell_w, cell_h);
                if resized {
                    // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): a terminal
                    // resize changes every line wrap and char offset — content
                    // anchors cannot survive it, so block-view selections are
                    // cleared (Warp clears on resize too). The grid selection
                    // model is untouched here (its viewport-relative drift
                    // guards live in the scroll path).
                    for pane in tab.panes_mut() {
                        if pane.selection_handler.block_view_selection.is_some() {
                            tracing::info!("terminal resized; cleared block selection");
                            // v1.10.26 (rust-reviewer S3): clear only the
                            // block-view half — the grid `Selection` and the
                            // in-progress `selecting` flag survive (they have
                            // their own viewport-relative resize semantics, and
                            // `clear()` would also drop them, contradicting the
                            // comment above).
                            pane.selection_handler.block_view_selection = None;
                            pane.selection_handler.block_doc_fingerprint = None;
                        }
                    }
                }
                if resized && i == active {
                    let active_id = tab.active_pane_id();
                    let layouts = tab.split_tree().layout(content_rect);
                    if let Some((_, rect)) = layouts.into_iter().find(|(id, _)| *id == active_id) {
                        let [x0, y0, x1, y1] = rect;
                        let w = (x1 - x0).max(0.0);
                        let h = (y1 - y0).max(0.0);
                        let cols = (w / cell_w).floor() as usize;
                        let rows = (h / cell_h).floor() as usize;
                        info!(active = ?active_id, rows, cols, pane_rect = ?rect, "terminal resized");
                    } else {
                        let (new_rows, new_cols) = base_layout.dimensions();
                        info!(rows = new_rows, cols = new_cols, "terminal resized");
                    }
                }
                // F7-1 (PLAN_v1184 §2-F7-1): background tabs skip the
                // block-scroll metrics reconciliation. This loop runs on the
                // hot no-Resized families (sidebar 8ms tween, divider drag,
                // font hotkeys, redraw-pump convergence checks) — for a
                // retain-evicted cache entry `reconciled_with_memo` pays the
                // O(document) `completed_output_rows` fallback rebuild, and
                // the tab is not painted anyway. A background tab clamps on
                // activation via the generation hook
                // (`arm_reclamp_on_tab_activation`) or its next active-pass
                // here.
                if !crate::app::reconcile_scroll::reconcile_in_scope(i, active) {
                    continue;
                }
                let pane_layout_ctx = tab
                    .split_tree()
                    .layout(content_rect)
                    .into_iter()
                    .find_map(|(id, rect)| {
                        (id == tab.active_pane_id()).then_some(layout_ctx.for_pane(rect))
                    })
                    .unwrap_or(layout_ctx);
                let previous = tab.block_scroll();
                // v1.14.3 (T2): the reconcile goes through the pane-level
                // memo (`app::reconcile_scroll`) — a same-key repeat (the
                // per-8ms drag recomputes with unchanged content/geometry)
                // reuses the previous metrics instead of re-scanning O(blocks)
                // per event. The owned `lock_terminal` guard is the
                // D9-blessed shape that lets `tab.reconcile_memo` be mutated
                // beside the guard (the guard carries no borrow of the pane).
                // v1.18.0 WP1.5 F2: plan §T2-2 ("the Resized/recompute path
                // stays UNCACHED — its key would mismatch anyway on fresh
                // geometry") is UNFROZEN: the freeze premise ("a fresh
                // geometry key always misses") does not hold for
                // `get_if_cached`, which is id-keyed and stale-tolerant. The
                // compute closure now reads the layout cache (stale scalars,
                // O(blocks)) and the memo key carries the `total_misses()`
                // fingerprint, so a rebuild after the final cols invalidates
                // the memo instead of fixating a stale total.
                let reconciliation = tab.lock_terminal().and_then(|terminal| {
                    crate::app::reconcile_scroll::reconciled_with_memo(
                        &mut tab.reconcile_memo,
                        &terminal,
                        &pane_layout_ctx,
                        header_rows,
                        previous,
                        block_cache.as_deref(),
                    )
                });
                if let Some((reconciled, total, visible)) = reconciliation {
                    // WP1.5 F3: this site just clamped from (possibly
                    // cols-stale) cache scalars — arm the epoch-driven
                    // re-clamp consumer. This covers the no-Resized families
                    // (divider drag / font change / sidebar toggles) whose
                    // cols change without any window Resized event.
                    // P1-1: every arm resets the "re-clamped since arm"
                    // dimension so the consumer's FIRST pass re-clamps
                    // unconditionally (see block_reclamp_action) — this
                    // family arms before any draw has rebuilt the cache,
                    // so the next pass can look spuriously settled.
                    self.window_runtime.block_reconcile_pending = true;
                    self.window_runtime.block_reclamp_reclamped = false;
                    if previous != reconciled {
                        info!(
                            tab = i,
                            previous,
                            reconciled,
                            total,
                            visible,
                            "reconciled block scroll during layout recompute"
                        );
                        tab.set_block_scroll(reconciled);
                    }
                }
            }
        }
        // WP1.5 F2 (review P1-1 borrow delimitation): the cache Ref must die
        // before the `&mut self` calls below — including
        // `scroll_active_tab_into_view()`.
        drop(block_cache);
        self.window_runtime.last_resize_instant = std::time::Instant::now();
        self.scroll_active_tab_into_view();
    }

    /// WP1.5 F3 (PLAN_v1180_WP15_G4_RECONCILE_CACHE): the epoch-driven
    /// re-clamp consumer — the persistent invariant behind the two armed
    /// reconcile sites. Runs at the TOP of `about_to_wait` every pass
    /// (落点定案: about_to_wait 开头,见交付报告): the draw has finished (the
    /// convergence pump bumps `total_misses()` inside draw, so this pass
    /// already sees the fresh epoch), the renderer borrow here is SHARED
    /// (no conflict with anything), `tabs_mut` has no cross-borrow, and
    /// winit guarantees the pass runs. The final pump rebuild's pass
    /// therefore completes the terminal re-clamp in the same pass.
    ///
    /// Semantics (plan F3): re-clamps ONLY the scroll offset
    /// (`reconciled_block_scroll` pure fn) — no layout recompute, no
    /// SIGWINCH. F7-1 (PLAN_v1184 §2-F7-1) narrows the iteration from ALL
    /// tabs to the ACTIVE tab (the draw paints only the active tab, and a
    /// background tab's re-clamp here would pay the O(document) metrics
    /// fallback for its evicted cache entries); background tabs clamp on
    /// activation via the generation hook. Self-clear = at least one
    /// re-clamp since the arm (P1-1) + epoch stable + quiescent — the first
    /// pass after an arm re-clamps unconditionally and a SECOND pass
    /// verifies and clears (`block_reclamp_action` truth table).
    ///
    /// F7-1 activation timing (truth table ③): the generation compare runs
    /// BEFORE the pending early-return so a tab activation arms here even
    /// when no effect batch drained first (tab-bar click et al.), and the
    /// same pass performs the clamp. The clamp runs AFTER the frame's draw
    /// (this is about_to_wait), so a real delta corrects a scroll the user
    /// already SAW stale — the delta therefore requests a corrective
    /// `request_redraw` (评审 P2-3).
    pub(super) fn reclamp_block_scroll_pending(&mut self) {
        // F7-1: the activation hook's second (coverage) site — see
        // `arm_reclamp_on_tab_activation` for why both sites share one
        // helper. Placed before the early-return: the hook is what ARMS
        // `block_reconcile_pending` for effect-less activation paths.
        self.arm_reclamp_on_tab_activation();
        if !self.window_runtime.block_reconcile_pending {
            return;
        }
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        // Same R2 contract as the two sites: try_borrow, never panic, and
        // a conflict is AUDIBLE. On conflict the flag stays armed — the
        // next pass retries.
        let cache = match renderer.block_layout_cache.try_borrow() {
            Ok(cache) => cache,
            Err(_) => {
                tracing::warn!("block reconcile cache unavailable");
                return;
            }
        };
        let epoch = cache.total_misses();
        let quiescent = cache.pending_ids_empty();
        let epoch_advanced = epoch != self.window_runtime.block_reconcile_epoch;
        let arm = crate::app::reconcile_scroll::block_reclamp_action(
            self.window_runtime.block_reconcile_pending,
            epoch_advanced,
            quiescent,
            self.window_runtime.block_reclamp_reclamped,
        );
        match arm {
            crate::app::reconcile_scroll::BlockReclampAction::Skip => return,
            crate::app::reconcile_scroll::BlockReclampAction::Reclamp
            | crate::app::reconcile_scroll::BlockReclampAction::ReclampAndClear => {}
        }
        // Re-clamp every tab against the CURRENT layout, cache-backed
        // (O(blocks) stale scalar reads — the same predicate the F1 site
        // runs, minus the resize/SIGWINCH side effects).
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let chrome_left = if self.panel.open {
            renderer.sidebar_push_width() as f64
        } else {
            0.0
        };
        let base_layout = terminal_layout_for_renderer(renderer, size, chrome_left);
        if base_layout.rows == 0 || base_layout.cols == 0 {
            return;
        }
        let header_rows = renderer.block_header_rows();
        let layout_ctx = base_layout.layout_ctx();
        let content_rect: weft_core::pane_layout::Rect = [
            base_layout.content.left as f32,
            base_layout.content.top as f32,
            base_layout.content.right as f32,
            base_layout.content.bottom as f32,
        ];
        // No `resize_all_panes_for_rect` here (F3 re-clamps the scroll
        // offset only — no pane resize, no SIGWINCH).
        // F7-1: active tab only (see the doc comment) — the predicate is the
        // shared scope fn the two reconcile loops use.
        let active_tab_index = self.sessions.active_idx();
        let mut clamp_delta = false;
        for (i, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
            if !tab.has_terminal() {
                continue;
            }
            // F7-1: the epoch/quiescence arming is a SHARED-cache property and
            // stays global; only the per-tab clamp is scoped. A background
            // tab's stale scroll is picked up by its own activation hook.
            if !crate::app::reconcile_scroll::reconcile_in_scope(i, active_tab_index) {
                continue;
            }
            let pane_layout_ctx = tab
                .split_tree()
                .layout(content_rect)
                .into_iter()
                .find_map(|(id, rect)| {
                    (id == tab.active_pane_id()).then_some(layout_ctx.for_pane(rect))
                })
                .unwrap_or(layout_ctx);
            let previous = tab.block_scroll();
            let reconciliation = tab.lock_terminal().and_then(|terminal| {
                crate::block_component::reconciled_terminal_block_scroll(
                    &terminal,
                    &pane_layout_ctx,
                    header_rows,
                    previous,
                    Some(&cache),
                )
            });
            if let Some((reconciled, _total, _visible)) = reconciliation {
                if previous != reconciled {
                    tab.set_block_scroll(reconciled);
                    clamp_delta = true;
                }
            }
        }
        // F7-1 (评审 P2-3): the clamp ran after this pass's draw — a real
        // delta means the user saw a frame with the stale offset. One
        // corrective redraw (draw only paints the active tab, which is
        // exactly the tab that clamped).
        if clamp_delta {
            self.request_redraw();
        }
        self.window_runtime.block_reconcile_epoch = epoch;
        // P1-1: the arm's re-clamp obligation is now discharged — the NEXT
        // pass (epoch stable) may verify-and-clear. The clear itself is
        // gated on the verdict arm (ReclampAndClear), NOT on quiescence:
        // the truth table keeps an epoch-advanced pass armed even when the
        // cache looks quiescent, so the fresh-epoch clamp gets one verified
        // pass before the flag dies.
        self.window_runtime.block_reclamp_reclamped = true;
        if matches!(
            arm,
            crate::app::reconcile_scroll::BlockReclampAction::ReclampAndClear
        ) {
            self.window_runtime.block_reconcile_pending = false;
        }
    }

    /// Convert pixel coordinates to grid (row, col).
    ///
    /// v1.3 Batch 6: for multi-pane tabs, the grid position is relative to
    /// the **active pane's** origin (not the full content area). Without this
    /// adjustment, clicking in a right-side pane would produce column indices
    /// offset by the left pane's width, clamping to the wrong cell. For
    /// single-pane tabs the pane origin equals the content origin — no change.
    pub(super) fn pixel_to_grid(&self, x: f64, y: f64) -> GridPos {
        let Some(layout) = self.terminal_layout() else {
            return GridPos::new(0, 0);
        };
        // Clamp to valid grid bounds. A click past the right/bottom edge (e.g.
        // a drag-to-select ending at the window margin) would otherwise yield
        // col == num_cols / row == num_rows and panic text_from_grid on copy.
        let (num_rows, num_cols) = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols)))
            .unwrap_or((1, 1));

        // v1.3 Batch 6: adjust (x, y) by the active pane's origin offset.
        // `grid_position` subtracts `content.left/top` internally; we want it
        // to subtract the pane's `x0/y0` instead. The adjustment is:
        //   adjusted_x = x - (pane_x0 - content.left)
        // so that grid_position computes (adjusted_x - content.left) = (x - pane_x0).
        // v1.10.20 (改动 3): a primary-screen TUI grid view additionally
        // insets by the BlockView gutter (renderer `GridViewPolicy`
        // `inset_block_gutter` — same judgment, renderer.rs:762). The third
        // tuple element is that gutter delta: hit-testing must use the SAME
        // origin as the render or clicks land ~1.5 cols right of the visible
        // character. Alt-screen TUIs stay edge-to-edge (delta 0).
        let inset_block_gutter = self
            .sessions
            .active()
            .and_then(|tab| {
                tab.with_terminal(|t| {
                    !t.is_alt_screen_active() && t.primary_screen_owns_live_view()
                })
            })
            .unwrap_or(false);
        let (adj_x, adj_y, gutter_delta) = if layout.contains_content(x, y) {
            let content_rect: weft_core::pane_layout::Rect = [
                layout.content.left as f32,
                layout.content.top as f32,
                layout.content.right as f32,
                layout.content.bottom as f32,
            ];
            // v1.12.25 (audit 3-B, P1-01): no active tab — same degenerate
            // origin as "active pane not found" below.
            self.sessions
                .active()
                .and_then(|tab| {
                    tab.split_tree()
                        .layout(content_rect)
                        .into_iter()
                        .find(|(id, _)| *id == tab.active_pane_id())
                        .map(|(_, rect)| {
                            let [px0, py0, _, _] = rect;
                            let dx = px0 as f64 - layout.content.left;
                            let dy = py0 as f64 - layout.content.top;
                            let pane_ctx = layout.layout_ctx().for_pane(rect);
                            let origin_x = crate::terminal_geometry::grid_hit_origin_x(
                                &pane_ctx,
                                inset_block_gutter,
                            );
                            (x - dx, y - dy, origin_x - px0 as f64)
                        })
                })
                .unwrap_or((x, y, 0.0))
        } else {
            (x, y, 0.0)
        };

        let (row, col) = layout.grid_position(adj_x - gutter_delta, adj_y, num_rows, num_cols);
        GridPos::new(row, col)
    }
}
