//! Parse-worker drain telemetry (PLAN_v1136 §5.1 lock-probe follow-up).
//!
//! The 2026-10-09 frame-trace round left a 1.35s GUI-vs-headless drain gap
//! with every main-thread suspect eliminated (render duty ~0%, capture free,
//! window size free). This module instruments the WORKER side: per batch it
//! accumulates lock-acquire wait (time from `lock()` call to guard in hand —
//! the window where main-thread work starves parsing) separately from parse
//! busy time, and emits a windowed INFO line when the diagnostic channel is
//! enabled. Gated exactly like `frame_trace` (`WEFT_TRACE_CHANNELS=1`); with
//! the switch off the accumulator is a few unchecked stores per batch.

use std::time::{Duration, Instant};

/// Window length for the emitted averages. Short enough to resolve a ~4s
/// flood into ~8 samples, long enough that the line stays rare.
const EMIT_WINDOW_MS: u64 = 500;

/// Accumulated drain statistics for one worker thread.
#[derive(Default)]
pub(crate) struct ParseWorkerStats {
    window_start: Option<Instant>,
    batches: u64,
    bytes: u64,
    lock_wait_ns: u64,
    parse_ns: u64,
}

impl ParseWorkerStats {
    pub(crate) fn record(
        &mut self,
        lock_wait: Duration,
        parse_busy: Duration,
        bytes: usize,
        now: Instant,
    ) {
        self.window_start.get_or_insert(now);
        self.batches += 1;
        self.bytes += bytes as u64;
        self.lock_wait_ns += lock_wait.as_nanos() as u64;
        self.parse_ns += parse_busy.as_nanos() as u64;
    }

    /// Reset the window once it has run long enough; emit a line only when
    /// the diagnostic channel is on AND the window saw batches. A disabled
    /// channel still resets (windows must not accumulate stale data). The
    /// `window_ms` label is the NOMINAL window — the actual span can exceed
    /// it after an idle gap (the check runs per batch); ratios within the
    /// window (lock_wait vs parse_busy) stay valid regardless.
    pub(crate) fn emit_if_due(&mut self, now: Instant, trace_enabled: bool) -> bool {
        let Some(start) = self.window_start else {
            return false;
        };
        if !emit_due(now.duration_since(start)) {
            return false;
        }
        if trace_enabled && self.batches > 0 {
            tracing::info!(
                batches = self.batches,
                bytes = self.bytes,
                avg_bytes = self.bytes / self.batches,
                lock_wait_ms = self.lock_wait_ns / 1_000_000,
                parse_busy_ms = self.parse_ns / 1_000_000,
                window_ms = EMIT_WINDOW_MS,
                "parse-worker drain window",
            );
        }
        *self = Self::default();
        true
    }
}

/// Pure emit decision: the window is due once it has reached its length.
fn emit_due(window: Duration) -> bool {
    window.as_millis() >= EMIT_WINDOW_MS as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_window_truth_table() {
        assert!(!emit_due(Duration::from_millis(0)));
        assert!(!emit_due(Duration::from_millis(499)));
        assert!(emit_due(Duration::from_millis(500)));
        assert!(emit_due(Duration::from_millis(5_000)));
    }

    #[test]
    fn record_accumulates_and_emit_resets_the_window() {
        let t0 = Instant::now();
        let mut stats = ParseWorkerStats::default();
        stats.record(Duration::from_millis(3), Duration::from_millis(10), 256, t0);
        stats.record(Duration::from_millis(1), Duration::from_millis(20), 512, t0);
        // Not due yet: nothing emitted, values retained.
        assert!(!stats.emit_if_due(t0 + Duration::from_millis(100), true));
        // Due + enabled: emits and resets.
        assert!(stats.emit_if_due(t0 + Duration::from_millis(600), true));
        assert_eq!(stats.batches, 0);
        // Due but disabled: resets silently (no window growth).
        stats.record(
            Duration::from_millis(1),
            Duration::from_millis(1),
            8,
            t0 + Duration::from_millis(700),
        );
        assert!(!stats.emit_if_due(t0 + Duration::from_millis(700), false));
        assert!(stats.emit_if_due(t0 + Duration::from_millis(1_300), false));
        assert_eq!(stats.batches, 0);
    }
}
