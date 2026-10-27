use super::super::CellFlags;
use super::*;
use crate::blocks::{DEFAULT_OUTPUT_CAP, OUTPUT_CAP_MAX_MIB};

fn row(text: &str, cols: usize) -> Row {
    let mut row = Row::new(cols);
    for (index, ch) in text.chars().enumerate() {
        row.cells[index].character = ch;
    }
    // v1.15.3 P1a: direct cell writes bypass the production funnels —
    // normalize the occupancy tail.
    row.recompute_full();
    row
}

#[test]
fn document_keeps_command_scrollback_and_final_viewport_only() {
    let mut grid = Grid::with_scrollback(4, 40, 20);
    grid.scrollback.push(&row("older shell history", 40));
    let command_start = grid.scrollback.position();
    grid.scrollback.push(&row("complete answer line 1", 40));
    grid.viewport[0] = row("complete answer line 2", 40);
    grid.viewport[2] = row("Press Ctrl-C again to exit", 40);
    grid.viewport[3] = row("claude --resume session-id", 40);

    assert_eq!(
        grid.document_text_from(command_start, DEFAULT_OUTPUT_CAP),
        "complete answer line 1\ncomplete answer line 2\n\nPress Ctrl-C again to exit\nclaude --resume session-id"
    );
}

/// v1.10.7 regression: the cursor snapshot line must be exact even when
/// the document starts in the SCROLLBACK (the shell prompt row scrolled
/// above the viewport origin). The v1.10.6 tracker decremented across the
/// whole scrollback→viewport chain, shifting the match by the number of
/// scrollback rows between `document_start` and the viewport origin — the
/// BlockView caret/preedit then landed N rows above the real input row.
#[test]
fn cursor_snapshot_line_counts_viewport_rows_only() {
    // 6 rows: 2 scrollback + 4 viewport. The document starts at the
    // scrollback row right after the (excluded) shell prompt row, so
    // ONE scrollback row sits between document_start and the viewport.
    let mut grid = Grid::with_scrollback(4, 40, 2);
    grid.scrollback.push(&row("shell prompt row", 40));
    let document_start = grid.scrollback.position();
    grid.scrollback.push(&row("banner of the app", 40));
    grid.viewport[0] = row("app header line", 40);
    grid.viewport[1] = row("app content line", 40);
    grid.viewport[2] = row("input prompt: ", 40);
    grid.viewport[3] = row("", 40);
    // Cursor sits on viewport row 2 ("input prompt: ") — the row the
    // user is typing at. In the document it is line index 3
    // (banner, header, content, input) — NOT 3 - 1 scrollback row.
    grid.cursor.row = 2;
    grid.cursor.col = 14;

    let (text, _styled, cursor_line) = grid.document_snapshot_from_position_with_resolver(
        document_start,
        |_| None,
        DEFAULT_OUTPUT_CAP,
    );
    assert_eq!(
        text,
        "banner of the app\napp header line\napp content line\ninput prompt:",
    );
    assert_eq!(
        cursor_line,
        Some(3),
        "cursor on viewport row 2 must map to document line 3, not shifted by scrollback rows"
    );
}

/// The cursor row must still be tracked when the document starts exactly
/// at the viewport origin (no scrollback rows between).
#[test]
fn cursor_snapshot_line_tracks_cursor_at_viewport_origin_document() {
    let mut grid = Grid::with_scrollback(4, 40, 20);
    grid.viewport[0] = row("first document row", 40);
    grid.viewport[1] = row("", 40);
    grid.viewport[2] = row("input row here", 40);
    grid.cursor.row = 2;
    let document_start = grid.scrollback.position();

    let (text, _styled, cursor_line) = grid.document_snapshot_from_position_with_resolver(
        document_start,
        |_| None,
        DEFAULT_OUTPUT_CAP,
    );
    assert_eq!(text, "first document row\n\ninput row here");
    assert_eq!(cursor_line, Some(2));
}

#[test]
fn document_clamps_a_cleared_scrollback_baseline() {
    let mut grid = Grid::with_scrollback(2, 20, 20);
    grid.viewport[0] = row("final screen", 20);
    assert_eq!(
        grid.document_text_from(99, DEFAULT_OUTPUT_CAP),
        "final screen"
    );
}

#[test]
fn document_uses_a_logical_baseline_after_ring_buffer_is_full() {
    let mut grid = Grid::with_scrollback(1, 24, 2);
    grid.scrollback.push(&row("old 1", 24));
    grid.scrollback.push(&row("old 2", 24));
    let command_start = grid.scrollback.position();
    grid.scrollback.push(&row("new answer 1", 24));
    grid.scrollback.push(&row("new answer 2", 24));
    grid.viewport[0] = row("resume tail", 24);

    assert_eq!(
        grid.document_text_from(command_start, DEFAULT_OUTPUT_CAP),
        "new answer 1\nnew answer 2\nresume tail"
    );
}

#[test]
fn document_baseline_survives_clear_and_more_than_baseline_pushes() {
    let mut grid = Grid::with_scrollback(1, 24, 20);
    for index in 0..5 {
        grid.scrollback.push(&row(&format!("old {index}"), 24));
    }
    let command_start = grid.scrollback.position();
    grid.clear_scrollback();
    for index in 0..7 {
        grid.scrollback.push(&row(&format!("new {index}"), 24));
    }
    grid.viewport[0] = row("resume", 24);

    assert_eq!(
        grid.document_text_from(command_start, DEFAULT_OUTPUT_CAP),
        "new 0\nnew 1\nnew 2\nnew 3\nnew 4\nnew 5\nnew 6\nresume"
    );
}

#[test]
fn absolute_viewport_boundary_survives_scroll_ring_overflow_and_csi3j_clear() {
    let mut grid = Grid::with_scrollback(4, 24, 2);
    grid.viewport[0] = row("old shell", 24);
    grid.viewport[1] = row("command", 24);
    grid.viewport[2] = row("answer one", 24);
    grid.viewport[3] = row("answer two", 24);
    let document_start = grid.scrollback.position() + 2;

    grid.scroll_up(3);
    assert_eq!(
        grid.document_snapshot_from_position(document_start, DEFAULT_OUTPUT_CAP)
            .0,
        "answer one\nanswer two"
    );
    grid.clear_scrollback();
    assert_eq!(
        grid.document_snapshot_from_position(document_start, DEFAULT_OUTPUT_CAP)
            .0,
        "answer two"
    );
    for index in 0..4 {
        grid.scrollback.push(&row(&format!("new {index}"), 24));
    }
    assert_eq!(
        grid.document_snapshot_from_position(document_start, DEFAULT_OUTPUT_CAP)
            .0,
        "new 2\nnew 3\nanswer two"
    );
}

#[test]
fn owned_viewport_snapshot_omits_stale_rows_without_mutating_grid() {
    let mut grid = Grid::with_scrollback(5, 24, 16);
    grid.viewport[0] = row("Claude banner", 24);
    grid.viewport[1] = row("stale shell table", 24);
    grid.viewport[2] = row("restored answer", 24);
    grid.viewport[3] = row("stale shell footer", 24);
    grid.viewport[4] = row("prompt", 24);
    let owned = [true, false, true, false, true];

    let snapshot = grid
        .document_snapshot_from_position_with_ownership_masks_and_resolver(
            0,
            &[],
            &owned,
            |_| None,
            DEFAULT_OUTPUT_CAP,
        )
        .0;

    assert_eq!(snapshot, "Claude banner\nrestored answer\nprompt");
    assert_eq!(grid.row_text(1), "stale shell table");
    assert_eq!(grid.row_text(3), "stale shell footer");
}

#[test]
fn owned_snapshot_filters_scrollback_and_viewport_with_aligned_masks() {
    let mut grid = Grid::with_scrollback(3, 24, 16);
    grid.scrollback.push(&row("old shell context", 24));
    grid.scrollback.push(&row("owned answer page", 24));
    grid.viewport[0] = row("owned answer tail", 24);
    grid.viewport[1] = row("stale shell footer", 24);
    grid.viewport[2] = row("owned prompt", 24);

    let snapshot = grid
        .document_snapshot_from_position_with_ownership_masks_and_resolver(
            0,
            &[false, true],
            &[true, false, true],
            |_| None,
            DEFAULT_OUTPUT_CAP,
        )
        .0;

    assert_eq!(
        snapshot,
        "owned answer page\nowned answer tail\nowned prompt"
    );
}

#[test]
fn reflow_maps_a_blank_boundary_after_wrapped_shell_rows() {
    let mut grid = Grid::with_scrollback(5, 12, 16);
    grid.viewport[0] = row("old shell", 12);
    grid.viewport[0].wrapped = true;
    grid.viewport[1] = row("history", 12);
    grid.viewport[2] = row("claude", 12);
    grid.viewport[4] = row("startup", 12);
    grid.cursor.row = 4;
    let document_start = grid.scrollback.position() + 3;

    let (mapped, _map) = grid.resize_preserving_document_position(document_start, 6, 6);
    let snapshot = grid
        .document_snapshot_from_position(mapped, DEFAULT_OUTPUT_CAP)
        .0;

    assert!(!snapshot.contains("old shell"));
    assert!(!snapshot.contains("history"));
    assert!(!snapshot.contains("claude"));
    assert_eq!(snapshot.replace('\n', ""), "startup");
}

#[test]
fn document_snapshot_keeps_foreground_alignment_across_wide_cells() {
    let mut grid = Grid::with_scrollback(1, 8, 8);
    let mut styled = Row::new(8);
    styled.cells[0].character = 'A';
    styled.cells[0].fg = crate::grid::CellColor::Palette(1);
    styled.cells[1].character = '中';
    styled.cells[1].width = crate::grid::CellWidth::Full;
    styled.cells[1].fg = crate::grid::CellColor::Rgb(crate::grid::Color::rgb(2, 3, 4));
    styled.cells[1].bg = crate::grid::CellColor::Palette(7);
    styled.cells[2].flags.insert(CellFlags::WIDE_SPACER);
    styled.cells[3].character = 'B';
    styled.cells[3].fg = crate::grid::CellColor::Palette(5);
    grid.viewport[0] = styled;

    let (text, snapshot) = grid.document_snapshot_from(0, DEFAULT_OUTPUT_CAP);
    assert_eq!(text, "A中B");
    let line = snapshot.line(0).expect("colored line");
    assert_eq!(
        line.foreground_at(0),
        Some(crate::grid::CellColor::Palette(1))
    );
    assert_eq!(
        line.foreground_at(1),
        Some(crate::grid::CellColor::Rgb(crate::grid::Color::rgb(
            2, 3, 4
        )))
    );
    assert_eq!(
        line.foreground_at(2),
        Some(crate::grid::CellColor::Palette(5))
    );
    assert_eq!(line.background_at(0), None);
    assert_eq!(
        line.background_at(1),
        Some(crate::grid::CellColor::Palette(7))
    );
    assert_eq!(line.background_at(2), None);
}

#[test]
fn document_snapshot_streams_text_into_the_capture_limit() {
    let cols = 1_100;
    let mut grid = Grid::with_scrollback(1, cols, 1_100);
    let full = "x".repeat(cols);
    for _ in 0..1_100 {
        grid.scrollback.push(&row(&full, cols));
    }

    let (text, styled) = grid.document_snapshot_from(0, DEFAULT_OUTPUT_CAP);
    assert!(text.len() > DEFAULT_OUTPUT_CAP);
    assert!(text.len() <= DEFAULT_OUTPUT_CAP + 4);
    assert!(styled.lines.is_empty(), "truncated text cannot keep styles");
}

/// PLAN_v11217 §3.5 (T4): the walk budget is DYNAMIC — with cap=64 MiB
/// the same grid content that truncated at the 1 MiB default now survives
/// the walk intact (budget = cap + 4, single source: the passed cap).
#[test]
fn snapshot_text_budget_follows_the_configured_cap() {
    // §3.5 acceptance letter: cap=2 MiB → budget = 2 MiB + 4.
    assert_eq!(snapshot_text_budget(2 * 1024 * 1024), 2 * 1024 * 1024 + 4);
    let cols = 1_100;
    let mut grid = Grid::with_scrollback(1, cols, 1_100);
    let full = "x".repeat(cols);
    for _ in 0..1_100 {
        grid.scrollback.push(&row(&full, cols));
    }

    // Raised cap: the >1MiB content is retained (budget 64 MiB + 4).
    let raised = grid.document_snapshot_from(0, OUTPUT_CAP_MAX_MIB * 1024 * 1024);
    assert!(
        raised.0.len() > DEFAULT_OUTPUT_CAP + 4,
        "raised cap must lift the walk budget past the old constant, got {}",
        raised.0.len()
    );
    // 1,100 rows × 1,100 B + the 1,099 '\n' separators between them.
    assert_eq!(
        raised.0.len(),
        cols * 1_100 + (1_100 - 1),
        "nothing truncated under the raised budget"
    );

    // Default cap: the same content still truncates (legacy twin).
    let default = grid.document_snapshot_from(0, DEFAULT_OUTPUT_CAP);
    assert!(default.0.len() > DEFAULT_OUTPUT_CAP);
    assert!(default.0.len() <= DEFAULT_OUTPUT_CAP + 4);
}

#[test]
fn document_snapshot_discards_styles_after_the_rle_span_limit() {
    let cols = MAX_SNAPSHOT_COLOR_SPANS + 2;
    let mut grid = Grid::with_scrollback(1, cols, 1);
    let mut styled = Row::new(cols);
    for (index, cell) in styled.cells.iter_mut().enumerate() {
        cell.character = 'x';
        cell.fg = crate::grid::CellColor::Palette((index % 2 + 1) as u8);
    }
    // v1.15.3 P1a: direct cell writes bypass the production funnels.
    styled.recompute_full();
    grid.viewport[0] = styled;

    let (text, styled) = grid.document_snapshot_from(0, DEFAULT_OUTPUT_CAP);
    assert_eq!(text.len(), cols);
    assert!(styled.lines.is_empty());
}

/// v1.13.10 FIX (audit P1): a style-budget overflow must degrade per-row —
/// the overflowing row is skipped, but rows BEFORE it keep their styles
/// (the old `style_enabled=false; lines.clear()` was all-or-nothing).
#[test]
fn document_snapshot_overflow_row_degrades_but_keeps_other_rows_styles() {
    let cols = MAX_SNAPSHOT_COLOR_SPANS + 2;
    let mut grid = Grid::with_scrollback(2, cols, 1);

    // Row 0: small styled content that must survive the later overflow.
    let mut head = Row::new(cols);
    for (index, cell) in head.cells.iter_mut().take(8).enumerate() {
        cell.character = 'a';
        cell.fg = crate::grid::CellColor::Palette((index % 2 + 1) as u8);
    }
    // v1.15.3 P1a: direct cell writes bypass the production funnels.
    head.recompute_full();
    grid.viewport[0] = head;

    // Row 1: exceeds the whole 4096-span budget by itself.
    let mut styled = Row::new(cols);
    for (index, cell) in styled.cells.iter_mut().enumerate() {
        cell.character = 'x';
        cell.fg = crate::grid::CellColor::Palette((index % 2 + 1) as u8);
    }
    // v1.15.3 P1a: direct cell writes bypass the production funnels.
    styled.recompute_full();
    grid.viewport[1] = styled;

    let (text, out) = grid.document_snapshot_from(0, DEFAULT_OUTPUT_CAP);
    assert!(text.contains("aaaaaaaa"), "head row text intact");
    assert!(text.contains('x'), "overflow row text intact");
    assert_eq!(out.lines.len(), 1, "exactly the head row's styles survive");
    assert_eq!(out.lines[0].line, 0);
    assert!(!out.lines[0].foregrounds.is_empty());
}

#[test]
fn shrinking_scrollback_keeps_newest_command_rows() {
    let mut grid = Grid::with_scrollback(1, 24, 6);
    grid.scrollback.push(&row("old", 24));
    let command_start = grid.scrollback.position();
    for index in 0..5 {
        grid.scrollback.push(&row(&format!("answer {index}"), 24));
    }
    grid.scrollback.set_max_lines(3, 24);

    assert_eq!(
        grid.document_text_from(command_start, DEFAULT_OUTPUT_CAP),
        "answer 2\nanswer 3\nanswer 4"
    );
}
