//! v1.13.11 test suites for the block-view warmup (PLAN_v1.13.11 测试计划
//! 1/2/3/4-机制/6/8). Companion file of `atlas_warmup.rs` via `#[path]` —
//! the main file sits close to the 800-line commit gate.
//!
//! Suite map (each rewritten B3-3 test keeps its invariant under the new
//! once-ever semantics, per N1.6):
//! - `frames_resume_from_the_watermark`      ← non_live/live pair (append-only)
//! - `watermark_is_monotonic_and_namespaced` ← same
//! - `scan_range_has_no_per_block_line_cap`  ← scan_range_clamps (cap removed)
//! - `identity_mismatch_invalidates_*`       ← NEW (continuation merge, 测试 3)
//! - `in_flight_sentinel_is_forgotten_*`     ← same
//! - `atlas_rebuild_clears_all_watermarks_*` ← same (+ line counts)
//! - `block_warm_window_*`                   ← NEW (superset, 测试 6)
//! - device-gated mechanism tests            ← NEW (测试 1/2/3/4-机制/6)
//!
//! v1.13.13: the FIX-C/D/E suites live in the sibling companion
//! `atlas_warmup_v11313_tests.rs` (this file sat just under the 800-line
//! commit gate; helpers below are `pub(crate)` for that sibling).
//!
//! 测试 0 conclusion (PLAN_v1.13.11 前置核查): the goldens
//! (`paint/block_view/golden_tests.rs`) are vertex-BYTE baselines whose
//! scenario literals are ASCII-only — they never produce color-atlas pixels
//! (their "color" is SGR palette bits), and they already device-gate via
//! `require_metal_or_skip`. This machine CAN verify the color-atlas path
//! headless (the cluster mechanism tests below + `style.rs`'s
//! `info_cluster_*` assert the `fg.a=2.0` sentinel through a real
//! rasterized ℹ️ cluster), so no golden changes: coverage responsibility
//! stays with the device-gated mechanism tests + user acceptance.

use super::{block_warm_window, BlockScanWatermarks, MetalRenderer, BLOCK_WARM_BUDGET_BYTES};
use crate::glyph::GlyphAtlas;
use crate::paint::grid_cache::BlockLayoutCache;
use crate::paint::live_cache::LiveLayoutCache;
use std::cell::RefCell;
use std::time::{Duration, SystemTime};
use weft_core::blocks::{Block, BlockId, BlockTracker, CapturedStyle};

// ── fixtures ────────────────────────────────────────────────────────────

pub(crate) fn fixture_block(id: u64, command: &str, output: &str) -> Block {
    let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: std::sync::Arc::from(output),
        styled_output: None,
        exit_code: Some(0),
        started_at: started,
        finished_at: Some(started + Duration::from_millis(1_200)),
        collapsed: false,
        screen_origin: false,
    }
}

pub(crate) fn tracker_with(blocks: Vec<Block>) -> BlockTracker {
    let mut tracker = BlockTracker::new();
    tracker.load_blocks(blocks);
    tracker
}

/// Mirror the golden skip precedent: no Metal device (CI without GPU)
/// skips instead of failing (the atlas rasterizer needs a device).
pub(crate) fn headless_atlas_or_skip() -> Option<GlyphAtlas> {
    let device = metal::Device::system_default()?;
    Some(GlyphAtlas::new(
        &device,
        &weft_core::config::FontConfig::default(),
        2.0,
    ))
}

/// Warm ONLY the block-view scan (all cache/window inputs at their defaults:
/// an empty layout cache forces the full-scan fallback, the live cache is
/// unowned). Returns the scanned output bytes.
pub(crate) fn warm_block(
    tracker: &BlockTracker,
    atlas: &mut GlyphAtlas,
    watermarks: &RefCell<BlockScanWatermarks>,
    namespace: u64,
    visible: Option<&[BlockId]>,
) -> u64 {
    MetalRenderer::warm_block_view_glyphs(
        atlas,
        tracker,
        true,
        namespace,
        visible,
        None,
        None,
        600.0,
        0.0,
        24,
        2.0,
        &BlockLayoutCache::default(),
        &LiveLayoutCache::default(),
        watermarks,
    )
}

// ── watermark suite (rewritten B3-3 semantics, N1.6) ───────────────────

/// Once-ever: EVERY frame (drag or idle) resumes from the recorded
/// watermark — finished output is immutable and in-flight output is
/// append-only, so lines below the watermark cannot produce new glyphs.
#[test]
fn frames_resume_from_the_watermark() {
    let mut wm = BlockScanWatermarks::default();
    let id = BlockId(7);
    let identity = 42usize;
    // Nothing recorded yet → scan from 0.
    assert_eq!(wm.output_scan_range(0, id, identity, 30), (0, 30));
    wm.record_scanned(0, id, 30, identity);
    assert_eq!(
        wm.output_scan_range(0, id, identity, 30),
        (30, 30),
        "no new lines → empty range"
    );
    // Appended lines only.
    assert_eq!(wm.output_scan_range(0, id, identity, 35), (30, 35));
}

#[test]
fn watermark_is_monotonic_and_namespaced() {
    let mut wm = BlockScanWatermarks::default();
    let id = BlockId(1);
    wm.record_scanned(0, id, 40, 1);
    wm.record_scanned(0, id, 20, 1); // transient regression → max wins
    assert_eq!(wm.output_scan_range(0, id, 1, 60), (40, 60));
    // Namespaces are independent (active pane vs background panes).
    assert_eq!(wm.output_scan_range(9, id, 1, 10), (0, 10));
}

#[test]
fn scan_range_has_no_per_block_line_cap() {
    // v1.13.11 N1.3: the B3-3 200-line cap is gone — the whole output is the
    // range and the per-frame BYTE budget bounds the cost at the call site
    // (pinned by the budget test below).
    let mut wm = BlockScanWatermarks::default();
    let id = BlockId(2);
    assert_eq!(wm.output_scan_range(0, id, 5, 8_192), (0, 8_192));
    wm.record_scanned(0, id, 8_192, 5);
    assert_eq!(wm.output_scan_range(0, id, 5, 8_192), (8_192, 8_192));
}

/// Continuation merges reuse the base BlockId with a REPLACED output Arc
/// (continuation.rs finalize). The identity mismatch must invalidate the
/// watermark so the merged-in lines are rescanned (测试 3 的机制层).
#[test]
fn identity_mismatch_invalidates_the_watermark() {
    let mut wm = BlockScanWatermarks::default();
    let id = BlockId(3);
    wm.record_scanned(0, id, 100, 111);
    assert_eq!(wm.output_scan_range(0, id, 111, 100), (100, 100));
    // Same id, different output Arc (the merge) → restart from 0.
    assert_eq!(
        wm.output_scan_range(0, id, 222, 120),
        (0, 120),
        "identity mismatch must invalidate the watermark"
    );
    // And the re-recorded watermark carries the NEW identity.
    wm.record_scanned(0, id, 120, 222);
    assert_eq!(wm.output_scan_range(0, id, 222, 120), (120, 120));
}

#[test]
fn line_count_cache_is_identity_checked() {
    let mut wm = BlockScanWatermarks::default();
    let id = BlockId(4);
    let out_a = "a\nb\nc\n";
    assert_eq!(wm.line_count(0, id, 10, out_a), 3);
    // Identity match → cached, no recount (the count stays valid even if a
    // different string were passed, which production never does).
    assert_eq!(wm.line_count(0, id, 10, "x\ny\nz\n"), 3);
    // Identity mismatch (merge replaced the output) → recount.
    let out_long = "line\n".repeat(500);
    assert_eq!(wm.line_count(0, id, 11, &out_long), 500);
}

#[test]
fn in_flight_sentinel_is_forgotten_between_commands() {
    // P3 fix (rust-reviewer): the sentinel key is reused across commands —
    // forgetting it when nothing is in flight stops the next command's
    // early output lines inheriting the previous command's watermark.
    // (P1-1's generation identity is the primary chained-command
    // invalidation; this pins the idle-frame forget hygiene. Identities are
    // opaque — literals stand in for generations here.)
    let mut wm = BlockScanWatermarks::default();
    wm.record_scanned(0, super::IN_FLIGHT_KEY, 30, 7);
    wm.forget(0, super::IN_FLIGHT_KEY);
    assert_eq!(
        wm.output_scan_range(0, super::IN_FLIGHT_KEY, 7, 5),
        (0, 5),
        "next command rescans from zero"
    );
}

#[test]
fn atlas_rebuild_clears_all_watermarks_and_in_flight_key_exists() {
    let mut wm = BlockScanWatermarks::default();
    assert_ne!(
        super::IN_FLIGHT_KEY,
        BlockId(0),
        "sentinel must not collide with real ids"
    );
    wm.record_scanned(0, super::IN_FLIGHT_KEY, 12, 7);
    wm.record_scanned(0, BlockId(3), 44, 7);
    wm.line_count(0, BlockId(3), 7, "a\nb\n");
    wm.clear();
    assert_eq!(
        wm.output_scan_range(0, super::IN_FLIGHT_KEY, 7, 20),
        (0, 20),
        "cleared watermark → live frame rescans from zero"
    );
    assert_eq!(wm.output_scan_range(0, BlockId(3), 7, 50), (0, 50));
    // P1-2b: line_counts SURVIVE the rebuild — identity-keyed, output
    // immutable per identity, nothing to do with atlas contents.
    assert_eq!(
        wm.line_count(0, BlockId(3), 7, "a\nb\nc\n"),
        2,
        "the surviving count cache must still be identity-checked"
    );
    // An identity mismatch (continuation merge) still recounts.
    assert_eq!(wm.line_count(0, BlockId(3), 9, "a\nb\nc\n"), 3);
}

// ── window suite (block_warm_window, 测试 6 的纯几何层) ────────────────

/// 10 blocks × 5 rows each, pitch 20 / header 24 → height(i) = 124·i.
fn window_fixture() -> Vec<usize> {
    (0..=10).map(|i| i * 5).collect()
}

#[test]
fn window_falls_back_to_full_scan_on_prefix_length_mismatch() {
    let ps = [0usize, 5, 10]; // len 3 != n + 1 = 4
    assert_eq!(
        block_warm_window(3, &ps, &[0, 0, 0], 20.0, 24.0, 0.0, 800.0, 0.0, 0.0),
        (0, 3),
        "stale/missing prefix sum → full scan (layout_pass.rs:294-295 同款)"
    );
}

/// 深滚验收场景：a block pushed far up by header depth (per-block header
/// bands accumulate) must stay inside the window when it is scrolled into
/// view. height(i) = 124·i; scrolling to block 8 (dist ≈ 992) puts the band
/// near it, and the window must contain index 8.
#[test]
fn window_covers_deep_scrolled_block_including_header_depth() {
    let ps = window_fixture();
    // Scroll so the viewport bottom sits at cumulative height 1200 px.
    let scroll_px = 1200.0;
    let (start, end) = block_warm_window(10, &ps, &[0; 11], 20.0, 24.0, 0.0, 800.0, scroll_px, 0.0);
    // threshold_low = 1200 - margin(64*20=1280) = -80 → window starts at 0;
    // threshold_high = 800 + 1200 + 1280 = 3280 → lower_bound(3280) = 10.
    // Either way index 8 (height 992 < 3280) must be inside.
    assert!(
        start <= 8 && end > 8,
        "window [{start}, {end}) must contain 8"
    );
}

/// clear 空隔场景：clear-block spacers (clear_ps × clear_pitch) push old
/// blocks further up; the window must still reach them (the v1 draft's P0 —
/// a bare prefix_sum window would miss them).
#[test]
fn window_counts_clear_spacers() {
    let ps: Vec<usize> = (0..=5).map(|i| i * 5).collect();
    // 2 clear blocks among the newest → heights jump by clear_pitch = 480.
    let clear_ps = [0, 0, 1, 1, 2, 2];
    let pitch = 20.0_f32;
    // heights: 0, 124, 728, 852, 1456, 1580. A shallow viewport (top of the
    // frame) at a deep scroll sits around height ~1500 (past both spacers):
    // threshold_low = 1500 - margin(1280) = 220 → lower_bound(220) = 2
    // (height 728); threshold_high = 1500 + 100 + 1280 = 2880 → all 5.
    let (start, end) = block_warm_window(5, &ps, &clear_ps, pitch, 24.0, 480.0, 100.0, 1500.0, 0.0);
    assert!(
        start <= 2,
        "window start must honor the clear-spacer heights"
    );
    assert!(
        end > 4,
        "window [{start}, {end}) must contain index 4 (height 1456)"
    );
}

/// 流式验收场景：an in-flight block adds live height, shifting the finished
/// blocks up; the window shifts with the (previous frame's) live bound.
#[test]
fn window_shifts_by_live_height_bound() {
    let ps = window_fixture();
    // No live: bottom-anchored (scroll 0) → newest end visible.
    let (start, _) = block_warm_window(10, &ps, &[0; 11], 20.0, 24.0, 0.0, 800.0, 0.0, 0.0);
    assert_eq!(start, 0);
    // Live block of 30 rows (600 px) → thresholds drop by 600. The newest
    // end stays 0 either way; the point is the shift does not push the
    // band's low side above 0 (which would EXCLUDE the newest blocks).
    let (start_live, _) = block_warm_window(10, &ps, &[0; 11], 20.0, 24.0, 0.0, 800.0, 0.0, 600.0);
    assert_eq!(
        start_live, 0,
        "live shift must not exclude the newest blocks"
    );
    // And with a deep scroll the shift moves the searched band's low edge —
    // a block right at the old boundary stays covered through the shift.
    let scrolled = 1500.0_f32;
    let (_, end_plain) =
        block_warm_window(10, &ps, &[0; 11], 20.0, 24.0, 0.0, 800.0, scrolled, 0.0);
    let (_, end_live) =
        block_warm_window(10, &ps, &[0; 11], 20.0, 24.0, 0.0, 800.0, scrolled, 600.0);
    assert!(
        end_plain > end_live || end_plain == 10,
        "live shift moves the window's old end ({end_plain} → {end_live})"
    );
}

/// live 结束后窗口复位：live_height_px back to 0 → identical window to the
/// no-live frame (评审二轮 P1 的纯函数侧). Uses a 60-block history so the
/// band is interior and the shift actually moves both edges (the margin must
/// be smaller than the shift for the effect to be observable).
#[test]
fn window_resets_when_live_bound_returns_to_zero() {
    // 60 blocks × 5 rows → height(i) = 124·i, total 7440 px.
    let ps: Vec<usize> = (0..=60).map(|i| i * 5).collect();
    let scrolled = 6000.0_f32;
    let with_live = block_warm_window(60, &ps, &[0; 61], 20.0, 24.0, 0.0, 800.0, scrolled, 2000.0);
    let after_live = block_warm_window(60, &ps, &[0; 61], 20.0, 24.0, 0.0, 800.0, scrolled, 0.0);
    assert_ne!(with_live, after_live, "live bound must affect the window");
    // And the reset window equals the never-live window.
    let never_live = block_warm_window(60, &ps, &[0; 61], 20.0, 24.0, 0.0, 800.0, scrolled, 0.0);
    assert_eq!(after_live, never_live);
}

// ── mechanism tests (device-gated; 测试 1/2/3/4-机制/6-warm) ────────────

/// 测试 1 + 窗口负例：a fresh atlas + one warm pass over the visible set →
/// ✅ (single-scalar emoji) and the ℹ️ cluster both hit; an OUT-of-window
/// block's glyph stays missing (once-ever must not degrade into scan-all).
#[test]
fn block_warm_populates_checkmark_and_info_cluster_but_not_out_of_window() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping block warm test: no Metal device available");
        return;
    };
    // 20 blocks; the newest carries ✅ + ℹ️, the oldest carries 🦀.
    let mut blocks: Vec<Block> = (1..=19)
        .map(|i| fixture_block(i, &format!("cmd-{i}"), &format!("out {i}\n")))
        .collect();
    blocks.push(fixture_block(
        20,
        "openclaw update",
        "check ok\nℹ\u{fe0f} info line\n✅ done\n",
    ));
    blocks[0] = fixture_block(1, "cmd-1", "out 1\n🦀 rust\n");
    let tracker = tracker_with(blocks);
    let wm = RefCell::new(BlockScanWatermarks::default());

    let visible = [BlockId(20)];
    let scanned = warm_block(&tracker, &mut atlas, &wm, 1, Some(&visible));
    assert!(scanned > 0, "visible block must be scanned");

    // Positive: both glyph paths the block view consumes now hit.
    assert!(
        atlas
            .get_style('✅', crate::glyph::GlyphStyle::REGULAR)
            .is_some(),
        "✅ must be warmed into the atlas"
    );
    assert!(
        atlas.get_cluster("ℹ\u{fe0f}").is_some(),
        "the ℹ️ cluster must be warmed via the cluster path"
    );
    // Negative: the out-of-window block was never scanned (it is outside
    // the newest-16 union and not in the visible override).
    assert!(
        atlas
            .get_style('🦀', crate::glyph::GlyphStyle::REGULAR)
            .is_none(),
        "out-of-window glyphs must stay missing (no scan-all regression)"
    );
}

/// 测试 2：once-ever + 分帧预算 — a 1 MiB block's first scan is split into
/// ≤ 8 frames of ≤ 128 KiB each, completes, then scans zero bytes forever.
#[test]
fn one_mib_block_scans_within_budget_and_completes_within_eight_frames() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping budget test: no Metal device available");
        return;
    };
    // 1024 lines × 1024 bytes = 1 MiB of output.
    let line = "x".repeat(1023) + "\n";
    let output = line.repeat(1024);
    assert_eq!(output.len(), 1024 * 1024);
    let tracker = tracker_with(vec![fixture_block(1, "big", &output)]);
    let wm = RefCell::new(BlockScanWatermarks::default());

    let mut frames = 0;
    loop {
        let bytes = warm_block(&tracker, &mut atlas, &wm, 0, None);
        assert!(
            bytes as usize <= BLOCK_WARM_BUDGET_BYTES,
            "frame {frames} scanned {bytes} bytes > budget"
        );
        if bytes == 0 {
            break;
        }
        frames += 1;
        assert!(
            frames <= 8,
            "1 MiB block must finish within 8 budget frames"
        );
    }
    assert_eq!(
        frames, 8,
        "1 MiB / 128 KiB budget = exactly 8 scanning frames"
    );
    // Steady state: repeated frames scan nothing (watermark + line-count
    // cache hits), and no duplicate scanning ever re-runs.
    for _ in 0..3 {
        assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);
    }
}

/// 测试 2 扩展 / P1-2 (rust-reviewer): a FULL-SELECT frame — the layout
/// cache's prefix sum doesn't cover the block set, so `block_warm_window`
/// falls back to (0, n) and EVERY block is selected at once. The scanned
/// bytes must still honor the frame budget (no header/line-count walk
/// bypass), with one slack line for the untouched-budget first-line
/// guarantee; the whole set then completes within bounded frames.
#[test]
fn full_select_fallback_frame_scans_within_budget() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping fallback budget test: no Metal device available");
        return;
    };
    // 4 blocks × 96 KiB = 384 KiB — more than one frame's budget.
    let line = "x".repeat(1023) + "\n";
    let output = line.repeat(96);
    assert_eq!(output.len(), 96 * 1024);
    let blocks: Vec<Block> = (1..=4).map(|i| fixture_block(i, "big", &output)).collect();
    let tracker = tracker_with(blocks);
    let wm = RefCell::new(BlockScanWatermarks::default());

    // warm_block's Default BlockLayoutCache → empty prefix sum → the
    // full-select fallback fires for this frame.
    let first = warm_block(&tracker, &mut atlas, &wm, 0, None);
    assert!(
        first as usize <= BLOCK_WARM_BUDGET_BYTES + 1024,
        "full-select frame scanned {first} bytes > budget (+1 slack line)"
    );
    let mut frames = 1;
    loop {
        let bytes = warm_block(&tracker, &mut atlas, &wm, 0, None);
        if bytes == 0 {
            break;
        }
        frames += 1;
        assert!(
            frames <= 8,
            "384 KiB / 128 KiB budget must complete within 8 frames"
        );
    }
    // Steady state: the fallback's once-ever watermarks hold.
    assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);
}

/// 测试 3：continuation 合并重扫 — the merge replaces the block's output
/// while keeping its id; the identity-keyed watermark invalidates and the
/// merged-in lines (their new glyph included) are picked up.
#[test]
fn continuation_merge_triggers_identity_invalidation_rescan() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping continuation merge test: no Metal device available");
        return;
    };
    // Same fixture shape as continuation.rs's replay tests (≥6 meaningful
    // lines, ≥160 bytes, ≥55% match) — the resume-link card lines are what
    // makes the NEXT command a continuation candidate.
    const REPLAY: &str = "banner line with enough content\nmodel and account information\nworking directory /tmp/project\nprompt asking for a performance report\nanswer line one with useful detail\nanswer line two with useful detail\nResume this session with:\ntool --resume session-id";
    let mut tracker = BlockTracker::new();
    tracker.on_prompt_start();
    tracker.on_command_start("tool".to_string());
    tracker.begin_screen_owned_output(0);
    tracker.replace_screen_output(REPLAY);
    tracker.on_command_end(0);
    let base_id = tracker.session_blocks()[0].id;

    // First warm: the whole output scans once (bytes > 0), then idles.
    let wm = RefCell::new(BlockScanWatermarks::default());
    assert!(warm_block(&tracker, &mut atlas, &wm, 0, None) > 0);
    assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);

    // Merge: same session id, output replaced (original + new answer with a
    // glyph that only exists in the appended line).
    tracker.on_prompt_start();
    tracker.on_command_start("tool --resume session-id".to_string());
    tracker.begin_screen_owned_output(10);
    tracker.replace_screen_output(&format!("{REPLAY}\némerged after resume\n"));
    tracker.on_command_end(0);
    assert_eq!(tracker.session_blocks().len(), 1, "merge keeps one block");
    assert_eq!(
        tracker.session_blocks()[0].id,
        base_id,
        "merge keeps the id"
    );
    assert!(
        tracker.session_blocks()[0]
            .output
            .ends_with("émerged after resume"),
        "fixture: the merged output carries the new line"
    );

    // Next frame: identity mismatch → full rescan of the merged output.
    let rescanned = warm_block(&tracker, &mut atlas, &wm, 0, None);
    assert!(
        rescanned > 0,
        "the merged block must be rescanned after the output identity changed"
    );
    assert!(
        atlas
            .get_style('é', crate::glyph::GlyphStyle::REGULAR)
            .is_some(),
        "a glyph unique to the merged-in lines must now be in the atlas"
    );
    // Once the rescan completes, the steady state is zero again.
    assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);
}

/// P1-1 (rust-reviewer): chained commands (`echo a; cargo build`) can
/// finalize command 1 and start command 2 inside ONE PTY batch — no idle
/// frame runs, so the forget-when-idle path never fires. The sentinel
/// watermark's identity is the per-command GENERATION
/// (`BlockTracker::command_generation`), so command 2's live scan restarts
/// at 0 instead of inheriting command 1's watermark (which would leave its
/// first lines permanently unscanned).
#[test]
fn chained_command_generation_restarts_the_live_scan() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping chained-command test: no Metal device available");
        return;
    };
    let mut tracker = BlockTracker::new();
    tracker.on_command_start("echo a".into());
    for ch in "alpha line\nbeta line\n".chars() {
        tracker.on_print(ch, CapturedStyle::default());
    }
    let wm = RefCell::new(BlockScanWatermarks::default());
    assert!(
        warm_block(&tracker, &mut atlas, &wm, 0, None) > 0,
        "command 1's live output must scan"
    );
    assert_eq!(tracker.command_generation(), 1);

    // The chain: D then B with NO warm frame in between.
    tracker.on_command_end(0);
    tracker.on_command_start("echo b".into());
    for ch in "锋 first\nsecond\n".chars() {
        tracker.on_print(ch, CapturedStyle::default());
    }
    assert_eq!(tracker.command_generation(), 2);

    // Without P1-1 the scan would start at command 1's watermark (2 lines)
    // and skip BOTH of command 2's lines — '锋' would stay missing forever.
    assert!(
        warm_block(&tracker, &mut atlas, &wm, 0, None) > 0,
        "command 2's live scan must run (generation mismatch → restart at 0)"
    );
    assert!(
        atlas
            .get_style('锋', crate::glyph::GlyphStyle::REGULAR)
            .is_some(),
        "command 2's first line must be scanned despite the inherited watermark"
    );
}

/// 测试 4（机制侧）：collapsed 短路 — no output scan AND no full watermark
/// (an expand must look like a first scan); command/duration strings still
/// warm. Expanding rescans the output.
#[test]
fn collapsed_block_skips_output_scan_and_records_no_watermark() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping collapsed test: no Metal device available");
        return;
    };
    let mut block = fixture_block(1, "cargo 构建", "compiling weft\n✅ finished\n");
    block.collapsed = true;
    let tracker = tracker_with(vec![block]);
    let wm = RefCell::new(BlockScanWatermarks::default());

    let scanned = warm_block(&tracker, &mut atlas, &wm, 0, None);
    assert_eq!(scanned, 0, "collapsed blocks must not scan output lines");
    // No full watermark: the range must still start at 0.
    let identity = super::output_identity(&tracker.session_blocks()[0].output);
    let mut wm_mut = wm.borrow_mut();
    assert_eq!(
        wm_mut.output_scan_range(0, BlockId(1), identity, 2),
        (0, 2),
        "collapsed warm must NOT record a watermark (评审 P2-2)"
    );
    drop(wm_mut);
    // The command string itself was warmed even while collapsed. P3-1
    // (rust-reviewer): assert a NON-ASCII char — ASCII is pre-rasterized at
    // atlas construction (glyph.rs), so `get_style('y')` was vacuously true.
    assert!(
        atlas
            .get_style('构', crate::glyph::GlyphStyle::REGULAR)
            .is_some(),
        "command strings still warm while collapsed"
    );

    // Expand → treated as a first scan.
    let mut tracker = tracker;
    tracker.toggle_collapse(BlockId(1));
    assert!(!tracker.session_blocks()[0].collapsed);
    let scanned = warm_block(&tracker, &mut atlas, &wm, 0, None);
    assert!(scanned > 0, "expanding must trigger the first output scan");
    assert!(
        atlas
            .get_style('✅', crate::glyph::GlyphStyle::REGULAR)
            .is_some(),
        "the expanded block's glyphs land in the atlas"
    );
}

/// 测试 6（warm 侧门控）：the live-height bound only applies when the live
/// cache belongs to the warmed pane AND a block is in flight — otherwise
/// the window must behave exactly like live_height = 0.
#[test]
fn live_height_bound_gates_on_pane_ownership() {
    let mut live = LiveLayoutCache::default();
    assert_eq!(live.pane(), 0, "fresh cache is unowned (pane 0)");
    // Sync under pane 9 → pane() reports 9; a warm for namespace 9 sees an
    // owned cache, a warm for any other namespace must ignore it.
    live.sync("a\nb\nc\n", 9, 1, 80, false, usize::MAX);
    assert_eq!(live.pane(), 9);
    assert!(live.total_display_rows() > 0);
    // (The geometric consequence is pinned by the pure window tests; this
    // test pins the getter contract the warm gating reads.)
}

/// 命名空间隔离（B3-3 不变量的新语义映射）：background panes must not
/// inherit the active pane's watermarks.
#[test]
fn warm_namespaces_are_independent() {
    let Some(mut atlas) = headless_atlas_or_skip() else {
        eprintln!("skipping namespace test: no Metal device available");
        return;
    };
    let tracker = tracker_with(vec![fixture_block(1, "ls", "alpha\nbeta\n")]);
    let wm = RefCell::new(BlockScanWatermarks::default());
    assert!(warm_block(&tracker, &mut atlas, &wm, 0, None) > 0);
    assert_eq!(warm_block(&tracker, &mut atlas, &wm, 0, None), 0);
    assert!(
        warm_block(&tracker, &mut atlas, &wm, 77, None) > 0,
        "a different namespace must rescan from zero"
    );
}
