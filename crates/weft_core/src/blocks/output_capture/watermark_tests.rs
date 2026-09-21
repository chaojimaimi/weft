//! M6-a (PLAN_M6 §A-1) rewrite-watermark unit tests: every content/cursor
//! op must either record its earliest touched offset (writes, CR, BS, EL,
//! row moves, gotos) or have a written safety argument for staying at/above
//! the tail boundary (append-only `print_ascii`, line-confined CHA/CUB,
//! guarded `replace`/`clear`). A missed record would silently re-enable the
//! live layout cache's append fast path over a rewritten document.

use super::*;

/// R9 guard: writes (print/newline/EL), CR, take/reset semantics, and the
/// append-only `print_ascii` fast path.
#[test]
fn watermark_records_writes_and_take_resets() {
    let mut output = OutputCapture::default();
    assert_eq!(output.min_write_offset_value(), usize::MAX, "fresh capture");
    output.print_ascii(b"hello", CapturedStyle::default(), 1024);
    assert_eq!(
        output.min_write_offset_value(),
        usize::MAX,
        "append-only print_ascii fast path must not lower the watermark"
    );
    output.newline(1024);
    assert_eq!(
        output.min_write_offset_value(),
        5,
        "newline records its pre-write cursor"
    );
    // Take semantics: read + reset to "pure append".
    assert_eq!(output.take_min_write_offset(), 5);
    assert_eq!(output.min_write_offset_value(), usize::MAX);
    // The cursor now sits after the '\n': appends stay at MAX.
    output.print_ascii(b"abc", CapturedStyle::default(), 1024);
    assert_eq!(output.min_write_offset_value(), usize::MAX);
    // CR records the tail line start (6); print the pre-cursor (6);
    // erase_line the pre-cursor (7) — the minimum stays 6, which equals
    // the sync boundary, so the cache's `>=` guard never false-trips.
    output.carriage_return();
    assert_eq!(output.min_write_offset_value(), 6);
    output.print('X', CapturedStyle::default(), 1024);
    assert_eq!(output.min_write_offset_value(), 6);
    output.erase_line(0);
    assert_eq!(output.min_write_offset_value(), 6);
    // A second take after further writes returns the new minimum.
    assert_eq!(output.take_min_write_offset(), 6);
    assert_eq!(output.min_write_offset_value(), usize::MAX);
}

/// `goto` to an early row (screen-exit tail materialization shape) must
/// pull the watermark below the tail boundary; `goto` within the tail
/// line must record exactly the boundary (no false trip).
#[test]
fn watermark_goto_early_row_trips_tail_boundary_is_exact() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"one\ntwo", CapturedStyle::default(), 1024);
    output.take_min_write_offset();
    // Tail line "two" starts at byte 4.
    output.goto(0, 0, 1024);
    assert_eq!(
        output.min_write_offset_value(),
        0,
        "early-row goto must invalidate the append fast path"
    );
    output.take_min_write_offset();
    assert_eq!(output.min_write_offset_value(), usize::MAX);
    output.goto(1, 1, 1024);
    assert_eq!(
        output.min_write_offset_value(),
        4,
        "tail-row goto records exactly the boundary"
    );
}

/// `clear` (on_command_start / screen handoff) records offset 0: the
/// new document shares no bytes with the consumed prefix, so the next
/// sync must not trust the old boundary.
#[test]
fn watermark_clear_records_offset_zero() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"hello", CapturedStyle::default(), 1024);
    output.take_min_write_offset();
    output.clear();
    assert_eq!(
        output.min_write_offset_value(),
        0,
        "cleared document shares no consumed prefix"
    );
}

/// `replace` (screen-snapshot path) is exempt from accounting — the sync
/// guard rejects the append fast path for screen-origin documents — but
/// `clear` in the same lifecycle still forces the fallback for the NEXT
/// ordinary command. Pin the co-existence.
#[test]
fn watermark_after_replace_cycle_forces_next_full_sync() {
    let mut output = OutputCapture::default();
    output.print_ascii(b"plain", CapturedStyle::default(), 1024);
    output.replace("frame\n", 1024);
    assert_eq!(output.take_min_write_offset(), usize::MAX);
    output.clear();
    assert_eq!(output.min_write_offset_value(), 0);
}
