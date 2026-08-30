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

// ── v1.11.12 (PLAN_v11112 M-A): line-ledger invariants ──────────────────
// Every mutation point of the two head ledgers — `ScreenHistory::line_count`
// (preserved-frame history) and `OutputCapture`'s prefix ledger — must leave
// the counter exactly equal to the O(n) recompute it replaced:
// `text.matches('\n').count() + usize::from(!text.is_empty())`. Exhaustive
// mutation list (PLAN_v11112 F3): history append / trim (three branches) /
// `ScreenHistory::default` reset; prefix append / drain (three branches) /
// clear / rebase seed. A drift turns `screen_history_lines`,
// `screen_prefix_line_count` and `screen_head_lines` into wrong caret and
// drag-selection offsets.

use crate::blocks::StyledOutput;

/// The O(n) ground truth both ledgers must always equal.
fn ledger_recount(text: &str) -> usize {
    text.matches('\n').count() + usize::from(!text.is_empty())
}

/// History append maintains the ledger across all shape branches: empty
/// history + single line (空串边界), empty history + multi-line, non-empty
/// history (separator branch), and the empty-text early return (no change).
#[test]
fn history_append_ledger_matches_recompute() {
    let mut t = Terminal::new(6, 40);

    // Empty history + single-line frame: the frame's own final line is +1.
    t.append_screen_history_frame("frame one", StyledOutput::default());
    let text = t.capabilities.screen_history.text.clone();
    assert_eq!(t.screen_history_lines(), ledger_recount(&text));
    assert_eq!(t.screen_history_lines(), 1);

    // Non-empty history + multi-line frame: separator + embedded newlines.
    t.append_screen_history_frame("a\nb\nc", StyledOutput::default());
    let text = t.capabilities.screen_history.text.clone();
    assert_eq!(t.screen_history_lines(), ledger_recount(&text));
    assert_eq!(t.screen_history_lines(), 4);

    // Empty-text early return: neither text nor ledger move.
    let before = t.screen_history_lines();
    t.append_screen_history_frame("", StyledOutput::default());
    assert_eq!(t.screen_history_lines(), before);
    assert_eq!(t.capabilities.screen_history.text, text);
}

/// The 1MiB-cap early return drops the frame without touching the ledger.
#[test]
fn history_append_at_cap_leaves_ledger_untouched() {
    let mut t = Terminal::new(6, 40);
    let filler = "x".repeat(crate::blocks::MAX_OUTPUT_BYTES);
    t.append_screen_history_frame(&filler, StyledOutput::default());
    let before = t.screen_history_lines();
    let bytes_before = t.capabilities.screen_history.text.len();
    t.append_screen_history_frame("dropped", StyledOutput::default());
    assert_eq!(t.screen_history_lines(), before);
    assert_eq!(t.capabilities.screen_history.text.len(), bytes_before);
}

/// Trim decrements by the exact `consumed_lines` expression for all three
/// consumption branches: zero, partial line boundary, whole history.
#[test]
fn history_trim_ledger_matches_recompute() {
    let mut t = Terminal::new(6, 40);
    t.append_screen_history_frame("l1\nl2\nl3\nl4", StyledOutput::default());
    assert_eq!(t.screen_history_lines(), 4);
    let full = t.capabilities.screen_history.text.clone();

    // Zero-consumption early return.
    t.trim_screen_history(0);
    assert_eq!(t.screen_history_lines(), ledger_recount(&full));

    // Partial: cut at the first line boundary ("l1\n" = 3 bytes) → 1 line.
    t.trim_screen_history(3);
    let rest = t.capabilities.screen_history.text.clone();
    assert_eq!(rest, "l2\nl3\nl4");
    assert_eq!(t.screen_history_lines(), ledger_recount(&rest));
    assert_eq!(t.screen_history_lines(), 3);

    // Whole-history branch: consumed == len counts the final line too → 0.
    let len = t.capabilities.screen_history.text.len();
    t.trim_screen_history(len);
    assert!(t.capabilities.screen_history.text.is_empty());
    assert_eq!(t.screen_history_lines(), 0);
    assert_eq!(t.screen_history_lines(), ledger_recount(""));
}

/// Overlong-line mid-cut trim: the boundary is not at a '\n', yet the
/// ledger must still equal the recompute of the remaining text.
#[test]
fn history_trim_mid_cut_keeps_ledger_aligned() {
    let mut t = Terminal::new(6, 40);
    t.append_screen_history_frame("aaaa\nbbbb", StyledOutput::default());
    assert_eq!(t.screen_history_lines(), 2);
    // Cut 6 bytes = "aaaa\nb" (mid-cut inside "bbbb") → removes line 1 only.
    t.trim_screen_history(6);
    let rest = t.capabilities.screen_history.text.clone();
    assert_eq!(rest, "bbb");
    assert_eq!(t.screen_history_lines(), ledger_recount(&rest));
    assert_eq!(t.screen_history_lines(), 1);
}

/// Reset mechanism 1: `ScreenHistory::default()` (capture restart) zeroes
/// the ledger together with the text.
#[test]
fn history_default_reset_zeroes_ledger() {
    let mut t = Terminal::new(6, 40);
    t.append_screen_history_frame("a\nb\nc", StyledOutput::default());
    assert_eq!(t.screen_history_lines(), 3);
    // The exact expression `begin_primary_screen_output_capture` runs.
    t.capabilities.screen_history = crate::vt::capability::ScreenHistory::default();
    assert!(t.capabilities.screen_history.text.is_empty());
    assert_eq!(t.capabilities.screen_history.line_count, 0);
    assert_eq!(t.screen_history_lines(), 0);
}

/// The head composition: `screen_head_lines` is the sum of both O(1) ledger
/// reads and matches a recompute over the two texts.
#[test]
fn head_lines_equal_history_plus_prefix_ledgers() {
    let mut t = Terminal::new(6, 40);
    t.append_screen_history_frame("h1\nh2", StyledOutput::default());
    // The tracker gates prefix appends on screen ownership, which requires
    // the CommandExecuting phase (same setup as the screen_capture tests).
    t.block_tracker_mut().on_command_start("tui".to_string());
    t.block_tracker_mut().begin_screen_owned_output(0);
    t.block_tracker_mut()
        .append_screen_prefix("p1\np2\np3", None);
    let head = t.screen_head_lines();
    assert_eq!(head, t.screen_history_lines() + t.screen_prefix_lines());
    assert_eq!(head, 2 + 3);
}

/// Prefix append mirrors the history ledger: empty prefix + single line,
/// multi-line segments, the empty-segment early return, and that `replace`
/// (the screen-snapshot path) never touches the prefix or its ledger.
#[test]
fn prefix_append_ledger_matches_recompute() {
    use crate::blocks::OutputCapture;
    let mut capture = OutputCapture::default();

    capture.append_screen_prefix("row one", None);
    assert_eq!(
        capture.screen_prefix_line_count(),
        ledger_recount(capture.screen_prefix_text())
    );
    assert_eq!(capture.screen_prefix_line_count(), 1);

    capture.append_screen_prefix("a\nb\nc", None);
    assert_eq!(
        capture.screen_prefix_line_count(),
        ledger_recount(capture.screen_prefix_text())
    );
    assert_eq!(capture.screen_prefix_line_count(), 4);

    // Empty segment: early return, ledger unchanged.
    capture.append_screen_prefix("", None);
    assert_eq!(capture.screen_prefix_line_count(), 4);

    // `replace` swaps the live segment only — prefix (and ledger) survive.
    capture.replace("viewport", crate::blocks::MAX_OUTPUT_BYTES);
    assert_eq!(capture.screen_prefix_line_count(), 4);
}

/// Prefix drain branches: zero early return, partial drain, and the
/// consume-everything clear.
#[test]
fn prefix_drain_ledger_matches_recompute() {
    use crate::blocks::OutputCapture;
    let mut capture = OutputCapture::default();
    capture.append_screen_prefix("l1\nl2\nl3\nl4", None);
    assert_eq!(capture.screen_prefix_line_count(), 4);

    // Zero drain: early return.
    capture.drain_screen_prefix(0);
    assert_eq!(capture.screen_prefix_line_count(), 4);

    // Partial: "l1\n" = 3 bytes → 1 line gone.
    capture.drain_screen_prefix(3);
    assert_eq!(capture.screen_prefix_text(), "l2\nl3\nl4");
    assert_eq!(
        capture.screen_prefix_line_count(),
        ledger_recount(capture.screen_prefix_text())
    );
    assert_eq!(capture.screen_prefix_line_count(), 3);

    // Everything (consumed >= len) → full clear, ledger 0.
    capture.drain_screen_prefix(usize::MAX);
    assert!(capture.screen_prefix_text().is_empty());
    assert_eq!(capture.screen_prefix_line_count(), 0);
}

/// Reset mechanism 2: `OutputCapture::clear` zeroes the prefix ledger
/// alongside the text (the second of the two distinct reset mechanisms).
#[test]
fn prefix_clear_zeroes_ledger() {
    use crate::blocks::OutputCapture;
    let mut capture = OutputCapture::default();
    capture.append_screen_prefix("a\nb\nc", None);
    assert_eq!(capture.screen_prefix_line_count(), 3);
    capture.clear();
    assert!(capture.screen_prefix_text().is_empty());
    assert_eq!(capture.screen_prefix_line_count(), 0);
}

/// The capture-start rebase seeds the prefix through the same append entry,
/// so its ledger is maintained too (Terminal-level, real VT stream).
#[test]
fn rebase_seed_keeps_prefix_ledger_aligned() {
    let mut terminal = Terminal::new(5, 40);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    // Pre-capture (below the 2-cursor-op threshold): LF overflow pushes
    // OWNED rows into the scrollback — exactly the retained rows the
    // capture-start rebase folds into the prefix.
    for i in 0..8 {
        terminal.process(format!("pre-{i}\r\n").as_bytes());
    }
    // Second cursor op → capture starts → rebase seeds the prefix.
    terminal.process("\x1b[2;1H".as_bytes());
    terminal.process("\x1b[3;1H".as_bytes());
    assert!(terminal.primary_screen_app_active());

    let prefix = terminal.block_tracker().screen_prefix_text().to_string();
    assert!(
        prefix.contains("pre-"),
        "rebase must seed the prefix, got {prefix:?}"
    );
    assert_eq!(
        terminal.block_tracker().screen_prefix_line_count(),
        ledger_recount(&prefix)
    );
}

/// Mixed sequence over both ledgers (append → append → trim → drain →
/// append) with a recompute assertion after every step — the drift catcher.
#[test]
fn mixed_mutation_sequence_keeps_both_ledgers_aligned() {
    let mut t = Terminal::new(6, 40);

    t.append_screen_history_frame("f1\nf2", StyledOutput::default());
    t.append_screen_history_frame("f3", StyledOutput::default());
    let history = t.capabilities.screen_history.text.clone();
    assert_eq!(t.screen_history_lines(), ledger_recount(&history));

    t.block_tracker_mut().append_screen_prefix("p1", None);
    t.block_tracker_mut().append_screen_prefix("p2\np3", None);
    let prefix = t.block_tracker().screen_prefix_text().to_string();
    assert_eq!(
        t.block_tracker().screen_prefix_line_count(),
        ledger_recount(&prefix)
    );
    assert_eq!(
        t.screen_head_lines(),
        ledger_recount(&history) + ledger_recount(&prefix)
    );

    // Trim half the history at a line boundary. The prefix drain has no
    // tracker-level wrapper (it runs inside `split_screen_history`), so its
    // half of the mixed sequence runs on the capture directly.
    let cut = history.find('\n').unwrap() + 1;
    t.trim_screen_history(cut);
    let history = t.capabilities.screen_history.text.clone();
    assert_eq!(t.screen_history_lines(), ledger_recount(&history));

    use crate::blocks::OutputCapture;
    let mut capture = OutputCapture::default();
    capture.append_screen_prefix("q1", None);
    capture.append_screen_prefix("q2\nq3", None);
    let drained = capture.screen_prefix_text().find('\n').unwrap() + 1;
    capture.drain_screen_prefix(drained);
    assert_eq!(capture.screen_prefix_text(), "q2\nq3");
    assert_eq!(capture.screen_prefix_line_count(), 2);

    // Grow both again.
    t.append_screen_history_frame("f9", StyledOutput::default());
    t.block_tracker_mut().append_screen_prefix("p9", None);
    let history = t.capabilities.screen_history.text.clone();
    let prefix = t.block_tracker().screen_prefix_text().to_string();
    assert_eq!(
        t.screen_head_lines(),
        ledger_recount(&history) + ledger_recount(&prefix)
    );
}

// ── v1.11.12 (PLAN_v11112 M-A): refresh-cost scaling benchmarks ─────────
// #[ignore] benchmarks feeding the .13 representation-layer decision (frozen
// head/tail split). Data goes into the version's PROGRESS/baseline notes:
// per-refresh cost as a function of the composed HEAD size (the O(head)
// tails: line-ledger reads ×2, styled shift, text re-copy) and the pure
// iteration cost of an all-false owned-mask scrollback span (walk starts at
// document_start, upper-bounded by the ring capacity). Run with:
//   cargo test -p weft_core --lib perf_ -- --ignored --nocapture

/// Screen-owned session with a TUI-engaged viewport (real VT stream).
fn screen_owned_terminal(rows: usize, cols: usize, ring: usize) -> Terminal {
    let mut t = Terminal::with_scrollback(rows, cols, ring);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
    // Two absolute-addressing ops cross the TUI capture threshold.
    t.process("\x1b[2;1H".as_bytes());
    t.process("\x1b[3;1H".as_bytes());
    t.process(b"prompt> ");
    t
}

/// Per-refresh cost vs composed head size (10k / 50k / 100k lines). The head
/// is seeded through the REAL append path so the ledger stays consistent;
/// 7-byte lines keep 100k lines under the 1MiB split budget.
#[test]
#[ignore]
fn perf_snapshot_refresh_cost_scaling() {
    for head_lines in [10_000usize, 50_000, 100_000] {
        let mut t = screen_owned_terminal(30, 100, 2_000);
        let frame: Vec<String> = (0..1_000).map(|i| format!("{i:06}")).collect();
        let frames = head_lines / 1_000;
        for _ in 0..frames {
            let text = frame.join("\n");
            t.append_screen_history_frame(&text, StyledOutput::default());
        }
        assert_eq!(t.screen_history_lines(), head_lines);

        // Warm caches, then average over N refreshes.
        for _ in 0..3 {
            t.refresh_primary_history_snapshot_now();
        }
        let iterations = 20u32;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            t.refresh_primary_history_snapshot_now();
        }
        let per_refresh_ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(iterations);
        println!(
            "V110_METRIC name=snapshot_refresh_scaling head_lines={head_lines} per_refresh_ms={per_refresh_ms:.3}"
        );
    }
}

/// P1-1: owned-path walk cost when the scrollback mask is ALL FALSE — the
/// walk iterates from document_start (ring start here) up to the ring
/// capacity while every scrollback row is skipped. Measured as the delta
/// between a full-span walk (9k all-false rows) and the same terminal
/// geometry with an empty ring (viewport-only walk).
#[test]
#[ignore]
fn perf_owned_walk_allfalse_mask_iteration_cost() {
    // Full span: ring starts empty at capture start, so document_start == 0
    // and 9k scrolled-out (mask-false) rows sit between it and the viewport.
    let mut full = screen_owned_terminal(10, 100, 10_000);
    for _ in 0..9_000 {
        full.process(b"\n");
    }
    let document_start = full
        .block_tracker()
        .screen_document_start()
        .expect("screen-owned");
    assert_eq!(document_start, 0, "ring was empty at capture start");

    // Control: identical geometry and viewport, nothing ever scrolled.
    let control = screen_owned_terminal(10, 100, 10_000);

    let walk = |t: &Terminal| {
        let start = std::time::Instant::now();
        let (_text, _, _) = t.primary_screen_document_snapshot(document_start.min(1));
        start.elapsed().as_secs_f64() * 1000.0
    };
    // Walk each twice, report the second (warmed) run.
    let _ = walk(&full);
    let full_ms = walk(&full);
    let _ = walk(&control);
    let control_ms = walk(&control);
    println!(
        "V110_METRIC name=snapshot_walk_allfalse_mask scrollback_rows=9000 full_ms={full_ms:.3} viewport_only_ms={control_ms:.3} iteration_delta_ms={:.3}",
        full_ms - control_ms
    );
}
