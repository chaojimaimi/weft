//! Tests for grid + block search (`super` / `super::blocks`).

use super::*;
use crate::blocks::{Block, BlockId};
use crate::grid::{CellFlags, Grid, Row};

fn write(grid: &mut Grid, row: usize, col: usize, s: &str) {
    let mut c = col;
    for ch in s.chars() {
        if c >= grid.num_cols {
            break;
        }
        let cell = grid.cell_mut(row, c);
        cell.character = ch;
        cell.flags = CellFlags::DIRTY;
        // Mark wide chars so find_in_grid reports match length in cells
        // (matching the real vt print path which sets CellWidth::Full).
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        let wide = w > 1;
        cell.width = if wide {
            crate::grid::CellWidth::Full
        } else {
            crate::grid::CellWidth::Half
        };
        // T3 review P2-2 fixture fix: the print path also flags the spacer
        // half — without it the snapshot builder can't skip the blank cell
        // and column arithmetic drifts after every wide char.
        if wide && c + 1 < grid.num_cols {
            grid.viewport[row].cells[c + 1].flags |= CellFlags::WIDE_SPACER;
        }
        c += if wide { 2 } else { 1 };
    }
}

fn make_block(id: u64, command: &str, output: &str) -> Block {
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::now(),
        finished_at: Some(std::time::SystemTime::now()),
        collapsed: false,
        screen_origin: false,
    }
}
#[test]
fn empty_query_returns_empty() {
    let mut g = Grid::new(5, 20);
    write(&mut g, 0, 0, "hello world");
    assert!(find_in_grid(&g, "", false, false).is_empty());
}

#[test]
fn finds_substring_case_insensitive() {
    let mut g = Grid::new(5, 20);
    write(&mut g, 0, 0, "Hello World");
    let m = find_in_grid(&g, "world", false, false);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].row, 0);
    assert_eq!(m[0].col, 6);
    assert_eq!(m[0].len, 5);
}

#[test]
fn finds_multiple_matches_in_document_order() {
    let mut g = Grid::new(5, 20);
    write(&mut g, 0, 0, "foo bar foo");
    write(&mut g, 1, 0, "baz foo qux");
    let m = find_in_grid(&g, "foo", false, false);
    assert_eq!(m.len(), 3);
    assert_eq!(
        m[0],
        FindMatch {
            row: 0,
            col: 0,
            len: 3
        }
    );
    assert_eq!(
        m[1],
        FindMatch {
            row: 0,
            col: 8,
            len: 3
        }
    );
    assert_eq!(
        m[2],
        FindMatch {
            row: 1,
            col: 4,
            len: 3
        }
    );
}

#[test]
fn wide_char_match_len_is_in_cells() {
    // 中 is double-width: 2 cells.
    let mut g = Grid::new(3, 20);
    write(&mut g, 0, 0, "中");
    let m = find_in_grid(&g, "中", false, false);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].len, 2, "CJK char match length is in cells");
}

#[test]
fn case_sensitive_skips_different_case() {
    let mut g = Grid::new(2, 20);
    write(&mut g, 0, 0, "Hello World");
    // Case-sensitive: "world" doesn't match "World".
    assert!(find_in_grid(&g, "world", true, false).is_empty());
    // Case-sensitive: "World" matches exactly once.
    let m = find_in_grid(&g, "World", true, false);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].col, 6);
}

#[test]
fn case_sensitive_blocks_skips_different_case() {
    let blocks = [make_block(1, "Echo HELLO", "out")];
    // Case-insensitive matches HELLO → hello.
    assert_eq!(
        find_in_blocks(blocks.iter(), "hello", false, false)
            .unwrap()
            .len(),
        1
    );
    // Case-sensitive: "hello" ≠ "HELLO".
    assert!(find_in_blocks(blocks.iter(), "hello", true, false)
        .unwrap()
        .is_empty());
    // Case-sensitive: exact match.
    assert_eq!(
        find_in_blocks(blocks.iter(), "HELLO", true, false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn find_in_blocks_returns_command_and_output_matches() {
    let blocks = [
        make_block(1, "echo hello", "hello\nworld hello"),
        make_block(2, "ls", "nothing here"),
    ];
    let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
    // 1 in command of block 1, 2 in output of block 1, 0 in block 2.
    assert_eq!(m.len(), 3);
    assert_eq!(m[0].block_id, BlockId(1));
    assert!(m[0].is_command);
    assert_eq!(m[0].line, 0);
    assert_eq!(m[0].col, 5);
    assert_eq!(m[0].len, 5);
    assert!(!m[1].is_command);
    assert_eq!(m[1].line, 0); // first line of output "hello"
    assert_eq!(m[2].line, 1); // second line "world hello"
    assert_eq!(m[2].col, 6);
}

#[test]
fn find_in_blocks_empty_query_returns_empty() {
    let blocks = [make_block(1, "echo hi", "hi")];
    assert!(find_in_blocks(blocks.iter(), "", false, false)
        .unwrap()
        .is_empty());
}

#[test]
fn find_in_blocks_case_insensitive() {
    let blocks = [make_block(1, "Echo HELLO", "out")];
    let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
    assert_eq!(m.len(), 1);
    assert!(m[0].is_command);
}

#[test]
fn regex_finds_in_blocks() {
    let blocks = [make_block(1, "echo foo123bar", "abc456def xyz")];
    let m = find_in_blocks(blocks.iter(), "[a-z]+[0-9]+", false, true).unwrap();
    assert_eq!(m.len(), 2);
    assert!(m[0].is_command); // foo123 in command
    assert!(!m[1].is_command); // abc456 in output
}

// ── v0.9 U-P1/U-P2: snapshot + regex tests ──────────────────────────

#[test]
fn snapshot_finds_same_matches_as_grid() {
    // Snapshot path must agree with the direct grid path on the same content.
    let mut g = Grid::new(3, 20);
    write(&mut g, 0, 0, "foo bar foo");
    write(&mut g, 1, 0, "baz foo qux");
    let direct = find_in_grid(&g, "foo", false, false);
    let snap = g.find_snapshot();
    let snap_matches = find_in_snapshot(&snap, "foo", false, false).unwrap();
    assert_eq!(
        direct, snap_matches,
        "snapshot matches must equal grid matches"
    );
}

#[test]
fn snapshot_supports_case_sensitive() {
    let mut g = Grid::new(2, 20);
    write(&mut g, 0, 0, "Hello World");
    let snap = g.find_snapshot();
    // Case-sensitive: "world" doesn't match "World".
    assert!(find_in_snapshot(&snap, "world", true, false)
        .unwrap()
        .is_empty());
    // Case-sensitive: "World" matches.
    let m = find_in_snapshot(&snap, "World", true, false).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].col, 6);
}

#[test]
fn regex_finds_pattern_matches() {
    // `foo.*bar` should match the whole "fooxyzbar" span.
    let mut g = Grid::new(2, 30);
    write(&mut g, 0, 0, "fooxyzbar baz");
    let m = find_in_grid(&g, "foo.*bar", false, true);
    assert_eq!(m.len(), 1, "regex should match fooxyzbar");
    assert_eq!(m[0].col, 0);
    assert_eq!(m[0].len, 9, "regex match length is in cells");
}

#[test]
fn regex_starting_inside_grapheme_maps_back_to_owning_cell() {
    let mut g = Grid::new(2, 20);
    write(&mut g, 0, 0, "ex");
    g.viewport[0].cells[0].flags.insert(CellFlags::EXTRA);
    g.viewport[0]
        .extras
        .set_grapheme(0, std::sync::Arc::from("e\u{0301}"));

    let direct = find_in_grid(&g, "\u{0301}x", true, true);
    assert_eq!(
        direct,
        [FindMatch {
            row: 0,
            col: 0,
            len: 2,
        }]
    );

    let snapshot = g.find_snapshot();
    let background = find_in_snapshot(&snapshot, "\u{0301}x", true, true).unwrap();
    assert_eq!(background, direct);
}

#[test]
fn regex_invalid_returns_empty() {
    let mut g = Grid::new(2, 20);
    write(&mut g, 0, 0, "hello");
    // Invalid regex `[a-z` (unclosed class) — find_in_grid returns empty.
    assert!(find_in_grid(&g, "[a-z", false, true).is_empty());
    // find_in_snapshot returns Err(RegexError).
    let snap = g.find_snapshot();
    assert!(find_in_snapshot(&snap, "[a-z", false, true).is_err());
}

#[test]
fn regex_snapshot_matches_grid_regex() {
    let mut g = Grid::new(3, 30);
    write(&mut g, 0, 0, "foo123bar");
    write(&mut g, 1, 0, "abc456def");
    let direct = find_in_grid(&g, "[a-z]+[0-9]+", false, true);
    let snap = g.find_snapshot();
    let snap_matches = find_in_snapshot(&snap, "[a-z]+[0-9]+", false, true).unwrap();
    // Both paths should find the same number of matches (one per line).
    assert_eq!(
        direct.len(),
        snap_matches.len(),
        "regex match count must agree"
    );
    for (d, s) in direct.iter().zip(snap_matches.iter()) {
        assert_eq!(d.row, s.row, "row must agree");
        assert_eq!(d.len, s.len, "len must agree");
    }
}

// ── T3: additional find coverage ───────────────────────────────────

/// Helper: build a Row with the given text written into its cells.
fn make_row(num_cols: usize, text: &str) -> crate::grid::Row {
    let mut row = crate::grid::Row::new(num_cols);
    for (i, ch) in text.chars().enumerate() {
        if i >= num_cols {
            break;
        }
        row.cells[i].character = ch;
        row.cells[i].flags = CellFlags::DIRTY;
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
        row.cells[i].width = if w > 1 {
            crate::grid::CellWidth::Full
        } else {
            crate::grid::CellWidth::Half
        };
    }
    row
}

#[test]
fn find_in_grid_covers_scrollback_and_viewport() {
    // find_in_grid uses a unified row index over (scrollback, viewport).
    // A match in scrollback and a match in viewport should both be found,
    // with row indices reflecting their unified position.
    let mut g = Grid::new(3, 25);
    // Push two rows into scrollback (the older region).
    g.scrollback.push(make_row(25, "old foo here"));
    g.scrollback.push(make_row(25, "second scrollback foo"));
    // Viewport has a match too.
    write(&mut g, 0, 0, "viewport foo");
    write(&mut g, 1, 0, "no match here");
    write(&mut g, 2, 0, "another foo");

    let m = find_in_grid(&g, "foo", false, false);
    // 4 matches: 2 in scrollback + 2 in viewport.
    assert_eq!(m.len(), 4);
    // Scrollback matches come first (unified row 0, 1).
    assert_eq!(m[0].row, 0, "scrollback row 0");
    assert_eq!(m[1].row, 1, "scrollback row 1");
    // Viewport matches follow at rows sb_len + vp_row.
    let sb_len = g.scrollback_len();
    assert_eq!(m[2].row, sb_len, "viewport row 0");
    assert_eq!(m[3].row, sb_len + 2, "viewport row 2");
}

#[test]
fn find_in_blocks_matches_command_only() {
    // When only the command line contains the needle (not the output),
    // find_in_blocks must still return a match marked is_command=true.
    let blocks = [make_block(7, "grep hello files", "no output matching")];
    let m = find_in_blocks(blocks.iter(), "hello", false, false).unwrap();
    assert_eq!(m.len(), 1);
    assert!(m[0].is_command, "match should be in command, not output");
    assert_eq!(m[0].block_id, BlockId(7));
    assert_eq!(m[0].col, 5);
    assert_eq!(m[0].len, 5);
}

#[test]
fn find_case_sensitive_empty_query_returns_empty() {
    // An empty query with case_sensitive=true must still return empty
    // (the early-return guard is independent of case sensitivity).
    let mut g = Grid::new(2, 20);
    write(&mut g, 0, 0, "hello world");
    assert!(find_in_grid(&g, "", true, false).is_empty());
    // find_in_snapshot path too.
    let snap = g.find_snapshot();
    assert!(find_in_snapshot(&snap, "", true, false).unwrap().is_empty());
    // And the blocks path.
    let blocks = [make_block(1, "echo hi", "hi")];
    assert!(find_in_blocks(blocks.iter(), "", true, false)
        .unwrap()
        .is_empty());
}

// ── T3 (D5-1): flat byte-snapshot search spec ──────────────────────────

/// The snapshot's flat scrollback half must search CJK, multi-scalar
/// clusters, and plain anchors with exact (row, col, len-in-cells), must
/// find hits on soft-wrapped continuation rows, must NOT match across a
/// wrap boundary (per-row semantics, unchanged from the cell-walk model),
/// and must not let hyperlink URIs leak into searchable text.
#[test]
fn find_snapshot_flat_history_search_spec() {
    use std::sync::Arc;

    let mut grid = Grid::with_scrollback(2, 80, 10);
    // Physical row 0: CJK lead + multi-scalar cluster + hyperlink anchor;
    // soft-wrapped into row 1.
    let mut r0 = Row::new(80);
    r0.cells[0].character = '汉';
    r0.cells[0].width = crate::grid::CellWidth::Full;
    r0.cells[1].flags |= CellFlags::WIDE_SPACER;
    r0.cells[2].character = 'e';
    r0.cells[2].flags |= CellFlags::EXTRA;
    r0.extras.set_grapheme(2, Arc::from("e\u{0301}"));
    for (i, ch) in "doc".chars().enumerate() {
        let cell = &mut r0.cells[4 + i];
        cell.character = ch;
        cell.flags |= CellFlags::HYPERLINK;
        r0.extras.set_hyperlink(4 + i, Some(7));
    }
    for (i, ch) in "see the ".chars().enumerate() {
        r0.cells[8 + i].character = ch;
    }
    r0.wrapped = true;
    // Physical row 1: the wrap continuation (logically "…see the tail").
    let mut r1 = Row::new(80);
    for (i, ch) in "tail".chars().enumerate() {
        r1.cells[i].character = ch;
    }
    grid.scrollback.push(r0);
    grid.scrollback.push(r1);
    // A live viewport row for the unified-index check.
    write(&mut grid, 0, 0, "viewport needle");

    let snap = grid.find_snapshot();
    // 2 retained scrollback rows + the grid's 2 viewport rows.
    assert_eq!(snap.total_rows(), 4);

    // CJK: col in cells, len 2.
    assert_eq!(
        find_in_snapshot(&snap, "汉", false, false).unwrap(),
        vec![FindMatch {
            row: 0,
            col: 0,
            len: 2
        }]
    );
    // Multi-scalar cluster: full decomposed text matches, len 1 cell.
    assert_eq!(
        find_in_snapshot(&snap, "e\u{0301}", false, false).unwrap(),
        vec![FindMatch {
            row: 0,
            col: 2,
            len: 1
        }]
    );
    // The lead scalar alone still matches the cluster cell.
    assert_eq!(
        find_in_snapshot(&snap, "e", false, false).unwrap()[0].col,
        2
    );
    // Hyperlink URIs never leak into searchable text.
    assert!(find_in_snapshot(&snap, "weft.dev", false, false)
        .unwrap()
        .is_empty());
    // …while the anchor text matches with hyperlink ids attached.
    assert_eq!(
        find_in_snapshot(&snap, "doc", false, false).unwrap(),
        vec![FindMatch {
            row: 0,
            col: 4,
            len: 3
        }]
    );
    // Hit on the soft-wrapped continuation row: unified row 1.
    assert_eq!(
        find_in_snapshot(&snap, "tail", false, false).unwrap(),
        vec![FindMatch {
            row: 1,
            col: 0,
            len: 4
        }]
    );
    // A query spanning the wrap boundary does not match (per-row search).
    assert!(find_in_snapshot(&snap, "the tail", false, false)
        .unwrap()
        .is_empty());
    // Viewport rows search with unified indices past the scrollback.
    assert_eq!(
        find_in_snapshot(&snap, "needle", false, false).unwrap(),
        vec![FindMatch {
            row: 2,
            col: 9,
            len: 6
        }]
    );
    // Chunked scan agrees with the whole-range scan.
    assert_eq!(
        find_in_snapshot_range(&snap, 1, 3, "tail", false, false).unwrap(),
        vec![FindMatch {
            row: 1,
            col: 0,
            len: 4
        }]
    );
}

/// T3 review P2-2: the RLE-run token walk must agree with `find_in_grid`'s
/// cell walk on the SAME materialized rows — CJK, multi-scalar clusters and
/// wide glyphs included. This differential is what authorizes the row-local
/// run arithmetic in place of `Index::content_offset_to_point` mapping.
#[test]
fn find_in_grid_and_flat_snapshot_agree_on_scrollback() {
    use std::sync::Arc;

    let mut grid = Grid::with_scrollback(2, 40, 10);
    let mut r0 = Row::new(40);
    r0.cells[0].character = '汉';
    r0.cells[0].width = crate::grid::CellWidth::Full;
    r0.cells[1].flags |= CellFlags::WIDE_SPACER;
    r0.cells[2].character = 'e';
    r0.cells[2].flags |= CellFlags::EXTRA;
    r0.extras.set_grapheme(2, Arc::from("e\u{0301}"));
    for (i, ch) in "doc".chars().enumerate() {
        let cell = &mut r0.cells[4 + i];
        cell.character = ch;
        cell.flags |= CellFlags::HYPERLINK;
        r0.extras.set_hyperlink(4 + i, Some(7));
    }
    r0.wrapped = true;
    let mut r1 = Row::new(40);
    for (i, ch) in "tail 中".chars().enumerate() {
        let cell = &mut r1.cells[i * 2];
        cell.character = ch;
        if ch != ' ' {
            cell.width = crate::grid::CellWidth::Full;
            r1.cells[i * 2 + 1].flags |= CellFlags::WIDE_SPACER;
        }
    }
    grid.scrollback.push(r0);
    grid.scrollback.push(r1);
    // Uppercase row: tripwire for the folding path shared by substring AND
    // regex (the T3 rework skipped folding for regex mode; `HELLO`/`hello`
    // regressed and this fixture exists so that cannot come back).
    let mut r2 = Row::new(40);
    for (i, ch) in "HELLO Doc".chars().enumerate() {
        r2.cells[i].character = ch;
    }
    grid.scrollback.push(r2);
    write(&mut grid, 0, 0, "viewport 中 needle");

    let snapshot = grid.find_snapshot();
    for query in [
        "汉",
        "e",
        "e\u{0301}",
        "doc",
        "DOC",
        "Doc",
        "tail",
        "中",
        "中 n",
        "needle",
        "the doc",
        "l",
    ] {
        for case_sensitive in [false, true] {
            let direct = find_in_grid(&grid, query, case_sensitive, false);
            let via_snapshot = find_in_snapshot(&snapshot, query, case_sensitive, false).unwrap();
            assert_eq!(
                direct, via_snapshot,
                "divergence for {query:?} (case_sensitive={case_sensitive})"
            );
        }
    }
    // Regex mode on the same corpus, including mixed-case patterns — the
    // two paths fold case identically for ASCII (both fold when
    // !case_sensitive) but diverge for non-ASCII, a documented pre-existing
    // split (see `find_in_snapshot_range`).
    for pattern in [
        "d[^ ]*c", "[a-z]+", "中|e", "^tail", "DOC", "Doc|tail", "[A-Z]",
    ] {
        let direct = find_in_grid(&grid, pattern, false, true);
        let via_snapshot = find_in_snapshot(&snapshot, pattern, false, true).unwrap();
        assert_eq!(direct, via_snapshot, "regex divergence for {pattern:?}");
    }
}
