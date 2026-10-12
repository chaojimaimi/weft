//! Live styled publish family (FIX_LIVE_STYLED_OUTPUT), moved verbatim
//! out of `blocks.rs` (v1.13.9 backlog clear zero-behavior file-budget
//! split; `impl BlockTracker` cross-file block per the continuation.rs
//! precedent).

use std::sync::Arc;
use std::time::Instant;

use super::{BlockTracker, LIVE_STYLED_PUBLISH_MIN_INTERVAL};

impl BlockTracker {
    // ── Live styled publish (FIX_LIVE_STYLED_OUTPUT) ─────────────────────

    /// Whether enough time has elapsed since the last live styled publish to
    /// rebuild the snapshot. Pure — unit-anchored in live_styled_tests.rs.
    pub(super) fn live_styled_publish_due(last: Option<Instant>, now: Instant) -> bool {
        !last.is_some_and(|t| now.duration_since(t) < LIVE_STYLED_PUBLISH_MIN_INTERVAL)
    }

    /// Publish a styled snapshot of the in-flight output so the live block
    /// renders program-emitted SGR attributes before finalize (Warp-parity:
    /// colors visible while streaming, not only after 133;D). Three states:
    /// runs present → fresh snapshot; run-cap overflow → freeze the LAST good
    /// snapshot instead of flickering to unstyled; runs cleared by an
    /// all-default rewrite (`\r` + reprint) → clear, so the live view drops
    /// stale colors immediately (review m1). Writes only
    /// `live_styled_snapshot` — never `styled_output` (review M1).
    fn publish_live_styled(&mut self) {
        match self.output.peek_styled() {
            Some(styled) => self.live_styled_snapshot = Some(Arc::new(styled)),
            None if self.output.style_overflow() => {}
            None => self.live_styled_snapshot = None,
        }
    }

    pub(super) fn maybe_publish_live_styled(&mut self, now: Instant) {
        if Self::live_styled_publish_due(self.last_live_styled_publish, now) {
            self.last_live_styled_publish = Some(now);
            self.publish_live_styled();
        }
    }

    /// Boundary publish for rewrite events (`\r`, EL, backspace, newline).
    /// Review round 2 MAJOR: an UNTHROTTLED rebuild here re-introduced the
    /// O(text) cost on every newline — newlines are the highest-frequency
    /// event in streamed output, so a 1 MiB colored stream paid ~6.7s of
    /// main-thread rebuilds. The m1 guarantee only needs immediacy when the
    /// snapshot's CATEGORY flips (first colors appear / all colors cleared),
    /// and that flip is O(1)-detectable from the run counters; Some→Some
    /// refreshes stay on the 100ms throttle like the print paths.
    pub(super) fn boundary_publish_live_styled(&mut self) {
        let peek_some = !(self.output.style_overflow() || self.output.style_runs_empty());
        // F3: batch clock (synced per process call) instead of per-line now.
        let now = self.batch_clock;
        if peek_some != self.live_styled_snapshot.is_some()
            || Self::live_styled_publish_due(self.last_live_styled_publish, now)
        {
            self.last_live_styled_publish = Some(now);
            self.publish_live_styled();
        }
    }
}
