//! M6-b (PLAN_M6 §三 B-6) tests: band-gated sync classification, metrics
//! scalar semantics, and the idle convergence pump.

use super::*;
use crate::block_component::{
    block_content_metrics_with_cache, clear_block_spacer_rows, command_output_gap_rows,
    completed_block_row_count,
};
use weft_core::blocks::{Block, BlockId};
use weft_core::vt::Terminal;

/// Block with `lines` short non-wrapping output lines. At wide cols its
/// `base_row_count` is `lines + 3` (lines + breathing row + command +
/// separator).
fn block(id: u64, lines: usize) -> Block {
    let output = (0..lines)
        .map(|i| format!("line-{i}\n"))
        .collect::<String>();
    Block {
        id: BlockId(id),
        command: "echo x".to_string(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }
}

fn full_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: usize::MAX,
    }
}

fn fresh_synced(blocks: &[Block], cols: usize) -> BlockLayoutCache {
    let mut cache = BlockLayoutCache::default();
    cache.sync_blocks(blocks, cols, full_band());
    cache
}

/// Stale bottoms (distance-from-content-bottom of each block's lower edge),
/// oldest-first blocks at `block(i, i as usize)` sizes → newest-to-oldest.
fn bottoms_newest_first(blocks: &[Block], cache: &BlockLayoutCache) -> Vec<usize> {
    let mut below = 0usize;
    let mut out = Vec::new();
    for block in blocks.iter().rev() {
        out.push(below);
        below += cache.get(block.id.0).base_row_count;
    }
    out
}

/// B-6 truth table: 8 blocks × band position (cols change on every block) →
/// per-block expectation. Blocks whose stale interval lies fully above the
/// band defer (misses stay 0 for them, entry keeps stale cols); the rest
/// rebuild immediately (one miss each).
#[test]
fn band_truth_table_classifies_each_block_and_counts_misses() {
    let blocks: Vec<Block> = (1..=8).map(|i| block(i, i as usize)).collect();
    let (cols_a, cols_b) = (80usize, 78usize);

    // bottoms newest-first for the 8-block session: [0, 11, 21, 30, 38, 45, 51, 56].
    let reference = fresh_synced(&blocks, cols_a);
    let bottoms = bottoms_newest_first(&blocks, &reference);
    assert_eq!(bottoms, vec![0, 11, 21, 30, 38, 45, 51, 56]);

    // Sweep band highs placed between/around consecutive bottoms: the
    // immediate set is exactly {blocks with bottom < high}.
    for &high in &[0usize, 11, 22, 31, 39, 46, 52, 57, 100] {
        let mut cache = fresh_synced(&blocks, cols_a);
        cache.take_hit_miss_counts();
        let expected_immediate = bottoms.iter().filter(|&&b| b < high).count();

        cache.sync_blocks(
            &blocks,
            cols_b,
            BandSync {
                low_rows: 0,
                high_rows: high,
            },
        );

        assert_eq!(
            cache.deferred_count(),
            8 - expected_immediate,
            "high={high}"
        );
        let (_, misses) = cache.take_hit_miss_counts();
        assert_eq!(misses, expected_immediate, "high={high}");
    }

    // 逐块期望 for one representative band: high=31 → newest 4 immediate,
    // oldest 4 deferred with stale cols.
    let mut cache = fresh_synced(&blocks, cols_a);
    cache.sync_blocks(
        &blocks,
        cols_b,
        BandSync {
            low_rows: 0,
            high_rows: 31,
        },
    );
    for (index, blk) in blocks.iter().enumerate() {
        let entry = cache.get(blk.id.0);
        if bottoms[7 - index] < 31 {
            assert_eq!(entry.cols, cols_b, "block {} rebuilt in-band", blk.id.0);
        } else {
            assert_eq!(entry.cols, cols_a, "block {} deferred stale", blk.id.0);
        }
    }
}

/// B-6 direction pinning (update side): a mismatched block lying entirely
/// BELOW the band (closer to the content bottom — smaller distance values) is
/// on the update side and must rebuild immediately.
#[test]
fn mismatch_fully_below_band_rebuilds_immediately() {
    let blocks = vec![block(1, 4)]; // base_row_count = 7
    let mut cache = fresh_synced(&blocks, 80);
    cache.take_hit_miss_counts();
    let band = BandSync {
        low_rows: 17,
        high_rows: 27,
    };
    cache.sync_blocks(&blocks, 78, band);
    assert_eq!(
        cache.get(1).cols,
        78,
        "below-band block is on the update side"
    );
    assert_eq!(cache.deferred_count(), 0);
    assert_eq!(cache.take_hit_miss_counts().1, 1);
}

/// B-6 direction pinning (old-history side): a mismatched block entirely
/// ABOVE the band defers — stale cols entry kept, zero rebuilds. A block
/// anchored at row 0 is only ever "fully above" the degenerate high=0 band
/// (its whole interval lies above it); multi-block above-band coverage is
/// pinned by the truth table and the P1 append-only test.
#[test]
fn mismatch_fully_above_band_defers() {
    let blocks = vec![block(1, 4)];
    let mut cache = fresh_synced(&blocks, 80);
    cache.take_hit_miss_counts();
    let band = BandSync {
        low_rows: 0,
        high_rows: 0,
    };
    cache.sync_blocks(&blocks, 78, band);
    assert_eq!(cache.get(1).cols, 80, "deferred entry keeps its stale cols");
    assert_eq!(cache.deferred_count(), 1);
    assert_eq!(cache.take_hit_miss_counts().1, 0);
}

/// B-6 P1 scenario: drag defers a block → cols settle → the user scrolls the
/// deferred block into the band → the append-only sync's band check rebuilds
/// it THE SAME FRAME (the idle pump is gated off while streaming, so this
/// path is the only correction during output flow).
#[test]
fn append_only_sync_band_check_rebuilds_deferred_block_in_band_same_frame() {
    let blocks = vec![block(1, 4), block(2, 5), block(3, 6)];
    let mut cache = fresh_synced(&blocks, 80);

    // Drag frame: cols 78, band covering the two newest intervals only
    // (id3 [0,9), id2 [9,17)); id1's bottom sits at 17 → deferred.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 17,
        },
    );
    assert_eq!(cache.deferred_count(), 1);
    assert_eq!(cache.get(1).cols, 80);
    cache.take_hit_miss_counts();

    // Cols stable + scrolled: same blocks and cols → append-only fast path —
    // the band check must still correct the pending block this frame.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 40,
        },
    );
    assert_eq!(cache.deferred_count(), 0);
    assert_eq!(cache.get(1).cols, 78);
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 1, "scrolled-in deferred block rebuilds immediately");
}

/// B-6 metrics scalar semantics: deferred blocks read stored scalars only —
/// zero `completed_output_rows` fallbacks, and the composition matches the
/// prefix sum's stale `base_row_count` (同源同值).
#[test]
fn metrics_read_stale_scalars_for_deferred_blocks_without_rebuilds() {
    let mut terminal = Terminal::new(24, 80);
    terminal.process(
        b"\x1b]133;A\x07echo one\x1b]133;B\x07\x1b]133;C\x07alpha beta gamma\r\ndelta\r\n\x1b]133;D;0\x07",
    );
    terminal.process(
        b"\x1b]133;A\x07echo two\x1b]133;B\x07\x1b]133;C\x07epsilon zeta\r\n\x1b]133;D;0\x07",
    );
    let blocks: Vec<Block> = terminal.block_tracker().session_blocks().to_vec();
    assert_eq!(blocks.len(), 2);

    let mut cache = BlockLayoutCache::default();
    for blk in &blocks {
        cache.ensure_cached(blk, 80);
    }
    // Defer the OLDEST block: its bottom = the newest block's full height.
    let high = cache.get(blocks[1].id.0).base_row_count;
    cache.sync_blocks(
        &blocks,
        60,
        BandSync {
            low_rows: 0,
            high_rows: high,
        },
    );
    assert_eq!(cache.deferred_count(), 1, "oldest block defers");
    assert_eq!(cache.get(blocks[0].id.0).cols, 80, "stale entry kept");
    assert_eq!(cache.get(blocks[1].id.0).cols, 60, "newest rebuilt in band");

    // Metrics path: every block has an entry → scalar reads, zero fallbacks.
    assert_eq!(cache.metrics_fallback_rebuilds(), 0);
    let (total, _) = block_content_metrics_with_cache(&terminal, 60, 1, Some(&cache), None);
    assert_eq!(
        cache.metrics_fallback_rebuilds(),
        0,
        "deferred must not rebuild"
    );

    // Same-source consistency: the deferred scalar composition equals the
    // metrics total, and stale_output_rows is exactly what base_row_count
    // was composed from.
    let deferred = cache.get(blocks[0].id.0);
    assert_eq!(
        deferred.stale_output_rows,
        deferred.width.hint_rows as usize + deferred.width.rows.len()
    );
    assert_eq!(
        deferred.base_row_count,
        deferred.stale_output_rows
            + command_output_gap_rows(deferred.stale_output_rows)
            + deferred.command_wrap_rows
            + 1
    );
    let viewport_rows = terminal.grid().num_rows;
    let mut expected = 0;
    for blk in &blocks {
        let c = cache.get(blk.id.0);
        let rows = if blk.collapsed {
            0
        } else {
            c.stale_output_rows
        };
        expected += completed_block_row_count(rows, 1, c.command_wrap_rows)
            + clear_block_spacer_rows(&blk.command, viewport_rows);
    }
    assert_eq!(total, expected, "metrics total == stale scalar composition");
}

/// B-6: after a deferral the prefix sum stays monotonic (one stale-but-
/// self-consistent height per block) and the get_if_cached metrics path
/// still computes scroll totals. (The tracker re-ids blocks it replays, so
/// the metrics total here exercises the fallback; scalar reads for deferred
/// ids are pinned by the test above.)
#[test]
fn deferred_prefix_sum_stays_monotonic_and_metrics_stay_computable() {
    let blocks = vec![block(1, 4), block(2, 5), block(3, 6)];
    let mut cache = fresh_synced(&blocks, 80);
    // Band over the two newest only → oldest (bottom = 17) defers.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 17,
        },
    );
    assert_eq!(cache.deferred_count(), 1);

    let ps = cache.prefix_sum();
    assert!(ps.windows(2).all(|w| w[0] <= w[1]), "monotonic: {ps:?}");
    assert_eq!(ps.last().copied(), Some(7 + 8 + 9));

    let (total, visible) =
        block_content_metrics_with_cache(&terminal_for_blocks(&blocks), 78, 1, Some(&cache), None);
    assert!(total > 0);
    assert!(total >= visible);
}

fn terminal_for_blocks(blocks: &[Block]) -> Terminal {
    let mut terminal = Terminal::new(24, 80);
    for blk in blocks {
        terminal.process(
            format!(
                "\x1b]133;A\x07{}\x1b]133;B\x07\x1b]133;C\x07{}\x1b]133;D;0\x07",
                blk.command,
                blk.output.replace('\n', "\r\n")
            )
            .as_bytes(),
        );
    }
    terminal
}

/// B-6: the pump drains the pending set one block per frame and the converged
/// geometry equals a one-shot full build, value for value.
#[test]
fn pump_drains_deferred_and_converges_to_one_shot_build() {
    let blocks: Vec<Block> = (1..=8).map(|i| block(i, i as usize)).collect();
    let mut one_shot = BlockLayoutCache::default();
    for blk in &blocks {
        one_shot.ensure_cached(blk, 78);
    }

    let mut cache = fresh_synced(&blocks, 80);
    cache.take_hit_miss_counts(); // reset the initial cold-sync counters
                                  // Degenerate band (high=0): every mismatched block is "fully above" → all
                                  // eight defer.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    assert_eq!(cache.deferred_count(), 8);
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 0, "deferral must not rebuild");

    // P2-1: the pump drains only on cols-stable frames — settle the cols
    // first (same blocks, same cols → append-only → last_sync_stable).
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 0,
        },
    );
    assert_eq!(
        cache.deferred_count(),
        8,
        "settle sync keeps the pending set"
    );
    cache.take_hit_miss_counts();

    let mut frames = 0;
    while cache.deferred_count() > 0 {
        cache.pump_deferred(&blocks, 78, full_band(), 1);
        frames += 1;
        assert!(frames <= 8, "pump must converge in ≤8 frames");
    }
    assert_eq!(frames, 8, "one block per frame");

    for blk in &blocks {
        let pumped = cache.get(blk.id.0);
        let reference = one_shot.get(blk.id.0);
        assert_eq!(pumped.cols, 78);
        assert_eq!(pumped.base_row_count, reference.base_row_count);
        assert_eq!(pumped.stale_output_rows, reference.stale_output_rows);
        assert_eq!(pumped.command_wrap_rows, reference.command_wrap_rows);
        assert_eq!(pumped.width.rows.len(), reference.width.rows.len());
    }

    // Converged: a steady-state sync is inert (no misses, nothing pending).
    cache.take_hit_miss_counts();
    cache.sync_blocks(&blocks, 78, full_band());
    assert_eq!(cache.deferred_count(), 0);
    assert_eq!(cache.take_hit_miss_counts().1, 0);
}

/// B-6: both sync callers derive the SAME band — one shared pure function
/// whose geometry is pinned here (≥1 viewport overscan per side, low clamps
/// at 0).
#[test]
fn band_derivation_is_identical_for_both_sync_call_sites() {
    for &(scroll, viewport) in &[(0usize, 40usize), (7, 30), (100, 24)] {
        // Paint path (block_view.rs) and hit-testing path (rows.rs) run the
        // same expression: BandSync::for_viewport(block_scroll, viewport).
        let paint_site = BandSync::for_viewport(scroll, viewport);
        let hit_test_site = BandSync::for_viewport(scroll, viewport);
        assert_eq!(paint_site, hit_test_site);
        assert_eq!(paint_site.low_rows, scroll.saturating_sub(viewport));
        assert_eq!(paint_site.high_rows, scroll + viewport * 2);
    }
    let degenerate = BandSync::for_viewport(0, 0);
    assert_eq!((degenerate.low_rows, degenerate.high_rows), (0, 2));
    // scroll > overscan keeps the full window: for_viewport(5, 0) → [4, 7).
    let scrolled = BandSync::for_viewport(5, 0);
    assert_eq!((scrolled.low_rows, scrolled.high_rows), (4, 7));
}

/// B-6: explicit invalidation outranks band deferral — an invalidated block
/// above the band still rebuilds immediately and leaves the pending set.
#[test]
fn explicit_invalidation_rebuilds_even_above_the_band() {
    let blocks = vec![block(1, 4), block(2, 6)];
    let mut cache = fresh_synced(&blocks, 80);
    // id1's bottom = base(id2) = 9 → band high 9 defers it.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 9,
        },
    );
    assert_eq!(cache.deferred_count(), 1);

    cache.invalidate(1);
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 9,
        },
    );
    assert_eq!(cache.get(1).cols, 78, "invalidation rebuilds despite band");
    assert_eq!(
        cache.deferred_count(),
        0,
        "rebuilt block leaves pending set"
    );
}

/// P2-1: the pump stays idle while `cols` keeps changing (drag frames) —
/// whatever it rebuilt would be re-deferred on the next frame — and resumes
/// draining on a cols-stable (append-only) frame.
#[test]
fn pump_is_inert_while_cols_keep_changing_and_resumes_when_stable() {
    let blocks = vec![block(1, 4), block(2, 5), block(3, 6)];
    let mut cache = fresh_synced(&blocks, 80);
    cache.take_hit_miss_counts();

    // Drag frame 1: cols 78 — id1 (bottom = 17) defers above the band.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: 17,
        },
    );
    assert_eq!(cache.deferred_count(), 1);
    cache.take_hit_miss_counts();

    // Drag frame 2: cols change again — the pump must NOT drain.
    cache.sync_blocks(
        &blocks,
        76,
        BandSync {
            low_rows: 0,
            high_rows: 17,
        },
    );
    cache.pump_deferred(&blocks, 76, full_band(), 1);
    assert_eq!(cache.deferred_count(), 1, "drag frame: pump must stay idle");
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 2, "only the in-band sync rebuilds, no pump rebuild");

    // Cols settle (append-only frame): the pump resumes draining.
    cache.sync_blocks(
        &blocks,
        76,
        BandSync {
            low_rows: 0,
            high_rows: 17,
        },
    );
    cache.pump_deferred(&blocks, 76, full_band(), 1);
    assert_eq!(cache.deferred_count(), 0, "stable frame: pump drains");
}

/// Block with `lines` fixed-width (non-breaking) output lines — deterministic
/// soft-wrap counts: `ceil(line_chars / cols)` visual rows per line.
fn block_of_line_width(id: u64, lines: usize, line_chars: usize) -> Block {
    let output = (0..lines)
        .map(|_| format!("{}\n", "x".repeat(line_chars)))
        .collect::<String>();
    Block {
        id: BlockId(id),
        command: "echo x".to_string(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }
}

/// P2-2(b) regression: on a cols-GROW frame the in-band giant block rebuilds
/// SHORTER, so walk 1's pre-rebuild estimate sits ABOVE the neighbor's true
/// position and misclassifies it `Defer`. The correctness contract is the
/// SECOND walk (`rebuild_pending_in_band`) running after walk 1's rebuilds
/// and reading post-rebuild bases — the mis-deferred block must be rebuilt
/// in the SAME sync (no one-frame stale render). Guards against silently
/// reordering the two walks.
#[test]
fn in_band_giant_shrink_corrects_below_band_deferral_same_frame() {
    // Giant: 40 lines x 500 chars → ~7 rows/line at cols 80 (base ≈ 283),
    // 1 row/line at cols 800 (base ≈ 43). Neighbor: 4 short lines (base 7).
    let giant = block_of_line_width(8, 40, 500);
    let neighbor = block(7, 4);
    let blocks = vec![neighbor, giant]; // oldest first, newest last

    let mut cache = fresh_synced(&blocks, 80);
    cache.take_hit_miss_counts();
    let base_old = cache.get(8).base_row_count;
    assert!(base_old >= 80, "giant must tower over the band at cols 80");

    // Cols-grow frame: band [0, 80). Walk 1 sees id7's bottom at base_old
    // (≥ 80) and misclassifies the visible neighbor as `Defer`.
    cache.sync_blocks(
        &blocks,
        800,
        BandSync {
            low_rows: 0,
            high_rows: 80,
        },
    );

    let base_new = cache.get(8).base_row_count;
    assert!(base_new < 80, "giant must shrink into the band at cols 800");
    assert_eq!(
        cache.get(7).cols,
        800,
        "walk 2 must correct the mis-deferred neighbor in the SAME sync"
    );
    assert_eq!(cache.deferred_count(), 0, "nothing stays pending");
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 2, "giant + corrected neighbor both rebuild");
}
