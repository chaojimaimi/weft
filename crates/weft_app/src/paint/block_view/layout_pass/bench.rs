//! Criterion micro-benchmark for the shared layout pass: all_visible (±∞
//! clip) vs culling_heavy at 1k/5k/10k/50k blocks. Extracted from
//! layout_pass.rs (M5-b, budget split).

use super::{compute_block_layout_pass, BlockLayoutCache, LayoutPassInput};
use crate::paint::live_cache::LiveLayoutCache;

/// Criterion micro-benchmark (run with `--release --ignored
/// --nocapture --test-threads=1`): all_visible (±∞ clip) vs.
/// culling_heavy (800px viewport) at 1k/5k/10k/50k blocks; the ratio
/// shows whether visibility culling is effective.
#[test]
#[ignore = "criterion micro-benchmark; run with --release --ignored --nocapture"]
fn bench_layout_pass() {
    use criterion::{black_box, Criterion};
    use std::sync::Arc;
    use std::time::SystemTime;
    use weft_core::blocks::{Block, BlockId};

    fn make_blocks(count: usize) -> Vec<Block> {
        let line = "x".repeat(80);
        let output: Arc<str> = Arc::from(
            (0..20u32)
                .map(|i| format!("{i:03}: {line}\n"))
                .collect::<String>()
                .as_str(),
        );
        (0..count)
            .map(|i| Block {
                id: BlockId(i as u64),
                command: format!("echo test_{i}"),
                cwd: Some("/home/user".into()),
                output: Arc::clone(&output),
                styled_output: None,
                exit_code: Some(0),
                started_at: SystemTime::now(),
                finished_at: Some(SystemTime::now()),
                collapsed: false,
                screen_origin: false,
            })
            .collect()
    }

    let mut criterion = Criterion::default().sample_size(10);
    let mut group = criterion.benchmark_group("layout_pass");

    for &count in &[1_000usize, 5_000, 10_000, 50_000] {
        let blocks = make_blocks(count);
        let mut cache = BlockLayoutCache::default();
        for b in &blocks {
            cache.ensure_cached(b, 80);
        }
        // Build prefix sum so the binary-search fast path is exercised.
        cache.build_prefix_sum(&blocks);

        // Scenario 1: all blocks visible (clip bounds = ±∞)
        group.bench_function(format!("all_visible/{count}"), |b| {
            b.iter(|| {
                let input = LayoutPassInput {
                    blocks: &blocks,
                    live: None,
                    pane_session_id: 1,
                    cwd: None,
                    git_branch: None,
                    block_scroll: 0.0,
                    viewport_rows: 40,
                    cols: 80,
                    pitch: 20.0,
                    header_height: 24.0,
                    content_bottom_y: 800.0,
                    clip_top: -1e9,
                    clip_bottom: 1e9,
                    resolve_styles: true,
                    styled_lookup_counter: None,
                    block_diagnose_state: &std::collections::HashMap::new(),
                };
                let out = compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
                black_box(out.expanded_block_count);
            });
        });

        // Heavy culling: 800px viewport, scroll=0 — only ~2 blocks
        // visible; the rest accumulate cursor_dist without expansion.
        group.bench_function(format!("culling_heavy/{count}"), |b| {
            b.iter(|| {
                let input = LayoutPassInput {
                    blocks: &blocks,
                    live: None,
                    pane_session_id: 1,
                    cwd: None,
                    git_branch: None,
                    block_scroll: 0.0,
                    viewport_rows: 40,
                    cols: 80,
                    pitch: 20.0,
                    header_height: 24.0,
                    content_bottom_y: 800.0,
                    clip_top: 0.0,
                    clip_bottom: 800.0,
                    resolve_styles: true,
                    styled_lookup_counter: None,
                    block_diagnose_state: &std::collections::HashMap::new(),
                };
                let out = compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
                black_box(out.expanded_block_count);
            });
        });
    }

    group.finish();
    criterion.final_summary();
}
