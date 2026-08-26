//! v1.8.3: Lightweight AI observability counters.
//!
//! Records per-request outcomes (success / error / cancelled / truncated) and
//! a ring buffer of recent latencies. The data is intentionally minimal — it
//! backs the debug log lines emitted on completion and is exposed via
//! [`AiState::metrics_snapshot`] for future UI surfacing. **No prompt or
//! response text is ever recorded**, only counts and durations
//! (`docs/V18_IMPLEMENTATION_PLAN.md` §6: "Debug 日志不含正文").

use std::time::Duration;

/// How many recent latencies to keep in the ring buffer.
const LATENCY_RING_SIZE: usize = 32;

/// Mutable counters held inside `AiState`.
#[derive(Debug, Default)]
pub struct AiMetrics {
    /// Total requests spawned (command-gen + diagnose + list-models).
    requests_total: u64,
    /// Requests that completed successfully.
    successes_total: u64,
    /// Requests that failed (network / status / parse / etc.).
    errors_total: u64,
    /// Requests cancelled by the user (superseded or UI closed).
    cancellations_total: u64,
    /// Requests whose input was truncated before sending (prompt builder
    /// clipped history/output to stay within the byte budget). v1.11.0:
    /// `record_truncation` was removed as dead code — nothing calls it, so
    /// this counter stays 0 until a recorder actually exists
    /// (AUDIT_v1.10.39 / PLAN_v111).
    truncations_total: u64,
    /// Ring buffer of recent request latencies (milliseconds). Oldest entry
    /// is overwritten when the buffer is full. Kept small so `snapshot()` is
    /// cheap to call per-frame from the redraw loop.
    latencies_ms: Vec<u64>,
    /// Index in `latencies_ms` where the next sample will be written.
    latency_head: usize,
}

impl AiMetrics {
    pub fn new() -> Self {
        Self {
            latencies_ms: Vec::with_capacity(LATENCY_RING_SIZE),
            ..Default::default()
        }
    }

    /// Record the start of a request. Called from `AiState::spawn_*`.
    pub(crate) fn record_request(&mut self) {
        self.requests_total += 1;
    }

    /// Record a successful completion with the given wall-clock latency.
    pub(crate) fn record_success(&mut self, latency: Duration) {
        self.successes_total += 1;
        self.push_latency(latency);
    }

    /// Record a failed completion with the given wall-clock latency.
    pub(crate) fn record_error(&mut self, latency: Duration) {
        self.errors_total += 1;
        self.push_latency(latency);
    }

    /// Record a cancellation. No latency sample is pushed (cancelled
    /// requests don't reflect steady-state latency).
    pub(crate) fn record_cancellation(&mut self) {
        self.cancellations_total += 1;
    }

    fn push_latency(&mut self, latency: Duration) {
        let ms = latency.as_millis().min(u32::MAX as u128) as u64;
        if self.latencies_ms.len() < LATENCY_RING_SIZE {
            self.latencies_ms.push(ms);
        } else {
            self.latencies_ms[self.latency_head] = ms;
        }
        self.latency_head = (self.latency_head + 1) % LATENCY_RING_SIZE;
    }

    /// Read-only snapshot for logging / UI. Cheap to call.
    pub fn snapshot(&self) -> AiMetricsSnapshot {
        let count = self.latencies_ms.len();
        let p95 = if count == 0 {
            0
        } else {
            let mut sorted = self.latencies_ms.clone();
            sorted.sort_unstable();
            // Nearest-rank p95: ceil(0.95 * n) - 1, clamped to 0.
            let idx = ((count as f64) * 0.95).ceil() as usize;
            sorted[idx.saturating_sub(1).min(count - 1)]
        };
        AiMetricsSnapshot {
            requests_total: self.requests_total,
            successes_total: self.successes_total,
            errors_total: self.errors_total,
            cancellations_total: self.cancellations_total,
            truncations_total: self.truncations_total,
            // v1.11.0: `samples` field removed — nothing read it at
            // runtime (AUDIT_v1.10.39 / PLAN_v111). `count` above still
            // drives the p95 computation.
            p95_latency_ms: p95,
        }
    }
}

/// Read-only view of [`AiMetrics`] used for logging and (eventually) the
/// Settings AI panel's observability row. Contains no prompt/response text.
#[derive(Debug, Clone, Default)]
pub struct AiMetricsSnapshot {
    pub requests_total: u64,
    pub successes_total: u64,
    pub errors_total: u64,
    pub cancellations_total: u64,
    pub truncations_total: u64,
    /// p95 latency in milliseconds (0 when no samples).
    pub p95_latency_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_overwrites_oldest() {
        let mut m = AiMetrics::new();
        for i in 0..(LATENCY_RING_SIZE as u64 + 5) {
            m.record_success(Duration::from_millis(i + 1));
        }
        let snap = m.snapshot();
        assert_eq!(snap.successes_total, LATENCY_RING_SIZE as u64 + 5);
        // p95 should be one of the recent values (>= the smallest after wrap).
        // (v1.11.0: the `samples` count field was removed as dead data —
        // AUDIT_v1.10.39 / PLAN_v111.)
        assert!(snap.p95_latency_ms > 0);
    }

    #[test]
    fn empty_metrics_snapshot_is_zero() {
        let m = AiMetrics::new();
        let s = m.snapshot();
        assert_eq!(s.requests_total, 0);
        assert_eq!(s.p95_latency_ms, 0);
    }

    #[test]
    fn cancellations_do_not_push_latency() {
        let mut m = AiMetrics::new();
        m.record_request();
        m.record_cancellation();
        let s = m.snapshot();
        assert_eq!(s.cancellations_total, 1);
        assert_eq!(s.p95_latency_ms, 0);
    }
}
