//! Opt-in GUI performance probe used by the macOS acceptance gate.
//!
//! Normal application runs pay only a disabled boolean check. With
//! `WEFT_GUI_PERF_PROBE=1`, the event loop warms up, measures idle wakes and
//! redraw CPU submission time, prints one machine-readable result, and exits.

use std::time::{Duration, Instant};

pub(crate) const ENV_NAME: &str = "WEFT_GUI_PERF_PROBE";
pub(crate) const WARMUP: Duration = Duration::from_secs(3);
pub(crate) const SAMPLE: Duration = Duration::from_secs(5);

const MAX_WAKE_HZ: f64 = 3.0;
const MAX_REDRAW_HZ: f64 = 4.0;
const MAX_CPU_FRAME_P95_MS: f64 = 20.0;

#[derive(Default)]
pub(crate) struct PerformanceProbe {
    enabled: bool,
    measuring_since: Option<Instant>,
    wakes: u64,
    redraws: u64,
    cpu_frames: Vec<Duration>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ProbeReport {
    pub(crate) seconds: f64,
    pub(crate) wakes: u64,
    pub(crate) redraws: u64,
    pub(crate) wake_hz: f64,
    pub(crate) redraw_hz: f64,
    pub(crate) cpu_frame_p95_ms: f64,
}

impl PerformanceProbe {
    pub(crate) fn from_env() -> Self {
        Self {
            enabled: flag_enabled(std::env::var_os(ENV_NAME).as_deref()),
            ..Self::default()
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn start(&mut self) {
        if !self.enabled {
            return;
        }
        self.measuring_since = Some(Instant::now());
        self.wakes = 0;
        self.redraws = 0;
        self.cpu_frames.clear();
    }

    pub(crate) fn record_wake(&mut self) {
        if self.measuring_since.is_some() {
            self.wakes += 1;
        }
    }

    pub(crate) fn record_redraw(&mut self, elapsed: Duration) {
        if self.measuring_since.is_some() {
            self.redraws += 1;
            self.cpu_frames.push(elapsed);
        }
    }

    pub(crate) fn finish(&mut self) -> Option<ProbeReport> {
        let elapsed = self.measuring_since.take()?.elapsed();
        let report = ProbeReport::new(elapsed, self.wakes, self.redraws, &self.cpu_frames);
        self.cpu_frames.clear();
        Some(report)
    }
}

impl ProbeReport {
    fn new(elapsed: Duration, wakes: u64, redraws: u64, frames: &[Duration]) -> Self {
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        Self {
            seconds,
            wakes,
            redraws,
            wake_hz: wakes as f64 / seconds,
            redraw_hz: redraws as f64 / seconds,
            cpu_frame_p95_ms: percentile_95_ms(frames),
        }
    }

    pub(crate) fn passes(&self) -> bool {
        self.wake_hz <= MAX_WAKE_HZ
            && self.redraw_hz <= MAX_REDRAW_HZ
            && self.cpu_frame_p95_ms <= MAX_CPU_FRAME_P95_MS
    }

    pub(crate) fn line(&self) -> String {
        format!(
            "WEFT_GUI_PERF status={} seconds={:.3} wakes={} wake_hz={:.3} redraws={} redraw_hz={:.3} cpu_frame_p95_ms={:.3}",
            if self.passes() { "PASS" } else { "FAIL" },
            self.seconds,
            self.wakes,
            self.wake_hz,
            self.redraws,
            self.redraw_hz,
            self.cpu_frame_p95_ms,
        )
    }
}

fn flag_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

fn percentile_95_ms(samples: &[Duration]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<_> = samples.iter().map(Duration::as_secs_f64).collect();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index] * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_requires_exactly_one() {
        assert!(flag_enabled(Some(std::ffi::OsStr::new("1"))));
        for value in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("true")),
        ] {
            assert!(!flag_enabled(value));
        }
    }

    #[test]
    fn percentile_uses_nearest_rank_and_empty_is_zero() {
        assert_eq!(percentile_95_ms(&[]), 0.0);
        let samples: Vec<_> = (1..=20).map(Duration::from_millis).collect();
        assert_eq!(percentile_95_ms(&samples), 19.0);
    }

    #[test]
    fn report_enforces_idle_and_frame_budgets() {
        let pass = ProbeReport::new(
            Duration::from_secs(5),
            10,
            12,
            &[Duration::from_millis(4), Duration::from_millis(8)],
        );
        assert!(pass.passes());
        assert!(pass.line().contains("status=PASS"));

        let busy = ProbeReport::new(Duration::from_secs(5), 16, 12, &[]);
        assert!(!busy.passes());
        assert!(busy.line().contains("status=FAIL"));
    }
}
