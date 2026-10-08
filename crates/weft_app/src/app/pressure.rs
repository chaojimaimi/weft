//! Memory-pressure response (T16d, PLAN_v11217 §3.11).
//!
//! One 1 Hz O(1) check (rides the `TabsAutoSave` tick, T14 pattern) reads
//! `os_proc_available_memory()` and — past the dual thresholds (<512 MiB =
//! warning, <256 MiB = critical) and a 60 s debounce — releases the render
//! caches that have existing release paths:
//!
//! - `background_block_caches` (T16b map) — `HashMap::clear`, the same
//!   wholesale clear the theme/font hooks run (`renderer/runtime.rs`);
//!   rebuilt from scratch on the next background frame.
//! - `background_grid_row_caches` (B3-2) — `HashMap::clear`; a fresh
//!   `or_default()` entry has `dims (0,0)`, which fails the fingerprint and
//!   forces a full row rebuild (`paint/grid.rs`), so clearing is safe.
//! - `styled_line_cache` — production `bump_generation()` (drops every
//!   entry; old keys can never hit again, lines rebuild+reinsert as painted
//!   — the exact semantics a theme change already pays).
//! - active `grid_row_cache` — full-force path (`force_full_grid_redraw`),
//!   rows rebuild in place next frame (≈1 MiB scale; no release API exists
//!   for the Vec's outer buffer and forcing is the plan's reusable path).
//!
//! **Inventory items deliberately NOT touched (plan's report-to-coordinator
//! clause — no new APIs designed):**
//! - `block_layout_cache` (≤256 MiB self-degrading budget, M6-c): its only
//!   per-entry release, `invalidate`, is `#[cfg(test)]` (grid_cache.rs), and
//!   invalidate-all would drain `dirty_ids` into a one-frame Both-rebuild of
//!   EVERY block (a multi-second stall on large histories). The cheap
//!   mechanism (`degrade_entry`/`enforce_table_budget`) is private. Releasing
//!   it needs an API-surface decision (un-gate `invalidate` vs. expose a
//!   degrade-all wrapper) — deferred to the coordinator.
//! - `GlyphAtlas`: single fixed texture, no paging — no "spare page" exists.
//! - `offscreen_texture` (GPU): recreated on resize only; no release API.
//! - `live_layout_cache` / `block_scan_watermarks`: bounded-byte structures
//!   without release APIs worth the churn.
//!
//! Static attribution for the "183-205 MB after load doesn't fall back"
//! observation (GUI footprint/vmmap capture is left to the acceptance
//! checklist): retained block text (`output: Arc<str>`, 1 MiB cap × 2000
//! retained blocks per pane) + scrollback are DATA, and macOS allocator
//! retention keeps freed pages resident — the releasable render caches above
//! are the minority outside pathological giant-block histories.

use std::time::{Duration, Instant};

use tracing::info;

use crate::renderer::MetalRenderer;

/// Available memory below which a warning-level response fires.
pub(crate) const PRESSURE_WARNING_BYTES: u64 = 512 * 1024 * 1024;
/// Available memory below which a critical-level response fires.
pub(crate) const PRESSURE_CRITICAL_BYTES: u64 = 256 * 1024 * 1024;
/// One response per window — a pressure storm must not thrash caches that
/// were just released (unified across warning/critical; the truth table pins
/// that a critical arriving inside the window waits like a warning does).
pub(crate) const PRESSURE_DEBOUNCE: Duration = Duration::from_secs(60);

// FFI (`<os/proc.h>`): public libSystem symbol since macOS 10.15 — zero new
// link-time dependencies (the `frame_trace.rs` getrusage extern precedent).
// `libc` is not a direct dependency, hence the manual declaration. A `0`
// return means the API failed/unavailable; the pure layer maps 0 to Nominal
// so a broken reading never triggers releases.
extern "C" {
    fn os_proc_available_memory() -> u64;
}

/// Safe wrapper for the tick and the link smoke test.
pub(crate) fn available_memory_bytes() -> u64 {
    // SAFETY: plain C function — no preconditions, no retained pointers.
    unsafe { os_proc_available_memory() }
}

/// Severity of a pressure reading. `Nominal` also covers the "unknown" (0)
/// reading so the FFI failure mode is a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PressureLevel {
    Nominal,
    Warning,
    Critical,
}

/// Pure threshold judgment (truth-tabled below). Strictly-less semantics:
/// exactly 256 MiB is Warning, exactly 512 MiB is Nominal.
pub(crate) fn pressure_level(available_bytes: u64) -> PressureLevel {
    if available_bytes == 0 {
        return PressureLevel::Nominal; // API unavailable — never fire
    }
    if available_bytes < PRESSURE_CRITICAL_BYTES {
        PressureLevel::Critical
    } else if available_bytes < PRESSURE_WARNING_BYTES {
        PressureLevel::Warning
    } else {
        PressureLevel::Nominal
    }
}

/// Pure fire decision (truth-tabled below): severity plus the unified 60 s
/// debounce. `None` = stay quiet this tick.
pub(crate) fn pressure_response(
    available_bytes: u64,
    last_response: Option<Instant>,
    now: Instant,
) -> Option<PressureLevel> {
    let level = pressure_level(available_bytes);
    if level == PressureLevel::Nominal {
        return None;
    }
    if last_response.is_some_and(|last| now.duration_since(last) < PRESSURE_DEBOUNCE) {
        return None;
    }
    Some(level)
}

/// What the response released — the info! log payload and the test oracle.
/// The styled cache reports BYTES only: its `len()` is `#[cfg(test)]` (the
/// only production gauge is `bytes()` — no new API is designed for this).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReleaseReport {
    pub(crate) background_block_caches: usize,
    pub(crate) background_grid_row_caches: usize,
    pub(crate) styled_line_bytes: usize,
}

/// Release every render cache that has an existing release path (see module
/// docs for the inventory and the deliberate exclusions). Each clear is
/// scoped so no RefCell borrow is alive when the force flag is set.
pub(crate) fn release_render_caches(renderer: &MetalRenderer) -> ReleaseReport {
    let background_block_caches = {
        let mut caches = renderer.background_block_caches.borrow_mut();
        let count = caches.len();
        caches.clear();
        count
    };
    let background_grid_row_caches = {
        let mut caches = renderer.background_grid_row_caches.borrow_mut();
        let count = caches.len();
        caches.clear();
        count
    };
    let styled_line_bytes = renderer.styled_line_cache.borrow().bytes();
    renderer.styled_line_cache.borrow_mut().bump_generation();
    // Active per-row grid cache: full force (rows rebuild in place next
    // frame) — the plan's reusable path; no separate release API exists.
    renderer.force_full_grid_redraw();
    ReleaseReport {
        background_block_caches,
        background_grid_row_caches,
        styled_line_bytes,
    }
}

impl crate::App {
    /// T16d: the O(1) pressure check on the 1 Hz autosave tick (T14's
    /// same-domain append). No renderer (startup/teardown transient) →
    /// nothing to release and the debounce anchor stays unarmed, so the
    /// first real observation after renderer creation can still fire.
    pub(crate) fn maybe_run_memory_pressure_tick(&mut self) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let available = available_memory_bytes();
        let now = Instant::now();
        let Some(level) = pressure_response(available, self.last_memory_pressure_response, now)
        else {
            return;
        };
        self.last_memory_pressure_response = Some(now);
        let report = release_render_caches(renderer);
        info!(
            level = ?level,
            available_mb = available / (1024 * 1024),
            bg_block_cache_panes = report.background_block_caches,
            bg_grid_row_cache_panes = report.background_grid_row_caches,
            styled_line_bytes = report.styled_line_bytes,
            grid_row_force = true,
            critical_redraw = level == PressureLevel::Critical,
            "memory pressure response: render caches released"
        );
        if level == PressureLevel::Critical {
            // Critical adds the full-redraw REQUEST so the released state is
            // rebuilt promptly even if the loop was idle (warning level lets
            // the armed force flag ride the next natural frame).
            self.request_redraw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── pure: threshold truth table ─────────────────────────────────────

    #[test]
    fn pressure_level_truth_table() {
        // 0 = API unavailable → Nominal (never fire on missing data).
        assert_eq!(pressure_level(0), PressureLevel::Nominal);
        // Critical band: (0, 256 MiB) exclusive.
        assert_eq!(pressure_level(1), PressureLevel::Critical);
        assert_eq!(
            pressure_level(PRESSURE_CRITICAL_BYTES - 1),
            PressureLevel::Critical
        );
        // Boundary: exactly 256 MiB is Warning (strictly-less thresholds).
        assert_eq!(
            pressure_level(PRESSURE_CRITICAL_BYTES),
            PressureLevel::Warning
        );
        assert_eq!(
            pressure_level(PRESSURE_WARNING_BYTES - 1),
            PressureLevel::Warning
        );
        // Boundary: exactly 512 MiB is Nominal.
        assert_eq!(
            pressure_level(PRESSURE_WARNING_BYTES),
            PressureLevel::Nominal
        );
        assert_eq!(pressure_level(u64::MAX), PressureLevel::Nominal);
    }

    // ── pure: fire decision + debounce truth table ──────────────────────

    #[test]
    fn pressure_response_truth_table() {
        let now = Instant::now();
        let nominal = PRESSURE_WARNING_BYTES;
        let warning = PRESSURE_WARNING_BYTES - 1;
        let critical = PRESSURE_CRITICAL_BYTES - 1;

        // Nominal never responds, with or without an anchor.
        assert_eq!(pressure_response(nominal, None, now), None);
        assert_eq!(pressure_response(nominal, Some(now), now), None);

        // First observation fires at each level.
        assert_eq!(
            pressure_response(warning, None, now),
            Some(PressureLevel::Warning)
        );
        assert_eq!(
            pressure_response(critical, None, now),
            Some(PressureLevel::Critical)
        );

        // Inside the debounce window → quiet (unified across levels: a
        // critical arriving 59 s after a warning response waits too — the
        // caches were just released, re-clearing is a no-op storm guard).
        let recent = now - Duration::from_secs(59);
        assert_eq!(pressure_response(warning, Some(recent), now), None);
        assert_eq!(pressure_response(critical, Some(recent), now), None);

        // At the window edge (elapsed == debounce) → fires again.
        let stale = now - PRESSURE_DEBOUNCE;
        assert_eq!(
            pressure_response(warning, Some(stale), now),
            Some(PressureLevel::Warning)
        );
    }

    // ── FFI link smoke test: linking the libSystem symbol IS the
    //    assertion (compilation pins it); on macOS ≥ 10.15 the call
    //    returns the real available-bytes figure. ────────────────────────

    #[test]
    fn available_memory_call_links_and_runs() {
        let _bytes = available_memory_bytes();
    }

    // ── release orchestration: seed real cache structures, assert the
    //    release clears each and arms the row-level force ────────────────

    /// Mirror the golden/bg-cache skip precedent: no Metal device (CI
    /// without GPU) skips instead of failing.
    fn headless_renderer_or_skip() -> Option<crate::renderer::MetalRenderer> {
        metal::Device::system_default()?;
        Some(crate::renderer::MetalRenderer::new_headless_paint(
            weft_core::config::Theme::weft_warm(),
        ))
    }

    fn test_fingerprint() -> crate::renderer::bg_block_cache::BgBlockFingerprint {
        crate::renderer::bg_block_cache::BgBlockFingerprint {
            live_output_version: 1,
            screen_head_lines: 0,
            cwd: None,
            git_branch: None,
            theme_generation: 1,
            cell_dims: (8, 16),
            rect: [0.0; 4],
            block_scroll: 0.0,
            editor_mode: false,
        }
    }

    fn test_styled_key() -> crate::paint::styled_line_cache::StyledLineCacheKey {
        crate::paint::styled_line_cache::StyledLineCacheKey {
            pane_session_id: 1,
            block_id: 1,
            line_idx: 0,
            chunk_idx: 0,
            cols: 80,
            char_offset: 0,
            render_generation: 1,
            palette_fingerprint: 1,
            fallback_fg: [0; 4],
            canvas: [0; 4],
        }
    }

    #[test]
    fn release_render_caches_clears_all_seeded_caches() {
        let Some(renderer) = headless_renderer_or_skip() else {
            eprintln!("skipping pressure release test: no Metal device available");
            return;
        };

        // Seed one entry per releasable structure (direct pub(crate) field
        // access — the same surface the theme/font hooks use).
        renderer.background_block_caches.borrow_mut().insert(
            11,
            crate::renderer::bg_block_cache::BgBlockCache {
                vertices: vec![0.0; 32],
                fingerprint: test_fingerprint(),
                last_rebuild: Instant::now(),
            },
        );
        renderer
            .background_grid_row_caches
            .borrow_mut()
            .insert(12, crate::paint::grid::BackgroundGridRowCache::default());
        renderer.styled_line_cache.borrow_mut().insert(
            test_styled_key(),
            Arc::from("seed"),
            None,
            vec![0.0; 8],
        );
        assert_eq!(renderer.background_block_caches.borrow().len(), 1);
        assert_eq!(renderer.background_grid_row_caches.borrow().len(), 1);
        assert_eq!(renderer.styled_line_cache.borrow().len(), 1);

        let report = release_render_caches(&renderer);

        // The report reflects what each release call removed…
        assert_eq!(report.background_block_caches, 1);
        assert_eq!(report.background_grid_row_caches, 1);
        assert!(report.styled_line_bytes > 0, "seeded entry must have bytes");
        // …and every structure is actually empty afterwards.
        assert!(renderer.background_block_caches.borrow().is_empty());
        assert!(renderer.background_grid_row_caches.borrow().is_empty());
        assert_eq!(renderer.styled_line_cache.borrow().len(), 0);
        // Row-level force armed for the next frame (and the draw() skip
        // flag flipped so the forced rebuild is actually uploaded).
        assert!(renderer.force_full_grid.get());
        assert!(!renderer.instances_unchanged.get());
    }
}
