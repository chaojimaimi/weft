//! Tab drag-to-reorder gesture controller (v1.11 ghost drag).
//!
//! Extracted from `mouse_controller.rs` (v1.11.13) to keep the mouse
//! dispatch file within its audited line budget. The gesture model:
//!
//! - press on a tab label records `TabBarDragState` (click-to-switch);
//!   press on the close "×" records the same state with `close_on_release`
//!   so a click closes but a drag reorders.
//! - moves past the 4px threshold engage the ghost drag: the `SessionManager`
//!   Vec stays untouched, the dragged tab renders as a floating pill at the
//!   pointer's grip, and `insert_index` tracks the gap slot under the pointer
//!   (midpoint rule, pure functions in `layout::drag`).
//! - release commits the reorder once (`move_tab`) or executes the deferred
//!   close; `cancel_tab_drag` aborts the gesture without committing when the
//!   pointer leaves the window, focus is lost, or a tab is closed mid-drag.

use super::*;

impl App {
    /// v1.11.13: Execute a deferred close request (press landed on a tab's
    /// close "×", released without exceeding the drag threshold). Performs
    /// the same close as the pre-deferral press handler — active tab closes
    /// with confirmation, background tab closes after its own confirmation.
    /// No-op when the tab vanished between press and release.
    pub(super) fn perform_close_request(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        if idx == self.sessions.active_idx() {
            let effects = self.close_active_tab_with_confirmation();
            self.drain_effects(effects);
            return;
        }
        if !self.confirm_background_tab_close(idx) {
            return;
        }
        // v1.13.6 T10 P2 (D2): async close contract (see close_tab) — a tab
        // with PTY-backed panes finishes on its workers' PtyExited events.
        let async_pending = self
            .sessions
            .tab_mut(idx)
            .map(crate::tab::Tab::begin_close)
            .unwrap_or(false);
        if async_pending {
            // P1-1b: arm the close watchdog (see close_tab) — background
            // closes are equally exposed to SIGHUP-immune children.
            self.schedule_close_watchdog_wake();
            tracing::info!(
                tab = idx,
                "background tab close: PTY teardown initiated; finishing on PtyExited"
            );
            return;
        }
        let blocks = self
            .sessions
            .tab_mut(idx)
            .map(crate::tab::Tab::finish_pending_blocks)
            .unwrap_or_default();
        self.sessions.close_background(idx);
        self.tab_bar.hovered_tab = None;
        let mut effects = Vec::new();
        if !blocks.is_empty() {
            effects.push(crate::effect::Effect::PersistBlocks { blocks });
        }
        effects.push(crate::effect::Effect::PersistTabs);
        self.drain_effects(effects);
    }

    /// v1.11.13: Cancel an in-flight tab drag without committing. Used when
    /// the pointer leaves the window, focus is lost, or a tab is closed
    /// mid-drag — otherwise the stale `tab_drag` state would keep the ghost
    /// pill on screen and swallow every subsequent CursorMoved.
    pub(super) fn cancel_tab_drag(&mut self) {
        if self.interaction.tab_drag.take().is_some() {
            // v1.14.0 (PLAN_v1140 T3): 取消时全部清空吸附 — the slot-shift
            // tweens are keyed by index and the drag preview is abandoned,
            // so the eased positions snap back to their natural slots.
            self.clear_tab_bar_tweens();
            self.request_redraw();
        }
    }

    // ── v1.14.0 (PLAN_v1140 T3): tab-bar motion (indicator + drag yield) ──

    /// Start the active-indicator slide (140ms ease-out) from the currently
    /// drawn underline position to `to_idx`'s tab rect. `from_idx` seeds the
    /// from-rect when no tween is in flight (the pre-switch active tab).
    /// Reduce Motion / missing layout snap: the underline draws in-slot.
    /// Call AFTER `scroll_active_tab_into_view` so the tween coordinates use
    /// the settled scroll offset.
    pub(super) fn begin_tab_indicator_slide(&mut self, from_idx: usize, to_idx: usize) {
        let now = std::time::Instant::now();
        if !crate::app::motion::gated(self.window_runtime.reduce_motion) {
            self.tab_bar.indicator = None;
            return;
        }
        let Some(strip) = self.tab_strip_layout() else {
            self.tab_bar.indicator = None;
            return;
        };
        let (from_x0, from_x1) = match self.tab_bar.indicator.as_ref() {
            // Continuity: retargeting mid-slide starts from the drawn value.
            Some((t0, t1)) => (t0.value_at(now), t1.value_at(now)),
            None => {
                let fx = strip.tabs_start + from_idx as f32 * strip.tab_width - strip.scroll_offset;
                (fx, fx + strip.tab_width)
            }
        };
        let to_x0 = strip.tabs_start + to_idx as f32 * strip.tab_width - strip.scroll_offset;
        let to_x1 = to_x0 + strip.tab_width;
        self.tab_bar.indicator = Some((
            crate::app::motion::Tween::new(
                from_x0,
                to_x0,
                crate::app::motion::TAB_INDICATOR_DUR,
                now,
            ),
            crate::app::motion::Tween::new(
                from_x1,
                to_x1,
                crate::app::motion::TAB_INDICATOR_DUR,
                now,
            ),
        ));
    }

    /// Rebuild the per-index slot-shift tweens (100ms ease-out) for the
    /// non-dragged tabs after an `insert_index` change. The tween drives an
    /// x DELTA relative to the tab's natural slot, so scroll-offset changes
    /// never invalidate it. Mid-flight retargets start from the drawn value
    /// (no jump). Reduce Motion → all shifts snap to zero (instant preview).
    pub(super) fn update_tab_slot_shifts(&mut self) {
        let Some(drag) = self.interaction.tab_drag else {
            return;
        };
        if !drag.moved {
            return; // sub-threshold: no gap is open yet
        }
        let n = self.sessions.len();
        let Some(strip) = self.tab_strip_layout() else {
            return;
        };
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        // Match the tab count. Tween is deliberately non-Clone (RAII counter
        // pairing), so this is `resize_with`/`truncate`, not `resize`.
        if self.tab_bar.tab_slot_shifts.len() < n {
            self.tab_bar.tab_slot_shifts.resize_with(n, || None);
        } else {
            self.tab_bar.tab_slot_shifts.truncate(n);
        }
        for i in 0..n {
            if i == drag.drag_index {
                continue;
            }
            let target_slot =
                crate::layout::render_slot(i, drag.drag_index, drag.insert_index, n).unwrap_or(i);
            let delta = (target_slot as f32 - i as f32) * strip.tab_width;
            let from = match self.tab_bar.tab_slot_shifts[i].as_ref() {
                Some(t) => t.value_at(now),
                None => 0.0,
            };
            if !animated || (from - delta).abs() < 0.5 {
                self.tab_bar.tab_slot_shifts[i] = None;
            } else {
                self.tab_bar.tab_slot_shifts[i] = Some(crate::app::motion::Tween::new(
                    from,
                    delta,
                    crate::app::motion::TAB_SHIFT_DUR,
                    now,
                ));
            }
        }
    }

    /// v1.14.0 (T3): per-frame tab-bar tween maintenance — drop finished
    /// tweens so the live counter releases. Paint falls back to the settled
    /// position (indicator → in-slot, shift → 0), which equals the tween's
    /// target, so the drop never jumps.
    pub(super) fn maintain_tab_bar_tweens(&mut self, now: std::time::Instant) {
        if let Some((t0, t1)) = self.tab_bar.indicator.as_ref() {
            if t0.finished(now) && t1.finished(now) {
                self.tab_bar.indicator = None;
            }
        }
        for entry in self.tab_bar.tab_slot_shifts.iter_mut() {
            if let Some(t) = entry.as_ref() {
                if t.finished(now) {
                    *entry = None;
                }
            }
        }
    }

    /// v1.14.0 (T3): snap both tab-bar animations to their settled positions
    /// (indicator glued to the active slot, shifts zeroed). Called at every
    /// index-instability point: scroll-offset change (防拖影), drag
    /// start/release/commit/cancel, close/new tab.
    pub(super) fn clear_tab_bar_tweens(&mut self) {
        self.tab_bar.indicator = None;
        self.tab_bar.tab_slot_shifts.clear();
    }

    /// v1.11: Handle tab drag-to-reorder. Called from CursorMoved before
    /// the normal pointer routing so the drag works even when the pointer is
    /// over the tab bar (not terminal content).
    ///
    /// Threshold: the pointer must move > 4px (Chebyshev distance) from the
    /// press position before the ghost drag engages. Below that, the gesture
    /// is treated as a plain click (tab already switched on press, or a
    /// deferred × close pending).
    ///
    /// Once engaged, the dragged tab is rendered as a ghost pill following
    /// the pointer (grip = `grab_offset`) and `insert_index` tracks the gap
    /// slot under the pointer (midpoint rule). The `SessionManager` order is
    /// untouched — the reorder commits once on release.
    pub(super) fn handle_tab_drag_move(&mut self, x: f64, y: f64) {
        let mut drag = match self.interaction.tab_drag {
            Some(d) => d,
            None => {
                tracing::debug!("TAB_DRAG_DIAG: handle_tab_drag_move called but tab_drag is None");
                return;
            }
        };
        let n = self.sessions.len();
        if drag.drag_index >= n {
            // Tab count shrank mid-drag (e.g. Cmd+W raced the gesture).
            // Drop the drag instead of indexing out of bounds.
            self.interaction.tab_drag = None;
            self.request_redraw();
            return;
        }

        // Threshold check — don't engage the drag until the pointer has
        // moved meaningfully. This preserves the click-to-switch (and the
        // deferred × close) behavior for sub-threshold movements.
        if !drag.moved {
            let dx = (x - drag.start_x).abs();
            let dy = (y - drag.start_y).abs();
            if dx.max(dy) <= 4.0 {
                return;
            }
            drag.moved = true;
            // A real drag revokes the deferred close request.
            drag.close_on_release = false;
            // v1.14.0 (PLAN_v1140 T3): drag start snaps the indicator
            // (防拖影 — the underline must not chase the ghost pill).
            self.tab_bar.indicator = None;
            // Capture the grip: pointer X offset within the dragged tab's
            // slot at lift, so the ghost pill never jumps on the first move.
            if let Some(strip) = self.tab_strip_layout() {
                let slot_x = strip.tabs_start + drag.drag_index as f32 * strip.tab_width
                    - strip.scroll_offset;
                drag.grab_offset = (x as f32 - slot_x).clamp(0.0, strip.tab_width);
            }
        }

        // Track the gap slot under the pointer (midpoint rule). The layout
        // is rebuilt from the same geometry the renderer consumes, so the
        // gap and the hit positions can never drift apart.
        let insert = match self.tab_strip_layout() {
            Some(strip) => crate::layout::insertion_index_for_x(&strip, x as f32, n),
            None => drag.insert_index,
        };
        drag.insert_index = insert;
        self.interaction.tab_drag = Some(drag);
        // v1.14.0 (T3): rebuild the 让位 tweens for the new gap slot.
        self.update_tab_slot_shifts();
        self.request_redraw();
    }
}
