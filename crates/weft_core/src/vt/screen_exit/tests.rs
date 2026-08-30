use super::*;
use crate::vt::ScreenOwner;

#[test]
fn document_start_maps_to_a_clamped_viewport_row() {
    assert_eq!(viewport_row_for_document_start(12, 10, 8), 2);
    assert_eq!(viewport_row_for_document_start(8, 10, 8), 0);
    assert_eq!(viewport_row_for_document_start(30, 10, 8), 8);
}

#[test]
fn tui_scroll_discards_blit_and_dirties_all_rows() {
    // v1.10.4: a primary-screen TUI (openclaw — relative cursor moves +
    // EL/ED redraws on the main screen) must NOT use the GPU scroll-blit
    // fast path: the app overwrites scrolled rows, so blitting stale
    // content under the redraw produced "content squeezed together".
    // `scroll_grid_up` on a TUI-owned viewport must clear pending_scroll
    // (renderer then rebuilds all rows instead of blitting).
    let mut t = Terminal::new(5, 20);
    // Shell integration → CommandExecuting, then TUI cursor addressing.
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
    // Fill the screen (5 rows → cursor lands on the bottom row).
    for i in 0..5 {
        t.process(format!("row{i}\r\n").as_bytes());
    }
    // Accumulate TUI cursor-addressing evidence (2+ ops) WITHOUT entering
    // alt-screen (the openclaw pattern: relative moves).
    t.process("\x1b[2A\x1b[3B".as_bytes());
    assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);

    // Scroll the TUI viewport up.
    t.process("\x1b[1S".as_bytes());
    // The renderer must NOT see a pending scroll blit delta.
    assert_eq!(
        t.grid().take_pending_scroll(),
        0,
        "TUI scroll must discard the blit delta (disable GPU scroll blit)"
    );
    // All rows dirty → renderer rebuilds the whole viewport.
    assert!(
        t.grid().dirty_rows().count() >= 5,
        "all rows must be dirty after TUI scroll"
    );

    // Sanity: the top row changed after scrolling (content moved up).
    let g = t.grid();
    let mut row0 = String::new();
    for c in 0..g.num_cols {
        row0.push(g.cell(0, c).character);
    }
    assert_ne!(row0.trim_end(), "row0", "top row must change after scroll");
}

#[test]
fn dec2026_synchronized_output_triggers_tui_detection() {
    // v1.10.31 (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): DEC 2026 alone no longer
    // counts. pi (coding-agent CLI) uses DEC 2026 synchronized output
    // (?2026h) on every repaint PLUS real cursor addressing (CUP/CUU). The
    // synchronization window only counts when non-trivial addressing is seen.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
    // Startup: one CUU + CHR 1 (<2 ops, not detected yet).
    t.process("\x1b[3A\x1b[1G".as_bytes());
    assert!(!t.primary_screen_app_active(), "<2 ops: not yet a TUI");
    assert!(t.show_block_view());

    // First keystroke: synchronized output begins, but bare ?2026h doesn't count.
    t.process("\x1b[?2026h".as_bytes());
    assert!(
        !t.primary_screen_app_active(),
        "bare DEC 2026 must NOT count as TUI evidence (v1.10.31 fix)"
    );
    assert!(
        t.show_block_view(),
        "still in block view before real addressing"
    );

    // Now add real cursor addressing (CUP) inside the window → TUI detected.
    t.process("\x1b[5;3Hxx".as_bytes());
    assert!(
        t.primary_screen_app_active(),
        "DEC 2026 + CUP must count as TUI evidence"
    );
    assert!(
        !t.show_block_view(),
        "a detected TUI owns the screen — the live grid renders it"
    );
}

#[test]
fn bare_2026_without_addressing_does_not_classify() {
    // v1.10.31 (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): verify that pure DEC 2026
    // frames without real cursor addressing never trigger TUI detection.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07test\x1b]133;C\x07");

    // Emit several DEC 2026 frames with only print (no cursor addressing)
    for _ in 0..5 {
        t.process(b"\x1b[?2026h");
        t.process(b"content");
        t.process(b"\x1b[?2026l");
    }

    assert!(
        !t.primary_screen_app_active(),
        "bare DEC 2026 frames must NOT trigger TUI detection"
    );
    assert!(t.show_block_view(), "should stay in block view");
}

#[test]
fn chr_input_line_redraw_uses_live_grid() {
    // v1.10.6: pi (coding-agent CLI) uses CHR (horizontal-only) for its
    // input-line redraw + DEC 2026 sync output. CHR counts toward TUI
    // detection. v1.10.12: a screen-owned TUI renders in the LIVE GRID
    // (full-screen, low-latency) — the BlockView only appears when the
    // user scrolls into history browsing.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
    assert!(!t.primary_screen_app_active(), "<2 ops: not yet a TUI");
    assert!(t.show_block_view());
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active(), "TUI detected (>= 2 ops)");
    assert!(
        !t.show_block_view(),
        "screen-owned TUI renders in the live grid"
    );
}

#[test]
fn relative_addressing_tui_uses_live_grid() {
    // openclaw/pi pattern — row-capable relative moves only (CUU; the
    // v1.10.38 rule drops horizontal hops C/D from the count). Two row
    // ops cross the >= 2 threshold. v1.10.12: a screen-owned TUI (any
    // addressing style) renders in the live grid; the BlockView appears
    // only while history browsing.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
    t.process("\x1b[999D\x1b[915A\x1b[1A".as_bytes());
    assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);
    assert!(
        !t.show_block_view(),
        "screen-owned TUI renders in the live grid"
    );
    // History browsing switches to the BlockView document snapshot.
    t.set_primary_history_view(true);
    assert!(t.show_block_view(), "history browsing uses the BlockView");
    t.set_primary_history_view(false);
    assert!(!t.show_block_view());
}

#[test]
fn absolute_addressing_tui_switches_to_live_grid() {
    // The Claude Code pattern — CUP addresses. Same live-grid path.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
    t.process("\x1b[H\x1b[2;1H".as_bytes());
    assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);
    assert!(
        !t.show_block_view(),
        "absolute-addressing TUI needs the live grid"
    );
}

#[test]
fn marker_boundary_rearms_per_command_addressing_evidence() {
    // v1.11.8 (PLAN_v1118 M-C1): renamed from
    // `osc133_reset_clears_absolute_addressing_flag` — the zero-read
    // `primary_screen_absolute_addressing` flag was deleted (F11). The
    // byte-sequence regression this test pins is unchanged: per-command
    // addressing evidence is re-armed at the 133;D→133;A boundary, so a
    // fresh relative-only command re-detects screen ownership from zero
    // and keeps the same (classic-tier) live-grid view as the CUP-only
    // command before it.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
    t.process("\x1b[H\x1b[2;1H".as_bytes());
    assert!(!t.show_block_view());
    // Command ends, next prompt, then a new command.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    t.settle_primary_screen_exit();
    t.process(b"sh\x1b]133;B\x07\x1b]133;C\x07\x1b[2A\x1b[3B");
    assert!(t.primary_screen_app_active());
    // v1.10.12: the relative-only move sequence re-arms ownership from
    // zero evidence; the screen-owned TUI still renders in the live grid.
    assert!(
        !t.show_block_view(),
        "relative-only command re-arms per-command addressing evidence → still uses the live grid"
    );
}

#[test]
fn screen_owned_snapshot_refreshes_without_history_view() {
    // v1.10.6: snapshot refresh is driven by screen ownership for
    // history browsing. A primary-screen TUI uses the live grid
    // (show_block_view == false), but the snapshot must still be
    // publishable for the moment the user scrolls into history
    // browsing (show_block_view → true via primary_history_view).
    let mut t = Terminal::new(5, 48);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
    t.process("\x1b[999D\x1b[915A\x1b[1A".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "cursor addressing must begin screen ownership"
    );

    t.process("choice A".as_bytes());
    assert!(
        !t.primary_history_view(),
        "precondition: following the live tail, not browsing history"
    );
    assert!(
        t.refresh_primary_history_snapshot_now(),
        "screen-owned snapshot refresh must work without history browsing"
    );
    assert!(
        t.block_tracker()
            .in_flight()
            .is_some_and(|live| live.output.contains("choice A")),
        "live block must publish the repainted content"
    );
}

#[test]
fn snapshot_refresh_requires_screen_ownership() {
    // Plain command output (no cursor addressing) is NOT screen-owned:
    // print capture still feeds the live block, so the snapshot refresh
    // must stay a no-op.
    let mut t = Terminal::new(5, 48);
    t.process(b"\x1b]133;A\x07echo hi\x1b]133;B\x07\x1b]133;C\x07");
    t.process("plain output".as_bytes());
    assert!(!t.primary_screen_app_active());
    assert!(t.block_tracker().screen_document_start().is_none());
    assert!(
        !t.refresh_primary_history_snapshot_now(),
        "non-screen-owned output must not refresh a screen snapshot"
    );
}

#[test]
fn non_integrated_addressing_engages_blit_discard_but_not_screen_ownership() {
    // v1.10.4 (reviewer MEDIUM-2): in a genuinely non-integrated session
    // (no OSC 133 ever — phase stays NotIntegrated) relative addressing
    // must still engage the TUI-safe scroll path (`tui_owned_scroll`),
    // while screen ownership / BlockView stay OFF because
    // `primary_screen_app_active()` requires CommandExecuting. The
    // count never resets without 133 markers — that leak is accepted and
    // documented on `note_primary_screen_cursor_addressing`.
    let mut t = Terminal::new(5, 20);
    assert_eq!(t.screen_owner(), ScreenOwner::Shell);
    t.process("\x1b[2A\x1b[3B".as_bytes());
    assert!(
        t.tui_owned_scroll(),
        "relative addressing must engage the TUI-safe scroll path"
    );
    assert_eq!(
        t.screen_owner(),
        ScreenOwner::Shell,
        "non-integrated phase must NOT grant screen ownership"
    );
    assert!(
        t.block_tracker().screen_document_start().is_none(),
        "no screen-owned snapshot state for a non-integrated session"
    );
    // The count persists (no 133 to reset it) — scroll path stays engaged.
    t.process("\x1b[4B".as_bytes());
    assert!(t.tui_owned_scroll());
}

#[test]
fn nested_run_starting_with_a_stays_in_live_grid() {
    // v1.10.12: the FIRST marker of a nested run can be `133;A` with NO
    // pending exit (omp/pi's inner zsh prompt before any internal command
    // completed). Screen ownership keeps the live grid stable across
    // nested markers and CUP repaints — no render-mode lock required.
    let mut t = Terminal::new(8, 40);
    t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

    // Nested run #1: A is the FIRST marker (no pending exit yet).
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(
        !t.show_block_view(),
        "nested markers must not flip the render mode"
    );
    assert!(t.block_tracker().phase() == ShellPhase::CommandExecuting);
    t.process("\x1b[2J\x1b[Hworking...".as_bytes());
    assert!(
        !t.show_block_view(),
        "CUP repaint inside a screen-owned TUI must keep the live grid"
    );

    // Nested run #2: D-then-A pair.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(!t.show_block_view());

    // History browsing switches to the BlockView snapshot.
    t.set_primary_history_view(true);
    assert!(t.show_block_view(), "history browsing uses the BlockView");

    // Real settle (idle timer path) ends the screen-owned session.
    t.process(b"\x1b]133;D;0\x07");
    assert!(t.settle_primary_screen_exit());
}

// v1.10.12 regression: while a nested 133;D defers the exit (pending
// settle window), history browsing must still show the BlockView —
// otherwise scrolling during a nested command lands on the empty grid
// scrollback ("cannot scroll through the history").
#[test]
fn history_browsing_works_while_exit_is_pending() {
    let mut t = Terminal::new(8, 40);
    t.process(b"\x1b]133;A\x07omp\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

    // Nested command finishes → pending exit (settle window open).
    t.process(b"\x1b]133;D;0\x07");
    assert!(
        t.primary_screen_exit_pending(),
        "nested D defers the exit into the settle window"
    );

    // Scrolling into history must still switch to the BlockView.
    t.set_primary_history_view(true);
    assert!(
        t.show_block_view(),
        "history browsing must work even while an exit is pending"
    );

    t.set_primary_history_view(false);
    assert!(
        !t.show_block_view(),
        "back to the live grid when not browsing"
    );
    // The pending exit still settles normally afterwards.
    assert!(t.settle_primary_screen_exit());
}

#[test]
fn sparse_repainter_stays_in_live_grid_through_cup_repaints() {
    // v1.10.12: a sparse repainter (omp/pi) detected with relative-only
    // addressing renders in the live grid for the whole command. Its
    // occasional full-viewport CUP repaint (task start, layout change)
    // must NOT flip the renderer to the BlockView — that flip truncated
    // the UI and blocked history scrolling.
    let mut t = Terminal::new(8, 40);
    t.process(b"\x1b]133;A\x07pi\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[1G\x1b[?25l".as_bytes());
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active(), "TUI detected");
    assert!(!t.show_block_view(), "screen-owned TUI uses the live grid");

    // Task start: pi clears the viewport and repaints (CUP addressing).
    t.process("\x1b[2J\x1b[Hworking...\x1b[2;1Hprogress".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(
        !t.show_block_view(),
        "a CUP repaint inside a screen-owned session must keep the live grid"
    );
    assert_eq!(
        t.primary_screen_visible_row_start(),
        None,
        "no row hiding for sparse repainters"
    );
    assert_eq!(t.primary_screen_viewport_ownership(), None);

    // Nested marker bursts must not flip the render mode.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(
        !t.show_block_view(),
        "nested markers must not flip the live grid"
    );

    // History browsing switches to the BlockView snapshot.
    t.set_primary_history_view(true);
    assert!(t.show_block_view(), "history browsing uses the BlockView");
    t.set_primary_history_view(false);
    assert!(!t.show_block_view());

    // Real exit settles the block; the NEXT command re-detects its mode.
    t.process(b"\x1b]133;D;0\x07");
    assert!(t.settle_primary_screen_exit());
    t.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[H\x1b[2;1H".as_bytes());
    assert!(
        !t.show_block_view(),
        "a later absolute-addressing TUI re-detects to the live grid"
    );
}

#[test]
fn absolute_tui_following_tail_refreshes_snapshot_for_the_final_block() {
    // v1.10.7: the v1.10.4 MEDIUM-1 skip is removed. An absolute-
    // addressing TUI (Claude Code) follows in the live grid, so the
    // BlockView has no snapshot consumer WHILE following — but the
    // session block's final content (exit history, resumed sessions)
    // comes from the snapshot, so it must keep refreshing. A sparse
    // repainter like pi toggles CUP per repaint; freezing on that
    // transient flag made blocks lose their body.
    let mut t = Terminal::new(5, 48);
    t.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[H\x1b[2;1H".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(
        !t.show_block_view(),
        "absolute TUI follows in the live grid"
    );

    t.process("answer".as_bytes());
    assert!(
        t.refresh_primary_history_snapshot_now(),
        "following an absolute TUI must still refresh the snapshot (final block content)"
    );
    assert!(t
        .block_tracker()
        .in_flight()
        .is_some_and(|live| live.output.contains("answer")));
    // History browsing keeps refreshing too.
    t.set_primary_history_view(true);
    assert!(t.refresh_primary_history_snapshot_now());
    assert!(t
        .block_tracker()
        .in_flight()
        .is_some_and(|live| live.output.contains("answer")));
}

#[test]
fn tui_lf_overflow_discards_blit_and_dirties_all_rows() {
    // v1.10.4 (reviewer HIGH): the LF-overflow path — content streaming
    // past the bottom row — is the DOMINANT scroll route for primary-
    // screen TUIs (openclaw streams lines with \r\n). It goes through
    // `index_primary_screen` → `grid.index()`, NOT `scroll_grid_rows`.
    // The TUI-safe discard must apply there too, or the GPU blit fires
    // under the app's redraw and reproduces the squeeze corruption.
    let mut t = Terminal::new(5, 20);
    // Shell integration → CommandExecuting, then TUI cursor addressing.
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
    // Establish TUI ownership (relative cursor moves, no alt-screen).
    t.process("\x1b[2A\x1b[3B".as_bytes());
    assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);

    // Overflow the viewport with plain line feeds.
    for i in 0..8 {
        t.process(format!("overflow-{i}\r\n").as_bytes());
    }
    assert_eq!(
        t.grid().take_pending_scroll(),
        0,
        "LF overflow on a TUI viewport must discard the blit delta"
    );
    assert!(
        t.grid().dirty_rows().count() >= 5,
        "all rows must be dirty after TUI LF overflow"
    );
}

// v1.11.11 (PLAN_v11111 M-D m3): end-to-end three-segment compose —
// preserved superseded frame + scroll-out prefix + viewport snapshot all
// non-empty at once, and the composed document stays seamless. The byte
// stream chains the v1.10.23/25 driver precedents: CUP repaint inside a
// DEC 2026 window (:1448 sparse_repainter pattern, but with a CSI 2J so
// the whole prior frame is preserved), LF overflow past the bottom
// (:1536 pattern, capturing the scroll-out prefix), then an explicit
// snapshot refresh (:1500 pattern). Probe markers sit >= 1 row from both
// seams and must not be empty (the snapshot walk omits empty rows —
// snapshot.rs `primary_screen_snapshot_line_for_viewport_row` doc).
#[test]
fn composed_history_prefix_and_snapshot_join_without_seams() {
    let mut t = Terminal::new(6, 40);
    // Shell integration + TUI detection: one row move, then real CUP
    // addressing inside a bare DEC 2026 window (no print pollution).
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[1G".as_bytes());
    t.process("\x1b[?2026h\x1b[5;3H\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active(), "TUI detected (>= 2 ops)");

    // Segment 1 seed: five full rows the upcoming full erase will destroy.
    // Row 0 is previously empty, so the preserved frame contains exactly
    // `frame-0..4` (5 non-empty rows >= the 4-line preserve floor).
    t.process("\x1b[H".as_bytes());
    for i in 0..5 {
        t.process(format!("frame-{i}\r\n").as_bytes());
    }
    // Full-frame repaint inside the sync window: CSI 2J preserves the
    // superseded document into the frame history, then the TUI paints a
    // fresh row and returns to a fresh line (the \r\n keeps the first
    // streamed tail off the repaint row — `print` precedes `\r\n`, so a
    // bare `working...` would merge with tail-0 at columns 10+).
    t.process("\x1b[?2026h\x1b[2J\x1b[Hworking...\r\n\x1b[?2026l".as_bytes());

    // Segment 2 seed: LF overflow pushes the repainted row and three
    // streamed rows out of the viewport into the scroll-out prefix.
    for i in 0..8 {
        t.process(format!("tail-{i}\r\n").as_bytes());
    }
    // Segment 3: publish the composed snapshot.
    assert!(
        t.refresh_primary_history_snapshot_now(),
        "screen-owned snapshot refresh must publish the composed document"
    );
    let output = t
        .block_tracker()
        .in_flight()
        .map(|live| live.output)
        .expect("publish must leave an in-flight block");
    let composed: Vec<&str> = output.lines().collect();
    let expected = [
        "frame-0",
        "frame-1",
        "frame-2",
        "frame-3",
        "frame-4", // history
        "working...",
        "tail-0",
        "tail-1",
        "tail-2", // scroll-out prefix
        "tail-3",
        "tail-4",
        "tail-5",
        "tail-6",
        "tail-7", // snapshot
    ];
    assert_eq!(
        composed, expected,
        "three compose segments must line up in document order with no duplicate/hole rows"
    );
    // Probe rows (non-empty, >= 1 row from both seams): frame-2 in the
    // history, tail-1 in the prefix, tail-4 in the snapshot.
    assert_eq!(composed.iter().position(|l| *l == "frame-2"), Some(2));
    assert_eq!(composed.iter().position(|l| *l == "tail-1"), Some(7));
    assert_eq!(composed.iter().position(|l| *l == "tail-4"), Some(10));
    assert_eq!(composed.len(), 14);

    // Cross-seam viewport mapping: the rendered line of each owned
    // viewport row must sit in the snapshot segment, strictly increasing
    // with the row — the head offset (history 5 + prefix 4 = 9) lands
    // rows 0..5 on composed lines 9..13 with no gap; the trailing empty
    // row is skipped by the snapshot (None), which is exactly why probes
    // must stay non-empty and off the seams.
    for (row, tail) in (0..5).zip(3..8) {
        let line = t.primary_screen_snapshot_line_for_viewport_row(row);
        assert_eq!(
            line,
            Some(9 + row),
            "viewport row {row} (tail-{tail}) must map past both seams"
        );
    }
    assert_eq!(
        t.primary_screen_snapshot_line_for_viewport_row(5),
        None,
        "the empty bottom row is omitted from the snapshot"
    );
}
