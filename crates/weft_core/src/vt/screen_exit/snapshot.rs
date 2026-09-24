//! Primary-screen snapshot pipeline — viewport-row mapping into the composed
//! document, rate-limited refresh, and publish/split.

use super::tail::{merge_primary_screen_interrupt_tail, space_primary_screen_exit_tail};
use super::Terminal;
use super::PRIMARY_HISTORY_SNAPSHOT_INTERVAL;
use crate::blocks::{StyledOutput, DEFAULT_OUTPUT_CAP};
use std::sync::OnceLock;
use std::time::Instant;

/// C1 (PLAN_S2_render Stream C) + M4.1: diagnostic-channel switches.
///
/// `WEFT_GUI_PERF_PROBE=1` drives the app-side acceptance probe, which by
/// design warms up, samples for a fixed window, prints `V110_METRIC` lines
/// and EXITS (performance_probe.rs). Coupling the snapshot phase log to it
/// made an instrumented interactive drag impossible (the app quits after
/// ~8s), so the log channel also accepts a dedicated
/// `WEFT_TRACE_CHANNELS=1` switch that ONLY enables the diagnostic log
/// lines and never exits. `WEFT_GUI_PERF_PROBE=1` still implies the
/// channels for the scripted acceptance gate.
const PERF_PROBE_ENV: &str = "WEFT_GUI_PERF_PROBE";
const TRACE_CHANNELS_ENV: &str = "WEFT_TRACE_CHANNELS";

/// M4.1: cached channel flag. Snapshot composition runs on every full-frame
/// repaint; `std::env::var_os` per call would put a syscall on that path.
/// Process-lifetime caching is correct — both switches are launch-time.
static PERF_PROBE_CACHE: OnceLock<bool> = OnceLock::new();

/// C1/M4.1: gate for the snapshot phase log (cached; see
/// [`PERF_PROBE_CACHE`]).
fn perf_probe_enabled() -> bool {
    *PERF_PROBE_CACHE.get_or_init(|| {
        perf_probe_enabled_impl(
            std::env::var_os(TRACE_CHANNELS_ENV).as_deref(),
            std::env::var_os(PERF_PROBE_ENV).as_deref(),
        )
    })
}

/// M4.1: pure decision over both switches — the dedicated channel switch
/// OR the acceptance probe implies the diagnostic log lines.
fn perf_probe_enabled_impl(
    trace_channels: Option<&std::ffi::OsStr>,
    perf_probe: Option<&std::ffi::OsStr>,
) -> bool {
    env_flag_enabled(trace_channels) || env_flag_enabled(perf_probe)
}

/// C1: pure decision over one env value — mirrors the app-side
/// `performance_probe::flag_enabled` exactly: only the literal `"1"`
/// enables; `true`/`yes`/unset do not. Unit-tested below (the process-level
/// [`OnceLock`] cache itself is not runtime-injectable).
fn env_flag_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value == Some(std::ffi::OsStr::new("1"))
}

impl Terminal {
    /// v1.10.20: snapshot line index of a live viewport row, computed with
    /// the exact same walk parameters as [`Self::primary_screen_document_snapshot`]
    /// (same document start and ownership masks). `None` when no screen
    /// document is captured or the row is skipped by the snapshot (unowned /
    /// empty). Used by the drag-selection anchor migration — the mapping
    /// must match the snapshot the history BlockView renders, and the
    /// empty-row skip breaks any 1:1 row arithmetic.
    pub fn primary_screen_snapshot_line_for_viewport_row(
        &self,
        viewport_row: usize,
    ) -> Option<usize> {
        let document_start = self.block_tracker.screen_document_start()?;
        let viewport_origin = self.grid.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.grid.scrollback.index_since(document_start), 0)
        } else {
            (
                self.grid.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        // PLAN_v11217 §3.5 (T4): the replayed walk must use the SAME derived
        // text budget as the snapshot builder — the tracker's configured cap.
        let text_cap = self.block_tracker.output_cap();
        let line = match self
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref()
        {
            Some(owned) => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                Some(&self.capabilities.primary_screen_ownership.scrollback),
                Some(owned),
                text_cap,
            ),
            None => self.grid.snapshot_line_index_for_viewport_row(
                viewport_row,
                scrollback_start,
                viewport_start,
                None,
                None,
                text_cap,
            ),
        };
        // v1.10.23: the rendered block prepends the preserved-frame history,
        // so the anchor's rendered line shifts by that many lines.
        // v1.10.25: the scroll-out prefix shifts it too (three-part compose).
        line.map(|line| line + self.screen_history_lines() + self.screen_prefix_lines())
    }

    /// v1.10.12: alt-screen history peek — true while the user is browsing the
    /// terminal's history BlockView over an alt-screen TUI (omp/less/man).
    pub fn is_alt_screen_history_peek(&self) -> bool {
        self.capabilities.alt_screen_history_peek
    }

    /// v1.10.12: enter/exit the alt-screen history peek. Entering makes
    /// `show_block_view()` true (BlockView overlays the TUI); exiting restores
    /// the live alt grid. Does NOT touch `grid.scroll_offset` or primary-screen
    /// snapshots — block positioning uses the pane's `block_scroll_anchor`.
    pub fn set_alt_screen_history_peek(&mut self, on: bool) {
        self.capabilities.alt_screen_history_peek = on;
    }

    /// v1.10.6: the cursor's line index in the most recent primary-screen
    /// snapshot. `None` until the first snapshot, or when the cursor sat on
    /// a row the snapshot omitted (unowned, leading, or trailing empty).
    pub fn primary_screen_cursor_snapshot_line(&self) -> Option<usize> {
        self.capabilities.primary_screen_cursor_snapshot_line
    }

    /// v1.10.6: refresh just the cursor's snapshot line, without the
    /// rate-limit or `replace_screen_snapshot` side effects. Called on
    /// every keystroke so the caret/preedit have a precise row even when
    /// no PTY output has arrived yet (IME preedit, idle TUI).
    /// v1.10.7 (reviewer MEDIUM): skip the full document rebuild when the
    /// cursor position is unchanged since the last caret refresh — the
    /// tracked line only depends on the cursor's row, and this call has no
    /// rate limit.
    pub fn snapshot_primary_screen_output_for_caret(&mut self) {
        if self.block_tracker.screen_document_start().is_none() {
            return;
        }
        let cursor = (self.grid.cursor.row, self.grid.cursor.col);
        if self.capabilities.last_caret_snapshot_cursor == Some(cursor) {
            return;
        }
        self.capabilities.last_caret_snapshot_cursor = Some(cursor);
        let document_start = self.block_tracker.screen_document_start().unwrap_or(0);
        let (_, _, cursor_line) = self.primary_screen_document_snapshot(document_start);
        // v1.10.26 (FIX_IME_PREEDIT): re-anchor from the published composed
        // text (`freeze::composed_cursor_snapshot_line`) — the prefix grows
        // between 50ms publishes, so counted offsets drift past painted rows.
        let segment_len = self
            .capabilities
            .primary_screen_cursor_segment_len
            .unwrap_or(0);
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
    }

    pub fn set_primary_history_view(&mut self, active: bool) {
        let entering = active && !self.capabilities.primary_history_view;
        self.capabilities.primary_history_view = active;
        if active {
            self.grid.set_scroll_offset(0);
        } else {
            self.capabilities.primary_history_snapshot_at = None;
        }
        if entering && self.primary_screen_app_active() {
            self.snapshot_primary_screen_output();
            self.capabilities.primary_history_snapshot_at = Some(Instant::now());
            tracing::debug!(
                bytes = self
                    .block_tracker
                    .in_flight()
                    .map_or(0, |live| live.output.len()),
                "snapshotted primary-screen TUI for history browsing"
            );
        }
    }

    /// Coalesced by the app after it drains the current frame's PTY batches,
    /// then rate-limited here so a high-frequency TUI cannot rescan the capped
    /// document on every display frame.
    pub fn refresh_primary_history_snapshot(&mut self) -> bool {
        self.refresh_primary_history_snapshot_at(Instant::now())
    }

    /// v1.10.4: immediate snapshot refresh for keypress-driven redraws.
    ///
    /// Keystrokes are low-frequency (tens of ms apart at most) compared to
    /// the display-frame rate limit, and they drive the TUI's repaint — a
    /// selection change must show up on the next frame. Waiting out the 50ms
    /// window makes the browsing view lag a blink behind, which reads as a
    /// flicker: frame N shows the old selection, frame N+1 the new one.
    ///
    /// v1.10.4 (round 4): gate on screen-ownership instead of
    /// `primary_history_view`. Once a primary-screen TUI is screen-owned
    /// (`screen_document_start` set), `is_capturing()` returns false and the
    /// live block is ONLY updated through this snapshot — so a relative-only
    /// TUI (openclaw) kept in the BlockView needs the refresh even while
    /// following the live tail (history browsing off).
    ///
    /// v1.10.7: the v1.10.4 MEDIUM-1 absolute-addressing skip is REMOVED —
    /// the snapshot is the ONLY content source for screen-owned blocks, so
    /// it must refresh regardless of the transient addressing mode. The rate
    /// limit still bounds the rescan cost. (v1.11.8 M-C1: the
    /// `primary_screen_absolute_addressing` flag itself is gone — zero
    /// readers since v1.10.12; this doc keeps the skip's rationale record.)
    pub fn refresh_primary_history_snapshot_now(&mut self) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(Instant::now());
        true
    }

    pub(in crate::vt) fn refresh_primary_history_snapshot_at(&mut self, now: Instant) -> bool {
        if self.block_tracker.screen_document_start().is_none() {
            return false;
        }
        if self
            .capabilities
            .primary_history_snapshot_at
            .is_some_and(|previous| {
                now.saturating_duration_since(previous) < PRIMARY_HISTORY_SNAPSHOT_INTERVAL
            })
        {
            return false;
        }
        self.snapshot_primary_screen_output();
        self.capabilities.primary_history_snapshot_at = Some(now);
        true
    }

    pub(in crate::vt) fn snapshot_primary_screen_output(&mut self) {
        if let Some(capture) = &self.capabilities.primary_screen_interrupt_capture {
            let (text, styled) = merge_primary_screen_interrupt_tail(
                capture.frozen_text.clone(),
                capture.frozen_styled.clone(),
                &capture.tail,
            );
            let segment_len = text.len();
            let (text, styled) = self.compose_screen_history(text, styled);
            self.publish_screen_snapshot(text, styled, segment_len);
            // v1.10.26 review B1: the merged text replaced the in-flight
            // output — the stored segment length must describe THIS string,
            // or a mid-window keypress slices at a stale offset (a
            // non-char-boundary panic under CJK). The raw grid cursor row
            // anchors the caret here; the interrupt window is transient.
            self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
            return;
        }
        let Some(document_start) = self.block_tracker.screen_document_start() else {
            return;
        };
        // v1.11.12 (PLAN_v11112 M-A): per-refresh phase timings (document walk
        // / space-tail / compose / publish). The four Instant reads are
        // negligible; the SNAPSHOT_PHASES line itself (including the O(n)
        // line count) is only materialized when DEBUG is enabled, so
        // production logs stay silent and `--nocapture`/RUST_LOG=debug can
        // still attribute refresh cost. The aggregate feeds the .13
        // representation-layer decision (frozen head/tail split).
        let walk_start = Instant::now();
        let (text, styled, cursor_line) = self.primary_screen_document_snapshot(document_start);
        let after_walk = Instant::now();
        let (text, styled) = space_primary_screen_exit_tail(text, styled);
        let after_tail = Instant::now();
        // v1.10.23 (FIX_OMP_CONTENT_LOSS): prepend the preserved superseded
        // frames so the block transcript stays complete across full-frame
        // repaints; the cursor line shifts by the prepended history.
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): the scroll-out prefix joins
        // between the frames and the snapshot.
        let segment_len = text.len();
        let (text, styled) = self.compose_screen_history(text, styled);
        let after_compose = Instant::now();
        // C1 (PLAN_S2_render Stream C): the WARN-escalated probe line must
        // fire even under the app's default `info` file filter, so the gate
        // is `probe || debug-enabled` (pre-existing DEBUG-only behavior is
        // unchanged when the probe is off).
        let composed_lines = (perf_probe_enabled() || tracing::enabled!(tracing::Level::DEBUG))
            .then(|| text.lines().count());
        self.publish_screen_snapshot(text, styled, segment_len);
        let after_publish = Instant::now();
        if let Some(lines) = composed_lines {
            let us = |from: Instant, to: Instant| (to - from).as_micros() as u64;
            let ms = |from: Instant, to: Instant| (to - from).as_millis() as u64;
            if perf_probe_enabled() {
                // C1: probe on — WARN with a grep-stable SNAPSHOT_PHASES
                // prefix (same collection channel as RESIZE_PROBE) and
                // millisecond phase fields; lands in weft.log under the
                // default `info` filter that the DEBUG line never reached.
                tracing::warn!(
                    "SNAPSHOT_PHASES walk_ms={} tail_ms={} compose_ms={} publish_ms={} total_ms={} lines={}",
                    ms(walk_start, after_walk),
                    ms(after_walk, after_tail),
                    ms(after_tail, after_compose),
                    ms(after_compose, after_publish),
                    ms(walk_start, after_publish),
                    lines,
                );
            } else {
                // C1: probe off — the pre-existing DEBUG line, unchanged.
                tracing::debug!(
                    "SNAPSHOT_PHASES walk={} tail={} compose={} publish={} total={} lines={}",
                    us(walk_start, after_walk),
                    us(after_walk, after_tail),
                    us(after_tail, after_compose),
                    us(after_compose, after_publish),
                    us(walk_start, after_publish),
                    lines,
                );
            }
        }
        // v1.10.6/25/26 (FIX_IME_PREEDIT): store the caret snapshot line
        // AFTER publish from the composed text actually written
        // (`freeze::composed_cursor_snapshot_line`) — pre-split offsets or
        // recomputed head counts drift past the painted rows on splits.
        self.capabilities.primary_screen_cursor_snapshot_line =
            self.composed_cursor_snapshot_line(cursor_line, segment_len);
        self.capabilities.primary_screen_cursor_segment_len = Some(segment_len);
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): publish a composed screen
    /// snapshot to the in-flight block — or split it into 1MiB finished
    /// blocks plus an in-flight tail when the session history crossed
    /// `DEFAULT_OUTPUT_CAP` (long TUI sessions no longer truncate at the
    /// snapshot budget).
    fn publish_screen_snapshot(&mut self, text: String, styled: StyledOutput, segment_len: usize) {
        // B-class (PLAN_v11217 §3.5): "chunk granularity, not retention" —
        // this gate only selects between an equal-sized replace and the
        // split machinery; both arms preserve the bytes (the split settles
        // byte-seamless heads). The threshold stays the DEFAULT constant
        // even when `[blocks] output_cap_mib` is raised: the split boundary
        // is arithmetic granularity, not a retention bound.
        if text.len() > DEFAULT_OUTPUT_CAP {
            self.split_screen_history(text, styled, segment_len);
        } else {
            self.block_tracker.replace_screen_snapshot(&text, styled);
        }
    }
}

#[cfg(test)]
mod perf_probe_tests {
    use super::{env_flag_enabled, perf_probe_enabled, PERF_PROBE_ENV};
    use std::ffi::OsStr;

    /// C1: the weft_core-side probe flag shares the app-side
    /// (`performance_probe::flag_enabled`) exact-"1" semantics: `true`/`yes`
    /// deliberately do NOT enable, matching that channel's existing contract
    /// and test suite.
    #[test]
    fn env_flag_mirrors_app_side_one_only_semantics() {
        assert!(env_flag_enabled(Some(OsStr::new("1"))));
        assert!(!env_flag_enabled(Some(OsStr::new("0"))));
        assert!(!env_flag_enabled(Some(OsStr::new("true"))));
        assert!(!env_flag_enabled(Some(OsStr::new("yes"))));
        assert!(!env_flag_enabled(Some(OsStr::new(""))));
        assert!(!env_flag_enabled(None));
    }

    /// The cached flag must agree with a fresh read of the same env var —
    /// pins the [`OnceLock`] wiring against drifting from the env contract.
    /// (The cache itself is process-level by design: the probe is a
    /// launch-time switch, so runtime injection is neither possible nor
    /// wanted; the acceptance's on/off behavior is verified end-to-end by
    /// `docs/MEASURE_drag_instrumentation.md`'s grep steps.)
    #[test]
    fn cached_probe_flag_matches_fresh_env_read() {
        let fresh = env_flag_enabled(std::env::var_os(PERF_PROBE_ENV).as_deref());
        assert_eq!(perf_probe_enabled(), fresh);
    }

    /// The env name must stay byte-identical to the app-side probe so the
    /// single `WEFT_GUI_PERF_PROBE=1` switch drives both crates.
    #[test]
    fn trace_channels_switch_enables_without_the_probe() {
        // M4.1: the dedicated channel switch alone must enable the log; the
        // probe env still implies it (acceptance-gate back-compat).
        assert!(super::env_flag_enabled(Some(std::ffi::OsStr::new("1"))));
        assert!(super::perf_probe_enabled_impl(
            Some(std::ffi::OsStr::new("1")),
            None
        ));
        assert!(super::perf_probe_enabled_impl(
            None,
            Some(std::ffi::OsStr::new("1"))
        ));
        assert!(!super::perf_probe_enabled_impl(None, None));
        assert!(!super::perf_probe_enabled_impl(
            Some(std::ffi::OsStr::new("true")),
            None
        ));
    }

    #[test]
    fn env_name_matches_app_side_probe() {
        assert_eq!(PERF_PROBE_ENV, "WEFT_GUI_PERF_PROBE");
    }
}
