//! M6-c (PLAN_M6 §三) tests: table-byte budget enforcement, degraded
//! entries, and the `table_bytes_total` incremental-maintenance invariant.
//! Companion file to `tests.rs` (same `#[path]` pattern as
//! `visual_rows_tests.rs`) to keep both under the 800-line gate.

use super::*;
use crate::block_component::{
    block_content_metrics_with_cache, clear_block_spacer_rows, command_output_gap_rows,
    completed_block_row_count,
};
use std::collections::HashMap;
use weft_core::blocks::{Block, BlockId};
use weft_core::vt::Terminal;

/// Block with `lines` short non-wrapping output lines; table bytes grow
/// strictly with `lines` (more graphemes + more line_meta entries).
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

/// Degenerate band: intersects nothing, so the budget's band exemption
/// shields no block (pure newest-K exemption remains).
fn no_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: 0,
    }
}

fn fresh_synced(blocks: &[Block], cols: usize) -> BlockLayoutCache {
    let mut cache = BlockLayoutCache::default();
    cache.sync_blocks(blocks, cols, full_band());
    cache
}

/// Degrade exactly the oldest `count` blocks (they sit outside the newest-4
/// exemption when `blocks.len() > 4`) via a budget pinched to their exact
/// byte size, oldest first.
fn degrade_oldest(cache: &mut BlockLayoutCache, blocks: &[Block], count: usize) {
    let mut budget = cache.table_bytes_total();
    for blk in blocks.iter().take(count) {
        budget -= cache.get(blk.id.0).estimated_table_bytes();
    }
    budget += count * std::mem::size_of::<u32>(); // degraded entries keep the line_row_base [0] sentinel
    cache.enforce_table_budget(blocks, no_band(), budget);
}

/// The estimate formula is pinned to the M5 structure field sizes (GEntry
/// 8B / VisualRow 16B, compile-pinned in visual_rows.rs) — L1 graphemes +
/// line_meta, L2 rows + line_row_base.
#[test]
fn estimated_table_bytes_matches_m5_structure_sizes() {
    let blk = block(1, 4);
    let layout = compute_block_layout(&blk, 80);
    let expected = layout.content.graphemes.len() * std::mem::size_of::<visual_rows::GEntry>()
        + layout.content.line_meta.len() * std::mem::size_of::<visual_rows::LMeta>()
        + layout.width.rows.len() * std::mem::size_of::<visual_rows::VisualRow>()
        + layout.width.line_row_base.len() * std::mem::size_of::<u32>();
    assert_eq!(layout.estimated_table_bytes(), expected);
    assert!(layout.estimated_table_bytes() > 0);
    // Formula documentation pins from visual_rows.rs.
    assert_eq!(std::mem::size_of::<visual_rows::GEntry>(), 8);
    assert_eq!(std::mem::size_of::<visual_rows::VisualRow>(), 16);
}

/// Budget enforcement degrades the OLDEST blocks first, exactly until the
/// total fits; every scalar survives (prefix sum / metrics zero drift) and
/// only the tables are dropped.
#[test]
fn enforce_degrades_oldest_first_and_preserves_scalars() {
    let blocks: Vec<Block> = (1..=6).map(|i| block(i, 4 + i as usize)).collect();
    let mut cache = fresh_synced(&blocks, 80);

    let bytes: Vec<usize> = blocks
        .iter()
        .map(|blk| cache.get(blk.id.0).estimated_table_bytes())
        .collect();
    let total0: usize = bytes.iter().sum();
    assert_eq!(cache.table_bytes_total(), total0);
    let ps_before = cache.prefix_sum().to_vec();
    let stale_before: Vec<usize> = blocks
        .iter()
        .map(|blk| cache.get(blk.id.0).stale_output_rows)
        .collect();
    let scalar_of = |cache: &BlockLayoutCache, id: u64| {
        let c = cache.get(id);
        (
            c.base_row_count,
            c.stale_output_rows,
            c.command_wrap_rows,
            c.is_clear,
            c.cols,
            c.foldable,
            c.collapsed,
            c.output_len,
        )
    };
    let scalars_before: Vec<_> = blocks
        .iter()
        .map(|blk| scalar_of(&cache, blk.id.0))
        .collect();

    // Budget pinched so exactly ONE degrade (the oldest, id 1) fits it.
    let budget_a = total0 - bytes[0] + std::mem::size_of::<u32>();
    cache.enforce_table_budget(&blocks, no_band(), budget_a);
    assert_eq!(
        cache.degraded_count(),
        1,
        "exactly the oldest block degrades"
    );
    assert!(cache.get(1).degraded);
    assert!(!cache.get(2).degraded, "walk stops once under budget");
    assert!(cache.table_bytes_total() <= budget_a);

    // Second pinch: id 2 degrades too; ids 3..6 stay tabled (newest-4).
    let budget_b = cache.table_bytes_total() - bytes[1] + std::mem::size_of::<u32>();
    cache.enforce_table_budget(&blocks, no_band(), budget_b);
    assert_eq!(cache.degraded_count(), 2);
    assert!(cache.get(2).degraded);
    for blk in &blocks[2..] {
        assert!(
            !cache.get(blk.id.0).degraded,
            "id {} is newest-4 exempt",
            blk.id.0
        );
    }
    assert!(cache.table_bytes_total() <= budget_b);

    // Zero drift: prefix sum and per-block scalars identical throughout.
    assert_eq!(cache.prefix_sum(), ps_before.as_slice());
    for (index, blk) in blocks.iter().enumerate() {
        let entry = cache.get(blk.id.0);
        assert_eq!(
            entry.stale_output_rows, stale_before[index],
            "id {}",
            blk.id.0
        );
        assert_eq!(
            scalar_of(&cache, blk.id.0),
            scalars_before[index],
            "id {}",
            blk.id.0
        );
    }
    // Degraded shells: tables gone, structural invariant kept.
    let d = cache.get(1);
    assert!(d.content.line_meta.is_empty() && d.width.rows.is_empty());
    assert_eq!(d.width.line_row_base, vec![0]);
    assert_eq!(d.estimated_table_bytes(), std::mem::size_of::<u32>());

    // Incremental total still equals the full recompute after both degrades.
    assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());

    // A steady sync afterwards changes nothing: under the production budget
    // the enforcement early-returns and degraded entries stay degraded.
    cache.sync_blocks(&blocks, 80, no_band());
    assert_eq!(cache.degraded_count(), 2);
    assert_eq!(cache.prefix_sum(), ps_before.as_slice());
    assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());
}

/// Exemptions: a block intersecting the band keeps its tables, as do the
/// newest 4. When everything left is exempt the total legitimately stays
/// over budget (walk exhausted) and nothing further degrades.
#[test]
fn enforce_exempts_band_intersecting_and_newest_blocks() {
    let blocks: Vec<Block> = (1..=6).map(|i| block(i, 4 + i as usize)).collect();
    let mut cache = fresh_synced(&blocks, 80);
    let bases: Vec<usize> = blocks
        .iter()
        .map(|blk| cache.get(blk.id.0).base_row_count)
        .collect();
    // Bottom-anchored spans, oldest-first: block i's span starts at the sum
    // of all NEWER bases.
    let span = |i: usize| -> (usize, usize) {
        let start: usize = bases[i + 1..].iter().sum();
        (start, start + bases[i])
    };

    // Band exactly over block 2's span (index 1): adjacent blocks only touch
    // its edges, so ONLY block 2 intersects. Exempt = {block 2} ∪ newest 4
    // (ids 3..6) → the oldest block (id 1) is the sole candidate.
    let (lo, hi) = span(1);
    cache.enforce_table_budget(
        &blocks,
        BandSync {
            low_rows: lo,
            high_rows: hi,
        },
        0,
    );
    assert_eq!(
        cache.degraded_count(),
        1,
        "band-exempt block survives budget 0"
    );
    assert!(!cache.get(2).degraded);
    assert!(cache.get(1).degraded);
    assert!(
        cache.table_bytes_total() > 0,
        "exempt entries keep their tables"
    );

    // Fresh cache, full band: EVERY block intersects → nothing degradable →
    // the walk exhausts and the total stays over budget (documented edge).
    let mut cache = fresh_synced(&blocks, 80);
    let bytes_total = cache.table_bytes_total();
    cache.enforce_table_budget(&blocks, full_band(), 0);
    assert_eq!(
        cache.degraded_count(),
        0,
        "full-band frame exempts everything"
    );
    assert_eq!(cache.table_bytes_total(), bytes_total);
}

/// B-6 P1 integration (复审 2 轮): a degraded block scrolled into the band
/// is Both-rebuilt THE SAME frame — cols-fresh, so only the pending-set
/// membership can catch it; a Hit would hand the render empty tables.
/// (5 blocks: the newest-4 exemption leaves exactly the oldest degradable.)
#[test]
fn degraded_block_rebuilds_both_when_scrolled_into_band() {
    let blocks: Vec<Block> = (1..=5).map(|i| block(i, 4 + i as usize)).collect();
    let reference = fresh_synced(&blocks, 80);
    let mut cache = fresh_synced(&blocks, 80);

    degrade_oldest(&mut cache, &blocks, 1);
    assert!(cache.get(1).degraded);
    assert!(cache.get(1).width.rows.is_empty());
    cache.take_hit_miss_counts();

    // Cols UNCHANGED + same blocks → the append-only fast path. The band
    // check must still revive the degraded block this frame.
    cache.sync_blocks(&blocks, 80, full_band());

    assert_eq!(
        cache.degraded_count(),
        0,
        "revived block leaves the pending set"
    );
    let entry = cache.get(1);
    assert!(!entry.degraded);
    assert_eq!(
        entry.content.line_meta.len(),
        reference.get(1).content.line_meta.len(),
        "full L1 table restored"
    );
    assert_eq!(
        entry.width.rows,
        reference.get(1).width.rows,
        "full L2 table restored"
    );
    assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 1, "exactly one Both rebuild, no Hit");
}

/// ensure_cached on a degraded entry must take the Both path even when
/// every freshness signal matches (the degraded flag outranks the
/// three-state verdict — never a Hit over empty tables).
#[test]
fn ensure_cached_degraded_entry_is_both_not_hit() {
    let blocks = vec![
        block(1, 4),
        block(2, 5),
        block(3, 6),
        block(4, 7),
        block(5, 8),
    ];
    let mut cache = fresh_synced(&blocks, 80);
    degrade_oldest(&mut cache, &blocks, 1);
    cache.take_hit_miss_counts();

    cache.ensure_cached(&blocks[0], 80); // same cols → verdict would be None

    assert!(!cache.get(1).degraded);
    assert!(!cache.get(1).content.line_meta.is_empty());
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 1, "degraded entry must not report a Hit");
}

/// B-3 metrics semantics on degraded entries: present-but-degraded reads the
/// stored `stale_output_rows` scalar — zero fallback rebuilds, total equal to
/// the scalar composition (same source as the prefix sum).
#[test]
fn metrics_read_stale_scalars_for_degraded_blocks_without_rebuilds() {
    let mut terminal = Terminal::new(24, 80);
    for i in 1..=6 {
        terminal.process(
            format!(
                "\x1b]133;A\x07echo {i}\x1b]133;B\x07\x1b]133;C\x07alpha {i} beta\r\ndelta {i}\r\n\x1b]133;D;0\x07"
            )
            .as_bytes(),
        );
    }
    let blocks: Vec<Block> = terminal.block_tracker().session_blocks().to_vec();
    assert_eq!(blocks.len(), 6);

    let mut cache = BlockLayoutCache::default();
    for blk in &blocks {
        cache.ensure_cached(blk, 80);
    }
    let ps_before = {
        cache.build_prefix_sum(&blocks);
        cache.prefix_sum().to_vec()
    };
    degrade_oldest(&mut cache, &blocks, 2);
    assert_eq!(cache.degraded_count(), 2);
    assert_eq!(cache.metrics_fallback_rebuilds(), 0);

    let (total, _) = block_content_metrics_with_cache(&terminal, 80, 1, Some(&cache), None);
    assert_eq!(
        cache.metrics_fallback_rebuilds(),
        0,
        "degraded entries must not trigger completed_output_rows"
    );

    // Same-source composition: stale scalars only, degraded or not.
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
        assert_eq!(c.base_row_count, {
            let rows = c.stale_output_rows;
            rows + command_output_gap_rows(rows) + c.command_wrap_rows + 1
        });
    }
    assert_eq!(total, expected, "metrics total == stale scalar composition");
    // Prefix sum untouched by the degradation.
    cache.build_prefix_sum(&blocks);
    assert_eq!(cache.prefix_sum(), ps_before.as_slice());
}

/// The running `table_bytes_total` stays equal to a full per-entry recompute
/// across a whole sync/invalidate/degrade/revive/evict sequence.
#[test]
fn table_bytes_total_matches_recompute_across_sequence() {
    let mut blocks: Vec<Block> = (1..=5).map(|i| block(i, 3 + i as usize)).collect();
    let mut cache = BlockLayoutCache::default();
    let check = |cache: &BlockLayoutCache, ctx: &str| {
        assert_eq!(
            cache.table_bytes_total(),
            cache.recomputed_table_bytes(),
            "{ctx}"
        );
    };

    cache.sync_blocks(&blocks, 80, full_band());
    check(&cache, "cold sync");
    cache.sync_blocks(&blocks, 60, full_band());
    check(&cache, "cols change (classified WidthOnly rebuilds)");
    blocks.push(block(6, 30));
    cache.sync_blocks(&blocks, 60, full_band());
    check(&cache, "append");
    cache.invalidate(2);
    check(&cache, "invalidate (entry removed)");
    cache.sync_blocks(&blocks, 60, full_band());
    check(&cache, "dirty rebuild");
    degrade_oldest(&mut cache, &blocks, 1);
    assert!(cache.get(1).degraded);
    check(&cache, "degrade");
    cache.sync_blocks(&blocks, 60, full_band());
    assert!(
        !cache.get(1).degraded,
        "full band revives the degraded block"
    );
    check(&cache, "revive");
    let shrunk: Vec<Block> = blocks[2..].to_vec();
    cache.sync_blocks(&shrunk, 60, full_band());
    check(&cache, "history retention evict");
}

/// Degraded blocks don't disturb the pump: a degraded+deferred overlap id
/// drained by the pump Both-rebuilds and leaves both pending sets.
#[test]
fn pump_drains_degraded_deferred_overlap_with_full_rebuild() {
    let blocks: Vec<Block> = (1..=6).map(|i| block(i, 4 + i as usize)).collect();
    let reference = fresh_synced(&blocks, 78);
    let mut cache = fresh_synced(&blocks, 80);
    degrade_oldest(&mut cache, &blocks, 1);
    assert!(cache.get(1).degraded);

    // Cols-change frame with a band above id 1: the degraded entry has stale
    // cols, so classification defers it — the id now sits in BOTH sets.
    let bottom1: usize = blocks[1..]
        .iter()
        .map(|b| cache.get(b.id.0).base_row_count)
        .sum();
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: bottom1,
        },
    );
    assert_eq!(
        cache.deferred_count(),
        1,
        "cols-mismatch degraded block defers"
    );
    assert_eq!(cache.degraded_count(), 1, "degraded marker persists");

    // Cols settle, then the pump drains the overlap id: degraded-first makes
    // it a Both rebuild (full tables at the NEW cols), not a WidthOnly over
    // the empty L1.
    cache.sync_blocks(
        &blocks,
        78,
        BandSync {
            low_rows: 0,
            high_rows: bottom1,
        },
    );
    cache.pump_deferred(&blocks, 78, full_band(), 1);
    assert_eq!(cache.deferred_count(), 0);
    assert_eq!(cache.degraded_count(), 0);
    let entry = cache.get(1);
    assert!(!entry.degraded);
    assert_eq!(entry.cols, 78);
    assert_eq!(entry.width.rows, reference.get(1).width.rows);
    assert_eq!(cache.table_bytes_total(), cache.recomputed_table_bytes());
}

/// Reference equality helper kept out of the tests above: one-shot full
/// builds match revived entries row-for-row (HashMap import exercised).
#[test]
fn revived_entry_matches_one_shot_build() {
    let blocks: Vec<Block> = (1..=6).map(|i| block(i, 4 + i as usize)).collect();
    let mut reference: HashMap<u64, usize> = HashMap::new();
    let one_shot = fresh_synced(&blocks, 80);
    for blk in &blocks {
        reference.insert(blk.id.0, one_shot.get(blk.id.0).width.rows.len());
    }
    let mut cache = fresh_synced(&blocks, 80);
    degrade_oldest(&mut cache, &blocks, 2);
    cache.sync_blocks(&blocks, 80, full_band());
    for blk in &blocks {
        assert_eq!(cache.get(blk.id.0).width.rows.len(), reference[&blk.id.0]);
    }
}
