use super::*;
use crate::blocks::DEFAULT_OUTPUT_CAP;

fn row(text: &str, cols: usize) -> Row {
    let mut row = Row::new(cols);
    for (index, ch) in text.chars().enumerate() {
        row.cells[index].character = ch;
    }
    row
}

/// v1.10.10 regression: an empty TUI input row between painted borders is a
/// real snapshot line. Mapping it to the upper border makes initial IME
/// preedit appear one row too high until committed text fills the input row.
#[test]
fn cursor_snapshot_line_tracks_empty_row_between_content() {
    let mut grid = Grid::with_scrollback(4, 40, 20);
    grid.viewport[0] = row("startup help", 40);
    grid.viewport[1] = row("----------------------------------------", 40);
    grid.viewport[2] = row("", 40);
    grid.viewport[3] = row("----------------------------------------", 40);
    grid.cursor.row = 2;

    let (text, _styled, cursor_line) = grid.document_snapshot_from_position_with_resolver(
        grid.scrollback.position(),
        |_| None,
        DEFAULT_OUTPUT_CAP,
    );
    assert_eq!(
        text,
        "startup help\n----------------------------------------\n\n----------------------------------------"
    );
    assert_eq!(cursor_line, Some(2));
}

#[test]
fn cursor_snapshot_line_tracks_second_of_multiple_middle_empty_rows() {
    let mut grid = Grid::with_scrollback(4, 20, 20);
    grid.viewport[0] = row("upper", 20);
    grid.viewport[2] = row("", 20);
    grid.viewport[3] = row("lower", 20);
    grid.cursor.row = 2;

    let (text, _, cursor_line) = grid.document_snapshot_from_position_with_resolver(
        grid.scrollback.position(),
        |_| None,
        DEFAULT_OUTPUT_CAP,
    );
    assert_eq!(text, "upper\n\n\nlower");
    assert_eq!(cursor_line, Some(2));
}

#[test]
fn cursor_snapshot_line_does_not_point_past_trailing_empty_rows() {
    for cursor_row in [1, 2] {
        let mut grid = Grid::with_scrollback(3, 20, 20);
        grid.viewport[0] = row("content", 20);
        grid.cursor.row = cursor_row;

        let (text, _, cursor_line) = grid.document_snapshot_from_position_with_resolver(
            grid.scrollback.position(),
            |_| None,
            DEFAULT_OUTPUT_CAP,
        );
        assert_eq!(text, "content");
        assert_eq!(cursor_line, None);
    }
}

#[test]
fn cursor_snapshot_line_does_not_claim_skipped_leading_empty_row() {
    let mut grid = Grid::with_scrollback(2, 20, 20);
    grid.viewport[1] = row("later content", 20);
    grid.cursor.row = 0;

    let (text, _, cursor_line) = grid.document_snapshot_from_position_with_resolver(
        grid.scrollback.position(),
        |_| None,
        DEFAULT_OUTPUT_CAP,
    );
    assert_eq!(text, "later content");
    assert_eq!(cursor_line, None);
}

#[test]
fn cursor_snapshot_line_stays_none_when_cursor_or_flush_row_is_unowned() {
    let mut grid = Grid::with_scrollback(3, 20, 20);
    grid.viewport[0] = row("upper", 20);
    grid.viewport[2] = row("lower", 20);
    grid.cursor.row = 1;
    let start = grid.scrollback.position();

    let (_, _, cursor_excluded) = grid
        .document_snapshot_from_position_with_ownership_masks_and_resolver(
            start,
            &[],
            &[true, false, true],
            |_| None,
            DEFAULT_OUTPUT_CAP,
        );
    assert_eq!(cursor_excluded, None);

    let (text, _, flush_excluded) = grid
        .document_snapshot_from_position_with_ownership_masks_and_resolver(
            start,
            &[],
            &[true, true, false],
            |_| None,
            DEFAULT_OUTPUT_CAP,
        );
    assert_eq!(text, "upper");
    assert_eq!(flush_excluded, None);
}
