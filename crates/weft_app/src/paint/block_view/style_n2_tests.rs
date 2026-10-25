//! v1.13.11 N2 cluster-fidelity suites (PLAN_v1.13.11 测试 4-选区半 / 7).
//! Companion of `style.rs` via `#[path]` — the inline tests module already
//! sits close to the 800-line commit gate (atlas_warmup_tests.rs precedent).
//!
//! - `info_cluster_*`            — 测试 7: block view vs live grid column
//!   parity (ℹ️ = ONE width-2 cell) + post-FE0F styled-RLE continuity.
//! - `selection_boundary_at_fe0f_*` — 测试 4-选区半: in-cluster decisions
//!   key on the LEAD scalar's RLE index (评审 P3-1).

use super::BlockOutputTextPaint;
use crate::paint::primitives::resolve_cell_color;
use crate::renderer::MetalRenderer;
use weft_core::blocks::{ForegroundSpan, StyledLine};
use weft_core::config::Theme;
use weft_core::grid::{CellColor, Color};

/// Headless renderer with exactly what the paint path resolves: the ℹ️
/// cluster via the CoreText cluster atlas (wide → width-2 cell) and 'X' via
/// the regular face. Skips on devices without Metal (golden precedent).
fn warmed_cluster_renderer() -> Option<MetalRenderer> {
    metal::Device::system_default()?;
    let mut renderer = MetalRenderer::new_headless_paint(Theme::weft_warm());
    renderer
        .atlas
        .get_or_rasterize_cluster("ℹ\u{fe0f}", 'ℹ', true);
    renderer
        .atlas
        .get_or_rasterize('X', crate::glyph::GlyphStyle::REGULAR);
    Some(renderer)
}

/// v1.13.11 N2 (PLAN_v1.13.11 测试 7): the block view iterates GRAPHEMES,
/// so an `ℹ️` cluster is ONE width-2 unit and the trailing "X" lands in
/// column 2 — the same column the live grid gives it (both paths share
/// `terminal_text_width`). The styled RLE's CHAR space keeps FE0F its own
/// index (shared with the base's span), so styling AFTER the cluster still
/// resolves against the correct span (no off-by-one "break" past a FE0F).
#[test]
fn info_cluster_is_one_width_two_cell_and_post_fe0f_style_stays_aligned() {
    let Some(renderer) = warmed_cluster_renderer() else {
        eprintln!("skipping cluster alignment test: no Metal device available");
        return;
    };
    // Styled RLE over scalars [ℹ, FE0F, X]: the base+FE0F pair shares
    // span A (index 0..2), "X" carries span B (index 2..3).
    const CLUSTER_RGB: CellColor = CellColor::Rgb(Color::rgb(200, 10, 10));
    const X_RGB: CellColor = CellColor::Rgb(Color::rgb(10, 10, 200));
    let styled = StyledLine {
        line: 0,
        foregrounds: vec![
            ForegroundSpan {
                start: 0,
                end: 2,
                color: CLUSTER_RGB,
            },
            ForegroundSpan {
                start: 2,
                end: 3,
                color: X_RGB,
            },
        ],
        ..Default::default()
    };
    let palette = Color::standard_palette();
    let paint = BlockOutputTextPaint {
        x: 0.0,
        y: 0.0,
        text: "ℹ\u{fe0f}X",
        semantic_text: "ℹ\u{fe0f}X",
        style: Some(&styled),
        char_offset: 0,
        fallback: [0.9, 0.9, 0.9, 1.0],
        canvas: [0.1, 0.1, 0.2, 1.0],
        selection: None,
        selection_painted: [0.0; 4],
        max_cols: 10,
        palette: &palette,
        row_pitch: 16.0,
        clip_bottom: f32::INFINITY,
    };
    let mut verts = Vec::new();
    renderer.push_block_output_text(&mut verts, paint);

    // Collect text quads: one quad = 6 vertices × 12 floats (push_quad
    // order; vertex 2 carries x1/y1). fg all-zero marks a background/
    // decoration quad — text quads always carry a fill.
    let cw = renderer.cell_width() as f32;
    let mut quads: Vec<[f32; 4]> = Vec::new(); // [x0, y0, x1, y1]
    let mut fgs: Vec<[f32; 4]> = Vec::new();
    for q in verts.chunks_exact(72) {
        let fg = [q[4], q[5], q[6], q[7]];
        if fg == [0.0; 4] {
            continue;
        }
        quads.push([q[0], q[1], q[24], q[25]]);
        fgs.push(fg);
    }
    assert_eq!(quads.len(), 2, "one quad per visible cell: {quads:?}");

    // Cluster quad: ONE width-2 cell (not a width-1 base + stray FE0F).
    assert!(
        (quads[0][2] - quads[0][0] - 2.0 * cw).abs() < 1e-3,
        "the ℹ️ cluster must be one width-2 cell, got {:?} (cw={cw})",
        quads[0]
    );
    assert_eq!(
        fgs[0][3], 2.0,
        "the warmed cluster resolves through the COLOR atlas (fg.a=2.0 sentinel)"
    );
    // Parity with the live grid: "X" starts exactly at column 2.
    assert!(
        (quads[1][0] - 2.0 * cw).abs() < 1e-3,
        "X must sit in column 2 (live-grid parity), got x0={} (cw={cw})",
        quads[1][0]
    );
    // FE0F does not break styling: X resolves span B, not the cluster's
    // span A (an off-by-one char_index would leak span A into X).
    let expected_x_fg = resolve_cell_color(X_RGB, [0.9, 0.9, 0.9, 1.0], &palette);
    assert_eq!(
        fgs[1], expected_x_fg,
        "styling after the FE0F must follow X's own span"
    );
}

/// v1.13.11 N2 (PLAN_v1.13.11 测试 4-选区半): in-cluster decisions key on
/// the LEAD scalar's RLE index — a selection boundary between the base and
/// its FE0F must not shift the cluster's highlight (评审 P3-1). A selection
/// covering only the FE0F scalar `(1, 2)` selects NOTHING (both cells keep
/// their explicit bg quads); `(0, 1)` selects the cluster cell → its bg
/// quad is skipped (C1 flip), X's is not.
#[test]
fn selection_boundary_at_fe0f_index_does_not_shift_cluster_highlight() {
    let Some(renderer) = warmed_cluster_renderer() else {
        eprintln!("skipping cluster selection test: no Metal device available");
        return;
    };
    const CLUSTER_FG: CellColor = CellColor::Rgb(Color::rgb(200, 10, 10));
    const X_FG: CellColor = CellColor::Rgb(Color::rgb(10, 10, 200));
    const BG: CellColor = CellColor::Rgb(Color::rgb(30, 60, 90));
    let styled = StyledLine {
        line: 0,
        foregrounds: vec![
            ForegroundSpan {
                start: 0,
                end: 2,
                color: CLUSTER_FG,
            },
            ForegroundSpan {
                start: 2,
                end: 3,
                color: X_FG,
            },
        ],
        backgrounds: vec![ForegroundSpan {
            start: 0,
            end: 3,
            color: BG,
        }],
        ..Default::default()
    };
    let palette = Color::standard_palette();
    let cw = renderer.cell_width() as f32;
    let expected_bg = resolve_cell_color(BG, [0.0; 4], &palette);

    // bg quads: fg all-zero, bg carries the explicit color. Returns
    // (x0, width) pairs.
    let bg_quads = |selection: Option<(usize, usize, [f32; 4])>| {
        let paint = BlockOutputTextPaint {
            x: 0.0,
            y: 0.0,
            text: "ℹ\u{fe0f}X",
            semantic_text: "ℹ\u{fe0f}X",
            style: Some(&styled),
            char_offset: 0,
            fallback: [0.9, 0.9, 0.9, 1.0],
            canvas: [0.1, 0.1, 0.2, 1.0],
            selection,
            selection_painted: [0.8, 0.8, 0.85, 1.0],
            max_cols: 10,
            palette: &palette,
            row_pitch: 16.0,
            clip_bottom: f32::INFINITY,
        };
        let mut verts = Vec::new();
        renderer.push_block_output_text(&mut verts, paint);
        let mut out = Vec::new();
        for q in verts.chunks_exact(72) {
            if [q[4], q[5], q[6], q[7]] != [0.0; 4] {
                continue; // text quad
            }
            if q[8] == 0.0 && q[9] == 0.0 && q[10] == 0.0 && q[11] == 0.0 {
                continue; // decoration quad without fill
            }
            assert_eq!([q[8], q[9], q[10], q[11]], expected_bg);
            out.push((q[0], q[24] - q[0]));
        }
        out
    };

    // Selection = the FE0F scalar alone → the whole cluster cell stays
    // unselected: both explicit bg quads survive.
    let quads = bg_quads(Some((1, 2, [0.4, 0.5, 0.6, 0.6])));
    assert_eq!(
        quads.len(),
        2,
        "FE0F-only selection selects nothing: {quads:?}"
    );
    assert!((quads[0].0 - 0.0).abs() < 1e-3 && (quads[0].1 - 2.0 * cw).abs() < 1e-3);
    assert!((quads[1].0 - 2.0 * cw).abs() < 1e-3);

    // Selection = the base scalar → the cluster cell IS selected: its bg
    // quad is skipped (C1 flip), X's survives.
    let quads = bg_quads(Some((0, 1, [0.4, 0.5, 0.6, 0.6])));
    assert_eq!(
        quads.len(),
        1,
        "base-scalar selection lifts the cluster bg: {quads:?}"
    );
    assert!((quads[0].0 - 2.0 * cw).abs() < 1e-3);
}

/// v1.14.2 (A1 ②, PLAN_v1142 §2): the styled-line cache stores unclipped
/// LOCAL-origin vertices, so the cached wrapper must (a) serve in-band rows
/// from the cache as before, (b) BYPASS on a row band crossing the hard
/// clip line (zero cache pollution — the counters never move), and (c)
/// emit nothing for a row fully below the line.
#[test]
fn crossing_or_below_rows_bypass_the_styled_line_cache() {
    let Some(renderer) = warmed_cluster_renderer() else {
        eprintln!("skipping cache bypass test: no Metal device available");
        return;
    };
    let palette = Color::standard_palette();
    let paint_at = |y: f32, clip_bottom: f32| BlockOutputTextPaint {
        x: 20.0,
        y,
        text: "abc",
        semantic_text: "abc",
        style: None,
        char_offset: 0,
        fallback: [0.9, 0.9, 0.9, 1.0],
        canvas: [0.1, 0.1, 0.2, 1.0],
        selection: None,
        selection_painted: [0.0; 4],
        max_cols: 10,
        palette: &palette,
        row_pitch: 16.0,
        clip_bottom,
    };
    // One shared source Arc: the cache's identity guard keys on the Arc
    // pointer, so the hit-path control below must reuse the SAME Arc
    // (mirrors the production block_arc_identity flow).
    let source = std::sync::Arc::<str>::from("abc");
    let key_at = |line_idx: usize| super::CacheKeyInput {
        pane_session_id: 1,
        block_id: 1,
        line_idx,
        chunk_idx: 0,
        render_generation: 0,
        palette_fingerprint: 0,
        source: Some(std::sync::Arc::clone(&source)),
        styled: None,
    };
    let cache = std::cell::RefCell::new(crate::paint::styled_line_cache::StyledLineCache::new());

    // (a) In-band row: first call = miss+insert, second = hit (the cache
    // path still works exactly as before inside the visible band).
    let mut verts = Vec::new();
    renderer.push_block_output_text_cached(&mut verts, paint_at(100.0, 600.0), key_at(0), &cache);
    let (hits, misses) = cache.borrow_mut().take_hit_miss_counts();
    assert_eq!((hits, misses), (0, 1), "first in-band call inserts");
    verts.clear();
    renderer.push_block_output_text_cached(&mut verts, paint_at(100.0, 600.0), key_at(0), &cache);
    let (hits, misses) = cache.borrow_mut().take_hit_miss_counts();
    assert_eq!((hits, misses), (1, 0), "second in-band call hits");
    assert!(!verts.is_empty());

    // (b) Crossing row (band [594, 610) against clip 600): bypasses the
    // cache — counters untouched — and the direct path clips to the line.
    let mut verts = Vec::new();
    renderer.push_block_output_text_cached(&mut verts, paint_at(594.0, 600.0), key_at(1), &cache);
    let (hits, misses) = cache.borrow_mut().take_hit_miss_counts();
    assert_eq!(
        (hits, misses),
        (0, 0),
        "a crossing row must never touch the styled-line cache"
    );
    assert!(!verts.is_empty(), "the crossing row still paints its top");
    let max_y = verts
        .iter()
        .skip(1)
        .step_by(12)
        .copied()
        .fold(f32::MIN, f32::max);
    assert!(
        max_y <= 600.0 + 1e-3,
        "crossing-row vertices must stop at the clip line, got {max_y}"
    );

    // (c) Below row: nothing at all, cache untouched.
    let mut verts = Vec::new();
    renderer.push_block_output_text_cached(&mut verts, paint_at(601.0, 600.0), key_at(2), &cache);
    let (hits, misses) = cache.borrow_mut().take_hit_miss_counts();
    assert_eq!((hits, misses), (0, 0));
    assert!(verts.is_empty(), "a fully-below row paints nothing");
}
