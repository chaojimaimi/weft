use super::Terminal;
use crate::vt::diagnostic_fixtures::{
    activate_primary_screen_tui, line_a, line_b, line_number_of, stream_lines,
};

/// v1.10.26 (FIX_IME_PREEDIT): the BlockView caret anchor must be derived
/// from the composed in-flight text itself (same-frame, same-source), not
/// from independently-recomputed head counts. With a scroll-out prefix
/// captured before the publish, `primary_screen_cursor_snapshot_line` must
/// resolve to exactly the grid cursor row inside the composed document.
#[test]
fn caret_anchor_tracks_composed_live_rows_with_prefix() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // ~600 KB (< 1MiB — no split). Streaming more lines than the viewport
    // pushes owned rows into the scroll-out prefix, so the composed document
    // has a real (non-empty) head the caret must be offset past.
    stream_lines(&mut terminal, 1, 3_000, line_b);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the composed document ({} lines)",
        lines.len()
    );
    // The composed line at the caret must carry the grid cursor row's line —
    // the exact "caret_line == composed cursor line index" invariant.
    let grid_row = terminal.grid().row_text(12).trim_end().to_string();
    let grid_line = line_number_of(&grid_row).expect("grid cursor row has a streamed line");
    let composed_line =
        line_number_of(lines[caret_line]).expect("composed caret line has a streamed line");
    assert_eq!(
        composed_line, grid_line,
        "caret must resolve to the grid cursor row inside the composed document"
    );
}

/// v1.10.26 (FIX_IME_PREEDIT): after a 1MiB split the caret must anchor into
/// the CONTINUATION tail (the in-flight block the BlockView paints), not the
/// pre-split offset into the full text (which would point past the tail and
/// never match `tui_caret_row_matches`).
#[test]
fn caret_anchor_survives_1mb_split_in_flight_tail() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // > 1MiB total; the prefix ALONE exceeds the budget, so the first refresh
    // must settle finished blocks and continue the tail in-flight.
    stream_lines(&mut terminal, 1, 5_300, line_b);
    assert!(terminal.refresh_primary_history_snapshot_now());
    assert!(
        !terminal.block_tracker().blocks().is_empty(),
        "test setup: the 1MiB split must settle a head block"
    );
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight tail");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the split tail ({} lines)",
        lines.len()
    );
    let grid_row = terminal.grid().row_text(12).trim_end().to_string();
    let grid_line = line_number_of(&grid_row).expect("grid cursor row has a streamed line");
    let composed_line =
        line_number_of(lines[caret_line]).expect("composed caret line has a streamed line");
    assert_eq!(
        composed_line, grid_line,
        "after the split the caret must resolve to the grid cursor row inside the continuation tail"
    );
}

/// v1.10.26 review B1 (FIX_IME_PREEDIT): the Ctrl-C interrupt window used
/// to leave `primary_screen_cursor_segment_len` describing the PREVIOUS
/// string while the merged transcript replaced the in-flight output — a
/// mid-window keypress then sliced at a stale offset and panicked on
/// non-char-boundary under CJK content. The interrupt publish must pair
/// the output with its segment length, and the head slice must back off
/// to a char boundary as defense in depth.
#[test]
fn caret_survives_interrupt_window_with_cjk_content() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // CJK makes every offset-sensitive bug a char-boundary panic instead
    // of a silent wrong line.
    let cjk = |n: usize| format!("line {n} 快快快快快 快快快快快");
    stream_lines(&mut terminal, 1, 100, cjk);
    terminal.process(b"\x1b[13;1H".as_slice());
    assert!(terminal.refresh_primary_history_snapshot_now());

    terminal.begin_primary_screen_interrupt_capture();
    // Tail output lands inside the window. The newlines matter: a stale
    // segment pairing counts them into the head and shifts the caret.
    terminal.process("快\n快\nx\n".as_bytes());
    // Publish the merged transcript while the window is still open, then
    // move the cursor and refresh the caret from a keystroke path — this
    // exact sequence panicked at freeze.rs before the fix.
    assert!(terminal.refresh_primary_history_snapshot_now());
    terminal.process(b"\x1b[5;1H".as_slice());
    terminal.snapshot_primary_screen_output_for_caret();

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("caret line inside the interrupt window");
    let live = terminal.block_tracker().in_flight().expect("in-flight");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the merged transcript ({} lines)",
        lines.len()
    );
    // 100 lines each followed by a trailing newline scroll the screen once
    // past it: rows 1..24 hold lines 78..100 + the cursor row, so
    // cursor row 5 (1-based) is line 82. A stale segment pairing shifts the
    // head count and lands the caret on the wrong content row even when it
    // stays in bounds.
    assert!(
        lines[caret_line].starts_with("line 82 "),
        "caret must anchor to the grid cursor row's content, got {:?} (caret_line {caret_line})",
        lines[caret_line]
    );
    terminal.cancel_primary_screen_interrupt_capture();
}

/// v1.10.26 (FIX_IME_PREEDIT): the disappearing-caret regression. The
/// scroll-out prefix grows between 50ms-rate-limited publishes but the
/// in-flight block text is STALE until the next publish. The keystroke caret
/// refresh (`snapshot_primary_screen_output_for_caret`) previously re-added
/// the grown prefix count against the stale text — the caret row jumped past
/// the last painted row, `tui_caret_row_matches` never matched, and the
/// caret + IME preedit silently vanished. The anchor must come from the
/// published text structure itself and stay in-bounds of what BlockView
/// paints.
#[test]
fn caret_keeps_matching_stale_live_rows_when_prefix_grows_between_publishes() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    // First publish: 100 lines (77 pushed out into the prefix) — the composed
    // in-flight text is exactly 100 lines.
    stream_lines(&mut terminal, 1, 100, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());
    let stale_line_count = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight")
        .output
        .lines()
        .count();

    // Stream MORE output but DO NOT publish (the app's 50ms snapshot window
    // is still open): the prefix grows in the tracker while the in-flight
    // text stays at the last published frame.
    stream_lines(&mut terminal, 101, 200, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    terminal.snapshot_primary_screen_output_for_caret();

    let caret_line = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("keystroke caret snapshot line");
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("in-flight block (stale, not yet re-published)");
    let lines: Vec<&str> = live.output.split('\n').collect();
    assert_eq!(
        lines.len(),
        stale_line_count,
        "a keystroke that does not publish must not mutate the in-flight text"
    );
    assert!(
        caret_line < lines.len(),
        "caret_line {caret_line} out of the stale composed text ({} lines): the caret must anchor to what BlockView paints (the stale frame), not the grown prefix count",
        lines.len()
    );
    assert!(
        lines[caret_line].starts_with("line "),
        "caret must land on a painted content row, got {:?} (caret_line {caret_line})",
        lines[caret_line]
    );
}

/// v1.10.26 post-release forensics (omp preedit still invisible): the
/// caret snapshot line is only ever SET, never cleared. When a screen-owned
/// session SETTLES and the next command streams in block view WITHOUT screen
/// ownership, `block_view_tui_cursor` (renderer.rs) prefers the stale
/// tracked line over the formula — the anchor then points past the new live
/// block and `tui_caret_row_matches` never fires, so the caret + IME preedit
/// vanish together even though the Batch C anchor math is correct.
#[test]
fn caret_snapshot_line_must_not_leak_across_settled_command_boundary() {
    let mut terminal = Terminal::new(24, 200);
    activate_primary_screen_tui(&mut terminal);
    stream_lines(&mut terminal, 1, 100, line_a);
    terminal.process("\x1b[13;1H".as_bytes());
    assert!(terminal.refresh_primary_history_snapshot_now());
    let stale = terminal
        .primary_screen_cursor_snapshot_line()
        .expect("screen-owned snapshot tracked a caret line");
    assert!(
        stale > 0,
        "setup: the settled session must leave a non-trivial line"
    );

    // The session settles (log: "settled primary-screen command finalization").
    terminal.process(b"\x1b]133;D;0\x07");
    assert!(terminal.settle_primary_screen_exit());
    assert!(
        terminal.block_tracker().in_flight().is_none(),
        "setup: settle must finalize the in-flight block"
    );

    // A NEW command streams WITHOUT screen ownership (plain output; no CUP
    // ops) — the omp input-box window in block view.
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07next\x1b]133;C\x07");
    terminal.process(b"hello\r\nworld\r\n");
    assert!(terminal.block_tracker().in_flight().is_some());
    assert!(!terminal.primary_screen_app_active());
    assert!(terminal.show_block_view());

    // Every keystroke refreshes the caret anchor (the app calls this in
    // tab.rs for block view). For a NON-screen-owned command it must not
    // keep the settled session's line — the renderer prefers the tracked
    // value over the formula, so a stale anchor never matches the new live
    // rows and the preedit is never painted.
    terminal.snapshot_primary_screen_output_for_caret();

    let line = terminal.primary_screen_cursor_snapshot_line();
    let live = terminal.block_tracker().in_flight().expect("live block");
    let line_count = live.output.lines().count();
    assert!(
        line.is_none_or(|l| l < line_count),
        "caret snapshot line {line:?} leaked from the settled session into the new block \
         ({} lines): tui_caret_row_matches can never fire, caret + preedit invisible",
        line_count
    );
}
