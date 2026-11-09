// v1.18.9 capture-cursor caret anchor tests (split from tracker_tests.rs to
// keep both within the architecture gate; child-module privacy reaches the
// private fields exactly like the inline module did).
// Spec: docs/PLAN_v1189_CAPTURE_CURSOR_CARET.md §2 (T1-T7, T11).

use super::*;

/// Enter CommandExecuting and print `text` verbatim ('\n' → on_newline),
/// leaving the capture cursor wherever the byte stream ended.
fn capturing_tracker(text: &str) -> BlockTracker {
    let mut t = BlockTracker::new();
    t.on_prompt_start();
    t.on_command_start("nested".to_string());
    for ch in text.chars() {
        if ch == '\n' {
            t.on_newline();
        } else {
            t.on_print(ch, CapturedStyle::default());
        }
    }
    t
}

/// T1 (修复目标): CUB (bash ←) moves the anchor column off the tail end —
/// the mirrored cursor IS the caret now. A pure move leaves bytes intact.
#[test]
fn capture_anchor_follows_cub_in_tail_line() {
    let mut t = capturing_tracker("sh-3.2# ls -l | gr");
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(18));
    // bash ← ×2 → grid cursor col 16 → mirrored via on_set_cursor_column.
    t.on_set_cursor_column(16);
    assert_eq!(
        t.live_cursor_capture_anchor_col(),
        Some(16),
        "anchor column must equal bash's visual caret after ← ×2"
    );
    assert_eq!(
        t.output.as_str(),
        "sh-3.2# ls -l | gr",
        "pure move keeps bytes"
    );
}

/// T2: ICH (bash's insert-char echo) inserts spaces at the cursor without
/// moving it — the anchor stays parked on the edit head.
#[test]
fn capture_anchor_ignores_ich_insert() {
    let mut t = capturing_tracker("sh-3.2# ls -l | gr");
    t.on_insert_blank(3); // ICH ×3 at the cursor (== tail end)
    assert_eq!(
        t.live_cursor_capture_anchor_col(),
        Some(18),
        "ICH moves bytes, not the cursor"
    );
    assert_eq!(t.output.as_str(), "sh-3.2# ls -l | gr   ");
}

/// T3: DCH (bash delete-char) shrinks the tail under a stationary cursor;
/// a following ECH at the line end is a no-op. Anchor tracks the cursor.
#[test]
fn capture_anchor_ignores_dch_and_ech_shrink() {
    let mut t = capturing_tracker("sh-3.2# ls -l | gr");
    t.on_set_cursor_column(16);
    t.on_delete_chars(2); // DCH 2: "gr" gone, cursor still col 16
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(16));
    assert_eq!(t.output.as_str(), "sh-3.2# ls -l | ");
    t.on_erase_chars(2); // ECH 2 at the line end wipes nothing
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(16));
}

/// T4: CSI A progress repaint parks the mirrored cursor on an early row →
/// None → the renderer keeps the tail-end formula anchor (v1.18.8
/// equivalence for brew/ollama frames).
#[test]
fn capture_anchor_none_when_cursor_moves_off_tail_line() {
    let mut t = capturing_tracker("line1\nline2");
    t.on_move_cursor_rows(-1); // CSI A: cursor up to the "line1" row start
    assert_eq!(
        t.live_cursor_capture_anchor_col(),
        None,
        "cursor off the tail line → formula anchor owns the caret"
    );
    assert_eq!(t.live_cursor_tail_line(), 1, "tail line unchanged");
}

/// T4b (N3): CSI B/E down-move saturation — with a trailing '\n' the head
/// lands on the phantom row (None); without one it saturates at the text
/// end and the anchor equals the tail line's full width.
#[test]
fn capture_anchor_row_down_saturates_by_tail_shape() {
    let mut t = capturing_tracker("line1\nline2\n");
    t.on_move_cursor_rows(1);
    assert_eq!(t.live_cursor_capture_anchor_col(), None);
    let mut t = capturing_tracker("line1\nline2");
    t.on_move_cursor_rows(100);
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(5));
}

/// T5 (streaming bare-value table): with no cursor traffic the anchor is
/// the tail line's raw display width — exactly what the formula produced
/// for non-clamped shapes. Clamping (220@110 → 219) is the renderer's B1'
/// job, asserted in tui_caret_anchor_tests.rs.
#[test]
fn capture_anchor_streaming_shapes_match_hand_widths() {
    // Non-wrapped tail: bare content width.
    assert_eq!(
        capturing_tracker("abc").live_cursor_capture_anchor_col(),
        Some(3)
    );
    // Multi-chunk tail (180 wide @ 110 cols folds 110/70): bare 180 —
    // 180 % 110 != 0 so B1' must NOT bite (painter's modulo map is exact).
    assert_eq!(
        capturing_tracker(&"x".repeat(180)).live_cursor_capture_anchor_col(),
        Some(180)
    );
    // k=1 exact full row (80 wide @ 80 cols): bare 80 — T16c right-edge
    // park; the clamp is gated on width > cols.
    assert_eq!(
        capturing_tracker(&"x".repeat(80)).live_cursor_capture_anchor_col(),
        Some(80)
    );
    // Trailing '\n' phantom row → None (formula keeps the last COMPLETE
    // line's end).
    assert_eq!(
        capturing_tracker("abc\n").live_cursor_capture_anchor_col(),
        None
    );
}

/// T6: CJK counts display cells, not bytes/chars — and mirrored
/// set_cursor_column lands the anchor on wide-char boundaries exactly.
#[test]
fn capture_anchor_counts_cjk_display_width() {
    // "你" is 2 cells, 'a' 1: the anchor after "你a" is col 3.
    assert_eq!(
        capturing_tracker("你a").live_cursor_capture_anchor_col(),
        Some(3)
    );
    // CUB to display col 2 lands right after the wide char.
    let mut t = capturing_tracker("你a");
    t.on_set_cursor_column(2);
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(2));
    // CUB into the wide char's second column: v1.18.7 set_cursor_column
    // semantics snap the byte cursor to the char start, then the :41 pad
    // loop prints ' ' AT the cursor — and print OVERWRITES the character
    // under it (output_capture.rs replace path), so the capture text
    // becomes " a" (the '你' is consumed) with the cursor advanced to
    // byte 1. The anchor faithfully measures the post-pad capture content:
    // width(" ") == 1. Production columns stay on char boundaries (bash
    // emits wcwidth sums), so this path is defensive-only — cf. the
    // existing set_cursor_column_into_wide_char_emits_space_and_overwrites
    // test in output_capture/cursor.rs. The caret stays consistent with
    // the rendered capture.
    let mut t = capturing_tracker("你a");
    t.on_set_cursor_column(1);
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(1));
}

/// T7: empty capture anchors at col 0; a torn (non-char-boundary) cursor
/// degrades to None instead of panicking; a past-the-end cursor clamps.
#[test]
fn capture_anchor_empty_capture_and_bad_boundaries() {
    // Empty capture: cursor 0 == len 0, no trailing newline → Some(0).
    assert_eq!(
        capturing_tracker("").live_cursor_capture_anchor_col(),
        Some(0)
    );
    // Cursor parked mid-char (defensive: mirrored ops always leave
    // boundaries, but the anchor must not panic on a torn cursor).
    let mut t = capturing_tracker("hi你");
    t.output.set_cursor_byte_for_test(4); // inside '你' (3-byte char at byte 2)
    assert_eq!(t.live_cursor_capture_anchor_col(), None);
    // Cursor past len clamps to len (defensive min).
    let mut t = capturing_tracker("hi你");
    t.output.set_cursor_byte_for_test(100);
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(4));
}

/// T11 (B2, intentional change): a single-line `\r` spinner parks the
/// mirrored cursor at the line start — the caret follows the write head
/// instead of staying glued to the frame's right edge.
#[test]
fn capture_anchor_cr_spin_parks_at_col_zero() {
    let mut t = capturing_tracker("Progress  42%");
    assert_eq!(t.live_cursor_capture_anchor_col(), Some(13));
    t.on_carriage_return();
    assert_eq!(
        t.live_cursor_capture_anchor_col(),
        Some(0),
        "\r parks the mirrored cursor at the line start (B2)"
    );
}
