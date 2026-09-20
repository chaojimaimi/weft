//! M5-b (PLAN_M5 §五): four-phase headless bench for the L1/L2 block-view
//! frame path, locked to the G1/G2 gates.
//!
//! Phases (one 118k-line finished block, headless renderer, real
//! `build_block_view_vertices`):
//! 1. `cold`     — fresh cache: L1 + L2 build (`sync_blocks` all-miss).
//! 2. `warm`     — steady-state frame with a synced cache. **G1 gate ≤ 3ms.**
//! 3. `rebuild`  — explicit invalidation + rebuild at the SAME cols
//!    (collapse/content-style invalidation cost).
//! 4. `cols_chg` — full frame after a viewport-width change: L2 rebuild +
//!    layout + paint. **G2 gate ≤ 15ms.**
//!
//! Run (release 口径 — the gates are asserted only in release builds):
//! `cargo test --release -p weft_app --bin weft bench_block_view_four_phase
//!  -- --ignored --nocapture --test-threads=1`
//!
//! Precedent: `paint::block_view::layout_pass::tests::bench_layout_pass`
//! and `grid_bench.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use weft_core::blocks::{Block, BlockId};
use weft_core::config::Theme;
use weft_core::grid::Color;
use weft_core::selection::SelectionHandler;

use crate::paint::block_view_model::BlockViewPaintModel;
use crate::renderer::MetalRenderer;

/// Deterministic 118k-line output: short prose lines dominate, a fraction
/// wraps (long lines), plus structure lines — exercising word-aware breaks,
/// PureBox/TableRow clips and the windowed emission.
fn make_output(lines: usize) -> Arc<str> {
    let hex = "0123456789abcdef".repeat(20);
    let mut out = String::with_capacity(lines * 72);
    for i in 0..lines {
        match i % 16 {
            0 => out.push_str(&format!(
                "src/weft/mod_{i}.rs:1: use weft_core::grid::terminal_text_width;\n"
            )),
            4 | 9 => {
                // Long line → soft-wraps into ~3 visual rows at 88 cols.
                out.push_str(&format!(
                    "result[{i}] = {} payload sha 256 bits\n",
                    &hex[..160]
                ));
            }
            8 => out.push_str("────────────────────────────────────────────────────────\n"),
            12 => out.push_str(&format!("│ row {i} │ Data 426G / 926G │ 充裕 │\n")),
            _ => out.push_str(&format!("line-{i}: compilation finished in 1.20s\n")),
        }
    }
    Arc::from(out.as_str())
}

fn make_block(lines: usize) -> Block {
    Block {
        id: BlockId(1),
        command: "cargo build --release".to_string(),
        cwd: Some("/tmp/weft".to_string()),
        output: make_output(lines),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH,
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH),
        collapsed: false,
        screen_origin: false,
    }
}

fn frame_model<'a>(
    blocks: &'a [Block],
    palette: &'a [Color; 256],
    diagnose: &'a HashMap<BlockId, crate::app_state::BlockDiagnoseState>,
) -> BlockViewPaintModel<'a> {
    BlockViewPaintModel {
        blocks,
        live_head_lines: 0,
        region_bottom_y: 600.0,
        cwd: Some("/tmp/weft"),
        git_branch: None,
        live: None,
        block_scroll: 0.0,
        viewport_rows: 40,
        block_hovered: None,
        block_selected: None,
        block_action_hovered: None,
        spinner_phase: -1.0,
        find_block_highlight: None,
        palette,
        cache_namespace: 7,
        block_diagnose_state: diagnose,
        ai_configured: false,
        tui_cursor: None,
        tui_preedit: None,
        cursor_blink_on: false,
        is_alt: false,
    }
}

fn run_frame(renderer: &MetalRenderer, blocks: &[Block], palette: &[Color; 256]) -> Duration {
    let diagnose = HashMap::new();
    let start = Instant::now();
    let _ = renderer.build_block_view_vertices(
        frame_model(blocks, palette, &diagnose),
        &mut SelectionHandler::new(),
    );
    start.elapsed()
}

fn measure(label: &str, runs: u32, mut f: impl FnMut() -> Duration) -> Duration {
    // 3 warm-up runs (atlas, allocators, caches) then take the MIN over the
    // timed runs — min is the cleanest signal on a shared machine.
    let mut best = Duration::MAX;
    for i in 0..runs + 3 {
        let d = f();
        if i >= 3 && d < best {
            best = d;
        }
    }
    println!(
        "  {label:<30} min {:.2} ms ({runs} runs)",
        best.as_secs_f64() * 1e3
    );
    best
}

#[test]
#[ignore = "G1/G2 gate bench; run with --release --ignored --nocapture --test-threads=1"]
fn bench_block_view_four_phase() {
    let lines = 118_000;
    let block = make_block(lines);
    let blocks = vec![block];
    let palette = Color::standard_palette();

    let mut renderer = MetalRenderer::new_headless_paint(Theme::weft_dark());
    let cols = 88;

    println!("== M5-b four-phase bench: 1 block, {lines} lines, {cols} cols ==");
    let t_cold = measure("cold (L1+L2 build + frame)", 5, || {
        renderer.block_layout_cache.borrow_mut().invalidate(1);
        run_frame(&renderer, &blocks, &palette)
    });

    // Steady state: cache synced, same model every frame.
    let t_warm = measure("warm frame (G1 <= 3ms)", 20, || {
        run_frame(&renderer, &blocks, &palette)
    });

    // Same-cols rebuild: collapse/content-style invalidation path.
    let t_rebuild = measure("same-cols rebuild + frame", 10, || {
        renderer.block_layout_cache.borrow_mut().invalidate(1);
        run_frame(&renderer, &blocks, &palette)
    });

    // Cols change: narrow the viewport by two cells → the derived layout
    // cols change → sync_blocks rebuilds the L2 table, then a full frame.
    let cell_w = renderer.cell_width() as f32;
    let base_w = renderer
        .layout_ctx
        .as_ref()
        .expect("headless layout ctx")
        .viewport
        .0;
    let mut flip = false;
    let t_cols = measure("cols-change frame (G2 <= 15ms)", 10, || {
        flip = !flip;
        // The layout cols derive from layout_ctx.viewport — narrow it by two
        // cells so sync_blocks rebuilds the L2 table for the new width.
        if let Some(ctx) = renderer.layout_ctx.as_mut() {
            ctx.viewport.0 = if flip { base_w - 2.0 * cell_w } else { base_w };
        }
        run_frame(&renderer, &blocks, &palette)
    });

    if let Some(c) = renderer.block_layout_cache.borrow().get_if_cached(1) {
        println!(
            "  L1: {} clusters / {} lines ({:.1} MB) | L2: {} visual rows + {} hint rows ({:.2} MB)",
            c.content.graphemes.len(),
            c.content.line_meta.len(),
            c.content.graphemes.len() as f64 * 8.0 / 1048576.0,
            c.width.rows.len(),
            c.width.hint_rows,
            c.width.rows.len() as f64 * 16.0 / 1048576.0,
        );
    }

    // G1/G2 gates — release 口径 (debug builds exceed the budget by
    // construction; this bench is meant for `--release`).
    if cfg!(not(debug_assertions)) {
        assert!(
            t_warm <= Duration::from_millis(3),
            "G1 FAIL: warm frame {t_warm:?} > 3ms"
        );
        assert!(
            t_cols <= Duration::from_millis(15),
            "G2 FAIL: cols-change frame {t_cols:?} > 15ms"
        );
        println!("  G1 (warm <= 3ms): PASS  |  G2 (cols-change <= 15ms): PASS");
    } else {
        println!("  debug build: gates not asserted (release 口径)");
    }
    let _ = (t_cold, t_rebuild);
}
