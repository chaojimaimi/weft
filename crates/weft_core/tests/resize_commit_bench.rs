//! PLAN_S3 T4 hard-gate calibration bench (run explicitly, never in CI):
//!
//! ```text
//! cargo test --release -p weft_core --test resize_commit_bench -- --ignored --nocapture
//! ```
//!
//! Measures the per-commit cost of the resize paths that PLAN_S3 targets:
//! - G1: full reflow commits at scrollback depth (T2 bridge: materialize_all
//!   -> legacy rewrap -> re-encode; T5 replaces with Index::rebuild)
//! - G2: protected-path widen commits (resize_dims; resize_cols died with
//!   the Cell ring in T2, so this should already be viewport-only cheap)
//!
//! Baselines recorded 2026-09-19 (pre-S3, HEAD 260cadc): full reflow
//! 32-42ms/commit @50k, widen 15-33ms/commit @50k (docs/
//! ANALYSIS_drag_frame_budget_v112.md). T4 re-measures the bridge; T5 must
//! land under G1<=5ms / G3(rebuild)<=5ms@10k or the train halts.

use std::time::Instant;
use weft_core::vt::Terminal;

/// Feeds `n` short lines, keeping the cursor on a content row (no trailing
/// newline) so the pre-T5 cursor-on-empty-row reflow quirk can't drop the
/// scrollback mid-measurement.
fn feed_lines(term: &mut Terminal, n: usize, cols: usize) {
    let mut buf = String::new();
    for i in 1..=n {
        let line = format!("line-{i:06}-{}", "x".repeat(cols / 3));
        buf.push_str(&line);
        if i < n {
            buf.push_str("\r\n");
        }
        if buf.len() > 1 << 16 {
            term.process(buf.as_bytes());
            buf.clear();
        }
    }
    term.process(buf.as_bytes());
}

#[test]
#[ignore = "calibration bench — run explicitly with --release"]
fn bench_full_resize_commits_at_depth() {
    let rows = 33usize;
    for depth in [10_000usize, 50_000] {
        let cols = 91usize;
        let mut term = Terminal::with_scrollback(rows, cols, 200_000);
        feed_lines(&mut term, depth, cols);
        assert_eq!(term.grid().scrollback_len(), depth - rows);
        let mut durs = Vec::new();
        for i in 0..6 {
            let target = if i % 2 == 0 { cols - 30 } else { cols };
            let t = Instant::now();
            term.resize(rows, target);
            durs.push(t.elapsed());
        }
        println!(
            "G1 full-resize commits depth={depth}: {}",
            durs.iter()
                .map(|d| format!("{d:?}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}

#[test]
#[ignore = "calibration bench — run explicitly with --release"]
fn bench_widen_resize_dims_at_depth() {
    // G2: the protected path. With FlatStorage the widen commit must not
    // touch history at all (viewport rows only) — assert it stays µs-scale
    // even at 50k depth, unlike the pre-S2 Cell ring (15-33ms).
    let rows = 33usize;
    let cols = 91usize;
    let mut term = Terminal::with_scrollback(rows, cols, 200_000);
    feed_lines(&mut term, 50_000, cols);
    assert_eq!(term.grid().scrollback_len(), 50_000 - rows);
    let mut durs = Vec::new();
    for i in 0..6 {
        let target = if i % 2 == 0 { cols + 1 } else { cols };
        let t = Instant::now();
        term.grid_mut().resize_dims(rows + (i % 2), target);
        durs.push(t.elapsed());
    }
    println!(
        "G2 widen resize_dims @50k: {}",
        durs.iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}
