//! Opt-in GUI performance probe used by the macOS acceptance gate.
//!
//! Normal application runs pay only a disabled boolean check. With
//! `WEFT_GUI_PERF_PROBE=1`, the event loop warms up, measures idle wakes and
//! redraw CPU submission time, prints one machine-readable result, and exits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) const ENV_NAME: &str = "WEFT_GUI_PERF_PROBE";
pub(crate) const WARMUP: Duration = Duration::from_secs(3);
pub(crate) const SAMPLE: Duration = Duration::from_secs(5);

/// v1.4.0 baseline: env var that overrides `WARMUP` (seconds). The default
/// acceptance gate uses 3s; the v1.4 perf-probe script sets this to 5s so the
/// warm-up covers the first redraws after launch without eating into the
/// 30-second sample window. Defaults to `WARMUP` when unset or unparseable.
const WARMUP_ENV: &str = "WEFT_GUI_PROBE_WARMUP_SECS";
/// v1.4.0 baseline: env var that overrides `SAMPLE` (seconds). The default
/// acceptance gate uses 5s (total probe runtime 8s); the v1.4 perf-probe
/// script sets this to 30s so steady-state p50/p95 percentiles are stable.
/// Defaults to `SAMPLE` when unset or unparseable.
const SAMPLE_ENV: &str = "WEFT_GUI_PROBE_SAMPLE_SECS";

const MAX_WAKE_HZ: f64 = 3.0;
const MAX_REDRAW_HZ: f64 = 4.0;
const MAX_CPU_FRAME_P95_MS: f64 = 20.0;
const MAX_IDLE_RSS_BYTES: u64 = 100 * 1024 * 1024;

static STARTUP_BEGIN: OnceLock<Instant> = OnceLock::new();
static FIRST_FRAME_REPORTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn start_startup_clock() {
    if flag_enabled(std::env::var_os(ENV_NAME).as_deref()) {
        let _ = STARTUP_BEGIN.set(Instant::now());
    }
}

pub(crate) fn report_first_frame_once() {
    let Some(started) = STARTUP_BEGIN.get() else {
        return;
    };
    if !FIRST_FRAME_REPORTED.swap(true, Ordering::Relaxed) {
        println!(
            "V110_METRIC name=cold_start first_frame_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// v1.11.12 (PLAN_v11112 M-B): compile-time-closed set of cold-start phases.
/// A fixed enum (not free-form names) keeps stray/mistyped phase names out of
/// baseline.json. Nine phases (the architect's P1-3 `path_scan` and P1-4
/// `stores_open` widen the original 8-value list): every V110_METRIC
/// `cold_start` line except `first_frame` is one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupPhase {
    /// Login-shell PATH scan (`scan_path_bins`) — spawns the login shell,
    /// 1500ms deadline; the top "+63ms" suspect.
    PathScan,
    /// Config load + `ConfigState` build (excludes the PATH scan).
    Config,
    /// UNUserNotificationCenter sink build (`build_sink`).
    Notifications,
    /// `MTLCreateSystemDefaultDevice`.
    MetalDevice,
    /// Glyph atlas construction (font load + rasterization warm-up).
    Atlas,
    /// `MetalRenderer::new` — shader compile + pipelines + layer.
    RendererInit,
    /// Window creation + pane/PTY spawn + sidebar/Dock icon application.
    WindowPane,
    /// blocks.db / annotation store / search index open (+ index rebuild)
    /// and palette search worker spawn.
    StoresOpen,
    /// Startup recovery detection + tab snapshot restore + history hydration.
    SessionRestore,
}

impl StartupPhase {
    pub(crate) const ALL: [StartupPhase; 9] = [
        StartupPhase::PathScan,
        StartupPhase::Config,
        StartupPhase::Notifications,
        StartupPhase::MetalDevice,
        StartupPhase::Atlas,
        StartupPhase::RendererInit,
        StartupPhase::WindowPane,
        StartupPhase::StoresOpen,
        StartupPhase::SessionRestore,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            StartupPhase::PathScan => "path_scan",
            StartupPhase::Config => "config",
            StartupPhase::Notifications => "notifications",
            StartupPhase::MetalDevice => "metal_device",
            StartupPhase::Atlas => "atlas",
            StartupPhase::RendererInit => "renderer_init",
            StartupPhase::WindowPane => "window_pane",
            StartupPhase::StoresOpen => "stores_open",
            StartupPhase::SessionRestore => "session_restore",
        }
    }
}

/// Per-phase de-duplication — each phase reports at most once per process
/// (architect P1-2: phases must NOT share `FIRST_FRAME_REPORTED`, whose swap
/// would let the first caller silence every later phase).
static PHASE_REPORTED: [AtomicBool; StartupPhase::ALL.len()] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];

/// Report one cold-start phase boundary. The metric is a cumulative instant
/// (ms since `STARTUP_BEGIN`); the per-phase attribution table is derived by
/// differencing consecutive lines. Zero cost when the probe is off (the
/// `STARTUP_BEGIN` OnceLock is never set).
pub(crate) fn report_phase(phase: StartupPhase) {
    let Some(started) = STARTUP_BEGIN.get() else {
        return;
    };
    if !claim_phase_index(phase as usize) {
        return;
    }
    println!("{}", phase_metric_line(phase, started.elapsed()));
}

/// Claim a phase's single report slot. Separated from `report_phase` so the
/// de-dup semantics are testable without the probe env var.
fn claim_phase_index(index: usize) -> bool {
    !PHASE_REPORTED[index].swap(true, Ordering::Relaxed)
}

/// The exact V110_METRIC line a phase emits (separated for the format test).
fn phase_metric_line(phase: StartupPhase, elapsed: Duration) -> String {
    format!(
        "V110_METRIC name=cold_start_phase phase={} ms={:.3}",
        phase.as_str(),
        elapsed.as_secs_f64() * 1000.0
    )
}

/// Resolve the warm-up duration from `WARMUP_ENV` or fall back to `WARMUP`.
/// Used by `app_runtime.rs` to schedule `PerformanceProbeStart`. Unparseable
/// values fall back to the default rather than panicking — the probe is a
/// diagnostic tool and should never break the app on a bad env var.
pub(crate) fn warmup_duration() -> Duration {
    parse_secs_env(std::env::var_os(WARMUP_ENV).as_deref()).unwrap_or(WARMUP)
}

/// Resolve the sample duration from `SAMPLE_ENV` or fall back to `SAMPLE`.
/// Used by `app_runtime.rs` to schedule `PerformanceProbeFinish`.
pub(crate) fn sample_duration() -> Duration {
    parse_secs_env(std::env::var_os(SAMPLE_ENV).as_deref()).unwrap_or(SAMPLE)
}

/// Parse a duration env var (seconds). Pure function for testability —
/// callers pass the resolved env value so the parser has no global state.
/// Returns `None` for empty, zero, negative, or non-numeric input.
fn parse_secs_env(raw: Option<&std::ffi::OsStr>) -> Option<Duration> {
    let raw = raw?;
    let secs: u64 = raw.to_str()?.trim().parse().ok()?;
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

#[derive(Default)]
pub(crate) struct PerformanceProbe {
    enabled: bool,
    measuring_since: Option<Instant>,
    wakes: u64,
    redraws: u64,
    cpu_frames: Vec<Duration>,
    /// v1.11.2 X6 (PLAN_v1112 §6): wall-clock duration of each 1 Hz
    /// TabsAutoSave handler run observed while measuring. Observational
    /// only — feeds the `autosave_tick_p95_ms` report line so the .9
    /// performance re-test can decide whether an epoch short-circuit is
    /// needed; it does NOT participate in pass/fail.
    autosave_ticks: Vec<Duration>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct ProbeReport {
    pub(crate) seconds: f64,
    pub(crate) wakes: u64,
    pub(crate) redraws: u64,
    pub(crate) wake_hz: f64,
    pub(crate) redraw_hz: f64,
    pub(crate) cpu_frame_p95_ms: f64,
    pub(crate) autosave_tick_p95_ms: f64,
    pub(crate) resident_bytes: u64,
    pub(crate) writable_resident_bytes: u64,
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
        self.autosave_ticks.clear();
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

    /// v1.11.2 X6: observe one TabsAutoSave handler run.
    pub(crate) fn record_autosave_tick(&mut self, elapsed: Duration) {
        if self.measuring_since.is_some() {
            self.autosave_ticks.push(elapsed);
        }
    }

    pub(crate) fn finish(&mut self) -> Option<ProbeReport> {
        let elapsed = self.measuring_since.take()?.elapsed();
        let report = ProbeReport::new(
            elapsed,
            self.wakes,
            self.redraws,
            &self.cpu_frames,
            &self.autosave_ticks,
            instant_resident_bytes(),
            writable_resident_bytes(),
        );
        self.cpu_frames.clear();
        self.autosave_ticks.clear();
        Some(report)
    }
}

impl ProbeReport {
    fn new(
        elapsed: Duration,
        wakes: u64,
        redraws: u64,
        frames: &[Duration],
        autosave_ticks: &[Duration],
        resident_bytes: u64,
        writable_resident_bytes: u64,
    ) -> Self {
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        Self {
            seconds,
            wakes,
            redraws,
            wake_hz: wakes as f64 / seconds,
            redraw_hz: redraws as f64 / seconds,
            cpu_frame_p95_ms: percentile_95_ms(frames),
            autosave_tick_p95_ms: percentile_95_ms(autosave_ticks),
            resident_bytes,
            writable_resident_bytes,
        }
    }

    pub(crate) fn passes(&self) -> bool {
        self.wake_hz <= MAX_WAKE_HZ
            && self.redraw_hz <= MAX_REDRAW_HZ
            && self.cpu_frame_p95_ms <= MAX_CPU_FRAME_P95_MS
            && self.writable_resident_bytes <= MAX_IDLE_RSS_BYTES
    }

    pub(crate) fn line(&self) -> String {
        format!(
            "WEFT_GUI_PERF status={} seconds={:.3} wakes={} wake_hz={:.3} redraws={} redraw_hz={:.3} cpu_frame_p95_ms={:.3} autosave_tick_p95_ms={:.3} resident_bytes={} writable_resident_bytes={}",
            if self.passes() { "PASS" } else { "FAIL" },
            self.seconds,
            self.wakes,
            self.wake_hz,
            self.redraws,
            self.redraw_hz,
            self.cpu_frame_p95_ms,
            self.autosave_tick_p95_ms,
            self.resident_bytes,
            self.writable_resident_bytes,
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

/// Instantaneous resident memory for the idle-RSS gate. `getrusage` reports
/// a lifetime high-water mark on macOS and therefore measures startup peaks,
/// not the plan's "after 60 seconds idle" contract. This probe-only path runs
/// once at shutdown and parses `ps` RSS (KiB); normal application runs never
/// spawn it.
fn instant_resident_bytes() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| parse_rss_kib(&text))
        .unwrap_or(u64::MAX)
}

/// Writable resident memory from `vmmap -summary`. On macOS 26 total RSS is
/// dominated by shared framework pages and varied from ~113 MiB to ~700 MiB
/// for the same binary depending on page-cache state. Writable resident bytes
/// track app-owned memory and are therefore the adjusted v1.10 budget metric;
/// total RSS remains in the report for diagnostics.
fn writable_resident_bytes() -> u64 {
    let output = std::process::Command::new("vmmap")
        .args(["-summary", &std::process::id().to_string()])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| parse_writable_resident(&text))
        .unwrap_or(u64::MAX)
}

fn parse_writable_resident(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.starts_with("Writable regions:"))?;
    let raw = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("resident="))?
        .split('(')
        .next()?;
    parse_vm_size(raw)
}

fn parse_vm_size(raw: &str) -> Option<u64> {
    let (number, multiplier) = match raw.chars().last()? {
        'K' => (&raw[..raw.len() - 1], 1024.0),
        'M' => (&raw[..raw.len() - 1], 1024.0 * 1024.0),
        'G' => (&raw[..raw.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (raw, 1.0),
    };
    Some((number.parse::<f64>().ok()? * multiplier) as u64)
}

fn parse_rss_kib(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()?.checked_mul(1024)
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
    fn ps_rss_parser_converts_kib_to_bytes() {
        assert_eq!(parse_rss_kib("  102400\n"), Some(100 * 1024 * 1024));
        assert_eq!(parse_rss_kib("n/a"), None);
    }

    #[test]
    fn vmmap_parser_extracts_writable_resident_bytes() {
        let sample = "Writable regions: Total=789.6M written=57.1M(7%) resident=68.0M(9%) swapped_out=554.2M(70%)\n";
        assert_eq!(parse_writable_resident(sample), Some(68 * 1024 * 1024));
    }

    #[test]
    fn report_enforces_idle_and_frame_budgets() {
        let pass = ProbeReport::new(
            Duration::from_secs(5),
            10,
            12,
            &[Duration::from_millis(4), Duration::from_millis(8)],
            &[Duration::from_micros(800)],
            80 * 1024 * 1024,
            60 * 1024 * 1024,
        );
        assert!(pass.passes());
        assert!(pass.line().contains("status=PASS"));
        // v1.11.2 X6: autosave tick p95 is part of the report line.
        assert!(pass.line().contains("autosave_tick_p95_ms=0.800"));

        let busy = ProbeReport::new(
            Duration::from_secs(5),
            16,
            12,
            &[],
            &[],
            80 * 1024 * 1024,
            60 * 1024 * 1024,
        );
        assert!(!busy.passes());
        assert!(busy.line().contains("status=FAIL"));

        let heavy = ProbeReport::new(
            Duration::from_secs(5),
            10,
            12,
            &[],
            &[],
            700 * 1024 * 1024,
            101 * 1024 * 1024,
        );
        assert!(!heavy.passes());
    }

    /// v1.11.2 X6 (PLAN_v1112 §6): autosave tick samples feed the p95 line
    /// but never the pass/fail gate — the metric exists to inform the .9
    /// decision on an epoch short-circuit, not to fail runs.
    #[test]
    fn slow_autosave_ticks_report_but_do_not_fail() {
        let report = ProbeReport::new(
            Duration::from_secs(30),
            40,
            40,
            &[Duration::from_millis(4)],
            &[Duration::from_millis(1), Duration::from_millis(20)],
            u64::MAX - 1,
            u64::MAX - 1,
        );
        assert_eq!(report.autosave_tick_p95_ms, 20.0);
        assert!(report.line().contains("autosave_tick_p95_ms=20.000"));
    }

    /// v1.11.2 X6: record_autosave_tick only collects while measuring.
    #[test]
    fn autosave_tick_recording_requires_active_measurement() {
        // enabled=true so start() actually begins a measurement window.
        let mut probe = PerformanceProbe {
            enabled: true,
            ..PerformanceProbe::default()
        };
        probe.record_autosave_tick(Duration::from_millis(9));
        probe.start();
        probe.record_autosave_tick(Duration::from_millis(3));
        let report = probe.finish().expect("measuring");
        assert_eq!(report.autosave_tick_p95_ms, 3.0);
    }

    #[test]
    fn parse_secs_env_accepts_positive_integers() {
        assert_eq!(
            parse_secs_env(Some(std::ffi::OsStr::new("30"))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_secs_env(Some(std::ffi::OsStr::new("  7  "))),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn parse_secs_env_rejects_zero_and_garbage() {
        assert_eq!(parse_secs_env(Some(std::ffi::OsStr::new("0"))), None);
        assert_eq!(parse_secs_env(Some(std::ffi::OsStr::new(""))), None);
        assert_eq!(parse_secs_env(Some(std::ffi::OsStr::new("abc"))), None);
        assert_eq!(parse_secs_env(Some(std::ffi::OsStr::new("-5"))), None);
        assert_eq!(parse_secs_env(None), None);
    }

    #[test]
    fn warmup_and_sample_default_to_constants_when_env_unset() {
        // No env var set → falls back to the gate's constants.
        std::env::remove_var(WARMUP_ENV);
        std::env::remove_var(SAMPLE_ENV);
        assert_eq!(warmup_duration(), WARMUP);
        assert_eq!(sample_duration(), SAMPLE);
    }

    // ── v1.11.12 (PLAN_v11112 M-B): cold-start phase probe ──────────────

    /// Phase names are unique, non-empty, machine-safe (the python gate
    /// parser regex is `[a-zA-Z0-9_]+`), and every slot has one.
    #[test]
    fn phase_names_are_unique_and_machine_safe() {
        let mut names: Vec<&str> = StartupPhase::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(names.len(), 9);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 9, "phase names must be unique");
        assert!(names.iter().all(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }));
    }

    /// The metric line format the gate's parser consumes.
    #[test]
    fn phase_metric_line_format() {
        let line = phase_metric_line(StartupPhase::PathScan, Duration::from_secs_f64(0.063256));
        assert_eq!(
            line,
            "V110_METRIC name=cold_start_phase phase=path_scan ms=63.256"
        );
    }

    /// Each phase slot claims exactly once — first claim wins, later claims
    /// (and therefore later prints) are suppressed. Exercises every index so
    /// the `PHASE_REPORTED` array length is pinned to the enum.
    #[test]
    fn phase_claim_is_exactly_once_per_index() {
        for (index, phase) in StartupPhase::ALL.iter().enumerate() {
            assert_eq!(*phase as usize, index, "enum ordinals must match ALL");
            assert!(claim_phase_index(index), "first claim must win");
            assert!(!claim_phase_index(index), "second claim must be suppressed");
        }
    }
}
