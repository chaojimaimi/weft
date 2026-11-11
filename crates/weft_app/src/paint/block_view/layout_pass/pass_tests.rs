//! v1.19.2 (PLAN_v1192 §1.1a): `layout_pass::tests`, moved out of
//! `layout_pass.rs` (was inline `mod tests`). Content unchanged.

use super::bounds::{lower_bound_height, upper_bound_height};
use super::*;

/// Empty input → empty output. The only layout-pass test not requiring
/// a MetalRenderer; GUI integration tests cover paint/hit-test parity.
#[test]
fn empty_layout_pass_produces_empty_output() {
    let input = LayoutPassInput {
        blocks: &[],
        live: None,
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows: 0,
        cols: 80,
        pitch: 20.0,
        header_height: 24.0,
        content_bottom_y: 800.0,
        clip_top: 0.0,
        clip_bottom: 800.0,
        resolve_styles: false,
        styled_lookup_counter: None,
        complete_block_overlay: false, // v1.20.0 (D10): legacy domain
        block_diagnose_state: &std::collections::HashMap::new(),
        now: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        region: None,
        region_state: None,
    };
    let cache = BlockLayoutCache::default();
    let mut live_cache = LiveLayoutCache::default();
    let out = compute_block_layout_pass(input, &cache, &mut live_cache);
    assert!(out.rows.is_empty());
    assert!(out.row_data.is_empty());
    assert_eq!(out.expanded_block_count, 0);
}

#[test]
fn live_output_keeps_newest_row_complete_at_viewport_bottom() {
    let output = (0..20)
        .map(|line| format!("line-{line}\n"))
        .collect::<String>();
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(InFlightBlock {
            command: "long-running-command",
            cwd: Some("/tmp"),
            output: &output,
            styled_output: None,
            version: 1,
            screen_origin: false,
            started_at: std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_700_000_000),
            min_write_offset: InFlightBlock::detached_watermark(),
        }),
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows: 6,
        cols: 80,
        pitch: 20.0,
        header_height: 24.0,
        content_bottom_y: 120.0,
        clip_top: 0.0,
        clip_bottom: 120.0,
        resolve_styles: false,
        styled_lookup_counter: None,
        complete_block_overlay: false, // v1.20.0 (D10): legacy domain
        block_diagnose_state: &std::collections::HashMap::new(),
        now: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        region: None,
        region_state: None,
    };
    let cache = BlockLayoutCache::default();
    let out = compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
    let newest = out
        .row_data
        .iter()
        .position(|row| matches!(row, LaidRow::Output { text, .. } if *text == "line-19"))
        .expect("newest live row");
    let newest_top = 120.0 - out.rows[newest];

    assert_eq!(newest_top, 100.0);
    assert_eq!(newest_top + 20.0, 120.0);
    assert!(out.rows.iter().any(|distance| 120.0 - distance < 0.0));
}

// Binary-search helpers: correctness of the height formula
// (`prefix_sum[j]*pitch + j*header_height + clear_ps[j]*clear_pitch`)
// keeps the O(log n) fast path from skipping or duplicating blocks.
// `clear_pitch = viewport_rows*pitch` recovers clear-block spacers
// (excluded from the prefix sum); zeroed clear args exercise the
// original semantics.

#[test]
fn lower_bound_height_empty_returns_zero() {
    // Empty prefix sum (just [0]) → always returns 0 (nothing >= threshold).
    assert_eq!(lower_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 100.0), 1);
    assert_eq!(lower_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 0.0), 0);
}

#[test]
fn lower_bound_height_all_below_threshold_returns_len() {
    // prefix_sum = [0, 5, 10, 15], pitch=20, header=24
    // heights = [0, 5*20+1*24=124, 10*20+2*24=248, 15*20+3*24=372]
    let ps = [0, 5, 10, 15];
    // threshold=400 → all below → returns 4 (len)
    assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 400.0), 4);
}

#[test]
fn lower_bound_height_all_above_threshold_returns_zero() {
    let ps = [0, 5, 10, 15];
    // threshold=-10 → all above (height[0]=0 >= -10) → returns 0
    assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, -10.0), 0);
}

#[test]
fn lower_bound_height_finds_first_ge_threshold() {
    // heights = [0, 124, 248, 372]
    let ps = [0, 5, 10, 15];
    // threshold=200 → first >= 200 is index 2 (height=248)
    assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 200.0), 2);
    // threshold=124 → first >= 124 is index 1 (exact match)
    assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 124.0), 1);
    // threshold=125 → first >= 125 is still index 2
    assert_eq!(lower_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 125.0), 2);
}

#[test]
fn upper_bound_height_empty_returns_zero() {
    // upper_bound is strict > : height[0]=0 > -1 → returns 0
    assert_eq!(upper_bound_height(&[0], &[0], 20.0, 24.0, 0.0, -1.0), 0);
    // height[0]=0 > 0 is false → returns 1 (len)
    assert_eq!(upper_bound_height(&[0], &[0], 20.0, 24.0, 0.0, 0.0), 1);
}

#[test]
fn upper_bound_height_all_le_threshold_returns_len() {
    // heights = [0, 124, 248, 372]
    let ps = [0, 5, 10, 15];
    // threshold=400 → all <= 400 → returns 4 (len)
    assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 400.0), 4);
}

#[test]
fn upper_bound_height_finds_first_gt_threshold() {
    // heights = [0, 124, 248, 372]
    let ps = [0, 5, 10, 15];
    // threshold=200 → first > 200 is index 2 (height=248)
    assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 200.0), 2);
    // threshold=124 → first > 124 is index 2 (strict: 124 is not > 124)
    assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 124.0), 2);
    // threshold=125 → first > 125 is index 2
    assert_eq!(upper_bound_height(&ps, &[0; 4], 20.0, 24.0, 0.0, 125.0), 2);
}

/// Nonzero `clear_ps` adds `clear_ps[i]*clear_pitch` to the height
/// (viewport-sized clear spacer excluded from the prefix sum); without
/// it, blocks above a `clear` are underestimated and culled after scroll.
#[test]
fn bound_height_counts_clear_spacer() {
    // prefix_sum = [0, 5, 10, 15, 20], clear_ps = [0, 0, 1, 1, 2]
    // (2 clear blocks among the 4 newest). pitch=20, header=24,
    // clear_pitch = viewport_rows=24 * pitch=20 = 480.
    let ps = [0, 5, 10, 15, 20];
    let clear_ps = [0, 0, 1, 1, 2];
    let pitch = 20.0_f32;
    let header = 24.0_f32;
    let clear_pitch = 480.0_f32;
    // heights: i=0 → 0; i=1 → 124; i=2 → 728; i=3 → 852; i=4 → 1456
    assert_eq!(
        lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 124.0),
        1
    );
    assert_eq!(
        lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 700.0),
        2
    );
    assert_eq!(
        lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 900.0),
        4
    );
    assert_eq!(
        lower_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 5000.0),
        5
    );
    assert_eq!(
        upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 124.0),
        2
    );
    assert_eq!(
        upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 852.0),
        4
    );
    assert_eq!(
        upper_bound_height(&ps, &clear_ps, pitch, header, clear_pitch, 1456.0),
        5
    );
}

/// Visible range = [lower_bound(low) - 1, upper_bound(high) + 1) with
/// 1-block overscan; verifies the bounds produce a valid covering range.
#[test]
fn binary_search_visible_range_contains_expected_blocks() {
    // 10 blocks, each base_row_count=5 → prefix_sum = [0,5,10,...,50]
    let ps: Vec<usize> = (0..=10).map(|i| i * 5).collect();
    let clear_ps = vec![0; ps.len()]; // no clear blocks → clear_pitch unused
    let pitch = 20.0_f32;
    let header = 24.0_f32;
    // height(i) = 5i * 20 + i * 24 = 124i
    // Say viewport covers blocks 3..6 (heights 372..744).
    // threshold_low=350 → lower_bound finds first >= 350 → idx 3 (height=372)
    // threshold_high=760 → upper_bound finds first > 760 → idx 7 (height=868)
    let j_low = lower_bound_height(&ps, &clear_ps, pitch, header, 0.0, 350.0);
    let j_high = upper_bound_height(&ps, &clear_ps, pitch, header, 0.0, 760.0);
    assert_eq!(j_low, 3);
    assert_eq!(j_high, 7);
    // first_visible = j_low - 1 = 2, with overscan start=1
    // end = j_high + 1 = 8
    // Visible range [1, 8) covers blocks 1..7 — includes 3..6 with overscan.
    let first_visible = j_low.saturating_sub(1);
    let start = first_visible.saturating_sub(1);
    let end = (j_high + 1).min(10);
    assert_eq!(start, 1);
    assert_eq!(end, 8);
}
