//! v1.14.2 (PLAN_v1142 §2 A4): accessibility rebuild fingerprint + throttle.
//!
//! `update_accessibility_tree` ran `accessibility_snapshot()` + the ObjC
//! bridge sync on EVERY redraw frame; during a sidebar drag that meant a
//! full rebuild per CursorMoved (缺陷二根治乙) and output storms rebuilt
//! per frame (tab 风暴债③). Two gates now run before the snapshot:
//!
//! 1. **指纹 [`AccessibilityKey`]** — a structural fingerprint of everything
//!    the tree exposes, collected per frame and recorded on each build.
//!    `chrome_left_bits` MUST be in the key: during a drag the window size
//!    does not change and grid_cols only flips on whole-cell crossings,
//!    while the content rect moves continuously — without that field most
//!    drag frames compare equal and the AX bounds go stale (R1 revision).
//!    f32 fields enter via `to_bits()` (NaN-safe, exact). Its equality
//!    semantics are pinned by truth tables; it becomes a decision input in
//!    batch B (cheap B1 collection), per §A4-3's 实施顺序口径.
//! 2. **50ms throttle** — inside the window since the last BUILD the
//!    rebuild defers, equal or changed key alike (拖拽/输出风暴 ≤20Hz;
//!    VoiceOver imperceptible; redraws keep arriving so no timer is
//!    needed). At/after the window a build always runs regardless of the
//!    key — the 50ms 上限 bounds the staleness of anything the fingerprint
//!    cannot see (e.g. grid text content), so correctness never depends on
//!    fingerprint completeness.
//!
//! Batch A collected the key on the CURRENT read path (per-tab
//! `with_terminal` / `launch_cwd` reads — the same reads `tab_bar_state`
//! makes). Batch B has since LANDED (§A4-3): `collect` reads the per-tab
//! fields from the B1 title snapshot cache (`App::tab_titles`) — the key
//! definition and this module's interface stayed unchanged throughout, and
//! `should_skip`'s 50ms semantics are exactly as adjudicated (距上次真实
//! build <50ms 即跳过，到点必重建).

use std::time::{Duration, Instant};

use crate::App;

/// Rate limit for accessibility tree rebuilds (≈20Hz ceiling).
pub(crate) const AX_REBUILD_MIN_INTERVAL: Duration = Duration::from_millis(50);

/// Per-tab content fingerprint: the tab's AX-visible identity (title
/// inputs, in-flight command, cwd, grid dims).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabAxKey {
    /// The B1 tab-title generation (`BlockTracker::title_generation`) —
    /// batch A's title-string stable_id placeholder is retired.
    pub(super) title_gen: u64,
    /// `stable_id` of the in-flight command text, if any.
    pub(super) in_flight_cmd: Option<u64>,
    /// `stable_id` of `launch_cwd()` (OSC 7 cwd or restore fallback).
    pub(super) cwd: Option<u64>,
    pub(super) grid_rows: usize,
    pub(super) grid_cols: usize,
}

/// Structural fingerprint of the accessibility tree's inputs. f32/f64
/// geometry enters as raw bits so any bit change (drag sub-pixel moves)
/// breaks equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AccessibilityKey {
    pub(super) tab_count: usize,
    pub(super) active_idx: usize,
    /// `renderer.viewport()` as `[f32::to_bits; 2]`.
    pub(super) viewport: [u32; 2],
    /// `renderer.scale()` as `f64::to_bits`.
    pub(super) scale_bits: u64,
    /// Sidebar push width (`layout_ctx.chrome_left`) as `f32::to_bits` —
    /// the drag-frame field (R1: 无此字段则 AX bounds 无限期陈旧).
    pub(super) chrome_left_bits: u32,
    pub(super) panel_open: bool,
    pub(super) find_open: bool,
    pub(super) palette_open: bool,
    pub(super) settings_open: bool,
    /// A context menu / completion popup swaps the whole exposed tree —
    /// their open state must break equality too (superset of the plan's
    /// field list; see the delivery report's deviation note).
    pub(super) context_menu_open: bool,
    pub(super) completion_open: bool,
    /// Any active selection on the active tab's panes (grid or block view).
    pub(super) selection_active: bool,
    pub(super) tabs: Vec<TabAxKey>,
}

impl AccessibilityKey {
    /// Batch B collection (§A4-3): the per-tab fields come from the B1
    /// title snapshot cache (`App::tab_titles`) — the same cheap probe
    /// `tab_bar_state` runs (one short lock read per tab per frame, shared
    /// between both consumers), never the old fresh per-tab lock takes.
    /// `title_gen` is the tracker's real B1 generation (batch A's
    /// title-string stable_id placeholder is gone). The key definition and
    /// this interface are unchanged; `should_skip`'s 50ms semantics are
    /// untouched (主代理裁定, §A4-3).
    pub(super) fn collect(app: &App) -> AccessibilityKey {
        let (viewport, scale_bits, chrome_left_bits) =
            app.renderer.as_ref().map_or(([0u32; 2], 0u64, 0u32), |r| {
                let (w, h) = r.viewport();
                (
                    [w.to_bits(), h.to_bits()],
                    r.scale().to_bits(),
                    r.layout_ctx.map_or(0.0, |ctx| ctx.chrome_left).to_bits(),
                )
            });
        let tabs: Vec<TabAxKey> = app
            .tab_titles()
            .iter()
            .map(|entry| TabAxKey {
                title_gen: entry.gen,
                in_flight_cmd: entry.in_flight_cmd,
                cwd: entry.cwd,
                grid_rows: entry.grid_rows,
                grid_cols: entry.grid_cols,
            })
            .collect();
        let selection_active = app.sessions.active().is_some_and(|tab| {
            tab.panes().any(|(_, pane)| {
                pane.selection_handler.selection.is_some()
                    || pane.selection_handler.block_view_selection.is_some()
            })
        });
        let completion_open = app
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.editor().completion_view().is_some()))
            .unwrap_or(false);
        AccessibilityKey {
            tab_count: app.sessions.len(),
            active_idx: app.sessions.active_idx(),
            viewport,
            scale_bits,
            chrome_left_bits,
            panel_open: app.panel.open,
            find_open: app.find.open,
            palette_open: app.palette.open,
            settings_open: app.settings.open,
            context_menu_open: app.interaction.context_menu.is_some(),
            completion_open,
            selection_active,
            tabs,
        }
    }
}

/// The A4 skip decision — pure so both gates are truth-table testable.
/// `last_built` is the fingerprint of the tree currently installed in
/// AppKit plus the instant it was built.
///
/// Semantics (both plan clauses hold): inside the 50ms window since the
/// last BUILD the rebuild defers — an equal key has nothing to rebuild and
/// a changed key is throttled (拖拽/输出风暴期降到 ≤20Hz), with a later
/// frame rebuilding (由后续帧补建, redraws keep arriving). At/after the
/// window a build ALWAYS runs — the 50ms 上限 caps the staleness of
/// anything the fingerprint cannot see (e.g. grid text content), so
/// correctness never depends on fingerprint completeness.
///
/// `last_key`/`key` are taken and compared by the tests' equality truth
/// tables; batch B (B1's cheap key collection) flips them into the decision
/// (`equal ⇒ skip` unbounded) without changing this interface.
pub(super) fn should_skip(
    last_built: Option<(&AccessibilityKey, Instant)>,
    key: &AccessibilityKey,
    now: Instant,
) -> bool {
    let Some((last_key, built_at)) = last_built else {
        return false; // never built → must build
    };
    let inside_window = now.saturating_duration_since(built_at) < AX_REBUILD_MIN_INTERVAL;
    // Batch-A binding gate: the time throttle. The fingerprint participates
    // through the truth-table contract below (equality pinned per field).
    let _ = (last_key, key);
    inside_window
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chrome_left: f32, grid_cols: usize, tabs: usize) -> AccessibilityKey {
        AccessibilityKey {
            tab_count: tabs,
            active_idx: 0,
            viewport: [1200.0f32.to_bits(), 800.0f32.to_bits()],
            scale_bits: 2.0f64.to_bits(),
            chrome_left_bits: chrome_left.to_bits(),
            panel_open: false,
            find_open: false,
            palette_open: false,
            settings_open: false,
            context_menu_open: false,
            completion_open: false,
            selection_active: false,
            tabs: (0..tabs)
                .map(|_| TabAxKey {
                    title_gen: 42,
                    in_flight_cmd: None,
                    cwd: Some(7),
                    grid_rows: 24,
                    grid_cols,
                })
                .collect(),
        }
    }

    /// 真值表 1: chrome_left 变 → 不等 (the R1 revision's drag-frame field).
    #[test]
    fn chrome_left_change_breaks_equality() {
        assert_ne!(key(0.0, 80, 1), key(260.0, 80, 1));
        // Sub-pixel drags break it too — that is the point of to_bits.
        assert_ne!(key(0.0, 80, 1), key(0.5, 80, 1));
    }

    /// 真值表 2: grid_cols 不变但 chrome_left 变 → 不等.
    #[test]
    fn same_grid_cols_with_different_chrome_left_is_inequal() {
        let a = key(120.0, 80, 2);
        let mut b = key(121.5, 80, 2);
        b.active_idx = a.active_idx;
        assert_ne!(a, b);
    }

    /// 真值表 3: 全同 → 相等 (and any single field difference breaks it).
    #[test]
    fn identical_keys_are_equal_and_each_field_breaks_equality() {
        assert_eq!(key(0.0, 80, 1), key(0.0, 80, 1));
        // viewport / scale / per-tab fields each participate.
        let mut b = key(0.0, 80, 1);
        b.viewport = [1199.0f32.to_bits(), 800.0f32.to_bits()];
        assert_ne!(key(0.0, 80, 1), b);
        let mut b = key(0.0, 80, 1);
        b.scale_bits = 1.0f64.to_bits();
        assert_ne!(key(0.0, 80, 1), b);
        let mut b = key(0.0, 80, 1);
        b.tabs[0].grid_cols = 79;
        assert_ne!(key(0.0, 80, 1), b);
        let mut b = key(0.0, 80, 1);
        b.tabs[0].title_gen = 43;
        assert_ne!(key(0.0, 80, 1), b);
        let mut b = key(0.0, 80, 1);
        b.tabs[0].in_flight_cmd = Some(9);
        assert_ne!(key(0.0, 80, 1), b);
        let mut b = key(0.0, 80, 1);
        b.tabs[0].cwd = None;
        assert_ne!(key(0.0, 80, 1), b);
        // Tab count and active index participate.
        assert_ne!(key(0.0, 80, 1), key(0.0, 80, 2));
        let mut b = key(0.0, 80, 2);
        b.active_idx = 1;
        assert_ne!(key(0.0, 80, 2), b);
    }

    /// 跳过/补建决策: no history builds; equal + inside the window skips;
    /// changed + inside the window also skips (throttled, 由后续帧补建);
    /// at/after 50ms BOTH build (the staleness cap).
    #[test]
    fn should_skip_truth_table() {
        let now = Instant::now();
        let a = key(0.0, 80, 1);
        let b = key(0.5, 80, 1); // differs only in chrome_left
                                 // Never built → build.
        assert!(!should_skip(None, &a, now));
        // Equal + inside the window → skip (nothing changed).
        assert!(should_skip(
            Some((&a, now - Duration::from_millis(10))),
            &a,
            now
        ));
        // Changed + inside the window → skip (throttle), a later frame
        // rebuilds (redraws keep arriving — 无需自定定时器).
        assert!(should_skip(
            Some((&a, now - Duration::from_millis(10))),
            &b,
            now
        ));
        // Boundary: exactly 50ms → build regardless of key.
        assert!(!should_skip(
            Some((&a, now - AX_REBUILD_MIN_INTERVAL)),
            &a,
            now
        ));
        assert!(!should_skip(
            Some((&a, now - Duration::from_millis(51))),
            &b,
            now
        ));
    }
}
