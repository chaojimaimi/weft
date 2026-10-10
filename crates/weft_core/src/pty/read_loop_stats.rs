//! Reader-side drain telemetry (PLAN_v1138 pre-work, the GUI-tax locator).
//!
//! The 1.13.7 GUI seq gap decomposition left a ~1.1s co-residency tax on the
//! reader/worker side (kernel floor ≈3.0s measured by a native minimal
//! reader at ~14B/read line-by-line delivery; weft headless pipeline sits on
//! the floor; GUI wall 4.11s). This module instruments the reader loop:
//! per-window read counts, byte totals, WouldBlock rate, and batch-flush
//! sizes, emitted on the diagnostic channel (`WEFT_TRACE_CHANNELS=1`,
//! same gate contract as frame_trace / parse_worker_stats). With the switch
//! off the accumulator is a few integer adds per read.

use std::time::{Duration, Instant};

/// Window length for the emitted averages.
const EMIT_WINDOW_MS: u64 = 500;

fn gate_enabled() -> bool {
    static GATE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GATE.get_or_init(|| {
        std::env::var_os("WEFT_TRACE_CHANNELS").is_some_and(|v| v == std::ffi::OsStr::new("1"))
    })
}

/// Accumulated reader statistics for one read loop.
#[derive(Default)]
pub(crate) struct ReadLoopStats {
    window_start: Option<Instant>,
    reads: u64,
    wouldblocks: u64,
    bytes: u64,
    batches: u64,
    batch_bytes: u64,
    max_batch: u64,
}

impl ReadLoopStats {
    pub(crate) fn record_read(&mut self, bytes: usize, now: Instant) {
        self.window_start.get_or_insert(now);
        self.reads += 1;
        self.bytes += bytes as u64;
    }

    pub(crate) fn record_wouldblock(&mut self, now: Instant) {
        self.window_start.get_or_insert(now);
        self.wouldblocks += 1;
    }

    pub(crate) fn record_flush(&mut self, batch_len: usize, now: Instant) {
        self.window_start.get_or_insert(now);
        self.batches += 1;
        self.batch_bytes += batch_len as u64;
        self.max_batch = self.max_batch.max(batch_len as u64);
    }

    /// Emit the window once it has run long enough; reset either way (a
    /// disabled channel must not accumulate stale windows).
    pub(crate) fn emit_if_due(&mut self, now: Instant) -> bool {
        let Some(start) = self.window_start else {
            return false;
        };
        if now.duration_since(start) < Duration::from_millis(EMIT_WINDOW_MS) {
            return false;
        }
        // `reads > 0`: pure-WouldBlock idle windows are dropped (nothing to
        // attribute); WouldBlock-rate denominators are `reads`, not windows.
        if gate_enabled() && self.reads > 0 {
            tracing::info!(
                reads = self.reads,
                wouldblocks = self.wouldblocks,
                bytes = self.bytes,
                avg_read = self.bytes / self.reads,
                batches = self.batches,
                avg_batch = self.batch_bytes.checked_div(self.batches).unwrap_or(0),
                max_batch = self.max_batch,
                window_ms = now.duration_since(start).as_millis() as u64,
                "read-loop drain window",
            );
        }
        *self = Self::default();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_accumulate_and_reset() {
        let t0 = Instant::now();
        let mut stats = ReadLoopStats::default();
        stats.record_read(16, t0);
        stats.record_wouldblock(t0);
        stats.record_flush(16, t0);
        assert!(!stats.emit_if_due(t0 + Duration::from_millis(100)));
        assert!(stats.emit_if_due(t0 + Duration::from_millis(600)));
        assert_eq!(stats.reads, 0);
    }
}
