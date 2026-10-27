//! Sidebar width-animation family, moved verbatim out of
//! `panel_controller.rs` (v1.15.2 governance cart, zero-rewrite).
//! Child module of the controller (mouse_controller/ precedent).

use super::*;

impl App {
    /// v1.14.0 (PLAN_v1140 T2): true when the sidebar width animation may
    /// run — expanded (non-Compact) windows only. Compact overlay drawers
    /// ignore `sidebar_width_override` entirely (ui_tokens::sidebar_visual_
    /// width returns the fixed metrics width), so their toggle stays instant.
    fn sidebar_anim_allowed(&self) -> bool {
        crate::app::motion::gated(self.window_runtime.reduce_motion)
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| r.sidebar_push_width() != 0.0)
            // rust-reviewer P3: a drag owns the width channel — starting a
            // tween under it would give the per-frame override write and the
            // drag's set_sidebar_width a double-writer race.
            && self.interaction.sidebar_drag.is_none()
    }

    /// v1.14.0 (T2): start the sidebar OPEN width animation (0→target,
    /// 180ms ease-out). The pre-animation override state is captured and
    /// restored when the tween lands (`sidebar_width_restore`) so a
    /// responsive-default user (`None`) is never frozen into a fixed width.
    /// Called with `panel.open` already true; Reduce Motion / Compact callers
    /// keep the instant path (no tween, override untouched).
    pub(crate) fn animate_sidebar_open(&mut self) {
        if !self.sidebar_anim_allowed() {
            return;
        }
        if self.panel.sidebar_width_anim.is_some() && self.panel.sidebar_width_opening {
            return; // already opening/open — the tween keeps driving
        }
        let now = std::time::Instant::now();
        // Start value: a mid-close reverse resumes from the animated width;
        // a closed sidebar sweeps from 0.
        let current = match self.panel.sidebar_width_anim.as_ref() {
            Some(anim) => anim.value_at(now),
            None => 0.0,
        };
        let Some(r) = &self.renderer else {
            return;
        };
        // Target = the width the sidebar settles at AFTER the animation —
        // the pre-animation override's effective logical width (None
        // resolves to the responsive default). While a close tween runs the
        // capture already holds the original value (keep it); otherwise it
        // is the live override, read before the first frame clobbers it.
        let restore = self
            .panel
            .sidebar_width_restore
            .unwrap_or(r.sidebar_width_override);
        let logical_viewport = r.viewport().0 / r.scale() as f32;
        let target = crate::ui_tokens::sidebar_visual_width(logical_viewport, restore);
        self.panel.sidebar_width_restore = Some(restore);
        self.panel.sidebar_width_opening = true;
        // v1.14.1 (F3): a fresh tween applies its first frame immediately.
        self.panel.sidebar_width_anim_last_apply = None;
        self.panel.sidebar_width_anim = Some(crate::app::motion::Tween::new(
            current,
            target,
            crate::app::motion::SIDEBAR_OPEN_DUR,
            now,
        ));
    }

    /// v1.14.0 (T2): start the sidebar CLOSE width animation (current→0,
    /// 150ms ease-out). `panel.open` stays true while the sidebar shrinks;
    /// the real close runs when the tween lands (先动画后状态 — plan §3).
    /// Reduce Motion / Compact callers keep the instant path.
    pub(crate) fn animate_sidebar_close(&mut self) {
        if !self.sidebar_anim_allowed() {
            self.panel.close();
            return;
        }
        if self.panel.sidebar_width_anim.is_some() && !self.panel.sidebar_width_opening {
            return; // already closing — let the tween land
        }
        let Some(r) = &self.renderer else {
            self.panel.close();
            return;
        };
        let current = r.sidebar_width() / r.scale() as f32;
        // rust-reviewer P1-3: the close must capture the restore SYMMETRICALLY
        // with the open. Without this, the per-frame tween writes clobber
        // `sidebar_width_override` and the land step restores… the captured
        // value from a possibly-ancient open — a drag-persisted 300pt width
        // was lost forever on close, and a mid-close reopen re-opened the
        // animation channel with the OVERRIDDEN intermediate value baked in.
        let restore = self
            .panel
            .sidebar_width_restore
            .take()
            .unwrap_or(r.sidebar_width_override);
        self.panel.sidebar_width_restore = Some(restore);
        self.panel.sidebar_width_opening = false;
        // v1.14.1 (F3): a fresh tween applies its first frame immediately.
        self.panel.sidebar_width_anim_last_apply = None;
        self.panel.sidebar_width_anim = Some(crate::app::motion::Tween::new(
            current,
            0.0,
            crate::app::motion::SIDEBAR_CLOSE_DUR,
            std::time::Instant::now(),
        ));
    }

    /// v1.14.0 (T2): per-frame sidebar width animation maintenance, run at
    /// the top of `run_redraw` (before every `sidebar_width()` consumer).
    /// In-flight frames write the tween value STRAIGHT into
    /// `renderer.sidebar_width_override` (bypassing `set_sidebar_width`'s
    /// `[240, 360]` clamp and `sidebar_visual_width`'s second clamp — the
    /// plan's 直写通道) and recompute layout so the terminal reflows each
    /// frame (per-frame SIGWINCH; the zoom 220ms per-frame resize is the
    /// accepted precedent). Landing restores the pre-animation override and
    /// performs the real close for a closing animation.
    pub(crate) fn maintain_sidebar_width_anim(&mut self) {
        let now = std::time::Instant::now();
        let (value, finished) = match self.panel.sidebar_width_anim.as_ref() {
            Some(anim) => (anim.value_at(now), anim.finished(now)),
            None => return,
        };
        // v1.14.1 (F3): 8ms apply-throttle (PLAN_v1141 §3.2.2) — the
        // override write + recompute_layout run at most once per
        // RESIZE_COMMIT_MIN_INTERVAL (the drag path's PTY-commit cadence;
        // the tween value is still computed every frame above). A finished
        // tween always applies so the landing below is never stalled
        // (终值必写).
        if !crate::app::motion::should_apply(
            now,
            self.panel.sidebar_width_anim_last_apply,
            finished,
        ) {
            return;
        }
        self.panel.sidebar_width_anim_last_apply = Some(now);
        if !finished {
            if let Some(r) = &mut self.renderer {
                r.sidebar_width_override = Some(value);
            }
            self.recompute_layout();
            return;
        }
        // Land: restore the pre-animation override state, then finish.
        let opening = self.panel.sidebar_width_opening;
        let restore = self.panel.sidebar_width_restore.take().unwrap_or(None);
        self.panel.sidebar_width_anim = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = restore;
        }
        if !opening {
            self.panel.close();
        }
        self.recompute_layout();
        self.request_redraw();
    }

    /// v1.14.0 (T2): cancel an in-flight sidebar width animation, freezing
    /// the current animated value into the override (拖拽起始取消动画取当前
    /// 值 — plan §3). The drag channel owns the width from here on; the
    /// restore capture is dropped (the drag release persists its own value).
    pub(crate) fn cancel_sidebar_width_anim(&mut self) {
        let Some(anim) = self.panel.sidebar_width_anim.take() else {
            return;
        };
        let current = anim.value_at(std::time::Instant::now());
        self.panel.sidebar_width_restore = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = Some(current);
        }
    }

    /// v1.14.0 (T2): Reduce Motion flip — snap an in-flight sidebar width
    /// animation to its end value immediately (lifecycle 1Hz bridge).
    pub(crate) fn snap_sidebar_width_anim(&mut self) {
        if self.panel.sidebar_width_anim.is_none() {
            return;
        }
        let opening = self.panel.sidebar_width_opening;
        let restore = self.panel.sidebar_width_restore.take().unwrap_or(None);
        self.panel.sidebar_width_anim = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = restore;
        }
        if !opening {
            self.panel.close();
        }
        self.recompute_layout();
    }

    pub(crate) fn update_sidebar_drag(
        &mut self,
        pointer_x: f64,
    ) -> crate::app::divider_drag::DragConsume {
        let Some(drag) = self.interaction.sidebar_drag else {
            return crate::app::divider_drag::DragConsume::NotConsumed;
        };
        let Some(renderer) = &mut self.renderer else {
            return crate::app::divider_drag::DragConsume::Consumed;
        };
        let width = crate::ui_tokens::sidebar_width_after_drag(
            drag.start_width,
            drag.start_x,
            pointer_x,
            renderer.scale() as f32,
        );
        // v1.14.2 (A2/PLAN §2): the drag path previously ran
        // set_sidebar_width + recompute_layout on EVERY CursorMoved (zero
        // throttle) — the defect-two firehose. It now shares the F3 tween
        // gate's 8ms cadence (`app::motion::should_apply` =
        // RESIZE_COMMIT_MIN_INTERVAL): an in-window event only parks its
        // width in `pending_drag_width`; a due event applies the current
        // width and clears the park. `handle_mouse_release` writes the
        // parked final width (终值必写).
        let due = drag_apply_step(
            &mut self.panel.sidebar_drag_last_apply,
            &mut self.panel.pending_drag_width,
            std::time::Instant::now(),
            width,
        );
        if due.is_none() {
            // v1.14.3 (T1): throttled park — consumed, but the caller must
            // NOT draw (the next Applied event produces the frame).
            return crate::app::divider_drag::DragConsume::Consumed;
        }
        renderer.set_sidebar_width(Some(width));
        self.recompute_layout();
        // v1.14.3 (T1): Applied — the caller draws same-tick via
        // `handle_redraw_requested_forced`; the trailing `request_redraw`
        // is GONE (强制与异步请求互斥).
        crate::app::divider_drag::DragConsume::Applied
    }

    /// v1.3.2: Update the pane divider ratio from the pointer position during
    /// a drag. Computes `new_ratio` from the drag state's `bounds` + the
    /// current pointer, applies it through the v1.14.3 (T2) 8ms gate (the
    /// sidebar A2 twin — `drag_apply_step`, ratio-shaped f32), then
    /// `recompute_layout` (which handles the throttled PTY resize). No-op if
    /// no drag is active. (The release-side helpers — `apply_divider_ratio` /
    /// `finish_pane_divider_drag` — live in `app::divider_drag`: this file is
    /// at its line budget.)
    pub(crate) fn update_pane_divider_drag(
        &mut self,
        x: f32,
        y: f32,
    ) -> crate::app::divider_drag::DragConsume {
        let Some(drag) = self.interaction.pane_divider_drag else {
            return crate::app::divider_drag::DragConsume::NotConsumed;
        };
        let [bx0, _by0, bx1, _by1] = drag.bounds;
        let new_ratio = match drag.axis {
            crate::paint::pane_dividers::DividerAxis::Vertical => {
                ((x - bx0) / (bx1 - bx0).max(1.0)).clamp(0.0, 1.0)
            }
            crate::paint::pane_dividers::DividerAxis::Horizontal => {
                let [_bx0, by0, _bx1, by1] = drag.bounds;
                ((y - by0) / (by1 - by0).max(1.0)).clamp(0.0, 1.0)
            }
        };
        // v1.14.3 (T2): the divider previously re-ratioed + reflowed on EVERY
        // CursorMoved (zero throttle — the pointer-lead root cause). An
        // in-window event parks its ratio in `pending_divider_ratio`; a due
        // event applies and clears the park. The release writes the parked
        // final ratio (`handle_mouse_release`, 终值必写).
        let due = drag_apply_step(
            &mut self.interaction.divider_drag_last_apply,
            &mut self.interaction.pending_divider_ratio,
            std::time::Instant::now(),
            new_ratio,
        );
        let Some(ratio) = due else {
            return crate::app::divider_drag::DragConsume::Consumed;
        };
        match self
            .sessions
            .active_mut()
            .map(|tab| tab.set_pane_ratio(drag.first, drag.second, ratio))
        {
            Some(Ok(true)) => {
                self.recompute_layout();
                // v1.14.3 (T1): Applied — same-tick forced draw at the
                // caller; the trailing `request_redraw` is gone (互斥).
                crate::app::divider_drag::DragConsume::Applied
            }
            // v1.12.25 (audit 3-B, P1-01): empty-tabs transient / no divider
            // / no tab — nothing to re-ratio; keep consuming the drag.
            Some(Ok(false)) | None => crate::app::divider_drag::DragConsume::Consumed,
            Some(Err(e)) => {
                tracing::warn!(error = ?e, "pane divider drag: set_ratio failed; aborting drag");
                self.interaction.pane_divider_drag = None;
                // v1.14.3 (T2): the aborted drag's throttle state dies with
                // it — a stale park must never land at a future release.
                self.interaction.divider_drag_last_apply = None;
                self.interaction.pending_divider_ratio = None;
                // Release the drag so subsequent moves don't re-log.
                crate::app::divider_drag::DragConsume::NotConsumed
            }
        }
    }

    pub(crate) fn finish_panel_scrollbar_drag(&mut self) -> bool {
        if self.interaction.panel_scrollbar_drag.take().is_none() {
            return false;
        }
        self.request_redraw();
        true
    }

    pub(crate) fn update_panel_scrollbar_drag(&mut self, pointer_y: f32) -> bool {
        let Some(drag) = self.interaction.panel_scrollbar_drag else {
            return false;
        };
        self.panel.scroll_offset = crate::panel_scrollbar::scroll_offset_for_pointer(
            &drag.layout,
            pointer_y,
            drag.grab_offset,
        );
        self.clamp_panel_scroll();
        self.clamp_panel_selection();
        self.request_redraw();
        true
    }
}
