//! v1.13.13 test suites for the block-view warmup changes (PLAN_v1.13.13
//! FIX-C/FIX-D/FIX-E). Second `#[path]` companion of `atlas_warmup.rs` —
//! the first (`atlas_warmup_tests.rs`) sat just under the 800-line commit
//! gate, so the new suites get their own file instead of pushing it over.
//! Shares the fixtures/helpers with `super::tests`.

use std::cell::RefCell;

use super::tests::{fixture_block, headless_atlas_or_skip, tracker_with, warm_block};
use super::{BlockScanWatermarks, BLOCK_WARM_BUDGET_BYTES};
use crate::glyph::GlyphStyle;
use weft_core::blocks::{BlockTracker, CapturedStyle, StyledOutput};

// ── FIX-C: in-flight scan wins the budget ───────────────────────────────

/// FIX-C: a 1 MiB finished block AND an in-flight block with fresh rows in
/// the SAME frame — the live rows must be scanned FIRST (budget priority).
/// Under the old trailing order the finished block consumed the whole
/// 128 KiB budget before the in-flight scan ran, so the live row's unique
/// glyph ('锋') stayed missing for the block's ≤ 8 budget frames.
#[test]
fn in_flight_rows_scan_ahead_of_the_megabyte_finished_block() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping FIX-C test: no Metal device available");
        return;
    };
    // 1024 lines × 1024 bytes = 1 MiB finished block (8 full budget frames).
    let line = "x".repeat(1023) + "\n";
    let output = line.repeat(1024);
    let mut tracker = tracker_with(vec![fixture_block(1, "big", &output)]);
    tracker.on_command_start("live".into());
    for ch in "锋 live row\n".chars() {
        tracker.on_print(ch, CapturedStyle::default());
    }
    let wm = RefCell::new(BlockScanWatermarks::default());

    let scanned = warm_block(&tracker, &mut atlas, &wm, 0, None);
    assert!(scanned > 0);
    assert!(
        scanned as usize <= BLOCK_WARM_BUDGET_BYTES,
        "the combined frame must honor the budget ({scanned})"
    );
    // The order assertion: the live row's glyph landed in the atlas on the
    // FIRST frame even though a megabyte block was also selected.
    assert!(
        atlas.get_style('锋', GlyphStyle::REGULAR).is_some(),
        "in-flight rows must win the shared budget before the finished loop"
    );
}

// ── FIX-D: screen rewrite epoch ─────────────────────────────────────────

/// FIX-D ①: a `replace_screen_snapshot` rewrite (primary-screen TUI snapshot
/// path) replaces the live text wholesale — the in-flight watermark must
/// reset so the rewritten rows' new glyphs get scanned.
#[test]
fn screen_snapshot_rewrite_invalidates_the_live_watermark() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping FIX-D test: no Metal device available");
        return;
    };
    let mut tracker = BlockTracker::new();
    tracker.on_command_start("tui".into());
    tracker.begin_screen_owned_output(0);
    tracker.replace_screen_snapshot("frame one\n", StyledOutput::default());
    assert_eq!(
        tracker.screen_rewrite_epoch(),
        1,
        "first real rewrite bumps"
    );

    let wm = RefCell::new(BlockScanWatermarks::default());
    assert!(warm_block(&tracker, &mut atlas, &wm, 0, None) > 0);
    assert_eq!(
        warm_block(&tracker, &mut atlas, &wm, 0, None),
        0,
        "append-only steady state between rewrites"
    );

    // Rewrite with NEW content carrying a glyph the old frame never had.
    tracker.replace_screen_snapshot("frame one\né rewritten\n", StyledOutput::default());
    assert_eq!(tracker.screen_rewrite_epoch(), 2);
    assert!(
        warm_block(&tracker, &mut atlas, &wm, 0, None) > 0,
        "the epoch change must restart the live scan (watermark invalidation)"
    );
    assert!(
        atlas.get_style('é', GlyphStyle::REGULAR).is_some(),
        "a glyph unique to the rewritten rows must be scanned"
    );
    assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);
}

/// FIX-D ③: the content-equality short-circuit — an idle TUI re-publishing
/// the SAME document every 50ms must not bump the epoch (a bump would
/// restart the warm scan every frame, the exact regression P1-3 ruled out).
#[test]
fn equal_screen_snapshot_republish_does_not_restart_the_warm_scan() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping FIX-D test: no Metal device available");
        return;
    };
    let mut tracker = BlockTracker::new();
    tracker.on_command_start("tui".into());
    tracker.begin_screen_owned_output(0);
    tracker.replace_screen_snapshot("same frame\n", StyledOutput::default());
    let wm = RefCell::new(BlockScanWatermarks::default());
    assert!(warm_block(&tracker, &mut atlas, &wm, 0, None) > 0);
    let epoch = tracker.screen_rewrite_epoch();

    // Idle tick: identical text + colorless styled → short-circuit.
    tracker.replace_screen_snapshot("same frame\n", StyledOutput::default());
    assert_eq!(
        tracker.screen_rewrite_epoch(),
        epoch,
        "equal republish must not bump the epoch"
    );
    assert_eq!(
        warm_block(&tracker, &mut atlas, &wm, 0, None),
        0,
        "equal republish must not rescan"
    );
}

/// FIX-D ④: append-only streaming must leave the epoch (and thus the
/// watermark identity) alone — no rewrite, no reset. Device-free.
#[test]
fn plain_streaming_does_not_bump_the_screen_rewrite_epoch() {
    let mut tracker = BlockTracker::new();
    tracker.on_command_start("seq 3".into());
    for ch in "a\nb\n".chars() {
        tracker.on_print(ch, CapturedStyle::default());
    }
    tracker.on_newline();
    tracker.on_carriage_return();
    assert_eq!(
        tracker.screen_rewrite_epoch(),
        0,
        "print/newline/CR are not rewrites"
    );
    assert_eq!(tracker.command_generation(), 1);
}

// ── FIX-E: in-flight header label warm ──────────────────────────────────

/// FIX-E: the in-flight header renders `live_context_label` (abbreviated
/// cwd · git branch · elapsed). Its product charset must reach the atlas —
/// here the CJK cwd chars can ONLY come from the label (they never appear
/// in the command or the output lines).
#[test]
fn in_flight_header_label_glyphs_reach_the_atlas() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping FIX-E test: no Metal device available");
        return;
    };
    let mut tracker = BlockTracker::new();
    tracker.set_cwd(Some("/users/demo/中文目录".to_string()));
    tracker.on_command_start("ls".into());
    // One output line so the warm pass has scan bytes to report.
    for ch in "plain row\n".chars() {
        tracker.on_print(ch, CapturedStyle::default());
    }
    let wm = RefCell::new(BlockScanWatermarks::default());

    assert!(warm_block(&tracker, &mut atlas, &wm, 0, None) > 0);
    for ch in ['中', '文', '目', '录'] {
        assert!(
            atlas.get_style(ch, GlyphStyle::REGULAR).is_some(),
            "{ch} from the live_context_label product must be warmed"
        );
    }
}
