//! Rate-limited primary-screen history snapshots with a guaranteed tail flush.

use std::time::{Duration, Instant};

use super::Tab;

#[derive(Default)]
pub(crate) struct PrimaryHistoryRefresh {
    due: Option<Instant>,
    wake_scheduled: bool,
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
        if !terminal.primary_history_view() || !terminal.primary_screen_app_active() {
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
    use weft_core::vt::Terminal;

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
    fn quiet_tail_refresh_survives_leave_and_reenter_before_old_deadline() {
        let mut tab = primary_tui_tab();
        tab.scroll_up_by(1);
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2J\x1b[Hfirst".to_vec()))
            .unwrap();
        tab.process_messages();
        assert!(tab.take_primary_history_refresh_wake_delay().is_some());

        // Leave and start a new history-view generation before the old wake.
        // The new generation must be able to claim its own deadline.
        tab.snap_to_bottom();
        tab.scroll_up_by(1);
        tab.msg_tx
            .send(AppMsg::PtyOutput(b"\x1b[2J\x1b[Hfinal".to_vec()))
            .unwrap();
        tab.process_messages();
        let delay = tab
            .take_primary_history_refresh_wake_delay()
            .expect("re-entered history must replace the old wake generation");
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
            "final"
        );
    }

    #[test]
    fn scrolling_to_history_tail_keeps_detached_view_until_explicit_snap() {
        let mut tab = primary_tui_tab();
        tab.scroll_up_by(1);
        assert!(tab.terminal.as_ref().unwrap().primary_history_view());

        tab.scroll_down_by(1);
        assert_eq!(tab.block_scroll(), 0);
        assert!(
            tab.terminal.as_ref().unwrap().primary_history_view(),
            "offset zero must not switch rendering models during wheel scroll"
        );

        tab.snap_to_bottom();
        assert!(!tab.terminal.as_ref().unwrap().primary_history_view());
    }
}
