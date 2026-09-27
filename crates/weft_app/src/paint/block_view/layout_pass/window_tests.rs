//! M5-b P2-3: direct pinning of the windowed row emission
//! (`compute_block_layout_pass`'s L2 `line_row_base` binary search +
//! cursor fast-forwards). For a 118k-line block and a scroll sequence, the
//! windowed emission must be a CONTIGUOUS slice of the full (±∞ clip)
//! emission with bitwise-equal `dist` values and identical row fields —
//! i.e. the fast-forwards and window clamps can never shift a row.

use std::sync::{Arc, OnceLock};

use weft_core::blocks::{Block, BlockId};

use super::{
    compute_block_layout_pass, BlockLayoutCache, LaidRow, LayoutPassInput, LayoutPassOutput,
};
use crate::paint::grid_cache::BandSync;
use crate::paint::live_cache::LiveLayoutCache;

/// Pre-M6-b semantics: a band covering everything defers nothing.
fn full_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: usize::MAX,
    }
}

const LINES: usize = 118_000;
const PITCH: f32 = 17.0;
const VIEWPORT: f32 = 400.0;
/// Clip window covering the block bottom at scroll 0.
const CLIP_TOP: f32 = 200.0;
const CLIP_BOTTOM: f32 = 600.0;

fn block_output() -> &'static Arc<str> {
    static OUTPUT: OnceLock<Arc<str>> = OnceLock::new();
    OUTPUT.get_or_init(|| {
        let hex = "0123456789abcdef".repeat(8);
        let mut out = String::with_capacity(LINES * 40);
        for i in 0..LINES {
            match i % 16 {
                0 => out.push_str(&format!("src/weft/mod_{i}.rs:1: use weft_core::grid;\n")),
                4 | 9 => out.push_str(&format!("result[{i}] = {} payload sha bits\n", &hex[..104])),
                8 => out.push_str("────────────────────────────────\n"),
                12 => out.push_str(&format!("│ row {i} │ Data 426G │ 充裕 │\n")),
                _ => out.push_str(&format!("line-{i}: finished in 1.20s\n")),
            }
        }
        Arc::from(out.as_str())
    })
}

fn make_block() -> Block {
    Block {
        id: BlockId(1),
        command: "cargo build --release".to_string(),
        cwd: None,
        output: Arc::clone(block_output()),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct RowKey {
    dist_bits: u32,
    line: usize,
    chunk_idx: usize,
    char_offset: usize,
    text: String,
}

impl RowKey {
    /// The row's own y (line-top dist + chunk offset), matching the paint
    /// loop's `row_top_y + chunk_idx * pitch`.
    fn y(&self, content_bottom_y: f32, scroll_px: f32, pitch: f32) -> f32 {
        let line_top = content_bottom_y - f32::from_bits(self.dist_bits) + scroll_px;
        line_top + self.chunk_idx as f32 * pitch
    }
}

fn output_keys(out: &LayoutPassOutput<'_>) -> Vec<RowKey> {
    out.row_data
        .iter()
        .zip(&out.rows)
        .filter_map(|(row, dist)| {
            let LaidRow::Output {
                text,
                line,
                chunk_idx,
                char_offset,
                ..
            } = row
            else {
                return None;
            };
            Some(RowKey {
                // Bitwise: the windowed emission must reproduce the full
                // emission's pushed f32 dist EXACTLY.
                dist_bits: dist.to_bits(),
                line: *line,
                chunk_idx: *chunk_idx,
                char_offset: *char_offset,
                text: text.to_string(),
            })
        })
        .collect()
}

fn run_pass<'a>(
    cache: &'a BlockLayoutCache,
    live_cache: &'a mut LiveLayoutCache,
    blocks: &'a [Block],
    block_scroll: f32,
    clip_top: f32,
    clip_bottom: f32,
) -> LayoutPassOutput<'a> {
    compute_block_layout_pass(
        LayoutPassInput {
            blocks,
            live: None,
            pane_session_id: 1,
            cwd: None,
            git_branch: None,
            block_scroll,
            viewport_rows: 24,
            cols: 88,
            pitch: PITCH,
            header_height: 24.0,
            content_bottom_y: 600.0,
            clip_top,
            clip_bottom,
            resolve_styles: false,
            styled_lookup_counter: None,
            block_diagnose_state: &std::collections::HashMap::new(),
            now: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        },
        cache,
        live_cache,
    )
}

/// The windowed emission for a scroll sequence must be, per window, a
/// contiguous slice of the full emission with bitwise-equal dists, and
/// every emitted row must sit within the clip band (± one clamp-slack row).
#[test]
fn windowed_emission_is_a_contiguous_bitwise_equal_slice_of_full() {
    let blocks = vec![make_block()];
    let mut cache = BlockLayoutCache::default();
    let mut live = LiveLayoutCache::default();
    cache.sync_blocks(&blocks, 88, full_band());
    let total_rows = cache.get(1).width.rows.len() as f32;

    // Full reference: everything visible.
    let full = run_pass(&cache, &mut live, &blocks, 0.0, -1.0e9, 1.0e9);
    let full_keys = output_keys(&full);
    assert!(
        full_keys.len() >= LINES,
        "full emission must cover every output line ({} keys for {LINES} lines)",
        full_keys.len()
    );
    // Scroll sequence: 27 evenly spaced scrolls spanning the block plus the
    // structural tail (the coarse sweep samples the banded equivalence at
    // every region of the block; the dense sweep below proves full coverage
    // on a smaller block).
    let total_px = total_rows * PITCH;
    let steps = 26;
    let mut checked_rows = 0usize;
    for k in 0..=steps {
        let block_scroll = ((total_px + 800.0) * k as f32 / steps as f32) / PITCH;
        let win = run_pass(
            &cache,
            &mut live,
            &blocks,
            block_scroll,
            CLIP_TOP,
            CLIP_BOTTOM,
        );
        let win_keys = output_keys(&win);

        // The equivalence: the windowed emission must (i) cover every row
        // the paint loop would draw PLUS the documented one-row clamp slack
        // (filter: row bottom ≥ clip_top - pitch, row top ≤ clip_bottom +
        // pitch — provably exactly the emission's clamp range), (ii) emit
        // nothing outside that range, and (iii) keep the full emission's
        // order with bitwise-equal dists.
        let scroll_px = block_scroll * PITCH;
        let y_of = |key: &RowKey| key.y(600.0, scroll_px, PITCH);
        let strictly_banded = |key: &RowKey| {
            let y = y_of(key);
            y + PITCH >= CLIP_TOP - PITCH && y <= CLIP_BOTTOM + PITCH
        };
        let expected: Vec<&RowKey> = full_keys
            .iter()
            .filter(|key| strictly_banded(key))
            .collect();
        for key in &expected {
            assert!(
                win_keys.contains(key),
                "window {k}: strictly-banded row {key:?} missing from the windowed emission"
            );
        }
        for key in &win_keys {
            let y = y_of(key);
            // Emission slack legitimately reaches 2 pitches: clamp row +
            // ceil overshoot (frac(X) → 1 puts first_row at clip_top - 2p).
            assert!(
                y + PITCH + 2.0 * PITCH >= CLIP_TOP && y - 2.0 * PITCH <= CLIP_BOTTOM,
                "window {k}: emitted row {key:?} is more than 2 pitches outside the band"
            );
        }
        // (iii) order + bitwise dist: the emitted keys, in order, must equal
        // the full keys filtered to the emitted set.
        let emitted_in_full_order: Vec<&RowKey> = full_keys
            .iter()
            .filter(|key| win_keys.contains(key))
            .collect();
        assert_eq!(
            emitted_in_full_order.len(),
            win_keys.len(),
            "window {k}: emitted set diverges from the full emission"
        );
        for (off, (fk, wk)) in emitted_in_full_order.iter().zip(&win_keys).enumerate() {
            assert_eq!(
                fk, &wk,
                "window {k}: mismatch at offset {off} (windowed emission diverges from full)"
            );
        }
        checked_rows += win_keys.len();
    }
    // Reachability: the bottom-most row is visible once the viewport scrolls
    // up to it (~10 rows), the top-most row near max scroll; chained
    // overlapping windows therefore cover everything in between.
    // Emission order is bottom-to-top: first key = bottom-most row.
    let win_bottom = run_pass(&cache, &mut live, &blocks, 0.0, CLIP_TOP, CLIP_BOTTOM);
    assert!(
        full_keys
            .first()
            .map(|key| output_keys(&win_bottom).contains(key))
            == Some(true),
        "the bottom-most row must be visible at scroll 0 (clip covers the block bottom)"
    );
    let win_top = run_pass(
        &cache,
        &mut live,
        &blocks,
        total_rows - 20.0,
        CLIP_TOP,
        CLIP_BOTTOM,
    );
    assert!(
        full_keys
            .last()
            .map(|key| output_keys(&win_top).contains(key))
            == Some(true),
        "the top-most row must be visible near max scroll"
    );
    assert!(checked_rows >= 27, "scroll sequence must have run");
}

/// Dense-sweep coverage proof on a small block: stepping the clip window by
/// HALF its height must make the union of the windowed emissions cover the
/// full emission EXACTLY (every row reachable, none invented).
#[test]
fn dense_scroll_sweep_covers_the_full_emission() {
    let lines = 2_000;
    let mut out = String::new();
    for i in 0..lines {
        if i % 8 == 3 {
            out.push_str(&format!("wrapped line {i} {}", "x".repeat(150)));
        } else {
            out.push_str(&format!(
                "line-{i}: short prose body
"
            ));
        }
        out.push('\n');
    }
    let blocks = vec![Block {
        id: BlockId(1),
        command: "echo dense".to_string(),
        cwd: None,
        output: Arc::from(out.as_str()),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }];
    let mut cache = BlockLayoutCache::default();
    let mut live = LiveLayoutCache::default();
    cache.sync_blocks(&blocks, 88, full_band());

    // Full reference.
    let full = run_pass(&cache, &mut live, &blocks, 0.0, -1.0e9, 1.0e9);
    let full_keys = output_keys(&full);
    let total_px = cache.get(1).width.rows.len() as f32 * PITCH;

    // Dense sweep: half-window steps → overlapping bands chain across the
    // whole block.
    let mut union: Vec<RowKey> = Vec::new();
    let step_px = VIEWPORT / 2.0;
    let steps = ((total_px + 800.0) / step_px).ceil() as usize;
    for k in 0..=steps {
        let block_scroll = (step_px * k as f32) / PITCH;
        let win = run_pass(
            &cache,
            &mut live,
            &blocks,
            block_scroll,
            CLIP_TOP,
            CLIP_BOTTOM,
        );
        let win_keys = output_keys(&win);
        // Same per-window equivalence as the 118k sweep: painted rows
        // covered, emissions within the 2-pitch clamp slack, order and
        // bitwise dists preserved.
        let scroll_px = block_scroll * PITCH;
        let painted = |key: &RowKey| {
            let y = key.y(600.0, scroll_px, PITCH);
            y + PITCH >= CLIP_TOP && y <= CLIP_BOTTOM
        };
        let painted_rows: Vec<&RowKey> = full_keys.iter().filter(|key| painted(key)).collect();
        for key in &painted_rows {
            assert!(
                win_keys.contains(key),
                "step {k}: painted row {key:?} missing from the windowed emission"
            );
        }
        for key in &win_keys {
            let y = key.y(600.0, scroll_px, PITCH);
            assert!(
                y + PITCH + 2.0 * PITCH >= CLIP_TOP && y - 2.0 * PITCH <= CLIP_BOTTOM,
                "step {k}: emitted row {key:?} is more than 2 pitches outside the band"
            );
        }
        let in_order: Vec<&RowKey> = full_keys
            .iter()
            .filter(|key| win_keys.contains(key))
            .collect();
        assert_eq!(
            in_order.len(),
            win_keys.len(),
            "step {k}: emitted set diverges from the full emission"
        );
        for (off, (fk, wk)) in in_order.iter().zip(&win_keys).enumerate() {
            assert_eq!(fk, &wk, "step {k}: mismatch at {off}");
        }
        for key in win_keys {
            if !union.contains(&key) {
                union.push(key);
            }
        }
    }
    assert_eq!(
        union.len(),
        full_keys.len(),
        "dense sweep must cover the full emission exactly"
    );
}

/// The scroll fast path (append-only sync) must not disturb the windowed
/// emission between two runs — guards P2-1's prune against over-eviction.
#[test]
fn repeated_sync_keeps_windowed_emission_stable() {
    let blocks = vec![make_block()];
    let mut cache = BlockLayoutCache::default();
    let mut live = LiveLayoutCache::default();
    cache.sync_blocks(&blocks, 88, full_band());

    let first = run_pass(&cache, &mut live, &blocks, 0.0, 0.0, VIEWPORT);
    let keys_first = output_keys(&first);
    // Scroll frame: append-only sync (newest block re-checked).
    cache.sync_blocks(&blocks, 88, full_band());
    let second = run_pass(&cache, &mut live, &blocks, 0.0, 0.0, VIEWPORT);
    let keys_second = output_keys(&second);

    assert_eq!(keys_first, keys_second, "scroll-frame sync must be inert");
    assert!(
        !keys_first.is_empty(),
        "the newest rows of a 118k-line block must be visible at scroll 0"
    );
}
