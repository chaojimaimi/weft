//! `dispatch_window_event`'s `Resized` arm, moved verbatim out of
//! `window_event_controller.rs` (v1.12.27b P1-04, former :15-204).
//!
//! Zero-rewrite: every statement and comment below is byte-identical to the
//! extracted arm. The external move is the plan's sanctioned contingency —
//! the same-file split landed at 813 lines (>800), and this is the largest
//! arm (~190 lines at baseline).

use crate::terminal_geometry::terminal_layout_for_renderer;
use tracing::info;

impl crate::App {
    /// v1.12.27b (P1-04): verbatim move of the `Resized` arm body (baseline
    /// :15-204) — the largest arm, split out for testability.
    pub(crate) fn on_window_resized(&mut self, physical_size: winit::dpi::PhysicalSize<u32>) {
        // PLAN_zoom (field run): a PROGRAMMATIC zoom (double-click)
        // shows up here as a one-shot size jump with inLiveResize
        // already FALSE (the zoom's internal live-resize window has
        // closed before winit dispatches Resized) -- while a drag
        // streams small steps with inLiveResize true. v4 (field run
        // 2): the zoom ANIMATES -- a stream of small-step Resized
        // events (~100 ms total) with inLiveResize false throughout,
        // so no single frame ever exceeds a jump threshold. Arm on
        // ANY size change; the inLiveResize guard below separates it
        // from drags.
        let size_changed = self.window_runtime.last_resized_physical
            != Some((physical_size.width, physical_size.height));
        self.window_runtime.last_resized_physical =
            Some((physical_size.width, physical_size.height));
        // Grid/PTY tracks the renderer's visible terminal content
        // rectangle. The editor box is an overlay, but title/tab
        // chrome is outside that rectangle and must be subtracted.
        if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
            // Responsive geometry depends on the new viewport, so
            // update the renderer before asking for sidebar/chrome
            // metrics or terminal rows/cols.
            renderer.resize(window, physical_size);
            // v0.9 W5: subtract sidebar width when the panel is open so
            // the grid reflows beside the sidebar (mirrors grid_dims).
            let chrome_left = if self.panel.open {
                renderer.sidebar_push_width() as f64
            } else {
                0.0
            };
            let base_layout = terminal_layout_for_renderer(renderer, physical_size, chrome_left);

            if base_layout.cols > 0 && base_layout.rows > 0 {
                // Queue every pane's latest target geometry. The active
                // pane commits PTY then Grid on this redraw; background
                // panes coalesce the cascade and commit both together
                // after it settles. Keeping the pair transactional
                // prevents old-width output from wrapping in a Grid
                // that has already adopted the new width.
                //
                // v1.3 Batch 6: resize ALL panes per tab according to
                // their split-tree rects. For single-pane tabs this is
                // equivalent to the old `resize_terminal_and_queue`.
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
                for (tab_index, tab) in self.sessions.tabs_mut().iter_mut().enumerate() {
                    if tab.terminal.is_some() {
                        tab.resize_all_panes_for_rect(content_rect, cell_w, cell_h);

                        // `block_scroll_offset` is measured from the
                        // bottom of a width-dependent document. A
                        // larger viewport usually wraps fewer rows and
                        // shows more of them, so an offset that was
                        // valid before maximize can exceed the new
                        // range and make the transcript tail
                        // unreachable. Reconcile against the detached
                        // snapshot now, before SIGWINCH causes the TUI
                        // to repaint asynchronously.
                        let previous = tab.block_scroll();
                        let reconciliation = tab.terminal.as_ref().and_then(|terminal| {
                            crate::block_component::reconciled_terminal_block_scroll(
                                terminal,
                                &layout_ctx,
                                header_rows,
                                previous,
                            )
                        });
                        if let Some((reconciled, total, visible)) = reconciliation {
                            if previous != reconciled {
                                info!(
                                    tab = tab_index,
                                    previous,
                                    reconciled,
                                    total,
                                    visible,
                                    "reconciled block scroll during resize"
                                );
                                tab.set_block_scroll(reconciled);
                            }
                        }
                    }
                }
                info!(
                    rows = base_layout.rows,
                    cols = base_layout.cols,
                    "all tabs resized (event)"
                );
                self.window_runtime.last_resize_instant = std::time::Instant::now();
                // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING)
                // DEBUG probe (stage 1/4): the Resized event — anchor
                // for the ioctl-commit / first-pty-output / first-present
                // RESIZE_PROBE chain that quantifies the resize blank
                // interval and the omp repaint latency.
                tracing::debug!(
                    rows = base_layout.rows,
                    cols = base_layout.cols,
                    "RESIZE_PROBE window_resized",
                );
                // `renderer.resize()` above already armed the stage-4
                // first-present probe.
                // v1.2-fix: re-clamp tab scroll offset after resize.
                // The window may have grown/shrunk, changing max_scroll.
                // Without this, a stale scroll_offset can leave tabs
                // culled (invisible) after resize.
                self.clamp_tab_scroll();
                self.scroll_active_tab_into_view();
            }
        }
        // v1.11.10 (PLAN_v11110 M-B/D-d): synchronous same-tick draw
        // while live resizing. The bounds change stretches the
        // previous drawable the instant AppKit commits it; a queued
        // RedrawRequested lands a beat later, so
        // presentsWithTransaction never saw the intermediate sizes.
        // Polling here (before the draw) plus the forced path
        // (bypassing the sync-output / route-consume early returns)
        // gives every resize tick an atomic frame — the missing two
        // of Warp's three-piece guarantee. Unconditional
        // set_live_resize: the false reset must not depend on a
        // later RedrawRequested arriving (the last drag event can be
        // a Resized).
        if let (Some(renderer), window) = (&mut self.renderer, self.window.as_ref()) {
            // PLAN_zoom Z-d: the zoom-sequence marker is armed by
            // `is_programmatic_resize_jump` outside a live-resize
            // gesture (arm block below); when hot it extends the
            // same-tick draw to the programmatic zoom.
            // HIGH-1 (round 3): arm the zoom channel ONLY outside a
            // live-resize gesture -- a fast drag coalesces Resized
            // events with >120 physical-px deltas (2x screen: 60 pt),
            // which would re-arm it (per-frame CA flush through the
            // drag plus a 300 ms tail, the cost PLAN §四 froze). The
            // zoom's own Resized arrives with inLiveResize already
            // false, so the guard separates the two cleanly. Arm
            // BEFORE the zoom_jump_hot read: the zoom's own final
            // Resized then engages the channel same-frame.
            // v4/Z-f: arm on EVERY non-gesture Resized with a size
            // change (the zoom animates in small steps). Z-f retires
            // the per-step forced DRAW below -- the arming survives
            // as observation/degrade marking only; animation-step
            // frames are supplied by the displayLayer pull
            // (paint/zoom_render.rs). Drags stream with inLiveResize
            // true and are excluded by the same guard (their sync
            // draw comes from the inLiveResize half).
            let in_live_resize_now = window.is_some_and(crate::macos_window::window_in_live_resize);
            if crate::paint::zoom_render::zoom_jump_should_arm(size_changed, in_live_resize_now) {
                self.window_runtime.zoom_jump_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(300));
                // Appendix F-3C: per-step watch update -- the first
                // zoom-channel step opens the window, later steps
                // extend it; the verdict runs at the deterministic
                // WaitUntil expiry (zoom_wait_policy), not here.
                crate::paint::zoom_render::note_zoom_step();
                // Phase F self-healing: re-assert the window beside
                // the watch -- closes the razor-thin re-zoom window
                // where a pending flush could clear the flag under a
                // live second animation.
                crate::paint::zoom_render::set_self_zoom_window(true);
            }
            // PLAN_zoom_drawable_stall Phase E: set_live_resize is fed
            // the gesture-only `in_live_resize_now` poll. The retired
            // `|| zoom_jump_hot` leg (Z-c/Z-d tx-release timing, dead
            // since the 1.12.10 binding removal) would arm the
            // serialized-present regime on animated programmatic
            // resizes (snap/tiling) where nothing presents new-size
            // frames until the <=300 ms flush.
            renderer.set_live_resize(in_live_resize_now);
            // PLAN_zoom Z-f (Appendix E-3): the v4 per-step forced
            // draw is RETIRED for the zoom channel -- a 5-9 ms
            // synchronous draw per animation step blocked the
            // system's zoom animation itself, which stretched the
            // distortion window (Appendix D-0). Only a real drag
            // gesture (inLiveResize true at dispatch) keeps the
            // same-tick synchronous draw.
            if crate::paint::zoom_render::forced_sync_draw_for_resize(in_live_resize_now) {
                self.handle_redraw_requested_forced();
            }
        }
    }
}
