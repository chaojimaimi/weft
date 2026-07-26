//! v1.4.2 Phase A: deterministic grid instance-build benchmark + dual-stream
//! prototype. Pure-logic (no MetalRenderer / no Window) so it can run under
//! `cargo test --release --ignored` without GUI dependencies.
//!
//! The benchmark constructs 240×80 grids representative of real terminal
//! sessions (default bg, colorful TUI, selection, streaming) and measures:
//!
//! 1. **Single-stream build time + bytes** (mimics `build_grid_instances`):
//!    every cell emits one 16-float instance (64 B) regardless of content.
//!    Cursor bar/underline and hyperlink underline emit additional 16-float
//!    instances with UV=(0,0,0,1) + fg=[0;4] so only the bg color shows.
//!
//! 2. **Dual-stream prototype bytes** (simulates B2+B3 without Metal):
//!    - Background stream: 8 floats (32 B) per *run*. Adjacent cells with the
//!      same resolved bg color merge into one run (the "background-run merge").
//!      Default-bg cells form runs so the persistent offscreen texture is
//!      fully covered. Cursor bar/underline and hyperlink underline emit
//!      bg-only runs (no glyph needed for pure decoration).
//!    - Glyph stream: 16 floats (64 B) per cell that has actual text content.
//!      Space characters are skipped (the bg run already covers the area).
//!      HIDDEN cells are skipped. WIDE_SPACER cells are skipped (the
//!      preceding wide cell covers them). REVERSE cells emit a glyph instance
//!      with swapped fg/bg.
//!
//! The bench reports the upload-bytes reduction (%) per scenario, which is
//! the primary v1.4.2 GO signal (prototype must reduce upload bytes or
//! instance count by ≥ 40% in representative scenarios).
//!
//! Run with:
//! ```bash
//! cargo test --release -p weft_app --bin weft bench_build_grid_instances \
//!   -- --nocapture --ignored --test-threads=1
//! ```

#![cfg(test)]

use std::time::Instant;
use weft_core::grid::{Cell, CellColor, CellFlags, CellWidth, Color, Grid};
use weft_core::selection::{GridPos, Selection, SelectionHandler, SelectionMode};

/// Bytes per single-stream cell instance (16 floats × 4 B).
const SINGLE_STREAM_INSTANCE_BYTES: usize = 16 * 4;
/// Bytes per dual-stream background run (8 floats × 4 B).
const DUAL_BG_RUN_BYTES: usize = 8 * 4;
/// Bytes per dual-stream glyph instance (16 floats × 4 B).
const DUAL_GLYPH_INSTANCE_BYTES: usize = 16 * 4;

/// Resolved cell colors after theme/palette resolution. Used by both the
/// single-stream and dual-stream collectors so they compare apples-to-apples.
#[derive(Clone, Copy, PartialEq)]
struct ResolvedCell {
    /// Final foreground RGBA (0.0–1.0). `[0;4]` for bg-only decorations.
    fg: [f32; 4],
    /// Final background RGBA (0.0–1.0).
    bg: [f32; 4],
    /// Whether the cell has a non-space character that needs a glyph.
    has_glyph: bool,
    /// Whether the cell is a pure decoration (cursor bar/underline, hyperlink
    /// underline) — contributes a bg-only run, no glyph.
    is_decoration: bool,
    /// Cell width in columns (1 or 2). Unused by collectors but kept for
    /// future wide-char run-merging experiments.
    #[allow(dead_code)]
    width: u8,
}

/// One scenario's measurement results.
#[derive(Debug, Clone)]
struct ScenarioReport {
    name: String,
    rows: usize,
    cols: usize,
    /// Total cells (excluding WIDE_SPACER) in the grid.
    total_cells: usize,
    /// Single-stream: total instances emitted (cells + decorations).
    single_instances: usize,
    /// Single-stream: total bytes uploaded.
    single_bytes: usize,
    /// Dual-stream: number of background runs (after merging adjacent same-bg).
    dual_bg_runs: usize,
    /// Dual-stream: number of glyph instances (non-space, non-hidden, non-spacer).
    dual_glyph_instances: usize,
    /// Dual-stream: total bytes uploaded (bg runs + glyph instances).
    dual_bytes: usize,
    /// (single - dual) / single × 100. Positive = dual-stream is smaller.
    reduction_pct: f64,
    /// Single-stream build time (μs).
    single_build_us: u64,
    /// Dual-stream build time (μs) — simulates the collector pass.
    dual_build_us: u64,
}

impl ScenarioReport {
    fn render_markdown_row(&self) -> String {
        format!(
            "| {} | {}×{} | {} | {} | {:.1} KiB | {} | {} | {:.1} KiB | {:.1}% | {:.0} µs | {:.0} µs |",
            self.name,
            self.rows,
            self.cols,
            self.total_cells,
            self.single_instances,
            self.single_bytes as f64 / 1024.0,
            self.dual_bg_runs,
            self.dual_glyph_instances,
            self.dual_bytes as f64 / 1024.0,
            self.reduction_pct,
            self.single_build_us,
            self.dual_build_us,
        )
    }
}

/// Resolve a cell's colors against the palette, mimicking `build_grid_instances`.
fn resolve_cell(
    cell: &Cell,
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    palette: &[Color; 256],
) -> ([f32; 4], [f32; 4]) {
    let resolve = |cc: CellColor, default: [f32; 4]| -> [f32; 4] {
        match cc {
            CellColor::Default => default,
            CellColor::Palette(idx) => {
                let c = &palette[idx as usize];
                [
                    c.r as f32 / 255.0,
                    c.g as f32 / 255.0,
                    c.b as f32 / 255.0,
                    c.a as f32 / 255.0,
                ]
            }
            CellColor::Rgb(c) => [
                c.r as f32 / 255.0,
                c.g as f32 / 255.0,
                c.b as f32 / 255.0,
                c.a as f32 / 255.0,
            ],
        }
    };
    let mut fg = resolve(cell.fg, default_fg);
    let mut bg = resolve(cell.bg, default_bg);
    if cell.flags.contains(CellFlags::REVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

/// Parameters for `collect_resolved_cells`, grouped to keep the signature
/// under clippy's `too_many_arguments` threshold.
struct CollectParams<'a> {
    grid: &'a Grid,
    palette: &'a [Color; 256],
    default_fg: [f32; 4],
    default_bg: [f32; 4],
    selection: &'a SelectionHandler,
    selection_bg: [f32; 4],
    cursor_row: usize,
    cursor_col: usize,
    show_cursor: bool,
}

/// Collect the resolved cell grid for a scenario. Returns a 2D Vec indexed
/// `[row][col]`. WIDE_SPACER cells are returned as `None` (the prototype
/// skips them — the preceding wide cell covers them).
fn collect_resolved_cells(p: &CollectParams<'_>) -> Vec<Vec<Option<ResolvedCell>>> {
    let CollectParams {
        grid,
        palette,
        default_fg,
        default_bg,
        selection,
        selection_bg,
        cursor_row,
        cursor_col,
        show_cursor,
    } = *p;
    let mut out = Vec::with_capacity(grid.num_rows);
    for row in 0..grid.num_rows {
        let mut row_cells = Vec::with_capacity(grid.num_cols);
        for col in 0..grid.num_cols {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                row_cells.push(None);
                continue;
            }
            let (mut fg, mut bg) = resolve_cell(cell, default_fg, default_bg, palette);
            let is_cursor = show_cursor && row == cursor_row && col == cursor_col;
            let is_selected = selection
                .selection
                .as_ref()
                .is_some_and(|sel| sel.contains(row, col));
            let is_hidden = cell.flags.contains(CellFlags::HIDDEN);
            let is_hyperlink = cell.flags.contains(CellFlags::HYPERLINK);

            // Mimic build_grid_instances: cursor block swaps fg/bg; bar/underline
            // overlay adds a decoration instance; selection overrides bg.
            let mut decorations: Vec<[f32; 4]> = Vec::new();
            if is_cursor {
                if cursor_style_is_block() {
                    std::mem::swap(&mut fg, &mut bg);
                } else {
                    // Bar/underline cursor: keep cell's bg, add a decoration.
                    // For the prototype we just count it as one bg-only run.
                    decorations.push([0.0, 0.0, 0.0, 1.0]); // cursor_color placeholder
                }
            }
            if is_selected {
                bg = selection_bg;
            }
            if is_hyperlink {
                // Hyperlink underline: a thin cyan line. Counts as a bg-only
                // decoration run (no glyph needed for the line itself).
                decorations.push([0.36, 0.62, 0.94, 1.0]);
            }

            // has_glyph: non-space character AND not HIDDEN.
            // Space characters don't need a glyph instance — the bg run covers
            // the cell area and the shader would sample an empty mask anyway.
            let ch = if cell.character == '\0' {
                ' '
            } else {
                cell.character
            };
            let has_glyph = ch != ' ' && !is_hidden;

            row_cells.push(Some(ResolvedCell {
                fg,
                bg,
                has_glyph,
                is_decoration: !decorations.is_empty(),
                width: if cell.width == CellWidth::Full { 2 } else { 1 },
            }));
            // Each decoration becomes an extra ResolvedCell (bg-only, no glyph).
            // We push them after the cell so the run-collector sees them in order.
            for dec_bg in decorations {
                row_cells.push(Some(ResolvedCell {
                    fg: [0.0; 4],
                    bg: dec_bg,
                    has_glyph: false,
                    is_decoration: true,
                    width: if cell.width == CellWidth::Full { 2 } else { 1 },
                }));
            }
        }
        out.push(row_cells);
    }
    out
}

/// Stub for cursor style check — the prototype doesn't need the real CursorStyle
/// enum, just whether the cursor is a block (swaps fg/bg) or bar/underline
/// (adds a decoration). We assume block for the bench (most common case).
fn cursor_style_is_block() -> bool {
    true
}

/// Count single-stream instances: one per cell (excluding WIDE_SPACER) + one
/// per decoration (cursor bar/underline, hyperlink underline).
fn count_single_stream(cells: &[Vec<Option<ResolvedCell>>]) -> usize {
    let mut count = 0;
    for row in cells {
        for cell in row {
            if cell.is_some() {
                count += 1;
            }
        }
    }
    count
}

/// Count dual-stream: background runs (after merging adjacent same-bg) + glyph
/// instances (non-space, non-hidden, non-spacer).
fn count_dual_stream(cells: &[Vec<Option<ResolvedCell>>]) -> (usize, usize) {
    let mut bg_runs = 0usize;
    let mut glyph_instances = 0usize;
    for row in cells {
        // Background runs: merge adjacent cells with the same bg color.
        // Decorations are NOT merged with their parent cell — they have a
        // different bg (cursor_color / hyperlink color) and must form their
        // own runs so the GPU draws them on top.
        let mut prev_bg: Option<[f32; 4]> = None;
        for cell_opt in row {
            match cell_opt {
                None => {
                    // WIDE_SPACER: skip — covered by preceding wide cell.
                    // Break the current run so the spacer doesn't merge.
                    prev_bg = None;
                }
                Some(cell) => {
                    if Some(cell.bg) != prev_bg {
                        bg_runs += 1;
                        prev_bg = Some(cell.bg);
                    }
                    if cell.has_glyph {
                        glyph_instances += 1;
                    }
                }
            }
        }
    }
    (bg_runs, glyph_instances)
}

/// Build a 240×80 empty grid (all default cells).
fn make_default_bg_grid() -> Grid {
    Grid::new(240, 80)
}

/// Build a 240×80 grid with a typical prompt + command output.
/// ~500 chars of text, rest default-bg spaces.
fn make_prompt_grid() -> Grid {
    let mut g = Grid::new(240, 80);
    // Row 0: prompt line
    let prompt = "$ ls -la /usr/bin | head -20";
    for (col, ch) in prompt.chars().enumerate() {
        if col < 80 {
            g.cell_mut(0, col).character = ch;
        }
    }
    // Rows 1-30: simulated command output (file listings)
    for row in 1..=30 {
        let line = format!(
            "-rwxr-xr-x  1 root  wheel  {:>8} Jul 26 10:32 file_{:03}",
            1000 + row * 17,
            row
        );
        for (col, ch) in line.chars().enumerate() {
            if col < 80 {
                g.cell_mut(row, col).character = ch;
            }
        }
    }
    // Row 31: next prompt (cursor position)
    let next_prompt = "$ ";
    for (col, ch) in next_prompt.chars().enumerate() {
        g.cell_mut(31, col).character = ch;
    }
    g
}

/// Build a 240×80 grid simulating a colorful TUI (vim with syntax highlighting).
/// ~50% of cells have non-default fg/bg colors.
fn make_colorful_tui_grid() -> Grid {
    let mut g = Grid::new(240, 80);
    for row in 0..240 {
        for col in 0..80 {
            let cell = g.cell_mut(row, col);
            // Simulate syntax highlighting: every other cell gets a palette color.
            if (row + col) % 2 == 0 {
                cell.fg = CellColor::Palette(((row + col) % 16) as u8);
            }
            // Simulate code structure: keywords, strings, comments.
            let ch = match (row % 4, col % 8) {
                (0, 0) => 'f',
                (0, 1) => 'n',
                (0, 2) => ' ',
                (1, 0..=4) => char::from(b'a' + (col % 26) as u8),
                (2, 0) => '/',
                (2, 1) => '/',
                _ => char::from(b'a' + ((row + col) % 26) as u8),
            };
            cell.character = ch;
            // Some cells get a non-default bg (e.g., visual selection in vim).
            if row % 20 == 0 && col < 40 {
                cell.bg = CellColor::Palette(238); // dark gray bg
            }
        }
    }
    g
}

/// Build a 240×80 grid with a selection active over rows 5-10.
fn make_selection_grid() -> (Grid, SelectionHandler) {
    let mut g = make_prompt_grid();
    // Add selection over rows 5-10, cols 0-40
    let sel = Selection::new(
        GridPos::new(5, 0),
        GridPos::new(10, 40),
        SelectionMode::Line,
    );
    let mut sh = SelectionHandler::new();
    sh.selection = Some(sel);
    sh.selecting = false;
    // Populate rows 5-10 with text so the selection has content
    for row in 5..=10 {
        let line = format!("selected line content row_{row} with some text");
        for (col, ch) in line.chars().enumerate() {
            if col < 80 {
                g.cell_mut(row, col).character = ch;
            }
        }
    }
    (g, sh)
}

/// Build a 240×80 grid simulating streaming output (new content arriving).
/// ~1000 chars of text in the last ~15 rows, rest default-bg spaces.
fn make_streaming_grid() -> Grid {
    let mut g = Grid::new(240, 80);
    // Simulate streaming log output in the last 15 rows
    for row in 225..240 {
        let line = format!(
            "[2026-07-26T10:32:{:02}] INFO processing request {} in {}ms",
            row % 60,
            row * 13,
            (row % 100) * 7
        );
        for (col, ch) in line.chars().enumerate() {
            if col < 80 {
                g.cell_mut(row, col).character = ch;
            }
        }
    }
    // Cursor at the start of the next line
    g.cell_mut(239, 0).character = '$';
    g.cell_mut(239, 1).character = ' ';
    g
}

/// Run one scenario: build single-stream + dual-stream counts and measure time.
fn run_scenario(
    name: &str,
    grid: &Grid,
    palette: &[Color; 256],
    selection: &SelectionHandler,
    cursor_row: usize,
    cursor_col: usize,
    show_cursor: bool,
) -> ScenarioReport {
    let default_fg = [
        Color::DEFAULT_FG.r as f32 / 255.0,
        Color::DEFAULT_FG.g as f32 / 255.0,
        Color::DEFAULT_FG.b as f32 / 255.0,
        Color::DEFAULT_FG.a as f32 / 255.0,
    ];
    let default_bg = [
        Color::DEFAULT_BG.r as f32 / 255.0,
        Color::DEFAULT_BG.g as f32 / 255.0,
        Color::DEFAULT_BG.b as f32 / 255.0,
        Color::DEFAULT_BG.a as f32 / 255.0,
    ];
    let selection_bg = [0.3, 0.5, 0.8, 0.6];

    // Measure single-stream collection time.
    let single_start = Instant::now();
    let cells = collect_resolved_cells(&CollectParams {
        grid,
        palette,
        default_fg,
        default_bg,
        selection,
        selection_bg,
        cursor_row,
        cursor_col,
        show_cursor,
    });
    let single_build_us = single_start.elapsed().as_micros() as u64;

    let single_instances = count_single_stream(&cells);
    let single_bytes = single_instances * SINGLE_STREAM_INSTANCE_BYTES;

    // Measure dual-stream collection time.
    let dual_start = Instant::now();
    let (dual_bg_runs, dual_glyph_instances) = count_dual_stream(&cells);
    let dual_build_us = dual_start.elapsed().as_micros() as u64;

    let dual_bytes =
        dual_bg_runs * DUAL_BG_RUN_BYTES + dual_glyph_instances * DUAL_GLYPH_INSTANCE_BYTES;
    let reduction_pct = if single_bytes > 0 {
        (1.0 - dual_bytes as f64 / single_bytes as f64) * 100.0
    } else {
        0.0
    };

    let total_cells = cells
        .iter()
        .map(|r| r.iter().filter(|c| c.is_some()).count())
        .sum();

    ScenarioReport {
        name: name.to_string(),
        rows: grid.num_rows,
        cols: grid.num_cols,
        total_cells,
        single_instances,
        single_bytes,
        dual_bg_runs,
        dual_glyph_instances,
        dual_bytes,
        reduction_pct,
        single_build_us,
        dual_build_us,
    }
}

#[test]
#[ignore = "v1.4.2 Phase A benchmark; run with --release --ignored --nocapture"]
fn bench_build_grid_instances() {
    let palette = Color::standard_palette();

    let scenarios: Vec<ScenarioReport> = vec![
        // Scenario 1: default bg (empty grid, all spaces)
        {
            let g = make_default_bg_grid();
            let sh = SelectionHandler::new();
            run_scenario("default_bg", &g, &palette, &sh, 0, 0, false)
        },
        // Scenario 2: prompt + command output (~500 chars of text)
        {
            let g = make_prompt_grid();
            let sh = SelectionHandler::new();
            run_scenario("prompt", &g, &palette, &sh, 31, 2, true)
        },
        // Scenario 3: colorful TUI (vim, ~50% cells colored)
        {
            let g = make_colorful_tui_grid();
            let sh = SelectionHandler::new();
            run_scenario("colorful_tui", &g, &palette, &sh, 0, 0, false)
        },
        // Scenario 4: selection active over 6 rows
        {
            let (g, sh) = make_selection_grid();
            run_scenario("selection", &g, &palette, &sh, 31, 2, true)
        },
        // Scenario 5: streaming output (new content, mostly default bg)
        {
            let g = make_streaming_grid();
            let sh = SelectionHandler::new();
            run_scenario("streaming", &g, &palette, &sh, 239, 2, true)
        },
    ];

    // Render markdown report.
    println!();
    println!("## v1.4.2 Phase A: background-run merge prototype");
    println!();
    println!("Grid size: 240×80 (19200 cells). Single-stream = 16 floats/cell (64 B).");
    println!("Dual-stream = 8 floats/bg-run (32 B) + 16 floats/glyph (64 B).");
    println!();
    println!("| scenario | size | cells | single inst | single bytes | dual bg runs | dual glyphs | dual bytes | reduction | single build | dual build |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for s in &scenarios {
        println!("{}", s.render_markdown_row());
    }
    println!();

    // GO/NO-GO: prototype must reduce upload bytes by ≥ 40% in at least
    // 2 of 5 representative scenarios (the plan says "≥ 40%" — we report
    // which scenarios meet the bar; the decision doc records the outcome).
    let go_threshold = 40.0;
    let passing: Vec<&ScenarioReport> = scenarios
        .iter()
        .filter(|s| s.reduction_pct >= go_threshold)
        .collect();
    println!("### GO/NO-GO: upload-bytes reduction ≥ 40%");
    println!();
    for s in &scenarios {
        let status = if s.reduction_pct >= go_threshold {
            "✅ GO"
        } else {
            "❌ NO-GO"
        };
        println!(
            "- {}: {:.1}% reduction — {}",
            s.name, s.reduction_pct, status
        );
    }
    println!();
    println!("Passing scenarios: {}/{}", passing.len(), scenarios.len());
}
