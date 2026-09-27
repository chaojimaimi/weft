//! Criterion micro-benchmark for the shared layout pass: all_visible (±∞
//! clip) vs culling_heavy at 1k/5k/10k/50k blocks. Extracted from
//! layout_pass.rs (M5-b, budget split).

use super::{compute_block_layout_pass, BlockLayoutCache, LayoutPassInput};
use crate::paint::grid_cache::MAX_LAYOUT_LINES_LIVE;
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
                    now: std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_000),
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
                    now: std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_700_000_000),
                };
                let out = compute_block_layout_pass(input, &cache, &mut LiveLayoutCache::default());
                black_box(out.expanded_block_count);
            });
        });
    }

    group.finish();
    criterion.final_summary();
}

/// M6-a live-sync gate (PLAN_M6 §五.2): appending a 4KB tail to a 1M-line
/// newline-terminated document must sync incrementally in ≤ 50µs — the
/// whole point of the rewrite-watermark guard (O(new bytes + one partial
/// line) instead of O(document)). Same-scene calibration prints, no gate:
/// the 4KB as a single growing partial line (the inherent per-sync cost of
/// an unterminated stream) and the one-shot full-rebuild baseline.
///
/// Run (release 口径 — the gate is asserted only in release builds):
/// `cargo test --release -p weft_app --bin weft bench_live_sync_incremental
///  -- --ignored --nocapture --test-threads=1`
///
/// Gate style follows `paint::block_view::bench` (G1/G2): min over timed
/// runs after warm-up, asserted in release only.
#[test]
#[ignore = "M6-a gate bench; run with --release --ignored --nocapture --test-threads=1"]
fn bench_live_sync_incremental() {
    use std::time::{Duration, Instant};

    fn measure_us(label: &str, runs: u32, mut f: impl FnMut() -> Duration) -> Duration {
        // 3 warm-up runs then take the MIN over the timed runs — the
        // cleanest signal on a shared machine (same convention as the
        // G1/G2 bench).
        let mut best = Duration::MAX;
        for i in 0..runs + 3 {
            let d = f();
            if i >= 3 && d < best {
                best = d;
            }
        }
        println!(
            "  {label:<44} min {:.2} µs ({runs} runs)",
            best.as_secs_f64() * 1e6
        );
        best
    }

    const LINES: usize = 1_000_000;
    const COLS: usize = 80;
    const TAIL_LINES: usize = 400; // 400 × 10B = 4KB

    let doc = (0..LINES)
        .map(|i| format!("line-{i}\n"))
        .collect::<String>();
    let tail = (0..TAIL_LINES)
        .map(|i| format!("tail-{i:04}\n"))
        .collect::<String>();
    assert_eq!(tail.len(), 4_000);

    // Full-rebuild baseline (fresh cache) — calibration, no gate.
    let t_full = measure_us("full rebuild 1M lines (baseline)", 3, || {
        let mut fresh = LiveLayoutCache::default();
        let start = Instant::now();
        fresh.sync(&doc, 1, 1, COLS, false, usize::MAX);
        assert_eq!(fresh.total_lines(), MAX_LAYOUT_LINES_LIVE);
        start.elapsed()
    });

    // Incremental append of the 4KB tail — GATE ≤ 50µs.
    let mut grown = doc.clone();
    let mut cache = LiveLayoutCache::default();
    cache.sync(&grown, 1, 1, COLS, false, usize::MAX);
    let mut version = 1u64;
    let t_append = measure_us("incremental append 4KB (GATE <= 50us)", 20, || {
        grown.push_str(&tail);
        version += 1;
        let start = Instant::now();
        cache.sync(&grown, 1, version, COLS, false, usize::MAX);
        start.elapsed()
    });
    // measure_us runs 3 warm-ups + 20 timed runs — every one must have
    // taken the append fast path.
    assert_eq!(cache.appends(), 23, "append fast path must have been taken");
    assert_eq!(cache.total_lines(), MAX_LAYOUT_LINES_LIVE);

    // Single long line growing without a newline: the partial line is
    // re-read every sync — inherent cost of an unterminated stream,
    // calibration only (no gate).
    let mut partial = "x".repeat(4096);
    let mut pcache = LiveLayoutCache::default();
    pcache.sync(&partial, 1, 1, COLS, false, usize::MAX);
    let mut pversion = 1u64;
    let t_partial = measure_us("single-line growth 4KB (calibration)", 20, || {
        partial.push_str(&"y".repeat(4096));
        pversion += 1;
        let start = Instant::now();
        pcache.sync(&partial, 1, pversion, COLS, false, usize::MAX);
        start.elapsed()
    });

    // Gate — release 口径 (debug builds exceed the budget by construction).
    if cfg!(not(debug_assertions)) {
        assert!(
            t_append <= Duration::from_micros(50),
            "M6-a FAIL: incremental 4KB append sync {t_append:?} > 50µs"
        );
        println!("  GATE (append 4KB <= 50µs): PASS");
    } else {
        println!("  debug build: gate not asserted (release 口径)");
    }
    println!("  calibration: full-rebuild baseline {t_full:?} | partial-line growth {t_partial:?}");
}
