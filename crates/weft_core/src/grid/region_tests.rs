//! v1.19.0 (PLAN_v1190 §3 P0-4): truth-table unit tests for the grid-region
//! read model (`region::logical_lines`) plus the D7 `Grid::revision` bump
//! table. Every scenario drives a real `Terminal::process` byte stream so
//! the region is exercised through the same mutation sites production hits
//! (print fast/slow lanes, ICH/EL edits, scroll, reflow) — no test-only
//! grid writers.

use super::region::{logical_lines, GridRegionSource};
use crate::vt::Terminal;

/// D4 head formula: inclusive end = document_index of the bottom-up first
/// non-empty viewport row + 1; an all-empty viewport collapses the region
/// to the scrollback prefix (`head = scrollback.len()`).
fn region_head(terminal: &Terminal) -> u64 {
    let grid = terminal.grid();
    let sb_len = grid.scrollback.len();
    (0..grid.num_rows)
        .rev()
        .find(|&row| !grid.displayed_row_text(row).trim_end().is_empty())
        .map_or(sb_len as u64, |row| (sb_len + row + 1) as u64)
}

/// Region texts with the D4 head formula applied.
fn region_texts(terminal: &Terminal, begin_position: u64) -> Vec<String> {
    logical_lines(
        terminal.grid(),
        GridRegionSource {
            begin_position,
            head: Some(region_head(terminal)),
        },
        |_| None,
    )
    .into_iter()
    .map(|line| line.text)
    .collect()
}

// ── Empty regions (truth table rows 1-2) ────────────────────────────────

/// A command that produced no output: `133;B` sampled, zero bytes printed —
/// the region must be empty, not "the prompt above the anchor".
#[test]
fn region_empty_when_command_has_no_output() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    let begin = terminal.grid().scrollback.position();
    assert!(
        region_texts(&terminal, begin).is_empty(),
        "no output → empty region"
    );
}

/// The suite's s01 anchor pin, core-side: `133;B` directly followed by
/// `133;D` samples a begin anchor but never fabricates content; the tracker
/// anchor lifecycle (sampled at B, cleared at D) is pinned alongside.
#[test]
fn region_empty_when_133b_directly_precedes_133d() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    let begin = terminal.grid().scrollback.position();
    assert_eq!(
        terminal.block_tracker().region_begin_position(),
        Some(begin),
        "133;B must sample the region begin anchor"
    );
    assert!(region_texts(&terminal, begin).is_empty());
    terminal.process(b"\x1b]133;D;0\x07");
    assert_eq!(
        terminal.block_tracker().region_begin_position(),
        None,
        "133;D must clear the region begin anchor"
    );
    assert!(region_texts(&terminal, begin).is_empty());
}

// ── Boundary defenses (truth table rows 3-4) ────────────────────────────

/// `head < begin` is a natural empty region — no `max(begin, head)` clamp
/// may fabricate content (the counter-check proves the document itself is
/// non-empty, so the emptiness comes from the boundary alone).
#[test]
fn head_below_begin_yields_empty_region_without_clamping() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"one\ntwo\nthree\nfour\nfive\n"); // position = 2
    let begin = terminal.grid().scrollback.position();
    assert_eq!(begin, 2, "two rows must have scrolled into scrollback");
    let lines = logical_lines(
        terminal.grid(),
        GridRegionSource {
            begin_position: begin,
            head: Some(0), // strictly below begin
        },
        |_| None,
    );
    assert!(lines.is_empty(), "head < begin → empty, never clamped");
    // Counter-check: the same document with a valid head is non-empty —
    // scrollback holds one/two, the viewport holds three/four/five, so the
    // region [2, 5) reads the viewport's three content rows.
    assert_eq!(
        region_texts(&terminal, begin),
        ["three".to_string(), "four".to_string(), "five".to_string()]
    );
}

/// Clear/recreate defense: an anchor from a previous buffer generation
/// (`begin_position > scrollback.position()`) yields an EMPTY region,
/// mirroring `FlatStorage::index_since`'s guard instead of clamping.
#[test]
fn begin_position_past_flat_position_yields_empty_region() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"line\n");
    let stale = terminal.grid().scrollback.position() + 100;
    let lines = logical_lines(
        terminal.grid(),
        GridRegionSource {
            begin_position: stale,
            head: None,
        },
        |_| None,
    );
    assert!(lines.is_empty(), "stale anchor → empty region (no clamp)");
}

// ── Line shape (D1: wrapped merge, trim_end, edge empties kept) ──────────

/// ICH echo scenario: `ESC[3@` + print must show the real rewritten row —
/// the shifted tail survives and the inserted text lands in the middle.
#[test]
fn ich_inline_edit_reflects_in_region_output() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"abcdef");
    terminal.process(b"\x1b[D\x1b[D\x1b[D"); // ← ×3 — CUB 3
    terminal.process(b"\x1b[3@"); // ICH 3 — "def" shifts right, blanks open
    terminal.process(b"XYZ"); // overwrite the fresh blanks
    assert_eq!(
        region_texts(&terminal, 0),
        ["abcXYZdef".to_string()],
        "region must reflect the in-place ICH rewrite"
    );
}

/// Consecutive rows flagged `wrapped` merge into ONE logical line.
#[test]
fn soft_wrapped_rows_merge_into_one_logical_line() {
    let mut terminal = Terminal::new(4, 10);
    terminal.process(b"ABCDEFGHIJKLMN"); // 14 chars @ 10 cols → rows 0+1
    let lines = logical_lines(
        terminal.grid(),
        GridRegionSource {
            begin_position: 0,
            head: Some(region_head(&terminal)),
        },
        |_| None,
    );
    assert_eq!(lines.len(), 1, "wrapped rows must merge: {:?}", lines);
    assert_eq!(lines[0].text, "ABCDEFGHIJKLMN");
    assert!(!lines[0].wrapped_tail, "the run closes inside the region");
}

/// The region cutting INSIDE a wrapped run flushes the partial line with
/// `wrapped_tail = true` (document rows continue past `head`).
#[test]
fn region_cut_mid_wrap_reports_wrapped_tail() {
    let mut terminal = Terminal::new(4, 10);
    terminal.process(b"ABCDEFGHIJKLMN"); // row 0 wrapped, row 1 the tail
    let lines = logical_lines(
        terminal.grid(),
        GridRegionSource {
            begin_position: 0,
            head: Some(1), // cut after row 0, mid-run
        },
        |_| None,
    );
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "ABCDEFGHIJ");
    assert!(
        lines[0].wrapped_tail,
        "end < total with a wrapped last row → wrapped_tail"
    );
}

/// Every logical line is `trim_end`ed (snapshot-side extent semantics).
#[test]
fn logical_lines_are_trim_end_per_line() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"abc   \n"); // trailing blanks on row 0
    terminal.process(b"def");
    assert_eq!(
        region_texts(&terminal, 0),
        ["abc".to_string(), "def".to_string()]
    );
}

/// Head boundary (inclusive-end correction): the LAST content row must be
/// inside the region even though trailing empty rows are excluded.
#[test]
fn head_includes_the_last_content_row() {
    let mut terminal = Terminal::new(6, 20);
    terminal.process(b"first\nlast\n"); // rows 0-1, rows 2-5 stay blank
    let texts = region_texts(&terminal, 0);
    assert_eq!(texts, ["first".to_string(), "last".to_string()]);
    assert_eq!(texts.last().map(String::as_str), Some("last"));
}

/// Interior empty rows stay as empty logical lines (dropping edge empties
/// is the app layer's job, P1-6 — the region keeps them).
#[test]
fn interior_empty_rows_stay_in_the_region() {
    let mut terminal = Terminal::new(6, 20);
    terminal.process(b"a\n\n\nb"); // rows: "a", blank, blank, "b"
    assert_eq!(
        region_texts(&terminal, 0),
        [
            "a".to_string(),
            String::new(),
            String::new(),
            "b".to_string()
        ]
    );
}

// ── D8 mixed case: in-place rewrite + tail append ───────────────────────

/// Tail growth AND old-row ICH/EL rewrite in one command: the rewritten
/// row must appear (not the stale original) alongside the appended tail.
#[test]
fn rewritten_old_row_and_new_tail_both_appear() {
    let mut terminal = Terminal::new(8, 20);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07");
    let begin = terminal.grid().scrollback.position();
    terminal.process(b"alpha\nbeta\ngamma\n"); // rows 0-2, cursor row 3
    terminal.process(b"\x1b[A\x1b[A\x1b[A"); // CUU ×3 → row 0
    terminal.process(b"\x1b[KNEWTAIL"); // EL right, then rewrite row 0
    terminal.process(b"\x1b[B\x1b[B\x1b[B\rdelta"); // CUD ×3 (column kept) → CR to col 0 → row 3, append
    assert_eq!(
        region_texts(&terminal, begin),
        [
            "NEWTAIL".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
        ],
        "D8: rewritten old row and appended tail must both be visible"
    );
}

// ── D7 revision bump table ──────────────────────────────────────────────

/// In-place print rewrite (same row, overwritten cells) bumps the revision.
#[test]
fn revision_bumps_on_in_place_print_rewrite() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"aaaaa");
    let before = terminal.grid().revision;
    terminal.process(b"\x1b[5Dbbbbb"); // CUB 5, overwrite cols 0-5
    assert!(
        terminal.grid().revision > before,
        "in-place rewrite must bump the region cache revision"
    );
}

/// Printing past the bottom row (scroll pushes rows into scrollback)
/// bumps the revision.
#[test]
fn revision_bumps_when_print_inserts_rows() {
    let mut terminal = Terminal::new(4, 20);
    let before = terminal.grid().revision;
    terminal.process(b"1\n2\n3\n4\n5\n"); // the last newline scrolls
    assert_eq!(
        terminal.grid().scrollback.len(),
        2,
        "two rows must have been pushed into scrollback"
    );
    assert!(
        terminal.grid().revision > before,
        "row insert/scroll must bump the region cache revision"
    );
}

/// A resize reflow (rewrap) bumps the revision — region caches keyed on
/// the revision rebuild wholesale (D5).
#[test]
fn revision_bumps_on_resize_reflow() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"payload text");
    let before = terminal.grid().revision;
    terminal.resize(3, 10);
    assert!(
        terminal.grid().revision > before,
        "reflow must bump the region cache revision"
    );
}
