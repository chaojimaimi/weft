//! Per-frame segment tracing + counters for the render pipeline.
//!
//! WARP_REFERENCE R3 task 6 calls for a five-segment frame trace
//! (parse / layout / build-vertices / encode / gpu-complete) plus visible
//! block/row/vertex/cache-hit counters. The parse segment runs in a separate
//! event-loop iteration from the render (see `app_runtime.rs::user_event` vs
//! `redraw_controller.rs`), and the GPU completes asynchronously 1–2 frames
//! behind because of triple-buffering (`renderer.rs` vertex ring). A single
//! spanning span would therefore not capture one logical frame.
//!
//! This module implements the tractable subset: three synchronous CPU segments
//! measured inside `RedrawRequested` (layout / build-vertices / encode), plus
//! an asynchronous GPU-complete correlation via `add_completed_handler`. The
//! GPU completion for frame N is recorded against frame N's `frame_id` and
//! drained on a later frame, so the report is eventual but complete.
//!
//! All recording is gated by the frame-trace output gate: the acceptance
//! probe `WEFT_GUI_PERF_PROBE=1` (see `performance_probe.rs`) or, since M6-d
//! (PLAN_M6 §三), the dedicated `WEFT_TRACE_CHANNELS=1` diagnostic channel
//! that never exits (see `trace_enabled`). Normal runs pay only a
//! disabled-bool check per frame — the `Instant::now()` calls inside the
//! probe path are nanosecond overhead, dwarfed by the Metal work they
//! bracket.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::time::Instant;

/// Why this frame was rendered. Derived from the implicit signals that today
/// drive `request_redraw` — no single enum is threaded through the call yet,
/// so the recorder classifies from observable state at frame start.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameReason {
    /// PTY output arrived and the view follows it.
    PtyOutput,
    /// Blinking cursor or spinner animation tick.
    Animation,
    /// Mouse hover / scroll / selection dragging. Not yet produced by
    /// `classify_reason` (no hover flag is threaded into the recorder yet);
    /// reserved for when a hovered-element signal lands. Kept so the variant
    /// space is documented and the trace format is stable.
    #[allow(dead_code)]
    Hover,
    /// Window or font resize.
    Resize,
    /// Anything else (config reload, menu, accessibility press, ...).
    Other,
}

impl FrameReason {
    fn as_str(self) -> &'static str {
        match self {
            FrameReason::PtyOutput => "pty",
            FrameReason::Animation => "anim",
            FrameReason::Hover => "hover",
            FrameReason::Resize => "resize",
            FrameReason::Other => "other",
        }
    }
}

/// Counters captured during build-vertices — the metrics WARP_REFERENCE R3
/// wants tracked so future work (background-run merge, height prefix index)
/// can be proven against a baseline.
#[derive(Default, Clone, Copy)]
pub(crate) struct FrameCounters {
    pub(crate) vertex_count: usize,
    pub(crate) instance_count: usize,
    pub(crate) dirty_rows: usize,
    /// Step 1: block-specific counters for BlockView path observability.
    /// All zero in grid view. In block view:
    /// - `session_block_count`: total finished blocks in the session
    ///   (`terminal.block_tracker().session_blocks().len()`)
    /// - `visible_block_count`: blocks whose rows were actually expanded
    ///   into `bv_rows` (after clipping). Batch 6 Step 1: now reflects the
    ///   actual expanded count from `compute_block_layout_pass`, not the
    ///   total. When << `session_block_count`, visibility culling is working.
    /// - `bv_rows_count`: total rows in the returned `bv_rows` Vec — the
    ///   rows the hit-tester / selection syncer / find highlighter will see.
    /// - `block_layout_cache_hits` / `_misses`: increments since last frame
    ///   from `BlockLayoutCache::ensure_cached`. A "miss" is a rebuild
    ///   (cache entry absent or stale); a "hit" is a no-op ensure_cached.
    ///   Steady-state should be hits >> misses.
    /// - `styled_line_lookups`: Batch 6 Step 1. Number of `styled.line()`
    ///   lookups performed this frame. Zero in grid view or hit-testing;
    ///   in block-view paint, equals the count of visible output lines.
    ///   Used to assess whether styled-line caching is worth the complexity.
    pub(crate) session_block_count: usize,
    pub(crate) visible_block_count: usize,
    pub(crate) bv_rows_count: usize,
    pub(crate) block_layout_cache_hits: usize,
    pub(crate) block_layout_cache_misses: usize,
    pub(crate) styled_line_lookups: usize,
    /// Batch 7 Step 4: wall-clock time spent inside `push_block_output_text`
    /// this frame, in microseconds. Complements `styled_line_lookups` (which
    /// only counts calls) by measuring the actual CPU cost of styled-line
    /// painting — including per-char `partition_point` lookups and glyph
    /// pushes. Zero in grid view. Used to decide whether styled-line caching
    /// is worth the vertex-relative-coords refactor.
    pub(crate) styled_paint_us: u64,
    /// R5 task 4: process resident memory (RSS) in bytes at frame start.
    /// Collected via `getrusage(RUSAGE_SELF)` on macOS. Used to detect
    /// memory leaks during long-running sessions and correlate with
    /// vertex/cache metrics for perf optimization decisions.
    pub(crate) resident_bytes: u64,
    /// v1.4.0 baseline: grid background instances uploaded this frame. v1.4.0
    /// keeps the single-stream grid pipeline, so this is always 0; v1.4.2's
    /// background-run merge will populate it. Reported now so the v1.4.2 GO
    /// decision can compare against a pre-existing counter instead of a new
    /// field landing alongside the change it measures.
    pub(crate) grid_bg_instances: usize,
    /// v1.4.0 baseline: grid glyph instances uploaded this frame. v1.4.0 fills
    /// this with the existing single-stream instance count (`instances.len() /
    /// 16`). After v1.4.2's split, this will count only the glyph stream while
    /// `grid_bg_instances` counts the background stream.
    pub(crate) grid_glyph_instances: usize,
    /// v1.4.0 baseline: bytes uploaded to GPU vertex/instance buffers this
    /// frame (`size_of_val(vertices) + size_of_val(instances)`). Computed from
    /// the slices passed to `encode_and_present`, so it matches the actual
    /// upload regardless of the idle fast-path skip. Used by v1.4.2 to prove
    /// the background-run merge reduces upload bytes.
    pub(crate) grid_upload_bytes: u64,
    /// v1.4.0 baseline: styled-line vertex cache hits this frame. Always 0 in
    /// v1.4.0 (no cache yet); v1.4.1 populates it. Reported now so the v1.4.1
    /// GO decision can cite a pre-existing counter.
    pub(crate) styled_cache_hits: u64,
    /// v1.4.0 baseline: styled-line vertex cache misses this frame. Always 0
    /// in v1.4.0; v1.4.1 populates it.
    pub(crate) styled_cache_misses: u64,
    /// v1.4.0 baseline: styled-line vertex cache resident bytes. Always 0 in
    /// v1.4.0; v1.4.1 populates it. Used to prove the cache stays within its
    /// 16 MiB byte budget.
    pub(crate) styled_cache_bytes: u64,
    /// v1.4.2 Phase A baseline: wall-clock time spent inside
    /// `build_grid_instances` this frame, in microseconds. Sub-timed within
    /// the broader `build_us` segment (which also covers block-view + overlay
    /// vertex emission). Zero in block view (counter is reset on view switch);
    /// small (short-circuit overhead only) on idle grid frames where
    /// `instances_unchanged` short-circuits. Used to decide whether the
    /// background-run merge (B1-B3) is worth the Metal pipeline complexity:
    /// the GO threshold is grid_build_us + encode_us p95 ≥ 1.0 ms OR
    /// ≥ 15% of cpu_total_us p95.
    pub(crate) grid_build_us: u64,
    /// M6-c (PLAN_M6 §三): total bytes held by the block layout cache's
    /// L1/L2 tables (`BlockLayoutCache::table_bytes_total`). Watch it
    /// plateau under the 256MiB budget (R5): drops mean budget degradation,
    /// one-frame jumps mean band-entry Both rebuilds of degraded blocks.
    pub(crate) layout_table_bytes: u64,
    /// M6-c: blocks currently deferred above the sync band (B-2). Grows with
    /// history size during a drag; the idle pump drains it after cols settle.
    pub(crate) deferred_blocks: usize,
}

/// Async GPU-completion message posted from `add_completed_handler` on a Metal
/// internal thread back to the main thread for correlation.
pub(crate) struct FrameGpuComplete {
    pub(crate) frame_id: u64,
    pub(crate) gpu_us: u64,
}

/// Channel for GPU-completion callbacks. A `OnceLock` so the completed-handler
/// blocks (which cannot capture `&self`) can reach it without a handle.
static GPU_CHANNEL: OnceLock<Sender<FrameGpuComplete>> = OnceLock::new();

/// Initialize the global GPU-completion channel. Returns the receiver the main
/// thread drains each frame. Idempotent — the first caller wins; subsequent
/// calls reuse the existing channel (tests that build multiple recorders share
/// one queue, which is fine since the probe is a singleton).
pub(crate) fn gpu_completion_rx() -> Receiver<FrameGpuComplete> {
    let (tx, rx) = mpsc::channel::<FrameGpuComplete>();
    // Ignore the error if a previous test/probe already installed a sender —
    // the receiver from that earlier call is the live one.
    let _ = GPU_CHANNEL.set(tx);
    rx
}

/// Sender side used by `add_completed_handler` blocks. Returns `None` if the
/// channel was never installed (probe disabled / not yet initialized); the
/// caller simply skips registering the handler in that case.
pub(crate) fn gpu_completion_tx() -> Option<&'static Sender<FrameGpuComplete>> {
    GPU_CHANNEL.get()
}

/// M6-d (PLAN_M6 §三): frame-trace output-channel switches.
///
/// The `frame frame_id=…` DEBUG line was historically emitted only while the
/// acceptance probe ran (`WEFT_GUI_PERF_PROBE=1`). That env also drives the
/// probe's warmup + sample + auto-exit (`performance_probe.rs`), which made
/// an instrumented interactive drag impossible (the app quits after ~8s).
/// M4.1 added the dedicated `WEFT_TRACE_CHANNELS=1` switch that ONLY enables
/// diagnostic log lines and never exits (weft_core
/// `vt/screen_exit/snapshot.rs`); M6-d unifies this gate onto the same pair:
/// the line emits when EITHER switch is on. The probe's own
/// warmup/sample/exit logic is untouched — it still runs only under
/// `WEFT_GUI_PERF_PROBE=1`. Only the literal `"1"` enables (M4.1 contract;
/// `true`/`yes`/`0`/unset do not).
const TRACE_CHANNELS_ENV: &str = "WEFT_TRACE_CHANNELS";

/// M6-d: cached raw channel-switch value. `trace_enabled` currently runs
/// once per process (main.rs startup); the OnceLock keeps it idempotent and
/// future-proofs any per-frame call site (the weft_core snapshot gate caches
/// for exactly that reason — it IS called per repaint). Process-lifetime
/// caching is correct — the switch is launch-time. The raw value (not the
/// decoded bool) is cached so the pure [`trace_enabled_impl`] stays on the
/// production path.
static TRACE_CHANNELS_CACHE: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();

/// M6-d: output gate for the frame-trace line — the acceptance probe is
/// active OR the dedicated diagnostic channel is enabled. A channels-only
/// run never triggers the probe's warmup/sample/auto-exit: those live behind
/// `PerformanceProbe::enabled`, which reads only `WEFT_GUI_PERF_PROBE`.
pub(crate) fn trace_enabled(probe_enabled: bool) -> bool {
    let trace_channels = TRACE_CHANNELS_CACHE.get_or_init(|| std::env::var_os(TRACE_CHANNELS_ENV));
    trace_enabled_impl(trace_channels.as_deref(), probe_enabled)
}

/// M6-d: pure decision over both switches — mirrors the weft_core M4.1
/// reader (`vt/screen_exit/snapshot.rs::perf_probe_enabled_impl`): the
/// dedicated channel switch OR the acceptance probe enables the line.
/// Unit-tested below as the gate truth table (the process-level [`OnceLock`]
/// cache itself is not runtime-injectable).
fn trace_enabled_impl(trace_channels: Option<&std::ffi::OsStr>, perf_probe: bool) -> bool {
    env_flag_enabled(trace_channels) || perf_probe
}

/// M6-d: pure decision over one env value — mirrors the app-side
/// `performance_probe::flag_enabled` exactly: only the literal `"1"`
/// enables; `true`/`yes`/`0`/unset do not.
fn env_flag_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

/// Records one frame's three CPU segments and its counters, then awaits GPU
/// completion correlation.
pub(crate) struct FrameTraceRecorder {
    enabled: bool,
    frame_id: u64,
    reason: FrameReason,
    layout_start: Option<Instant>,
    build_start: Option<Instant>,
    encode_start: Option<Instant>,
    layout_us: u64,
    build_us: u64,
    encode_us: u64,
    counters: FrameCounters,
}

impl FrameTraceRecorder {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            frame_id: 0,
            reason: FrameReason::Other,
            layout_start: None,
            build_start: None,
            encode_start: None,
            layout_us: 0,
            build_us: 0,
            encode_us: 0,
            counters: FrameCounters::default(),
        }
    }

    pub(crate) fn begin(enabled: bool, frame_id: u64, reason: FrameReason) -> Self {
        Self {
            enabled,
            frame_id,
            reason,
            layout_start: None,
            build_start: None,
            encode_start: None,
            layout_us: 0,
            build_us: 0,
            encode_us: 0,
            counters: FrameCounters {
                resident_bytes: resident_bytes(),
                ..FrameCounters::default()
            },
        }
    }

    /// Mark the start of the LAYOUT segment (LayoutCtx construction).
    pub(crate) fn layout_start(&mut self) {
        if self.enabled {
            self.layout_start = Some(Instant::now());
        }
    }

    /// Mark the end of the LAYOUT segment.
    pub(crate) fn layout_end(&mut self) {
        if let (Some(start), true) = (self.layout_start.take(), self.enabled) {
            self.layout_us = start.elapsed().as_micros() as u64;
        }
    }

    /// Mark the start of the BUILD-VERTICES segment (grid/block/overlay builders).
    pub(crate) fn build_start(&mut self) {
        // M6-b: armed even when the probe is disabled — build_us is the only
        // always-running per-frame timer, and the block-view convergence pump
        // reads the PREVIOUS frame's value as its idle gate.
        self.build_start = Some(Instant::now());
    }

    /// M6-b B-5: previous build-segment duration in µs. Recorded regardless
    /// of the probe (see `build_start`); 0 before the first completed frame.
    pub(crate) fn build_us(&self) -> u64 {
        self.build_us
    }

    /// Mark the end of BUILD-VERTICES and record the per-frame counters.
    ///
    /// `resident_bytes` is captured at `begin()` (frame start) and is NOT
    /// overwritten by `counters` from the render thread — the render thread
    /// doesn't call `getrusage`, so the value it would supply is always 0.
    /// Preserving the begin-time sample keeps the trace honest.
    pub(crate) fn build_end(&mut self, counters: FrameCounters) {
        if let Some(start) = self.build_start.take() {
            // build_us records unconditionally (pump-gate port); the trace
            // line and counters stay probe-gated.
            self.build_us = start.elapsed().as_micros() as u64;
            if self.enabled {
                // Preserve resident_bytes captured at begin(); the caller
                // (renderer thread) doesn't collect RSS, so its value is 0.
                let resident_bytes = self.counters.resident_bytes;
                self.counters = counters;
                self.counters.resident_bytes = resident_bytes;
            }
        }
    }

    /// Mark the start of the ENCODE segment (Metal command buffer encoding).
    pub(crate) fn encode_start(&mut self) {
        if self.enabled {
            self.encode_start = Some(Instant::now());
        }
    }

    /// Mark the end of the ENCODE segment (just before `command_buffer.commit`).
    pub(crate) fn encode_end(&mut self) {
        if let Some(start) = self.encode_start.take() {
            if self.enabled {
                self.encode_us = start.elapsed().as_micros() as u64;
            }
        }
    }

    /// Finish recording: emit the frame trace line, including any GPU-complete
    /// messages that have arrived for this or earlier frames.
    pub(crate) fn finish(self, gpu_rx: &Receiver<FrameGpuComplete>) {
        if !self.enabled {
            return;
        }
        let Self {
            frame_id,
            reason,
            layout_us,
            build_us,
            encode_us,
            counters,
            ..
        } = self;

        // Drain whatever GPU completions have landed. These may be for this
        // frame (rare — usually lands 1–2 frames later) or for earlier frames;
        // log each so no completion is lost. The triple-buffer means frame N's
        // GPU time typically arrives during frame N+1 or N+2.
        let mut gpu_max_us: u64 = 0;
        let mut gpu_count = 0u64;
        while let Ok(complete) = gpu_rx.try_recv() {
            gpu_count += 1;
            gpu_max_us = gpu_max_us.max(complete.gpu_us);
            tracing::debug!(
                frame_id = complete.frame_id,
                gpu_us = complete.gpu_us,
                "gpu-complete",
            );
        }

        let cpu_total_us = layout_us + build_us + encode_us;
        tracing::debug!(
            frame_id,
            reason = reason.as_str(),
            layout_us,
            build_us,
            encode_us,
            cpu_total_us,
            vertices = counters.vertex_count,
            instances = counters.instance_count,
            dirty_rows = counters.dirty_rows,
            session_blocks = counters.session_block_count,
            visible_blocks = counters.visible_block_count,
            bv_rows = counters.bv_rows_count,
            cache_hits = counters.block_layout_cache_hits,
            cache_misses = counters.block_layout_cache_misses,
            styled_lookups = counters.styled_line_lookups,
            styled_paint_us = counters.styled_paint_us,
            resident_bytes = counters.resident_bytes,
            grid_bg_instances = counters.grid_bg_instances,
            grid_glyph_instances = counters.grid_glyph_instances,
            grid_upload_bytes = counters.grid_upload_bytes,
            styled_cache_hits = counters.styled_cache_hits,
            styled_cache_misses = counters.styled_cache_misses,
            styled_cache_bytes = counters.styled_cache_bytes,
            grid_build_us = counters.grid_build_us,
            layout_table_bytes = counters.layout_table_bytes,
            deferred_blocks = counters.deferred_blocks,
            gpu_completions_this_frame = gpu_count,
            gpu_max_us,
            "frame",
        );
    }
}

/// R5 task 4: collect process resident memory (RSS) in bytes via
/// `getrusage(RUSAGE_SELF)`. On macOS `ru_maxrss` is already in bytes
/// (unlike Linux, where it is in KB). Returns 0 if the syscall fails —
/// the trace then simply reports `resident_bytes=0`, which is harmless.
///
/// `ru_maxrss` is the high-water mark over the process's lifetime, not
/// the instantaneous RSS, but it is the cheapest cross-platform-ish
/// signal available without pulling in a `libc` dependency or a
/// `mach_task_basic_info` FFI dance. It is sufficient for leak
/// detection (the value only grows) and for correlating long-running
/// sessions with vertex/cache metrics.
fn resident_bytes() -> u64 {
    // Minimal FFI: avoid a `libc` crate dependency by declaring just
    // the symbols we touch. Layout matches macOS `struct rusage`.
    #[repr(C)]
    struct Rusage {
        ru_utime: Timeval,
        ru_stime: Timeval,
        ru_maxrss: i64,
        ru_ixrss: i64,
        ru_idrss: i64,
        ru_isrss: i64,
        ru_minflt: i64,
        ru_majflt: i64,
        ru_nswap: i64,
        ru_inblock: i64,
        ru_oublock: i64,
        ru_msgsnd: i64,
        ru_msgrcv: i64,
        ru_nsignals: i64,
        ru_nvcsw: i64,
        ru_nivcsw: i64,
    }

    #[repr(C)]
    struct Timeval {
        tv_sec: i64,
        tv_usec: i32,
    }

    extern "C" {
        fn getrusage(who: i32, usage: *mut Rusage) -> i32;
    }

    // RUSAGE_SELF = 0 on macOS.
    const RUSAGE_SELF: i32 = 0;

    let mut usage = Rusage {
        ru_utime: Timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: Timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_maxrss: 0,
        ru_ixrss: 0,
        ru_idrss: 0,
        ru_isrss: 0,
        ru_minflt: 0,
        ru_majflt: 0,
        ru_nswap: 0,
        ru_inblock: 0,
        ru_oublock: 0,
        ru_msgsnd: 0,
        ru_msgrcv: 0,
        ru_nsignals: 0,
        ru_nvcsw: 0,
        ru_nivcsw: 0,
    };

    // SAFETY: `getrusage` writes into the provided buffer; `Rusage`
    // layout matches the macOS kernel struct. Return value −1 means
    // the call failed — we surface 0 in that case.
    let rc = unsafe { getrusage(RUSAGE_SELF, &mut usage) };
    if rc == 0 && usage.ru_maxrss > 0 {
        usage.ru_maxrss as u64
    } else {
        0
    }
}

/// Helpers for classifying `FrameReason` from the implicit signals the app
/// already tracks. Kept here so the recorder owns the derivation logic.
pub(crate) fn classify_reason(
    had_pty_output: bool,
    cursor_anim_active: bool,
    spinner_anim_active: bool,
    resizing: bool,
) -> FrameReason {
    // PTY output is the dominant redraw driver and the one we most want to
    // attribute; check it first.
    if had_pty_output {
        return FrameReason::PtyOutput;
    }
    if cursor_anim_active || spinner_anim_active {
        return FrameReason::Animation;
    }
    if resizing {
        return FrameReason::Resize;
    }
    // Without a hovered-element flag threaded through, mouse-driven redraws
    // fall into Other. Hover is the lowest-priority bucket; misclassification
    // there does not hide a PTY/animation regression.
    FrameReason::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disabled_recorder_finish_silently() {
        // A disabled recorder must never panic and must not require a real
        // GPU channel. finish() on a disabled recorder is a no-op.
        let (_, rx) = mpsc::channel();
        let r = FrameTraceRecorder::disabled();
        r.finish(&rx);
    }

    #[test]
    fn segments_accumulate_microseconds() {
        let mut r = FrameTraceRecorder::begin(true, 42, FrameReason::PtyOutput);
        r.layout_start();
        // layout segment
        std::thread::sleep(Duration::from_micros(50));
        r.layout_end();
        r.build_start();
        std::thread::sleep(Duration::from_micros(50));
        r.build_end(FrameCounters {
            vertex_count: 100,
            instance_count: 2000,
            dirty_rows: 5,
            session_block_count: 0,
            visible_block_count: 0,
            bv_rows_count: 0,
            block_layout_cache_hits: 0,
            block_layout_cache_misses: 0,
            styled_line_lookups: 0,
            styled_paint_us: 0,
            resident_bytes: 0,
            grid_bg_instances: 0,
            grid_glyph_instances: 2000,
            grid_upload_bytes: 0,
            styled_cache_hits: 0,
            styled_cache_misses: 0,
            styled_cache_bytes: 0,
            grid_build_us: 0,
            // M6-c: new field-existence pins — the trace line grows with the
            // layout-table budget observability.
            layout_table_bytes: 983_041,
            deferred_blocks: 3,
        });
        r.encode_start();
        std::thread::sleep(Duration::from_micros(50));
        r.encode_end();
        // v1.11.16: the upper bound guards against a gross unit/accumulation bug,
        // not against scheduling jitter. The three segments each `sleep(50µs)`,
        // so a correct recorder yields a few dozen µs; a unit error (e.g.
        // recording seconds, or summing across frames) lands in the millions and
        // is still caught. On a loaded shared CI runner the thread can be
        // descheduled for tens of ms inside the 50µs window (observed 7513µs),
        // so a tight 5ms cap was a flaky false failure. 100ms keeps the real
        // regression signal while tolerating host jitter. (A ms-vs-µs bug yields
        // `as_millis()` ≈ 0, which the `>= 40` lower bound already rejects.)
        assert!(r.layout_us >= 40, "layout_us too small: {}", r.layout_us);
        assert!(
            r.layout_us < 100_000,
            "layout_us={} (expected < 100ms; check unit/accumulation)",
            r.layout_us
        );
        assert!(r.build_us >= 40, "build_us={}", r.build_us);
        assert!(r.encode_us >= 40, "encode_us={}", r.encode_us);
        assert_eq!(r.counters.vertex_count, 100);
        assert_eq!(r.counters.instance_count, 2000);
        assert_eq!(r.counters.dirty_rows, 5);
        // M6-c: the two new counter fields must survive build_end intact —
        // they feed the frame trace's `layout_table_bytes` / `deferred_blocks`.
        assert_eq!(r.counters.layout_table_bytes, 983_041);
        assert_eq!(r.counters.deferred_blocks, 3);
        assert_eq!(r.frame_id, 42);
        let (_, rx) = mpsc::channel();
        r.finish(&rx);
    }

    #[test]
    fn disabled_recorder_still_records_build_us_for_pump_gate() {
        // M6-b B-5: build_us is the always-on port the convergence pump
        // reads — a disabled (probe-off) recorder must still time the build
        // segment, even though it emits no trace line.
        let mut r = FrameTraceRecorder::disabled();
        assert_eq!(r.build_us(), 0);
        r.build_start();
        std::thread::sleep(Duration::from_micros(50));
        r.build_end(FrameCounters::default());
        assert!(r.build_us() >= 40, "build_us={}", r.build_us());
        let (_, rx) = mpsc::channel();
        r.finish(&rx); // disabled: no output, no panic.
    }

    #[test]
    fn classify_prefers_pty_then_animation_then_resize() {
        assert_eq!(
            classify_reason(true, true, false, true),
            FrameReason::PtyOutput
        );
        assert_eq!(
            classify_reason(false, true, false, true),
            FrameReason::Animation
        );
        assert_eq!(
            classify_reason(false, false, true, false),
            FrameReason::Animation
        );
        assert_eq!(
            classify_reason(false, false, false, true),
            FrameReason::Resize
        );
        assert_eq!(
            classify_reason(false, false, false, false),
            FrameReason::Other
        );
    }

    #[test]
    fn gpu_channel_is_idempotent() {
        // Multiple gpu_completion_rx() calls must not panic — the second just
        // reuses the first's sender (or fails to install, which is also fine).
        let _rx1 = gpu_completion_rx();
        let _rx2 = gpu_completion_rx();
        // If a tx was installed, dropping both receivers is safe.
    }

    #[test]
    fn resident_bytes_is_nonzero_under_running_process() {
        // R5 task 4: getrusage(RUSAGE_SELF) on a running process should
        // always report a non-zero RSS (the test binary itself plus its
        // dependencies occupy memory). A zero return would indicate the
        // FFI binding is wrong (wrong struct layout, wrong syscall
        // constant, or rc != 0). Tolerate a degenerate 0 only if the
        // syscall actually failed (rc != 0) — otherwise assert growth.
        let bytes = resident_bytes();
        // Typical test-binary RSS is in the low MiB range; accept anything
        // above 100 KiB to avoid flakiness on minimal CI runners.
        assert!(
            bytes >= 100 * 1024,
            "resident_bytes returned {bytes}, expected at least ~100 KiB"
        );
    }

    // ── M6-d (PLAN_M6 §三): output-gate truth table ────────────────────────
    // The gate decides whether the `frame frame_id=…` line is emitted at
    // all: default (both switches off) must stay silent so production logs
    // do not grow, probe-on keeps the acceptance-gate path, and
    // channels-only is the new interactive-capture path (emit, but no probe
    // warmup/sample/auto-exit — those live behind PerformanceProbe::enabled).

    #[test]
    fn trace_gate_default_silent_probe_or_channels_open() {
        // probe off + channels off → closed (default runs emit nothing).
        assert!(!trace_enabled_impl(None, false));
        assert!(!trace_enabled_impl(Some(std::ffi::OsStr::new("0")), false));
        assert!(!trace_enabled_impl(
            Some(std::ffi::OsStr::new("yes")),
            false
        ));
        // probe on → open regardless of the channel switch (the scripted
        // acceptance gate keeps its frame lines).
        assert!(trace_enabled_impl(None, true));
        assert!(trace_enabled_impl(Some(std::ffi::OsStr::new("0")), true));
        // channels only → open: the M6-d headline case (interactive drag
        // capture without the auto-exiting probe).
        assert!(trace_enabled_impl(Some(std::ffi::OsStr::new("1")), false));
    }

    #[test]
    fn trace_channels_flag_requires_exactly_one() {
        // Literal-"1" contract (M4.1): mirrors performance_probe::flag_enabled.
        assert!(env_flag_enabled(Some(std::ffi::OsStr::new("1"))));
        for value in [
            None,
            Some(std::ffi::OsStr::new("0")),
            Some(std::ffi::OsStr::new("true")),
            Some(std::ffi::OsStr::new("yes")),
        ] {
            assert!(!env_flag_enabled(value));
        }
    }
}
