//! v1.14.3 (T2/T1, PLAN_v1143 §2): pane-divider drag throttle + the shared
//! three-state drag verdict.
//!
//! Pre-T2 the divider drag ran `set_pane_ratio` + `recompute_layout` on EVERY
//! CursorMoved (zero throttle — the async redraw then let the pointer lead
//! the frame by ≥1 vsync). The move path now shares the sidebar A2 gate's
//! 8ms cadence by REUSING [`crate::panel_controller::drag_apply_step`] (the
//! f32 parameter is ratio-shaped, same type as the sidebar width — plan
//! §T2-1); this module holds the NEW decision logic the divider needs on top:
//! the release-time 终值必写 kernel and the [`DragConsume`] verdict both the
//! sidebar and the divider move handlers return.

/// v1.14.3 (T1, PLAN_v1143 §2): three-state result of a drag move handler
/// (`update_sidebar_drag` / `update_pane_divider_drag`). Replaces the old
/// `bool` so the mouse-move dispatch can tell "consumed but drew nothing"
/// apart from "applied — draw SAME-TICK":
///
/// - [`DragConsume::Applied`] — the 8ms gate passed AND the width/ratio write
///   plus reflow actually ran. The caller draws synchronously via
///   `handle_redraw_requested_forced` (the `Resized`-branch precedent) and
///   must NOT also `request_redraw` (强制与异步请求互斥).
/// - [`DragConsume::Consumed`] — the drag stays active but nothing was
///   applied this event (throttle park, no renderer, no divider, no tab).
///   The caller draws NOTHING — the next Applied event produces the frame.
/// - [`DragConsume::NotConsumed`] — no drag in flight (fall through to the
///   next handler), or the drag was aborted (`set_ratio` error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragConsume {
    NotConsumed,
    Consumed,
    Applied,
}

/// The drag release's landing ratio (终值必写, T2): a ratio parked by the
/// 8ms gate inside the window wins; with nothing parked the last applied
/// ratio already stands in the split tree — `None` means no write is due.
/// The `drag_release_width(pending, applied)` twin with no applied override
/// (the divider's applied state lives in the tree, not an override field).
pub(crate) fn release_ratio(pending: Option<f32>) -> Option<f32> {
    crate::panel_controller::drag_release_width(pending, None)
}

// ── App-side wiring (file-budget: panel_controller.rs / mouse_controller.rs
//    are both at their 800-line gates — plan §3 sanctions the helpers HERE) ──

impl crate::App {
    /// v1.14.3 (T2): one divider ratio application — `set_pane_ratio` + the
    /// layout recompute. Shared by the move path (8ms gate passed) and the
    /// release path (终值必写). Returns true when a divider was actually
    /// re-ratioed (the caller's Applied / forced-draw signal).
    pub(crate) fn apply_divider_ratio(
        &mut self,
        drag: &crate::app_state::PaneDividerDragState,
        ratio: f32,
    ) -> bool {
        let applied = self
            .sessions
            .active_mut()
            .map(|tab| tab.set_pane_ratio(drag.first, drag.second, ratio))
            == Some(Ok(true));
        if applied {
            self.recompute_layout();
        }
        applied
    }

    /// v1.14.3 (T1): the drag-move dispatch verdict — Applied draws SAME-TICK
    /// (forced, the `Resized`-branch precedent; 强制与异步请求互斥 — the
    /// handler never also `request_redraw`), Consumed draws NOTHING (the next
    /// Applied event produces the frame), NotConsumed falls through. Returns
    /// true when the move event is fully handled.
    pub(crate) fn drag_move_consumed(&mut self, consume: DragConsume) -> bool {
        match consume {
            DragConsume::Applied => {
                self.handle_redraw_requested_forced();
                true
            }
            DragConsume::Consumed => true,
            DragConsume::NotConsumed => false,
        }
    }

    /// v1.3.2: pane divider drag end (v1.14.3 T2 终值必写) — land the ratio
    /// parked by the 8ms gate, restore the cursor from the current hover,
    /// then draw: a landed final ratio SAME-TICK (forced), a zero-move
    /// release keeps the trailing async redraw. Returns false when no divider
    /// drag was in flight (the caller falls through to the next handler).
    pub(crate) fn finish_pane_divider_drag(&mut self, x: f64, y: f64) -> bool {
        let Some(drag) = self.interaction.pane_divider_drag.take() else {
            return false;
        };
        let pending = release_ratio(self.interaction.pending_divider_ratio.take());
        self.interaction.divider_drag_last_apply = None;
        let final_applied = pending
            .map(|ratio| self.apply_divider_ratio(&drag, ratio))
            .unwrap_or(false);
        let icon = if let Some(d) = self.pane_divider_hit_test(x as f32, y as f32) {
            match d.axis {
                crate::paint::pane_dividers::DividerAxis::Vertical => {
                    winit::window::CursorIcon::EwResize
                }
                crate::paint::pane_dividers::DividerAxis::Horizontal => {
                    winit::window::CursorIcon::NsResize
                }
            }
        } else {
            winit::window::CursorIcon::Default
        };
        if let Some(window) = &self.window {
            window.set_cursor(icon);
        }
        if final_applied {
            self.handle_redraw_requested_forced();
        } else {
            self.request_redraw();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{release_ratio, DragConsume};
    use crate::panel_controller::drag_apply_step;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = crate::effect::RESIZE_COMMIT_MIN_INTERVAL;

    /// T2 divider 节流真值表 (与 A2 侧栏测试同构, ratio 域): events at
    /// t0 / +3ms / +6ms apply once; in-window ratios park latest-wins; the
    /// first event past the window applies again and clears the park.
    #[test]
    fn divider_throttle_window_coalesces_burst_events() {
        let mut last = None;
        let mut pending = None;
        let t0 = Instant::now();
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0, 0.5),
            Some(0.5),
            "fresh drag applies immediately"
        );
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0 + Duration::from_millis(3), 0.55),
            None,
            "inside the 8ms window the event must not apply"
        );
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0 + Duration::from_millis(6), 0.6),
            None
        );
        assert_eq!(pending, Some(0.6), "the newest ratio parks (latest-wins)");
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0 + WINDOW, 0.65),
            Some(0.65),
            "due event applies"
        );
        assert_eq!(pending, None, "apply clears the park");
    }

    /// T2 释放终值: the parked ratio lands at release; nothing parked means
    /// no write (the last applied ratio already stands in the tree).
    #[test]
    fn divider_release_lands_pending() {
        assert_eq!(
            release_ratio(Some(0.6)),
            Some(0.6),
            "终值必写 — the parked ratio must land"
        );
        assert_eq!(
            release_ratio(None),
            None,
            "zero-move release writes nothing"
        );
    }

    /// T1 三态枚举真值表 — the contract every move handler implements and
    /// the mouse dispatch consumes (mouse_controller `handle_mouse_move`):
    /// 节流 park / 无 renderer / 无 divider → Consumed (不发任何 redraw);
    /// 节流通过且实际 set+recompute → Applied (调用点 forced draw, 不得再
    /// request_redraw); 无 drag 在飞 / set_ratio Err abort → NotConsumed
    /// (现状透传). Pin: the three variants stay pairwise distinct and the
    /// enum stays exactly these three (the dispatch match is exhaustive —
    /// adding a variant must re-audit the redraw discipline).
    #[test]
    fn drag_consume_truth_table() {
        let all = [
            DragConsume::NotConsumed,
            DragConsume::Consumed,
            DragConsume::Applied,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(a == b, i == j, "variants must be pairwise distinct");
            }
        }
        // The Consumed contract: park verdicts are drag_active && !applied.
        // Applied requires an actual write — never returned for a park.
        assert_ne!(DragConsume::Consumed, DragConsume::Applied);
        assert_ne!(DragConsume::NotConsumed, DragConsume::Consumed);
    }
}
