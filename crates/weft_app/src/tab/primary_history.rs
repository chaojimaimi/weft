//! Rate-limited primary-screen history snapshots with a guaranteed tail flush.

use std::time::{Duration, Instant};

use super::Tab;

#[derive(Default)]
pub(crate) struct PrimaryHistoryRefresh {
    due: Option<Instant>,
    wake_scheduled: bool,
    /// v1.10.4: a keystroke arrived while browsing a primary-screen TUI's
    /// history; snapshot refreshes bypass the rate limit until this deadline.
    /// openclaw's keypress redraw arrives in TWO pty batches (a cursor-move
    /// header, then the repainted content) — a one-shot bypass would be
    /// consumed by the header before the content lands, so the content
    /// waits out the 50ms rate limit and the selection change lags a blink
    /// (read as a flicker). A short window covers every batch.
    force_until: Option<Instant>,
}

/// v1.10.4: how long after a keypress snapshot refreshes stay unthrottled.
/// Covers split pty batches of one redraw (header + content).
const FORCE_WINDOW: Duration = Duration::from_millis(100);

impl PrimaryHistoryRefresh {
    /// Open the rate-limit bypass window (v1.10.4, keypress-driven redraws).
    pub(crate) fn arm_force(&mut self) {
        self.force_until = Some(Instant::now() + FORCE_WINDOW);
    }

    /// Whether the bypass window is open. NOT consumed — every refresh inside
    /// the window is forced; the window expires on its own.
    pub(crate) fn take_force(&mut self) -> bool {
        let armed = self.force_until.is_some_and(|until| Instant::now() < until);
        if !armed {
            self.force_until = None;
        }
        armed
    }
}

impl Tab {
    /// Refresh the detached primary-screen document after PTY output. When the
    /// core's 50ms limiter rejects an early update, retain one deadline so a
    /// quiet TUI still publishes its final frame after output stops.
    pub(super) fn refresh_primary_history_snapshot(&mut self, output_arrived: bool) -> bool {
        let now = Instant::now();
        let deadline_reached = self
            .primary_history_refresh
            .due
            .is_some_and(|due| now >= due);
        if !output_arrived && !deadline_reached {
            return false;
        }

        let Some(terminal) = self.terminal.as_mut() else {
            self.primary_history_refresh = PrimaryHistoryRefresh::default();
            return false;
        };
        // v1.10.4 (round 4): snapshot refreshes are driven by screen
        // ownership, not just history browsing — a relative-only TUI
        // (openclaw) kept in the BlockView has suspended print capture once
        // screen-owned, so its live block updates ONLY through this
        // snapshot, whether the user follows the live tail (history browsing
        // off) or is detached in history.
        // v1.10.7: the previous `!show_block_view()` gate (v1.10.4
        // MEDIUM-1) is REMOVED. A sparse repainter like pi performs
        // occasional CUP full-viewport repaints, which flips
        // `show_block_view()` false while following — the gate then froze
        // the snapshot and the session block kept stale content: after the
        // task, browsing/exit/resume showed only the last pre-CUP frame
        // (block 1698 in the wild had just the banner of a resumed
        // session; the replay body never made it into the block). The
        // snapshot is the ONLY content source for screen-owned blocks, so
        // it must refresh regardless of the transient render mode. Cost:
        // a 50ms-rate-limited document rescan while a CUP TUI follows —
        // acceptable vs. losing the transcript.
        // v1.10.7: freeze while the user is DETACHED browsing
        // (`primary_history_view`). The block scroll is bottom-relative —
        // every refresh that grows the live block shifts the user's view
        // toward older content mid-read ("executing prompt output pushes
        // history away / cannot read beyond the window"). The keystroke
        // force path (`refresh_primary_history_snapshot_now`) is NOT gated
        // here, so a TUI redraw still publishes while the user types.
        if !terminal.primary_screen_app_active() || terminal.primary_history_view() {
            self.primary_history_refresh = PrimaryHistoryRefresh::default();
            return false;
        }
        if terminal.refresh_primary_history_snapshot() {
            self.primary_history_refresh = PrimaryHistoryRefresh::default();
            return true;
        }

        if deadline_reached {
            // A view generation or timer boundary can make the app deadline
            // precede the core's eligibility by a fraction. Never strand a
            // past-due state whose already-claimed wake cannot be scheduled
            // again: roll it forward and release the wake claim.
            self.primary_history_refresh.due =
                Some(now + weft_core::vt::PRIMARY_HISTORY_SNAPSHOT_INTERVAL);
            self.primary_history_refresh.wake_scheduled = false;
        } else {
            self.primary_history_refresh
                .due
                .get_or_insert(now + weft_core::vt::PRIMARY_HISTORY_SNAPSHOT_INTERVAL);
        }
        false
    }

    pub(super) fn reset_primary_history_refresh(&mut self) {
        self.primary_history_refresh = PrimaryHistoryRefresh::default();
    }

    /// Claim the single runtime wake that will flush a rate-limited snapshot.
    pub(crate) fn take_primary_history_refresh_wake_delay(&mut self) -> Option<Duration> {
        let due = self.primary_history_refresh.due?;
        if self.primary_history_refresh.wake_scheduled {
            return None;
        }
        self.primary_history_refresh.wake_scheduled = true;
        Some(due.saturating_duration_since(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::Pane;
    use crate::AppMsg;

    fn primary_tui_tab() -> Tab {
        // v1.3: build a pane with a live Terminal (no PTY), wrap in a single-pane
        // tab, then drive the OSC 133 prompt/command sequence so the block tracker
        // enters CommandExecuting — the precondition for primary-history snapshots.
        let mut pane = Pane::with_terminal_only(100);
        pane.terminal
            .as_mut()
            .unwrap()
            .process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b[6G\x1b[13G");
        Tab::with_single_pane(pane)
    }

    #[test]
    fn keypress_force_window_covers_split_redraw_batches() {
        // v1.10.4: a keystroke opens a bypass WINDOW (not one-shot) so every
        // pty batch of the redraw gets an unthrottled snapshot.
        let mut refresh = PrimaryHistoryRefresh::default();
        assert!(!refresh.take_force(), "unarmed window is closed");
        refresh.arm_force();
        assert!(refresh.take_force(), "batch 1 forced");
        assert!(refresh.take_force(), "batch 2 forced");
        refresh.force_until = Some(Instant::now() - Duration::from_millis(1));
        assert!(!refresh.take_force(), "window must expire");
        assert!(refresh.force_until.is_none(), "expired window cleared");
    }

    #[test]
    fn keypress_during_execution_arms_force_without_history_view() {
        // v1.10.4 (round 4): EVERY keystroke during execution arms the
        // snapshot bypass window, even while following the live tail
        // (history browsing off). A screen-owned TUI (openclaw) has stopped
        // print capture, so its live block needs the unthrottled snapshot;
        // the previous `primary_history_view`-gated arm missed exactly the
        // "scrolled back to the tail, then keypress" flicker case.
        let mut tab = primary_tui_tab();
        assert!(
            !tab.terminal.as_ref().unwrap().primary_history_view(),
            "precondition: following the live tail"
        );
        // No PTY in this test pane — `write_user_input` returns Err AFTER
        // the executing/arm branch, so the arm side-effect is observable.
        // NOTE (round 4 review LOW-2): this asserts on the function's
        // execution ORDER (arm before the pty-not-connected error); a future
        // reorder of `write_user_input` will silently break this test.
        assert!(tab.write_user_input(b"j").is_err());
        assert!(
            tab.primary_history_refresh.take_force(),
            "executing keystroke must arm the bypass window"
        );
    }

    /// v1.10.7: detached browsing freezes the live snapshot so fresh output
    /// cannot push the user's view toward older content mid-read. The wake
    /// delay is not claimed while browsing; returning to the tail (follow)
    /// resumes the refresh cycle.
    #[test]
    fn detached_browsing_freezes_snapshot_until_tail() {
        let mut tab = primary_tui_tab();
        tab.scroll_up_by(1);
        assert!(tab.terminal.as_ref().unwrap().primary_history_view());

        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gfresh".to_vec()))
            .unwrap();
        tab.process_messages();
        assert_eq!(
            tab.terminal
                .as_ref()
                .unwrap()
                .block_tracker()
                .in_flight()
                .unwrap()
                .output,
            "",
            "browsing must freeze the live snapshot (no refresh scheduled)"
        );
        assert!(
            tab.take_primary_history_refresh_wake_delay().is_none(),
            "browsing must not schedule a refresh wake"
        );

        // Back to the tail (FollowBottom) → the refresh cycle resumes. The
        // first refresh after `set_primary_history_view(false)` clears the
        // rate-limit stamp, so it succeeds immediately; the SECOND output
        // (within the 50ms window) schedules the wake.
        tab.snap_to_bottom();
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gtail".to_vec()))
            .unwrap();
        tab.process_messages();
        assert_eq!(
            tab.terminal
                .as_ref()
                .unwrap()
                .block_tracker()
                .in_flight()
                .unwrap()
                .output,
            "tail",
            "following the tail must refresh the live snapshot again"
        );
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gtail2".to_vec()))
            .unwrap();
        tab.process_messages();
        assert!(
            tab.take_primary_history_refresh_wake_delay().is_some(),
            "following the tail must resume snapshot refresh scheduling"
        );
    }

    #[test]
    fn quiet_tail_refresh_survives_leave_and_reenter_before_old_deadline() {
        let mut tab = primary_tui_tab();
        // First output: no rate-limit stamp yet → refresh succeeds directly.
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gfirst".to_vec()))
            .unwrap();
        tab.process_messages();
        // Second output inside the 50ms window → rate-limited → deadline set.
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gsecond".to_vec()))
            .unwrap();
        tab.process_messages();
        assert!(tab.take_primary_history_refresh_wake_delay().is_some());

        // Leave (browse) and come back before the old wake: browsing freezes
        // the refresh state; returning to the tail must start a fresh
        // generation that can claim its own deadline.
        tab.scroll_up_by(1);
        tab.snap_to_bottom();
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gfinal".to_vec()))
            .unwrap();
        tab.process_messages();
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2K\x1b[1Gfinal2".to_vec()))
            .unwrap();
        tab.process_messages();
        let delay = tab
            .take_primary_history_refresh_wake_delay()
            .expect("follow after browsing must schedule its own refresh wake");
        std::thread::sleep(delay + Duration::from_millis(5));

        let (_, _, need_redraw) = tab.process_messages();
        assert!(need_redraw);
        assert_eq!(
            tab.terminal
                .as_ref()
                .unwrap()
                .block_tracker()
                .in_flight()
                .unwrap()
                .output,
            "final2"
        );
    }

    #[test]
    fn scrolling_to_history_tail_returns_to_live_grid() {
        // v1.10.6: scrolling back to the tail (FollowBottom) exits history
        // browsing so the TUI returns to the live grid (native cursor/IME/
        // color). Previously `primary_history_view` stayed true forever once
        // the user scrolled, trapping the TUI in the BlockView where IME is
        // a fallback (no preedit, imprecise caret).
        let mut tab = primary_tui_tab();
        tab.scroll_up_by(1);
        assert!(tab.terminal.as_ref().unwrap().primary_history_view());

        tab.scroll_down_by(1);
        assert_eq!(tab.block_scroll(), 0);
        assert!(
            !tab.terminal.as_ref().unwrap().primary_history_view(),
            "scrolling to the tail must exit history browsing (return to live grid)"
        );

        tab.scroll_up_by(2);
        assert!(tab.terminal.as_ref().unwrap().primary_history_view());
        tab.snap_to_bottom();
        assert!(!tab.terminal.as_ref().unwrap().primary_history_view());
    }
}
