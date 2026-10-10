// ── Tests ───────────────────────────────────────────────────────────────

use super::*;
use crate::paint::primitives::color_to_normalized;
use weft_core::grid::{Cell, CellColor, CellFlags, Color, Cursor, CursorStyle, Grid};
use weft_core::selection::{GridPos, Selection, SelectionHandler, SelectionMode};

// Test constants.
const FG: [f32; 4] = [0.8, 0.8, 0.8, 1.0];
const BG: [f32; 4] = [0.1, 0.1, 0.2, 1.0];
const CURSOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const SELECTION: [f32; 4] = [0.3, 0.5, 0.7, 0.6];
// v1.10.22: painted selection = SELECTION composited onto BG.
const SELECTION_PAINTED: [f32; 4] = [0.22, 0.34, 0.50, 1.0];
const CW: f32 = 10.0;
const CH: f32 = 20.0;
// v1.11.6 (M6): theme.link default — the old HYPERLINK_COLOR const.
const LINK: [f32; 4] = [0.36, 0.62, 0.94, 1.0];

// v1.10.4: the color-emoji sentinel is fg.a = 2.0; every real fg alpha
// source (palette u8/255, contrast boost, REVERSE swap, DIM, opacity)
// stays ≤ 1.0, so `fg.a > 1.5` in the shader is unambiguous. Assert the
// test constants here at compile time.
const _: () = {
    assert!(FG[3] <= 1.0 && BG[3] <= 1.0 && CURSOR[3] <= 1.0 && SELECTION[3] <= 1.0);
};

/// Build instances for row 0 of a 1-row grid.
fn build(
    grid: &Grid,
    cursor: &Cursor,
    style: CursorStyle,
    show: bool,
    sel: &SelectionHandler,
    opacity: f32,
) -> GridRowInstances {
    let palette = Color::standard_palette();
    build_row_instances(
        grid,
        &palette,
        0,
        FG,
        BG,
        CURSOR,
        SELECTION,
        SELECTION_PAINTED,
        cursor,
        style,
        show,
        sel,
        opacity,
        1.0,
        CW,
        CH,
        0.0,
        0.0,
        false,
        LINK,
    )
}

/// Build with default cursor (hidden) and no selection.
fn build_plain(grid: &Grid) -> GridRowInstances {
    let cursor = Cursor::default();
    let sel = SelectionHandler::new();
    build(grid, &cursor, CursorStyle::Block, false, &sel, 1.0)
}

fn make_grid(cells: &[Cell]) -> Grid {
    let cols = cells.len().max(1);
    let mut grid = Grid::new(1, cols);
    for (i, cell) in cells.iter().enumerate() {
        grid.viewport[0].cells[i] = *cell;
    }
    grid
}

// ── Test 1: empty row ──────────────────────────────────────────

#[test]
fn empty_row_emits_single_default_bg_run_no_glyphs() {
    let grid = Grid::new(1, 5);
    let result = build_plain(&grid);

    // One bg run covering the full row, no glyphs.
    assert_eq!(result.bg_instances.len(), 1);
    assert_eq!(result.bg_instances[0].x0, 0.0);
    assert_eq!(result.bg_instances[0].w, 50.0); // 5 cols * 10 px
    assert_eq!(result.bg_instances[0].bg, BG);
    assert_eq!(result.glyph_instances.len(), 0);
}

// ── Test 2: single text cell ───────────────────────────────────

#[test]
fn single_text_cell_emits_bg_run_and_text_glyph() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 1);
    assert_eq!(result.glyph_instances.len(), 1);
    match &result.glyph_instances[0] {
        GlyphInstance::Text {
            dst,
            ch,
            fg,
            cluster: _,
            style: _,
        } => {
            assert_eq!(*ch, 'A');
            assert_eq!(*fg, FG);
            assert_eq!(*dst, [0.0, 0.0, CW, CH]);
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Test 3: wide char spans two columns ────────────────────────

#[test]
fn wide_char_cell_spans_two_columns_in_bg_run() {
    // '中' is a wide char → width = Full, spans 2 cols.
    // Need a 2-col grid with a WIDE_SPACER at col 1 (as the VT parser
    // would set up) for the wide char to actually span 2 columns.
    let mut grid = Grid::new(1, 2);
    grid.viewport[0].cells[0] = Cell::with_char('中');
    grid.viewport[0].cells[1].flags = CellFlags::WIDE_SPACER;
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 1);
    assert_eq!(result.bg_instances[0].w, CW * 2.0); // 2 cols
    assert_eq!(result.glyph_instances.len(), 1);
    match &result.glyph_instances[0] {
        GlyphInstance::Text { dst, ch, .. } => {
            assert_eq!(*ch, '中');
            assert_eq!(dst[2] - dst[0], CW * 2.0); // width = 2 cells
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Test 4: wide spacer is skipped ─────────────────────────────

#[test]
fn wide_spacer_cell_is_skipped_entirely() {
    // A standalone WIDE_SPACER (edge case) produces nothing.
    let spacer = Cell {
        flags: CellFlags::WIDE_SPACER,
        ..Cell::default()
    };
    let grid = make_grid(&[spacer]);
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 0);
    assert_eq!(result.glyph_instances.len(), 0);
}

// ── Test 5: consecutive same-bg cells merge ────────────────────

/// v1.10.26 Batch B (FIX_WRAP_EPOCH_AND_VIEWPORT_KEEP): the viewport may
/// hold a row wider than `num_cols` right after a narrowing resize (rows
/// only grow). The renderer must stay bounded by `num_cols` — the leftover
/// right half is clipped, never drawn.
#[test]
fn wide_viewport_row_renders_exactly_num_cols() {
    let mut grid = Grid::new(1, 4);
    // Post-narrowing state: the row kept its original 8-cell width.
    grid.viewport[0].cells = (0..8u8)
        .map(|i| Cell::with_char(char::from(b'A' + i)))
        .collect();
    let result = build_plain(&grid);

    assert_eq!(
        result.glyph_instances.len(),
        4,
        "only the first num_cols glyphs render; E..H are outside the window"
    );
    // Background run spans exactly num_cols * cw.
    assert_eq!(result.bg_instances.len(), 1);
    assert_eq!(result.bg_instances[0].w, 4.0 * CW);
}

#[test]
fn consecutive_same_bg_cells_merge_into_one_run() {
    let mut cells: Vec<Cell> = "hello".chars().map(Cell::with_char).collect();
    // All default bg → should merge into one run.
    for c in &mut cells {
        c.bg = CellColor::Default;
    }
    let grid = make_grid(&cells);
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 1);
    assert_eq!(result.bg_instances[0].w, 50.0); // 5 * 10
    assert_eq!(result.glyph_instances.len(), 5); // 5 text glyphs
}

// ── Test 6: different bg colors break the run ──────────────────

#[test]
fn different_bg_colors_break_into_separate_runs() {
    let red = CellColor::Rgb(Color::rgb(255, 0, 0));
    let green = CellColor::Rgb(Color::rgb(0, 255, 0));

    let mut cell_a = Cell::with_char('A');
    cell_a.bg = red;
    let mut cell_b = Cell::with_char('B');
    cell_b.bg = green;

    let grid = make_grid(&[cell_a, cell_b]);
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 2);
    // First run: red bg, width = CW
    assert_eq!(result.bg_instances[0].w, CW);
    assert_eq!(result.bg_instances[0].bg, [1.0, 0.0, 0.0, 1.0]);
    // Second run: green bg, width = CW
    assert_eq!(result.bg_instances[1].w, CW);
    assert_eq!(result.bg_instances[1].bg, [0.0, 1.0, 0.0, 1.0]);
}

// ── Test 7: reverse video swaps fg and bg ──────────────────────

#[test]
fn reverse_video_swaps_fg_and_bg() {
    let mut cell = Cell::with_char('A');
    cell.fg = CellColor::Rgb(Color::rgb(255, 0, 0)); // red fg
    cell.bg = CellColor::Rgb(Color::rgb(0, 0, 255)); // blue bg
    cell.flags = CellFlags::REVERSE;

    let grid = make_grid(&[cell]);
    let result = build_plain(&grid);

    // After swap: fg = blue, bg = red.
    assert_eq!(result.bg_instances[0].bg, [1.0, 0.0, 0.0, 1.0]); // red bg
    match &result.glyph_instances[0] {
        GlyphInstance::Text { fg, .. } => {
            assert_eq!(*fg, [0.0, 0.0, 1.0, 1.0]); // blue fg
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Test 8: hidden cell emits bg but no glyph ──────────────────

#[test]
fn hidden_cell_emits_bg_but_no_glyph() {
    let mut cell = Cell::with_char('X');
    cell.flags = CellFlags::HIDDEN;
    let grid = make_grid(&[cell]);
    let result = build_plain(&grid);

    assert_eq!(result.bg_instances.len(), 1); // bg still painted
    assert_eq!(result.glyph_instances.len(), 0); // no text glyph
}

// ── Test 9: cursor block overrides bg, uses black fg ───────────

#[test]
fn cursor_block_overrides_bg_and_uses_black_fg() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let cursor = Cursor {
        row: 0,
        col: 0,
        visible: true,
        wrap_pending: false,
    };
    let sel = SelectionHandler::new();
    let result = build(&grid, &cursor, CursorStyle::Block, true, &sel, 1.0);

    // bg = cursor_color (overrides default).
    assert_eq!(result.bg_instances[0].bg, CURSOR);
    // fg = black (text on cursor block).
    match &result.glyph_instances[0] {
        GlyphInstance::Text { fg, .. } => assert_eq!(*fg, [0.0, 0.0, 0.0, 1.0]),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── Test 10: cursor bar emits decoration glyph ─────────────────

#[test]
fn cursor_bar_emits_decoration_glyph() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let cursor = Cursor {
        row: 0,
        col: 0,
        visible: true,
        wrap_pending: false,
    };
    let sel = SelectionHandler::new();
    let result = build(&grid, &cursor, CursorStyle::Bar, true, &sel, 1.0);

    // Text glyph + bar decoration.
    assert_eq!(result.glyph_instances.len(), 2);
    let has_deco = result
        .glyph_instances
        .iter()
        .any(|g| matches!(g, GlyphInstance::Decoration { color, .. } if *color == CURSOR));
    assert!(has_deco, "expected a cursor-colored Decoration glyph");

    // bg should NOT be cursor_color (bar doesn't override bg).
    assert_eq!(result.bg_instances[0].bg, BG);
}

// ── Test 11: cursor underline emits decoration glyph ───────────

#[test]
fn cursor_underline_emits_decoration_glyph() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let cursor = Cursor {
        row: 0,
        col: 0,
        visible: true,
        wrap_pending: false,
    };
    let sel = SelectionHandler::new();
    let result = build(&grid, &cursor, CursorStyle::Underline, true, &sel, 1.0);

    assert_eq!(result.glyph_instances.len(), 2);
    let has_deco = result.glyph_instances.iter().any(|g| {
        matches!(g, GlyphInstance::Decoration { dst, color }
            if *color == CURSOR && (dst[3] - dst[1]) == UNDERLINE_HEIGHT)
    });
    assert!(has_deco, "expected a cursor-colored underline Decoration");
}

// ── Test 12: selection overrides cell bg ───────────────────────

#[test]
fn selection_overrides_cell_bg() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let cursor = Cursor::default();
    let mut sel = SelectionHandler::new();
    sel.selection = Some(Selection::new(
        GridPos::new(0, 0),
        GridPos::new(0, 0),
        SelectionMode::Simple,
    ));
    let result = build(&grid, &cursor, CursorStyle::Block, false, &sel, 1.0);

    assert_eq!(result.bg_instances[0].bg, SELECTION);
}

// ── Test 13: hyperlink emits underline decoration ──────────────

#[test]
fn hyperlink_cell_emits_underline_decoration() {
    let mut cell = Cell::with_char('A');
    cell.flags = CellFlags::HYPERLINK;
    let grid = make_grid(&[cell]);
    let result = build_plain(&grid);

    // Text glyph + hyperlink underline decoration.
    assert_eq!(result.glyph_instances.len(), 2);
    let has_link = result.glyph_instances.iter().any(|g| {
        matches!(g, GlyphInstance::Decoration { color, dst }
            if *color == LINK && (dst[3] - dst[1]) == UNDERLINE_HEIGHT)
    });
    assert!(has_link, "expected a hyperlink underline Decoration");
}

// ── Test 14: opacity scales background alpha ───────────────────

#[test]
fn opacity_scales_background_alpha() {
    let grid = Grid::new(1, 1);
    let result = build_plain(&grid);
    // opacity = 1.0 → bg alpha unchanged.
    assert_eq!(result.bg_instances[0].bg[3], BG[3]);

    let cursor = Cursor::default();
    let sel = SelectionHandler::new();
    let result_half = build(&grid, &cursor, CursorStyle::Block, false, &sel, 0.5);
    // opacity = 0.5 → bg alpha halved.
    assert!((result_half.bg_instances[0].bg[3] - BG[3] * 0.5).abs() < 1e-6);
}

// ── Serialization smoke test ───────────────────────────────────

#[test]
fn batch_push_row_serializes_both_streams() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let row = build_plain(&grid);

    let mut batch = GridInstanceBatch::default();
    let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
        ([0.1, 0.2, 0.3, 0.4], false)
    };
    let ranges = batch.push_row(&row, &resolve_uv);

    // Bg stream: 8 floats per run.
    assert_eq!(batch.bg_stream.len(), 8);
    assert_eq!(ranges.bg_range, (0, 8));

    // Glyph stream: 16 floats per glyph.
    assert_eq!(batch.glyph_stream.len(), 16);
    assert_eq!(ranges.glyph_range, (0, 16));
}

// ── v1.10.4: color-emoji fg sentinel ─────────────────────────────

#[test]
fn color_glyph_replaces_fg_with_alpha_2_sentinel() {
    let grid = make_grid(&[Cell::with_char('A')]);
    let row = build_plain(&grid);

    // Color-atlas glyph (emoji): fg must become the [0,0,0,2.0] sentinel
    // so the shader routes the quad to the RGBA color texture. Normal
    // fg alpha is ≤ 1.0, so 2.0 is unambiguous (checked below).
    let mut batch = GridInstanceBatch::default();
    let color_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
        ([0.1, 0.2, 0.3, 0.4], true)
    };
    batch.push_row(&row, &color_uv);
    // Layout: origin(2) size(2) uv(4) fg(4) bg(4) → fg = floats[8..12].
    assert_eq!(&batch.glyph_stream[8..12], &[0.0, 0.0, 0.0, 2.0]);
    // UV unchanged.
    assert_eq!(&batch.glyph_stream[4..8], &[0.1, 0.2, 0.3, 0.4]);

    // Mask-atlas glyph: fg passes through untouched.
    let mut batch = GridInstanceBatch::default();
    let mask_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
        ([0.1, 0.2, 0.3, 0.4], false)
    };
    batch.push_row(&row, &mask_uv);
    assert_eq!(&batch.glyph_stream[8..12], &FG[..]);
}

// ── Test 16: multi-pane ranges don't overlap ──────────────────

#[test]
fn multi_pane_ranges_are_sequential_and_non_overlapping() {
    // Two rows, each producing 1 bg run (8 floats) + 1 glyph (16 floats).
    let grid_a = make_grid(&[Cell::with_char('A')]);
    let grid_b = make_grid(&[Cell::with_char('B')]);
    let row_a = build_plain(&grid_a);
    let row_b = build_plain(&grid_b);

    let mut batch = GridInstanceBatch::default();
    let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
        ([0.1, 0.2, 0.3, 0.4], false)
    };
    let ranges_a = batch.push_row(&row_a, &resolve_uv);
    let ranges_b = batch.push_row(&row_b, &resolve_uv);

    // Pane A occupies floats [0, 8) in bg and [0, 16) in glyph.
    assert_eq!(ranges_a.bg_range, (0, 8));
    assert_eq!(ranges_a.glyph_range, (0, 16));

    // Pane B occupies floats [8, 16) in bg and [16, 32) in glyph.
    assert_eq!(ranges_b.bg_range, (8, 16));
    assert_eq!(ranges_b.glyph_range, (16, 32));

    // Total: 2 bg runs × 8 floats + 2 glyphs × 16 floats.
    assert_eq!(batch.bg_stream.len(), 16);
    assert_eq!(batch.glyph_stream.len(), 32);
}

// ── Test 17: pixel-equivalence harness ─────────────────────────
//
// v1.4.2 Phase B3 exit criterion: verify that dual-stream rendering
// (bg runs + glyph instances with transparent bg) produces the same
// pixels as single-stream rendering (per-cell instances with baked-in
// bg). Since we can't rasterize in a unit test (no Metal device), we
// verify the data-level equivalence properties:
//
// 1. **Coverage**: the union of all bg run x-ranges == [0, num_cols·cw).
//    No gaps → no unpainted pixels.
// 2. **Non-overlap**: bg runs don't overlap in x. No double-blending.
// 3. **Glyph transparency**: all Text glyph instances have bg=[0;4].
//    The bg stream is solely responsible for the background.
// 4. **Glyph coverage**: every cell with visible text has a matching
//    Text glyph at the same dst rect.
// 5. **Run color consistency**: within a run, all covered cells share
//    the same final_bg (cursor/selection/normal).
//
// Uses a complex row mixing: default bg, custom bg, text, empty cells,
// cursor (bar style), and selection — exercising all merge-break paths.

#[test]
fn pixel_equivalence_dual_stream_matches_single_stream() {
    // Build a row: [empty | text 'A' | custom-bg 'B' | default | selected 'C']
    // Col indices:     0      1          2                3       4      5
    let mut grid = Grid::new(1, 6);
    grid.viewport[0].cells[1] = Cell::with_char('A');
    let mut cell_b = Cell::with_char('B');
    cell_b.bg = CellColor::Palette(1); // custom bg → breaks the run
    grid.viewport[0].cells[2] = cell_b;
    // col 3: default Cell (default bg) → breaks the custom-bg run
    grid.viewport[0].cells[4] = Cell::with_char('C');
    // col 5: empty

    // Cursor at col 1 (bar style → decoration glyph + cursor fg).
    let cursor = Cursor {
        row: 0,
        col: 1,
        visible: true,
        wrap_pending: false,
    };
    // Selection covering col 4 → selection bg override.
    let mut sel = SelectionHandler::new();
    sel.start(GridPos { row: 0, col: 4 }, SelectionMode::Simple);
    sel.end();

    let row_instances = build(&grid, &cursor, CursorStyle::Bar, true, &sel, 1.0);

    // ── Property 1: Coverage ───────────────────────────────────
    // The full row [0, 6·cw) must be covered by bg runs.
    let full_width = 6.0 * CW;
    let mut covered: Vec<(f32, f32)> = row_instances
        .bg_instances
        .iter()
        .map(|r| (r.x0, r.x0 + r.w))
        .collect();
    covered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    assert!(!covered.is_empty(), "bg stream must have at least one run");
    assert!(
        (covered[0].0 - 0.0).abs() < 0.01,
        "first run must start at x=0, got {}",
        covered[0].0
    );
    let mut prev_end = covered[0].1;
    for &(start, end) in covered.iter().skip(1) {
        assert!(
            (start - prev_end).abs() < 0.01,
            "gap between runs: prev_end={}, next_start={}",
            prev_end,
            start
        );
        prev_end = end;
    }
    assert!(
        (prev_end - full_width).abs() < 0.01,
        "last run must end at {}, got {}",
        full_width,
        prev_end
    );

    // ── Property 2: Non-overlap ────────────────────────────────
    for i in 0..covered.len() {
        for j in (i + 1)..covered.len() {
            let (a0, a1) = covered[i];
            let (b0, b1) = covered[j];
            let overlap = a0 < b1 && b0 < a1;
            assert!(
                !overlap,
                "runs {} and {} overlap: [{},{}) vs [{},{})",
                i, j, a0, a1, b0, b1
            );
        }
    }

    // ── Property 3: Glyph transparency ─────────────────────────
    // Text glyphs must have bg = [0;4] (the bg stream paints bg).
    // Verified via batch serialization: floats [12..16) are the bg field.
    let mut batch = GridInstanceBatch::default();
    let resolve_uv = |_ch: char, _cluster: Option<&str>, _style: crate::glyph::GlyphStyle| {
        ([0.5, 0.5, 0.6, 0.6], false)
    };
    batch.push_row(&row_instances, &resolve_uv);
    let gs = &batch.glyph_stream;
    for chunk in gs.chunks(16) {
        if chunk.len() == 16 {
            let bg_field = [chunk[12], chunk[13], chunk[14], chunk[15]];
            // Text glyphs (uv != [0,0,0,1]) must have transparent bg.
            let uv = [chunk[4], chunk[5], chunk[6], chunk[7]];
            if uv != [0.0, 0.0, 0.0, 1.0] {
                assert_eq!(
                    bg_field,
                    [0.0, 0.0, 0.0, 0.0],
                    "Text glyph must have transparent bg (uv={:?})",
                    uv
                );
            }
        }
    }

    // ── Property 4: Glyph coverage ────────────────────────────
    // Every cell with visible text (A, B, C) must have a matching Text
    // glyph at the correct dst rect.
    let text_cells = [(1usize, 'A'), (2, 'B'), (4, 'C')];
    for (col, ch) in text_cells {
        let x0 = col as f32 * CW;
        let x1 = x0 + CW;
        let found = row_instances.glyph_instances.iter().any(|gi| {
            if let GlyphInstance::Text { dst, ch: gc, .. } = gi {
                let [dx0, _dy0, dx1, _dy1] = dst;
                (dx0 - x0).abs() < 0.01 && (dx1 - x1).abs() < 0.01 && *gc == ch
            } else {
                false
            }
        });
        assert!(
            found,
            "col {} char '{}' must have a Text glyph at [{},{})",
            col, ch, x0, x1
        );
    }

    // ── Property 5: Run color consistency ────────────────────
    // The cursor cell (col 1, bar style) uses cursor_color as fg (not bg),
    // so its bg run still uses the default bg. The selected cell (col 4)
    // uses SELECTION as its bg run color. Verify the run covering col 4
    // has the selection color.
    let sel_x0 = 4.0 * CW;
    let sel_run = row_instances
        .bg_instances
        .iter()
        .find(|r| r.x0 <= sel_x0 && r.x0 + r.w > sel_x0);
    assert!(
        sel_run.is_some(),
        "must have a bg run covering col 4 (selection)"
    );
    let sel_run = sel_run.unwrap();
    assert_eq!(
        sel_run.bg, SELECTION,
        "selection cell's bg run must use selection color"
    );

    // The custom-bg cell (col 2) must have a run with palette[1] color.
    let cb_x0 = 2.0 * CW;
    let cb_run = row_instances
        .bg_instances
        .iter()
        .find(|r| r.x0 <= cb_x0 && r.x0 + r.w > cb_x0);
    assert!(
        cb_run.is_some(),
        "must have a bg run covering col 2 (custom bg)"
    );
    let palette = Color::standard_palette();
    let expected_bg = color_to_normalized(palette[1]);
    let cb_run = cb_run.unwrap();
    assert_eq!(
        cb_run.bg, expected_bg,
        "custom-bg cell's bg run must use palette[1] color"
    );

    // ── Property 6: Cursor decoration present ───────────────────
    // Bar-style cursor at col 1 must emit a Decoration glyph.
    let cursor_decoration = row_instances.glyph_instances.iter().any(|gi| {
        if let GlyphInstance::Decoration { dst, color } = gi {
            let [dx0, _dy0, dx1, _dy1] = dst;
            (dx0 - 1.0 * CW).abs() < 0.01 && *color == CURSOR && dx1 - dx0 < CW
        } else {
            false
        }
    });
    assert!(
        cursor_decoration,
        "bar cursor at col 1 must emit a Decoration glyph"
    );
}
