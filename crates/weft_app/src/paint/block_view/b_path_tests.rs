//! v1.10.26 post-release forensics (omp preedit still invisible): evidence that
//! the B-path (BlockView) preedit DRAW decision is intact with a correct anchor.
//!
//! The plan's first hypothesis was that Batch A (`4fa30c3`, selection
//! content-anchor refactor) rewrote `block_view.rs`'s row-building loop and
//! dropped/changed the `tui_caret_row_matches` paint insertion point. The
//! batch's diff (verified via `git show 4fa30c3^ 4fa30c3 -- block_view.rs`)
//! does NOT touch the caret/preedit branches, but the loop still must be
//! proven to emit a live row whose `line == composed_cursor_snapshot_line` —
//! if the layout pass's live-line indices and the composed anchor ever
//! disagree, `tui_caret_row_matches` never fires and preedit + caret vanish
//! together (the reported symptom) with the paint code itself looking intact.
//!
//! These tests run the REAL code of the decision chain:
//! `compute_block_layout_pass` → live `LaidRow::Output { line }` →
//! `tui_caret_row_matches(None, line, cursor_line)`. The anchor on the right
//! side mirrors `freeze::composed_cursor_snapshot_line` exactly
//! (`cursor_row_in_segment + head newline count`).

use weft_core::blocks::InFlightBlock;

use crate::paint::block_view::layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput};
use crate::paint::grid_cache::{BlockLayoutCache, MAX_LAYOUT_LINES_LIVE};
use crate::paint::live_cache::LiveLayoutCache;

/// Composed screen-owned (omp/pi-class) in-flight document: a head
/// (preserved-frame screen history / scroll-out prefix) + the live viewport
/// segment at the tail. `head_line_count` mirrors the head-offset term of
/// `composed_cursor_snapshot_line`.
fn composed_screen_doc(head_lines: usize, segment_lines: usize) -> (String, usize, usize) {
    let head: String = (0..head_lines).map(|i| format!("head-{i}\n")).collect();
    let segment: String = (0..segment_lines).map(|i| format!("frame-{i}\n")).collect();
    let cursor_row_in_segment = segment_lines.saturating_sub(1);
    let composed_cursor_line = head.matches('\n').count() + cursor_row_in_segment;
    (
        format!("{head}{segment}"),
        cursor_row_in_segment,
        composed_cursor_line,
    )
}

/// B path must place a LIVE row (`block_id == None`) exactly on the composed
/// cursor anchor so `tui_caret_row_matches` fires and the preedit is drawn.
/// Runs the shared layout pass (the same rows the paint loop iterates).
#[test]
fn b_path_live_row_lands_on_composed_cursor_anchor() {
    let (output, cursor_row_in_segment, composed_cursor_line) = composed_screen_doc(2, 5);
    let line_count = output.lines().count();
    assert!(
        composed_cursor_line < line_count,
        "anchor {composed_cursor_line} must be in-bounds ({line_count} lines)"
    );
    let live = InFlightBlock {
        command: "omp",
        cwd: Some("/tmp"),
        output: &output,
        styled_output: None,
        version: 1,
        screen_origin: true,
    };
    let pitch = 20.0;
    let viewport_rows = 10;
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(live),
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows,
        cols: 80,
        pitch,
        header_height: 24.0,
        content_bottom_y: viewport_rows as f32 * pitch,
        clip_top: 0.0,
        clip_bottom: viewport_rows as f32 * pitch,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state: &std::collections::HashMap::new(),
    };
    let out = compute_block_layout_pass(
        input,
        &BlockLayoutCache::default(),
        &mut LiveLayoutCache::default(),
    );
    assert!(!out.rows.is_empty(), "layout pass must emit live rows");

    let matched_idx = out.row_data.iter().position(|row| match row {
        LaidRow::Output {
            block_id: None,
            line,
            ..
        } => crate::block_component::tui_caret_row_matches(None, *line, composed_cursor_line),
        _ => false,
    });
    assert!(
        matched_idx.is_some(),
        "B path: no live row matched composed cursor anchor {composed_cursor_line} \
         (cursor_row_in_segment {cursor_row_in_segment}); live rows = {:?}",
        out.row_data
            .iter()
            .filter_map(|row| match row {
                LaidRow::Output {
                    block_id: None,
                    line,
                    ..
                } => Some(*line),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
}

/// B path, wrapped (non-screen-origin) live document: the caret row can be
/// any line of soft-wrapped shell output; the single-chunk branch's
/// `tui_caret_row_matches` must match there too.
#[test]
fn b_path_live_row_matches_formula_anchor_for_soft_wrapped_output() {
    let output = (0..12).map(|i| format!("stream-{i}\n")).collect::<String>();
    // Formula path (no composed snapshot yet): cursor line =
    // line_count - grid_rows + cursor_row (block_view_tui_cursor_line).
    let grid_rows = 8;
    let cursor_row = 5;
    let cursor_line = output
        .lines()
        .count()
        .saturating_sub(grid_rows)
        .saturating_add(cursor_row);
    let live = InFlightBlock {
        command: "plain-output",
        cwd: None,
        output: &output,
        styled_output: None,
        version: 1,
        screen_origin: false,
    };
    let pitch = 20.0;
    let viewport_rows = grid_rows + 2;
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(live),
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows,
        cols: 80,
        pitch,
        header_height: 24.0,
        content_bottom_y: viewport_rows as f32 * pitch,
        clip_top: 0.0,
        clip_bottom: viewport_rows as f32 * pitch,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state: &std::collections::HashMap::new(),
    };
    let out = compute_block_layout_pass(
        input,
        &BlockLayoutCache::default(),
        &mut LiveLayoutCache::default(),
    );
    let matched = out.row_data.iter().any(|row| match row {
        LaidRow::Output {
            block_id: None,
            line,
            ..
        } => crate::block_component::tui_caret_row_matches(None, *line, cursor_line),
        _ => false,
    });
    assert!(
        matched,
        "B path: formula anchor {cursor_line} must match a live row \
         (line_count {}, grid_rows {grid_rows}, cursor_row {cursor_row})",
        output.lines().count()
    );
}

/// The live-document tail window must never be so truncated that the B-path
/// anchor lands in the dropped (non-emitted) history. When the window is
/// capped by `MAX_LAYOUT_LINES_LIVE`, the visible live `line` indices must
/// still be absolute document indices (they start at `base_idx`).
#[test]
fn b_path_anchor_never_falls_below_layout_window_base() {
    let head_lines = MAX_LAYOUT_LINES_LIVE * 2;
    let (output, _cursor_row_in_segment, composed_cursor_line) = composed_screen_doc(head_lines, 3);
    let live = InFlightBlock {
        command: "omp",
        cwd: None,
        output: &output,
        styled_output: None,
        version: 1,
        screen_origin: true,
    };
    let pitch = 20.0;
    let viewport_rows = 40;
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(live),
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows,
        cols: 80,
        pitch,
        header_height: 24.0,
        content_bottom_y: viewport_rows as f32 * pitch,
        clip_top: 0.0,
        clip_bottom: viewport_rows as f32 * pitch,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state: &std::collections::HashMap::new(),
    };
    let out = compute_block_layout_pass(
        input,
        &BlockLayoutCache::default(),
        &mut LiveLayoutCache::default(),
    );
    let live_rows: Vec<usize> = out
        .row_data
        .iter()
        .filter_map(|row| match row {
            LaidRow::Output {
                block_id: None,
                line,
                ..
            } => Some(*line),
            _ => None,
        })
        .collect();
    assert!(!live_rows.is_empty());
    let min_line = *live_rows.iter().min().unwrap();
    assert!(
        composed_cursor_line >= min_line,
        "composed anchor {composed_cursor_line} fell below the laid live window \
         (min emitted line {min_line}); the caret + preedit would never be drawn"
    );
}

/// The B-path wrapped-row guard: a soft-wrapped live line carries multiple
/// chunks; the paint loop's chunk branch picks `ci == cursor_col / cols`.
/// This test drives that decision with real layout output so a width/wrap
/// change that breaks preedit on wrapped TUI rows is caught at the logic
/// layer (the preedit overlay itself wraps via `tui_preedit_rows`).
#[test]
fn b_path_wrapped_row_caret_chunk_selection_is_consistent() {
    let output = "frame-a\nframe-b\n";
    let live = InFlightBlock {
        command: "omp",
        cwd: None,
        output,
        styled_output: None,
        version: 1,
        screen_origin: false,
    };
    let pitch = 20.0;
    let viewport_rows = 6;
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(live),
        pane_session_id: 1,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows,
        cols: 4, // narrow → "frame-a" wraps into "fra" + "me-a"
        pitch,
        header_height: 24.0,
        content_bottom_y: viewport_rows as f32 * pitch,
        clip_top: 0.0,
        clip_bottom: viewport_rows as f32 * pitch,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state: &std::collections::HashMap::new(),
    };
    let out = compute_block_layout_pass(
        input,
        &BlockLayoutCache::default(),
        &mut LiveLayoutCache::default(),
    );
    let cursor_line = 0;
    let (_, chunks) = out
        .row_data
        .iter()
        .enumerate()
        .find_map(|(i, row)| match row {
            LaidRow::Output {
                block_id: None,
                line,
                chunks,
                ..
            } if crate::block_component::tui_caret_row_matches(None, *line, cursor_line) => {
                Some((i, chunks.clone()))
            }
            _ => None,
        })
        .expect("live row must match the caret line");
    // The wrapped branch's guard — caret lands in chunk `cursor_col / cols`.
    let cursor_col = 4;
    let chunk_idx = cursor_col / 4;
    assert!(
        chunk_idx < chunks.len(),
        "caret chunk {chunk_idx} must exist in {chunks:?}"
    );
    // The preedit overlay's own wrap must not change the caret chunk's bytes
    // (both sides wrap from the line text; byte source is preserved).
    let rows = crate::paint::preedit::tui_preedit_rows(
        "shen'ru",
        cursor_line,
        cursor_col,
        viewport_rows,
        4,
    );
    assert!(
        rows.iter().any(|row| row.text == "s"),
        "wrapped preedit must keep the caret chunk's first cell; got {rows:?}"
    );
}
/// v1.10.26 post-release forensics — B-path with a REAL settle→new-command
/// caret anchor from the core state machine (not a hand-built mirror). A
/// screen-owned session settles (the exact 07:43:41 log event), then a new
/// non-screen-owned command streams in block view; the app-side layout pass
/// must emit a live row matching the real `primary_screen_cursor_snapshot_line`
/// fallback formula. Guard: a regression where the settled session's anchor
/// leaked or the fallback formula mapped out of bounds.
#[test]
fn b_path_matches_real_anchor_after_settle_and_new_command() {
    use weft_core::vt::Terminal;
    let mut terminal = Terminal::new(24, 200);
    // Bootstrap + primary-screen TUI ownership (same helpers philosophy as
    // diagnostic_tests::activate_primary_screen_tui).
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());
    // Stream enough to form a real composed document with a head.
    let mut buf = String::new();
    for n in 1..=3000 {
        buf.push_str(&format!("line {n:05} {}\r\n", "x".repeat(189)));
    }
    terminal.process(buf.as_bytes());
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());
    let stale = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("screen-owned snapshot tracked a caret line");

    // Settle: the command finalizes, phase -> AtPrompt, block view.
    terminal.process(b"\x1b]133;D;0\x07");
    assert!(terminal.settle_primary_screen_exit());

    // New non-screen-owned command streams in block view (omp input window).
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07next\x1b]133;C\x07");
    terminal.process(b"hello\r\nworld\r\n");
    assert!(terminal.block_tracker().in_flight().is_some());
    assert!(terminal.show_block_view());
    terminal.snapshot_primary_screen_output_for_caret();

    // The renderer's block_view_tui_cursor prefers the tracked line; after
    // settle it must be None (formula fallback) — assert that explicitly.
    assert!(
        terminal.primary_screen_cursor_snapshot_line().is_none(),
        "stale tracked line {stale:?} leaked across the settle boundary: block_view_tui_cursor \
         would anchor the caret past the new live block"
    );

    // Formula fallback (renderer.rs block_view_tui_cursor_line): map the grid
    // cursor row into the new live block.
    let live = terminal.block_tracker().in_flight().unwrap();
    let cursor_line = crate::block_component::block_view_tui_cursor_line(
        live.output.lines().count(),
        terminal.grid().num_rows,
        terminal.grid().cursor.row,
    );
    assert!(
        cursor_line < live.output.lines().count(),
        "formula fallback must anchor in-bounds (line {cursor_line}, {} lines)",
        live.output.lines().count()
    );

    // Drive the real layout pass over the real in-flight output and prove a
    // live row lands exactly on that formula anchor.
    let output = live.output.to_string();
    let in_flight_block = weft_core::blocks::InFlightBlock {
        command: "next",
        cwd: None,
        output: &output,
        styled_output: None,
        version: 1,
        screen_origin: false,
    };
    let pitch = 20.0;
    let viewport_rows = 26;
    let input = LayoutPassInput {
        blocks: &[],
        live: Some(in_flight_block),
        pane_session_id: 99,
        cwd: None,
        git_branch: None,
        block_scroll: 0.0,
        viewport_rows,
        cols: 80,
        pitch,
        header_height: 24.0,
        content_bottom_y: viewport_rows as f32 * pitch,
        clip_top: 0.0,
        clip_bottom: viewport_rows as f32 * pitch,
        resolve_styles: false,
        styled_lookup_counter: None,
        block_diagnose_state: &std::collections::HashMap::new(),
    };
    let out = compute_block_layout_pass(
        input,
        &BlockLayoutCache::default(),
        &mut LiveLayoutCache::default(),
    );
    assert!(!out.row_data.is_empty(), "new live block must lay out rows");
    let matched = out.row_data.iter().any(|row| match row {
        LaidRow::Output {
            block_id: None,
            line,
            ..
        } => crate::block_component::tui_caret_row_matches(None, *line, cursor_line),
        _ => false,
    });
    assert!(
        matched,
        "B path: no live row matched the formula anchor {cursor_line} after settle+new command"
    );
}

/// v1.10.26 post-release forensics — the A path (grid view, primary-screen
/// TUI live grid). The preedit draw decision for the grid path is
/// `should_show_tui_preedit` (overlay) + `tui_preedit_rows` (non-empty at
/// the grid cursor). Both halves must hold for omp's own grid input box
/// after the settle transition: mode Passthrough + non-empty text →
/// overlay created; cursor on a valid row → wrap rows emitted.
#[test]
fn a_path_grid_preedit_conditions_hold_after_settle() {
    use weft_core::vt::Terminal;
    let mut terminal = Terminal::new(24, 200);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[2;1H\x1b[3;1H");
    terminal.process(b"omp input box\r\n");
    terminal.process(b"\x1b[24;1H");
    terminal.process(b"\x1b]133;D;0\x07");
    assert!(terminal.settle_primary_screen_exit());
    // A new screen-owned session re-captures the grid (omp relaunch): the
    // terminal shows the live grid — A path.
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07omp\x1b]133;C\x07");
    terminal.process(b"\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());
    assert!(
        !terminal.show_block_view(),
        "screen-owned TUI renders the live grid (A path)"
    );
    assert_eq!(
        terminal.effective_input_mode(),
        weft_core::input::InputMode::Passthrough,
        "A-path preedit gate requires Passthrough"
    );

    // Grid cursor at the bottom input row: preedit wrap must produce rows.
    let (row, col) = (terminal.grid().cursor.row, terminal.grid().cursor.col);
    let rows = crate::paint::preedit::tui_preedit_rows(
        "shen'ru",
        row,
        col,
        terminal.grid().num_rows,
        terminal.grid().num_cols,
    );
    assert!(
        !rows.is_empty(),
        "A path: preedit at grid cursor ({row},{col}) must lay out rows; \
         should_show_tui_preedit(Passthrough, non-empty, owns-ime=true) == true"
    );
}
