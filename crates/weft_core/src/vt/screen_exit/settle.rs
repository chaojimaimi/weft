//! Primary-screen exit settlement — the pending-exit request, activity
//! tracking and the 200ms idle settle window.

use super::Terminal;
use std::time::{Duration, Instant};

pub const PRIMARY_SCREEN_EXIT_SETTLE_DELAY: Duration = Duration::from_millis(200);

pub(in crate::vt) struct PendingPrimaryScreenExit {
    pub(in crate::vt) exit_code: Option<i32>,
    pub(in crate::vt) last_activity: Instant,
}

impl Terminal {
    pub(in crate::vt) fn defer_primary_screen_exit(&mut self, exit_code: Option<i32>) {
        self.block_tracker.defer_screen_command_end();
        self.capabilities.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code,
            last_activity: Instant::now(),
        });
        // No block_id here: the deferred command's BlockId is not allocated
        // until `settle_primary_screen_exit` → `finish_deferred_screen_command`
        // runs. Logging the previous block's id would mislead log analysis.
        tracing::info!(
            ?exit_code,
            settle_delay_ms = PRIMARY_SCREEN_EXIT_SETTLE_DELAY.as_millis(),
            // v1.11.8 (M-B): the tier + exemption trio that drove
            // `show_block_view` for this session — captured at the defer
            // point while the command state is still live.
            mode = ?self.tui_render_mode,
            exempt = self.capabilities.interactive_stdin_seen
                || self.capabilities.mouse_protocol != crate::input::MouseProtocol::Off,
            mouse = ?self.capabilities.mouse_protocol,
            "deferred primary-screen command finalization"
        );
    }

    pub(in crate::vt) fn note_primary_screen_exit_activity(&mut self) {
        if let Some(pending) = &mut self.capabilities.primary_screen_exit {
            pending.last_activity = Instant::now();
        }
    }

    /// A late primary-screen exit tail commonly rewrites rows from column 0
    /// without first issuing EL. Clear the old row before that first scalar so
    /// shorter status/resume lines cannot retain stale suffix cells.
    pub(in crate::vt) fn prepare_primary_screen_exit_row_overwrite(&mut self) {
        if self.capabilities.primary_screen_exit.is_some()
            && !self.capabilities.alt_active
            && self.grid.cursor.col == 0
        {
            let row = self.grid.cursor.row;
            self.grid.clear_line_all();
            self.hyperlinks.unlink_row(row);
        }
    }

    pub fn settle_primary_screen_exit_if_idle(&mut self, now: Instant) -> bool {
        let ready = self
            .capabilities
            .primary_screen_exit
            .as_ref()
            .is_some_and(|pending| {
                now.saturating_duration_since(pending.last_activity)
                    >= PRIMARY_SCREEN_EXIT_SETTLE_DELAY
            });
        ready && self.settle_primary_screen_exit()
    }

    pub fn settle_primary_screen_exit(&mut self) -> bool {
        let Some(pending) = self.capabilities.primary_screen_exit.take() else {
            return false;
        };
        // v1.11.8 (M-B): capture the tier + exemption trio BEFORE the
        // boundary hygiene below wipes interactive-stdin/mouse state — the
        // settle log must describe the session that just ended, not the
        // post-reset defaults.
        let ended_mode = self.tui_render_mode;
        let ended_exempt = self.capabilities.interactive_stdin_seen
            || self.capabilities.mouse_protocol != crate::input::MouseProtocol::Off;
        let ended_mouse = self.capabilities.mouse_protocol;
        self.snapshot_primary_screen_output();
        self.block_tracker
            .finish_deferred_screen_command(pending.exit_code);
        // v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.1, P1-2): a real command
        // boundary — clear the interactive-stdin exemption so the NEXT command
        // starts untainted (classic fallback cannot leak across commands).
        self.capabilities.interactive_stdin_seen = false;
        // v1.11.7 (P0-2): `composed_cursor_snapshot_line` used to return None
        // implicitly at settle (its `in_flight()?` died on the AtPrompt
        // phase), which doubled as the caret-anchor reset. The widened
        // `in_flight()` gate (settling) keeps the live block alive through
        // the settle window, so the reset must be explicit now: the anchor
        // belongs to the finalized session and must not leak into the next
        // command (b_path regression: block_view_tui_cursor would anchor the
        // caret past the new live block). Same boundary hygiene for the
        // keystroke-skip cache (`last_caret_snapshot_cursor`).
        self.capabilities.primary_screen_cursor_snapshot_line = None;
        self.capabilities.primary_screen_cursor_segment_len = None;
        self.capabilities.last_caret_snapshot_cursor = None;
        // v1.10.7: the render-mode lock belongs to the command being
        // finalized — release it here (covers the idle-timer settle AND the
        // 133;B settle; nested-marker paths never settle, so the lock
        // survives them). The next command re-detects and re-locks at its
        // first screen ownership.
        self.capabilities.primary_screen_interrupt_capture = None;
        // A killed TUI is not guaranteed to emit DEC mouse-mode resets. Do
        // not let stale reporting state turn later shell clicks into literal
        // SGR mouse coordinates such as `48;62;25M`.
        self.capabilities.mouse_protocol = crate::input::MouseProtocol::Off;
        self.capabilities.sgr_mouse = false;
        let block_id = self
            .block_tracker
            .blocks()
            .last()
            .map(|b| b.id.0)
            .unwrap_or(0);
        tracing::info!(
            ?pending.exit_code,
            block_id,
            mode = ?ended_mode,
            exempt = ended_exempt,
            mouse = ?ended_mouse,
            "settled primary-screen command finalization"
        );
        true
    }

    /// v1.10.26 Batch D (D-3): consume the head count of the most recent 1MiB
    /// history split, if any. The app reads this after settling/draining a
    /// frame (or closing a tab) and advances a detached `block_scroll_anchor`
    /// by `heads × BLOCK_SPLIT_HEAD_CHROME_ROWS` so the user's viewport stays
    /// put when the split blocks' chrome rows are inserted (see
    /// `Tab::compensate_anchor_for_split` in weft_app).
    pub fn take_pending_screen_split_heads(&mut self) -> Option<usize> {
        self.capabilities.pending_screen_split_heads.take()
    }
}
