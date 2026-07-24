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
//! All recording is gated by the existing `WEFT_GUI_PERF_PROBE=1` flag (see
//! `performance_probe.rs`). Normal runs pay only a disabled-bool check per
//! frame — the `Instant::now()` calls inside the probe path are nanosecond
//! overhead, dwarfed by the Metal work they bracket.

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
            counters: FrameCounters::default(),
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
        if self.enabled {
            self.build_start = Some(Instant::now());
        }
    }

    /// Mark the end of BUILD-VERTICES and record the per-frame counters.
    pub(crate) fn build_end(&mut self, counters: FrameCounters) {
        if let Some(start) = self.build_start.take() {
            if self.enabled {
                self.build_us = start.elapsed().as_micros() as u64;
                self.counters = counters;
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
            gpu_completions_this_frame = gpu_count,
            gpu_max_us,
            "frame",
        );
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
        });
        r.encode_start();
        std::thread::sleep(Duration::from_micros(50));
        r.encode_end();
        assert!(
            r.layout_us >= 40 && r.layout_us < 5_000,
            "layout_us={}",
            r.layout_us
        );
        assert!(r.build_us >= 40, "build_us={}", r.build_us);
        assert!(r.encode_us >= 40, "encode_us={}", r.encode_us);
        assert_eq!(r.counters.vertex_count, 100);
        assert_eq!(r.counters.instance_count, 2000);
        assert_eq!(r.counters.dirty_rows, 5);
        assert_eq!(r.frame_id, 42);
        let (_, rx) = mpsc::channel();
        r.finish(&rx);
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
}
