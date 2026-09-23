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

/// Appendix I optimization bench (2026-09-23): the field zoom-out shrink —
/// seq-shaped SHORT lines (no wrap splits) at maximized width, then the
/// restore shrink. Separates "index rebuild" cost from "wrap split" cost.
#[test]
#[ignore = "calibration bench — run explicitly with --release"]
fn bench_zoom_out_shrink_seq_short_lines() {
    let rows = 62usize;
    let cols = 200usize;
    let mut term = Terminal::with_scrollback(rows, cols, 200_000);
    // seq 1 18900, twice (field: two runs).
    let mut buf = String::new();
    for run in 0..2 {
        for i in 1..=18900 {
            buf.push_str(&(run * 18900 + i).to_string());
            buf.push_str("\r\n");
        }
    }
    term.process(buf.as_bytes());
    let depth = term.grid().scrollback_len();
    let mut durs = Vec::new();
    for i in 0..6 {
        let target = if i % 2 == 0 {
            (91usize, 33usize)
        } else {
            (cols, rows)
        };
        let t = Instant::now();
        term.resize(target.1, target.0);
        durs.push(t.elapsed());
    }
    println!(
        "G4 seq-shrink commits depth={depth}: {}",
        durs.iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Appendix I optimization bench: the WRAP-SPLIT shrink — wide lines that
/// actually split when the width narrows (200 cols -> 91 splits every line).
#[test]
#[ignore = "calibration bench — run explicitly with --release"]
fn bench_zoom_out_shrink_wrap_split() {
    let rows = 62usize;
    let cols = 200usize;
    let mut term = Terminal::with_scrollback(rows, cols, 400_000);
    let mut buf = String::new();
    for i in 1..=18900 {
        buf.push_str(&format!("row-{i:06}-{}", "x".repeat(cols - 16)));
        buf.push_str("\r\n");
    }
    term.process(buf.as_bytes());
    let depth = term.grid().scrollback_len();
    // First shrink materializes the split; alternate to re-split each time.
    let mut durs = Vec::new();
    for i in 0..6 {
        let target = if i % 2 == 0 { 91usize } else { cols };
        let t = Instant::now();
        term.resize(rows, target);
        durs.push(t.elapsed());
    }
    println!(
        "G6 wrap-split shrink commits depth={depth}: {}",
        durs.iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

/// Appendix K analysis (2026-09-23): right-aligned progress lines (brew
/// style -- name + padding spaces + progress at the print-time right edge)
/// through a shrink/grow cycle. Pins the STANDARD terminal semantics:
/// content preserved, wrap split lands inside the padding, no
/// re-alignment of historical lines. Run: cargo test -p weft_core --test
/// resize_commit_bench -- --ignored --nocapture verify_padded_rewrap
#[test]
#[ignore = "semantic verification — run explicitly"]
fn verify_padded_rewrap_semantics() {
    let mut term = Terminal::with_scrollback(62, 200, 200_000);
    let name = "Cask cockpit-tools (1.3.59)";
    let prog = "Downloaded  107.2MB/107.2MB";
    let pad = " ".repeat(200 - name.len() - prog.len() - 2);
    let printed = format!("{name}  {pad}{prog}");
    assert_eq!(printed.chars().count(), 200);
    term.process(printed.as_bytes());
    term.process(b"\r\n");

    fn doc_tail_rows(term: &Terminal, n: usize) -> Vec<String> {
        // The visible viewport holds the document tail after the resize
        // (rows=62 tall, content shorter than that at these sizes).
        let grid = term.grid();
        let mut out = Vec::new();
        for r in (0..grid.num_rows).rev() {
            let text = grid.row_text(r);
            if !text.trim().is_empty() {
                out.push(text.trim_end().to_string());
            }
            if out.len() == n {
                break;
            }
        }
        out.reverse();
        out
    }

    term.resize(62, 91);
    let shrunk = doc_tail_rows(&term, 3);
    println!("after shrink to 91 cols:");
    for (i, r) in shrunk.iter().enumerate() {
        println!("  row{i}: {r:?}");
    }
    let joined: String = shrunk.concat();
    assert!(joined.contains(name), "name preserved through shrink");
    assert!(joined.contains(prog), "progress preserved through shrink");

    term.resize(62, 200);
    let grown = doc_tail_rows(&term, 3);
    println!("after grow back to 200 cols:");
    for (i, r) in grown.iter().enumerate() {
        println!("  row{i}: {r:?}");
    }
    let joined2: String = grown.concat();
    assert!(joined2.contains(name), "name preserved through grow");
    assert!(joined2.contains(prog), "progress preserved through grow");
}
