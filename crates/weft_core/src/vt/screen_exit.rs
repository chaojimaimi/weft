use super::Terminal;
use std::time::{Duration, Instant};

pub const PRIMARY_SCREEN_EXIT_SETTLE_DELAY: Duration = Duration::from_millis(200);

pub(super) struct PendingPrimaryScreenExit {
    exit_code: Option<i32>,
    last_activity: Instant,
}

impl Terminal {
    pub fn show_block_view(&self) -> bool {
        self.block_tracker.bootstrap_ready()
            && !self.alt_active
            && !self.primary_screen_exit_pending()
            && (!self.primary_screen_app_active() || self.primary_history_view)
    }

    pub fn primary_screen_exit_pending(&self) -> bool {
        self.primary_screen_exit.is_some()
    }

    pub fn primary_history_view(&self) -> bool {
        self.primary_history_view
    }

    pub fn set_primary_history_view(&mut self, active: bool) {
        self.primary_history_view = active;
        if active {
            self.grid.scroll_offset = 0;
        }
    }

    pub(super) fn snapshot_primary_screen_output(&mut self) {
        let Some(start) = self.block_tracker.screen_scrollback_start() else {
            return;
        };
        let (text, styled) = self.grid.document_snapshot_from(start);
        self.block_tracker.replace_screen_snapshot(&text, styled);
    }

    pub(super) fn defer_primary_screen_exit(&mut self, exit_code: Option<i32>) {
        self.block_tracker.defer_screen_command_end();
        self.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code,
            last_activity: Instant::now(),
        });
        tracing::info!(
            ?exit_code,
            settle_delay_ms = PRIMARY_SCREEN_EXIT_SETTLE_DELAY.as_millis(),
            "deferred primary-screen command finalization"
        );
    }

    pub(super) fn note_primary_screen_exit_activity(&mut self) {
        if let Some(pending) = &mut self.primary_screen_exit {
            pending.last_activity = Instant::now();
        }
    }

    pub fn settle_primary_screen_exit_if_idle(&mut self, now: Instant) -> bool {
        let ready = self.primary_screen_exit.as_ref().is_some_and(|pending| {
            now.saturating_duration_since(pending.last_activity) >= PRIMARY_SCREEN_EXIT_SETTLE_DELAY
        });
        ready && self.settle_primary_screen_exit()
    }

    pub fn settle_primary_screen_exit(&mut self) -> bool {
        let Some(pending) = self.primary_screen_exit.take() else {
            return false;
        };
        self.snapshot_primary_screen_output();
        self.block_tracker
            .finish_deferred_screen_command(pending.exit_code);
        tracing::info!(
            exit_code = ?pending.exit_code,
            "settled primary-screen command finalization"
        );
        true
    }
}
