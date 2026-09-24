// v1.11.8 snapshot-equality tests (split from style.rs to keep it within its
// architecture-gate budget; includes the PLAN_v11217 §3.5 T4 P0 bullseye).

use super::*;
use crate::blocks::DEFAULT_OUTPUT_CAP;

// ── v1.11.8 (PLAN_v1118 M-A): replace_screen_snapshot content-equality
// short-circuit. The predicate matrix:
//   (text same, styled same)            → skip (no bump, keep Arc)
//   (text same, styled diff)            → bump  (colored→colored, colored→None)
//   (text diff, styled same)            → bump  (P0-1: styled kept when colored)
//   (both none / double-none equal)     → skip
// Every cell below has a version assertion AND (where applicable) an
// Arc-identity assertion — the short-circuit's whole point is that the
// renderer's styled_line_cache stays hot via Arc::ptr_eq (F4).

fn screen_owned_tracker() -> BlockTracker {
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("screen-app".to_string());
    tracker.begin_screen_owned_output(0);
    tracker
}

fn colored_styled() -> StyledOutput {
    StyledOutput {
        lines: vec![StyledLine {
            line: 0,
            foregrounds: vec![ForegroundSpan {
                start: 0,
                end: 2,
                color: CellColor::Palette(2),
            }],
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: Vec::new(),
            underline_colors: Vec::new(),
        }],
    }
}

/// (text same, styled same) → the whole publish is skipped: version must
/// NOT bump and the stored Arc must be the SAME allocation (ptr_eq), so
/// the live-cache vertex memo stays valid.
#[test]
fn equal_resnapshot_skips_version_bump_and_keeps_arc() {
    let mut tracker = screen_owned_tracker();
    let styled = colored_styled();
    tracker.replace_screen_snapshot("ab", styled.clone());
    let version = tracker.live_output_version;
    let arc = tracker.styled_output.clone().expect("styled published");
    tracker.replace_screen_snapshot("ab", styled);
    assert_eq!(
        tracker.live_output_version, version,
        "equal snapshot must not churn the version"
    );
    assert!(
        Arc::ptr_eq(&arc, tracker.styled_output.as_ref().expect("styled kept")),
        "equal snapshot must keep the old Arc allocation"
    );
    assert_eq!(tracker.output.as_str(), "ab");
}

/// (text same, styled DIFFERS: added a background span) → full path:
/// version bumps and the Arc is replaced.
#[test]
fn styled_change_bumps_and_replaces_arc() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", colored_styled());
    let version = tracker.live_output_version;
    let old_arc = tracker.styled_output.clone().unwrap();
    let richer = StyledOutput {
        lines: vec![StyledLine {
            line: 0,
            foregrounds: vec![ForegroundSpan {
                start: 0,
                end: 2,
                color: CellColor::Palette(2),
            }],
            backgrounds: vec![ForegroundSpan {
                start: 0,
                end: 2,
                color: CellColor::Palette(6),
            }],
            links: Vec::new(),
            attributes: Vec::new(),
            underline_colors: Vec::new(),
        }],
    };
    tracker.replace_screen_snapshot("ab", richer);
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "styled change must bump"
    );
    let kept = tracker.styled_output.as_ref().expect("styled kept");
    assert!(
        !Arc::ptr_eq(&old_arc, kept),
        "styled change must swap the Arc"
    );
}

/// Double-none equal republish (old None + new colorless) → short-circuit:
/// no bump.
#[test]
fn colorless_equal_republish_skips_bump() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", StyledOutput::default());
    assert!(tracker.styled_output.is_none());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("ab", StyledOutput::default());
    assert_eq!(
        tracker.live_output_version, version,
        "double-none equal republish must short-circuit"
    );
    assert!(tracker.styled_output.is_none());
}

/// (old None + new colored) → full path: must bump and install the Arc
/// (the double-none branch must NOT swallow it).
#[test]
fn colorless_to_colored_bumps() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", StyledOutput::default());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("ab", colored_styled());
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "colorless→colored must bump"
    );
    assert!(
        tracker.styled_output.is_some(),
        "colorless→colored must install the styled Arc"
    );
}

/// (old colored + new colorless, same text) → NOT equal (the predicate's
/// second branch only covers double-none) → full path clears the Arc.
#[test]
fn colored_to_colorless_clears_and_bumps() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", colored_styled());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("ab", StyledOutput::default());
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "colored→colorless must bump"
    );
    assert!(
        tracker.styled_output.is_none(),
        "colored→colorless must drop the Arc"
    );
}

/// P0-1 core cell (the regression lock): text CHANGED + colored → the
/// styled Arc is KEPT. The styled gate on the full path is the
/// POST-replace truncation comparison, NOT the pre-replace
/// `text_unchanged` — swapping in `text_unchanged` here drops colors on
/// the most common cell (text change + colored).
#[test]
fn text_change_keeps_colors() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", colored_styled());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("abc", colored_styled());
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "text change must bump"
    );
    assert!(
        tracker.styled_output.is_some(),
        "text change + colored must keep the styled Arc (P0-1)"
    );
}

/// Current-behavior lock: text changed + colorless → the styled Arc is
/// dropped (has_colors() gate) and the version bumps.
#[test]
fn text_change_colorless_clears_styles() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", colored_styled());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("abc", StyledOutput::default());
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "text change must bump"
    );
    assert!(
        tracker.styled_output.is_none(),
        "text change + colorless must clear the styled Arc"
    );
}

/// The truncation-guard semantics: a snapshot beyond DEFAULT_OUTPUT_CAP
/// truncates the stored text, so the post-replace comparison fails and
/// the styled Arc must be dropped even though the snapshot was colored.
/// (The 1MiB publish split lives upstream at `publish_screen_snapshot`;
/// this is the guard that must never be weakened to a pre-replace
/// `text_unchanged` test.)
#[test]
fn oversize_text_drops_styles_via_truncation_guard() {
    let mut tracker = screen_owned_tracker();
    let big = "x".repeat(DEFAULT_OUTPUT_CAP + 100);
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot(&big, colored_styled());
    assert_eq!(
        tracker.live_output_version,
        version.wrapping_add(1),
        "an oversize publish is never equal — it must bump like today"
    );
    assert!(
        tracker.styled_output.is_none(),
        "truncated text must not keep styles"
    );
    assert!(
        tracker.output.as_str().len() <= DEFAULT_OUTPUT_CAP,
        "output must be truncated to the budget"
    );
}

// ── PLAN_v11217 §3.5 (T4): the P0 bullseye ──────────────────────────

/// cap=2 MiB tracker: a ~2 MiB in-flight screen-owned block must survive
/// a boundary rewrite (`replace_screen_snapshot` / `replace_screen_output`)
/// WITHOUT being re-truncated back to the historical 1 MiB constant. With
/// a constant at the replace sites this was the silent P0 regression the
/// T4 review flagged (cap raised → first rewrite drops half the block).
#[test]
fn raised_cap_in_flight_block_survives_boundary_rewrite() {
    let mib = 1024 * 1024;
    let mut tracker = screen_owned_tracker();
    tracker.set_output_cap(2 * mib);

    // ~2 MiB in-flight publish at the configured cap: kept in full.
    let big = "x".repeat(2 * mib - 8);
    tracker.replace_screen_output(&big);
    assert_eq!(
        tracker.output.as_str().len(),
        2 * mib - 8,
        "a publish within the configured cap must keep every byte"
    );

    // A boundary rewrite (content change + colored styles — the full
    // path) must NOT re-truncate the retained text to 1 MiB.
    let mut grown = big.clone();
    grown.push_str("tail");
    tracker.replace_screen_snapshot(&grown, colored_styled());
    assert_eq!(
        tracker.output.as_str().len(),
        grown.len(),
        "boundary rewrite must not re-truncate the in-flight block to 1 MiB"
    );
    assert!(
        tracker.output.as_str().len() > crate::blocks::DEFAULT_OUTPUT_CAP,
        "the retained text must exceed the old constant (the P0 re-truncation shape)"
    );
    assert!(tracker.styled_output.is_some(), "within cap → styles kept");

    // Beyond the configured cap the truncation boundary sits at the NEW
    // cap (not the historical 1 MiB constant).
    let oversized = "y".repeat(2 * mib + 100);
    tracker.replace_screen_output(&oversized);
    assert_eq!(
        tracker.output.as_str().len(),
        2 * mib,
        "the truncation boundary must move with the configured cap"
    );
}

/// v1.11.8 interop case: `replace_screen_output` (unconditional clear +
/// bump, no styled arg) then an equal COLORLESS snapshot lands on the
/// double-none short-circuit — the unconditionally-cleared state must
/// participate in the skip, not force an extra bump.
#[test]
fn replace_screen_output_then_equal_colorless_republish_skips() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("ab", colored_styled());
    tracker.replace_screen_output("ab");
    assert!(tracker.styled_output.is_none());
    let version = tracker.live_output_version;
    tracker.replace_screen_snapshot("ab", StyledOutput::default());
    assert_eq!(
        tracker.live_output_version, version,
        "post-replace_screen_output double-none equal republish must short-circuit"
    );
    assert!(tracker.styled_output.is_none());
}

/// Settle chain (defer → equal republish → finish): the equal republish
/// inside the settle window still short-circuits, and the finalized
/// block carries the exact content + styles (settle is independent of the
/// replace layer — F9).
#[test]
fn settle_chain_keeps_content_and_short_circuits_equal_republish() {
    let mut tracker = screen_owned_tracker();
    tracker.replace_screen_snapshot("final\nframe", colored_styled());
    let version = tracker.live_output_version;
    tracker.defer_screen_command_end();
    tracker.replace_screen_snapshot("final\nframe", colored_styled());
    assert_eq!(
        tracker.live_output_version, version,
        "equal republish inside the settle window must short-circuit"
    );
    tracker.finish_deferred_screen_command(Some(0));
    assert_eq!(tracker.blocks().len(), 1);
    assert_eq!(tracker.blocks()[0].output.as_ref(), "final\nframe");
    assert!(
        tracker.blocks()[0].styled_output.is_some(),
        "settled block keeps the styled snapshot"
    );
}
