//! `BlockLayoutCache` unit tests (split out of grid_cache.rs per the
//! commit-gate `*/tests.rs` budget exemption; M6-b pushed the file over
//! its audited ceiling).

use super::*;
use weft_core::blocks::{Block, BlockId};

fn mk_block_with_output(id: u64, command: &str, output: &str) -> Block {
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: None,
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: None,
        collapsed: false,
        screen_origin: false,
    }
}

/// Pre-M6-b semantics: a band covering everything defers nothing.
fn full_band() -> BandSync {
    BandSync {
        low_rows: 0,
        high_rows: usize::MAX,
    }
}

#[test]
fn block_layout_cache_computes_on_first_access() {
    let block = mk_block_with_output(1, "echo hello", "hello\nworld\n");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(layout.content.line_meta.len(), 2);
    assert!(layout.foldable);
}

#[test]
fn block_layout_cache_trims_trailing_empty() {
    let block = mk_block_with_output(1, "echo", "output\n\n\n");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(
        layout.content.line_meta.len(),
        1,
        "trailing empty lines should be trimmed"
    );
    assert_eq!(&block.output[layout.content.line_range(0)], "output");
}

#[test]
fn block_layout_cache_trims_trailing_prompt() {
    let block = mk_block_with_output(1, "echo", "output\n%\n$\n#\n");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(
        layout.content.line_meta.len(),
        1,
        "trailing prompt lines should be trimmed"
    );
}

#[test]
fn block_layout_cache_byte_offsets_correct() {
    let block = mk_block_with_output(1, "echo", "first\nsecond\nthird\n");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(layout.content.line_meta.len(), 3);
    assert_eq!(&block.output[layout.content.line_range(0)], "first");
    assert_eq!(&block.output[layout.content.line_range(1)], "second");
    assert_eq!(&block.output[layout.content.line_range(2)], "third");
}

#[test]
fn block_layout_cache_wraps_long_lines() {
    // 20 chars at cols=10 → 2 chunks
    let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
    let layout = compute_block_layout(&block, 10);
    assert_eq!(layout.content.line_meta.len(), 1);
    assert_eq!(layout.width.rows.len(), 2, "20 chars at cols=10 → 2 chunks");
    let line = &block.output[layout.content.line_range(0)];
    let base = layout.width.line_row_base[0] as usize;
    let row_text = |r: usize| layout.content.row_text(line, &layout.width.rows[base + r]);
    assert_eq!(row_text(0), "0123456789");
    assert_eq!(row_text(1), "abcdefghij");
}

#[test]
fn block_layout_cache_foldable_false_for_empty_output() {
    let block = mk_block_with_output(1, "true", "\n\n\n");
    let layout = compute_block_layout(&block, 80);
    assert!(!layout.foldable, "all-empty output should not be foldable");
    assert_eq!(layout.content.line_meta.len(), 0, "all lines trimmed");
}

#[test]
fn block_layout_cache_ensure_cached_reuses() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "hello\n");
    cache.ensure_cached(&block, 80);
    let layout1 = cache.get(1).clone();

    // Same content + cols → should NOT rebuild (same instance).
    cache.ensure_cached(&block, 80);
    let layout2 = cache.get(1).clone();
    assert_eq!(
        layout1.content.line_meta.len(),
        layout2.content.line_meta.len()
    );
    assert_eq!(layout1.cols, layout2.cols);
}

#[test]
fn block_layout_cache_rebuilds_on_output_change() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "hello\n");
    cache.ensure_cached(&block, 80);
    assert_eq!(cache.get(1).content.line_meta.len(), 1);

    // Output grew → cache should detect and rebuild.
    let block2 = mk_block_with_output(1, "echo", "hello\nworld\n");
    cache.ensure_cached(&block2, 80);
    assert_eq!(
        cache.get(1).content.line_meta.len(),
        2,
        "output change should trigger rebuild"
    );
}

#[test]
fn block_layout_cache_rebuilds_for_equal_length_replacement() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "abcdef\n");
    cache.ensure_cached(&block, 80);
    assert_eq!(cache.get(1).content.line_range(0).end, 6);

    // Same BlockId and byte length, but a different immutable allocation
    // and UTF-8 boundary. Reusing the old byte range could panic while
    // slicing the replacement output.
    let replacement = mk_block_with_output(1, "echo", "中文\n");
    assert_eq!(block.output.len(), replacement.output.len());
    cache.ensure_cached(&replacement, 80);
    let range = cache.get(1).content.line_range(0);
    assert_eq!(&replacement.output[range], "中文");
}

#[test]
fn block_layout_cache_rebuilds_on_cols_change() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
    cache.ensure_cached(&block, 10);
    assert_eq!(
        cache.get(1).width.rows.len(),
        2,
        "20 chars / cols=10 → 2 chunks"
    );

    // Resize to cols=20 → should rebuild with 1 chunk.
    cache.ensure_cached(&block, 20);
    assert_eq!(
        cache.get(1).width.rows.len(),
        1,
        "20 chars / cols=20 → 1 chunk"
    );
}

#[test]
fn block_layout_cache_rebuilds_on_collapse_toggle() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "hello\n");
    cache.ensure_cached(&block, 80);
    assert!(!cache.get(1).collapsed);

    let mut block2 = block.clone();
    block2.collapsed = true;
    cache.ensure_cached(&block2, 80);
    assert!(
        cache.get(1).collapsed,
        "collapse toggle should trigger rebuild"
    );
}

#[test]
fn stable_history_sync_checks_only_newest_block() {
    let blocks: Vec<_> = (1..=1000)
        .map(|id| mk_block_with_output(id, "echo", "one line\n"))
        .collect();
    let mut cache = BlockLayoutCache::default();
    cache.sync_blocks(&blocks, 80, full_band());
    cache.take_hit_miss_counts();

    cache.sync_blocks(&blocks, 80, full_band());
    let (hits, misses) = cache.take_hit_miss_counts();
    assert_eq!((hits, misses), (1, 0));
}

#[test]
fn explicit_invalidation_rebuilds_non_newest_block() {
    let mut blocks = vec![
        mk_block_with_output(1, "echo", "old\n"),
        mk_block_with_output(2, "echo", "new\n"),
    ];
    let mut cache = BlockLayoutCache::default();
    cache.sync_blocks(&blocks, 80, full_band());
    cache.take_hit_miss_counts();

    blocks[0].collapsed = true;
    cache.invalidate(1);
    cache.sync_blocks(&blocks, 80, full_band());
    assert!(cache.get(1).collapsed);
    let (_, misses) = cache.take_hit_miss_counts();
    assert_eq!(misses, 1);
}

#[test]
fn block_layout_cache_empty_output() {
    let block = mk_block_with_output(1, "true", "");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(layout.content.line_meta.len(), 0);
    assert!(!layout.foldable);
}

// ── R2-2 (Batch 7): prefix sum tests ───────────────────────────────

/// `base_row_count` includes one breathing row when output exists, plus
/// command + separator. Collapsed/empty-output blocks remain at `2`.
#[test]
fn base_row_count_matches_layout_formula() {
    // 3 output lines + breathing row + command + separator = 6.
    let block = mk_block_with_output(1, "echo", "a\nb\nc\n");
    let layout = compute_block_layout(&block, 80);
    assert_eq!(layout.base_row_count, 6);

    // Collapsed → output_rows=0, hint_rows=0 → base = 2
    let mut collapsed = block;
    collapsed.collapsed = true;
    let layout_c = compute_block_layout(&collapsed, 80);
    assert_eq!(layout_c.base_row_count, 2);

    // Empty output → output_rows=0 → base = 0 + 0 + 2 = 2
    let empty = mk_block_with_output(2, "true", "");
    let layout_e = compute_block_layout(&empty, 80);
    assert_eq!(layout_e.base_row_count, 2);
}

/// B6 回归:长命令折行必须计入 `command_wrap_rows`(旧 `+2` 常量低估高度),
/// 且缓存/回退两条 metrics 路径与单一来源 helper 一致,scrollbar/find 几何不漂移。
#[test]
fn command_wrap_rows_counts_wrapped_commands_and_stays_consistent() {
    let block = mk_block_with_output(1, "a very long command that surely wraps", "ok\n");
    let cols = 20;
    let layout = compute_block_layout(&block, cols);
    // 36 列命令,foldable → first_cols=17,折成 2 行。
    assert_eq!(layout.command_wrap_rows, 2);
    // 与 block_component 的单一来源 helper 同值(防量纲漂移)。
    assert_eq!(
        crate::block_component::command_wrap_rows_for(&block, cols, layout.foldable),
        layout.command_wrap_rows
    );
    // 折叠块命令恒 1 行。
    let mut collapsed = block.clone();
    collapsed.collapsed = true;
    assert_eq!(compute_block_layout(&collapsed, cols).command_wrap_rows, 1);

    // metrics 缓存路径(读 c.command_wrap_rows)必须与直接计算一致。
    let mut terminal = weft_core::vt::Terminal::new(24, cols);
    terminal.process(
        b"\x1b]133;A\x07a very long command that surely wraps\x1b]133;B\x07\x1b]133;C\x07ok\r\n\x1b]133;D;0\x07",
    );
    let (direct, _) = crate::block_component::block_content_metrics(&terminal, cols, 1);
    let mut cache = BlockLayoutCache::default();
    for blk in terminal.block_tracker().session_blocks() {
        cache.ensure_cached(blk, cols);
    }
    let (cached, _) = crate::block_component::block_content_metrics_with_cache(
        &terminal,
        cols,
        1,
        Some(&cache),
        None,
    );
    assert_eq!(cached, direct);
}

#[test]
fn build_prefix_sum_empty_blocks() {
    let mut cache = BlockLayoutCache::default();
    cache.build_prefix_sum(&[]);
    assert_eq!(cache.prefix_sum(), &[0]);
}

#[test]
fn build_prefix_sum_single_block() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "a\nb\nc\n"); // base=6
    cache.ensure_cached(&block, 80);
    cache.build_prefix_sum(&[block]);
    // prefix_sum[0]=0, prefix_sum[1]=6
    assert_eq!(cache.prefix_sum(), &[0, 6]);
}

#[test]
fn build_prefix_sum_multiple_blocks_cumulative() {
    let mut cache = BlockLayoutCache::default();
    // Newest-first ordering in `blocks` vec; build_prefix_sum walks
    // .rev() so prefix_sum[1] = newest block's base, prefix_sum[2] =
    // newest + second-newest, etc.
    // blocks[0] = oldest (id=1, base=2: empty output)
    // blocks[1] = newest (id=2, base=6: 3 lines + breathing row)
    let b1 = mk_block_with_output(1, "true", "");
    let b2 = mk_block_with_output(2, "echo", "a\nb\nc\n");
    let blocks = vec![b1, b2];
    cache.ensure_cached(&blocks[0], 80);
    cache.ensure_cached(&blocks[1], 80);
    cache.build_prefix_sum(&blocks);
    // rev() walks b2 (base=6) then b1 (base=2).
    // prefix_sum = [0, 6, 8]
    assert_eq!(cache.prefix_sum(), &[0, 6, 8]);
}

/// Rebuild is skipped when neither block IDs nor any cache entry changed.
/// This is the steady-state hot path (no rebuild per frame).
#[test]
fn build_prefix_sum_skips_rebuild_when_unchanged() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "a\nb\n");
    cache.ensure_cached(&block, 80);
    cache.build_prefix_sum(std::slice::from_ref(&block));
    let ps1 = cache.prefix_sum().to_vec();

    // Second call with same blocks — should be a no-op.
    cache.build_prefix_sum(&[block]);
    assert_eq!(cache.prefix_sum(), ps1.as_slice());
}

/// Adding a block triggers rebuild (ids_changed path).
#[test]
fn build_prefix_sum_rebuilds_on_block_added() {
    let mut cache = BlockLayoutCache::default();
    let b1 = mk_block_with_output(1, "echo", "a\n");
    cache.ensure_cached(&b1, 80);
    cache.build_prefix_sum(std::slice::from_ref(&b1));
    assert_eq!(cache.prefix_sum(), &[0, 4]); // output + breathing + structural = 4

    // Add a second block (older). blocks = [b2, b1] (b1 is newest).
    let b2 = mk_block_with_output(2, "echo", "x\ny\nz\n");
    cache.ensure_cached(&b2, 80);
    cache.build_prefix_sum(&[b2, b1.clone()]);
    // rev() → b1 (base=4) then b2 (base=6). prefix_sum = [0, 4, 10]
    assert_eq!(cache.prefix_sum(), &[0, 4, 10]);
}

/// A cache miss (output change) sets prefix_sum_dirty, forcing rebuild
/// on the next build_prefix_sum call even if IDs are unchanged.
#[test]
fn build_prefix_sum_rebuilds_on_output_change() {
    let mut cache = BlockLayoutCache::default();
    let block = mk_block_with_output(1, "echo", "a\n");
    cache.ensure_cached(&block, 80);
    cache.build_prefix_sum(std::slice::from_ref(&block));
    assert_eq!(cache.prefix_sum(), &[0, 4]);

    // Output grows → ensure_cached triggers rebuild, sets dirty.
    let block2 = mk_block_with_output(1, "echo", "a\nb\nc\nd\n");
    cache.ensure_cached(&block2, 80);
    cache.build_prefix_sum(&[block2]);
    // base = 4 output + breathing + command + separator = 7
    assert_eq!(cache.prefix_sum(), &[0, 7]);
}

/// Collapsed blocks contribute base_row_count=2 to the prefix sum,
/// matching the layout_pass formula (cursor_dist += pitch*0 + pitch
/// + header_height + pitch for command+header+separator).
#[test]
fn build_prefix_sum_handles_collapsed_blocks() {
    let mut cache = BlockLayoutCache::default();
    let mut b1 = mk_block_with_output(1, "echo", "a\nb\nc\n");
    b1.collapsed = true; // base = 2
    let b2 = mk_block_with_output(2, "echo", "x\n"); // base = 4
    let blocks = vec![b1, b2];
    cache.ensure_cached(&blocks[0], 80);
    cache.ensure_cached(&blocks[1], 80);
    cache.build_prefix_sum(&blocks);
    // rev() → b2 (base=4) then b1 (base=2). prefix_sum = [0, 4, 6]
    assert_eq!(cache.prefix_sum(), &[0, 4, 6]);
}
