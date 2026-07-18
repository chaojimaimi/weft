use super::*;
use crate::blocks::ShellPhase;

fn term() -> Terminal {
    Terminal::new(24, 80)
}

#[test]
fn synchronized_output_mode_tracks_open_tui_frame_boundaries() {
    let mut t = term();
    t.process(b"\x1b[?2026hpartial frame");
    assert!(t.synchronized_output());
    t.process(b"\x1b[?2026l");
    assert!(!t.synchronized_output());
}

#[test]
fn opentui_drawing_symbols_follow_single_cell_cursor_math() {
    let mut t = Terminal::new(4, 80);
    let line = "╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";
    t.process(format!("\x1b[?2026h\x1b[2;5H{line}\x1b[?2026l").as_bytes());

    assert_eq!(t.grid().cursor.col, 4 + line.chars().count());
    for col in 4..t.grid().cursor.col {
        let cell = t.grid().cell(1, col);
        assert_eq!(
            cell.width,
            CellWidth::Half,
            "{} at col {col}",
            cell.character
        );
        assert!(!cell.flags.contains(CellFlags::WIDE_SPACER));
    }

    t.process("\x1b[3;5H█▄┃·…中".as_bytes());
    assert_eq!(t.grid().cursor.col, 4 + 5 + 2);
    assert_eq!(t.grid().cell(2, 9).character, '中');
    assert_eq!(t.grid().cell(2, 9).width, CellWidth::Full);
}

#[test]
fn repeated_primary_screen_addressing_temporarily_owns_the_grid_view() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(t.show_block_view());

    // Claude Code enables color-scheme reporting and then paints its primary
    // screen with absolute cursor moves instead of entering DEC 1049.
    t.process(b"\x1b[?2031h\x1b[6G");
    assert!(
        t.show_block_view(),
        "one absolute move is not enough evidence"
    );
    t.process(b"\x1b[13G");
    assert!(t.primary_screen_app_active());
    assert!(!t.show_block_view());
    t.set_primary_history_view(true);
    assert!(
        t.show_block_view(),
        "history browsing uses structured blocks"
    );
    t.set_primary_history_view(false);
    assert!(!t.show_block_view());

    t.process(b"\x1b[2;1HClaude Code\x1b[3;1Hglm-5.2\x1b[20;1H>\x1b[20;3G");
    assert_eq!(t.grid().row_text(1), "Claude Code");
    assert_eq!(t.grid().row_text(2), "glm-5.2");

    t.process(b"\rprogress\x1b[K");
    assert!(!t.show_block_view());
    t.process(b"\x1b[?1049h");
    assert!(t.is_alt_screen_active());
    assert!(
        !t.primary_screen_app_active(),
        "alternate-screen ownership must supersede primary-screen evidence"
    );
    t.process(b"\x1b[?1049l");
    assert!(t.primary_screen_app_active());
    t.block_tracker_mut().reset_to_prompt();
    assert!(!t.primary_screen_app_active());
    assert!(t.show_block_view());

    t.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[6G\x1b[13G");
    assert!(t.primary_screen_app_active());
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(!t.primary_screen_app_active());
    assert!(t.primary_screen_exit_pending());
    assert!(!t.show_block_view());
    t.settle_primary_screen_exit();
    assert!(t.show_block_view());
}

#[test]
fn primary_screen_tui_resize_is_dimension_only() {
    let mut t = Terminal::new(4, 8);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"\x1b[2J\x1b[1;1H\x1b[1;1HABCDEFGH");
    assert!(t.primary_screen_app_active());

    t.resize(4, 4);
    assert_eq!(t.grid().row_text(0), "ABCD");
    assert!(
        t.grid().row_text(1).is_empty(),
        "absolute-positioned TUI rows must not reflow before SIGWINCH repaint"
    );

    t.block_tracker_mut().reset_to_prompt();
    assert!(!t.primary_screen_app_active());
}

#[test]
fn primary_screen_exit_snapshot_keeps_scrollback_and_ctrl_c_resume_tail() {
    let mut terminal = Terminal::new(4, 48);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    terminal.process(
        b"\x1b[2J\x1b[Hanswer line 1\r\nanswer line 2\r\nanswer line 3\r\nPress Ctrl-C again to exit\r\nResume this session with:\r\nclaude --resume session-id",
    );
    terminal.process(b"\x1b]133;D;130\x07\x1b]133;A\x07");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "answer line 1\nanswer line 2\nanswer line 3\nPress Ctrl-C again to exit\nResume this session with:\nclaude --resume session-id"
    );
}

#[test]
fn primary_screen_exit_waits_for_late_resume_tail_before_freezing_block() {
    let mut terminal = Terminal::new(6, 64);
    terminal.process(b"\x1b]7;file://localhost/Users/me/project\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    terminal.process(b"\x1b[2J\x1b[H\x1b[38;2;222;120;80manswer\x1b[0m");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    assert!(terminal.block_tracker().blocks().is_empty());
    assert!(!terminal.show_block_view());

    terminal.process(
        b"\x1b[3;1HPress Ctrl-C again to exit\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume late-id",
    );
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(block.cwd.as_deref(), Some("/Users/me/project"));
    assert!(block.output.contains("answer"));
    assert!(block.output.contains("Press Ctrl-C again to exit"));
    assert!(block.output.contains("claude --resume late-id"));
    let styled = block
        .styled_output
        .as_ref()
        .expect("screen snapshot styles");
    assert!(styled
        .line(0)
        .and_then(|line| line.foreground_at(0))
        .is_some_and(|color| matches!(color, crate::grid::CellColor::Rgb(_))));
}

#[test]
fn primary_screen_snapshot_excludes_rows_scrolled_before_tui_detection() {
    let mut terminal = Terminal::new(3, 40);
    terminal.process(b"old shell row 1\r\nold shell row 2\r\nold shell row 3\r\n");
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07startup\r\n");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());
    terminal.process(b"\x1b[2J\x1b[Hfinal answer\x1b]133;D;0\x07");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(block.output.as_ref(), "final answer");
}

#[test]
fn primary_screen_destructive_repaint_requires_a_synchronized_frame() {
    let mut terminal = Terminal::new(6, 40);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[Hprogress\x1b[2;1Hmore");
    assert!(terminal.primary_screen_app_active());
    assert!(!terminal.primary_screen_repaint_capable());

    terminal.process(b"\x1b[?2026hframe\x1b[?2026l");
    assert!(!terminal.primary_screen_repaint_capable());
    terminal.process(b"\x1b[?2026h\x1b[2Jframe\x1b[?2026l");
    assert!(terminal.primary_screen_repaint_capable());
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(!terminal.primary_screen_repaint_capable());
}

#[test]
fn contiguous_line_erases_covering_viewport_prove_full_frame_repaint() {
    let mut terminal = Terminal::new(3, 20);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[H");
    terminal.process(b"\x1b[?2026h\x1b[H\x1b[2K\x1b[1B\x1b[2K\x1b[1B\x1b[2K\x1b[?2026l");
    assert!(terminal.primary_screen_repaint_capable());
}

#[test]
fn unclosed_full_frame_evidence_cannot_cross_command_boundaries() {
    let mut terminal = Terminal::new(4, 20);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[H");
    terminal.process(b"\x1b[?2026h\x1b[2J");
    terminal.process(b"\x1b]133;D;1\x07\x1b]133;A\x07");
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[H\x1b[?2026l");
    assert!(terminal.primary_screen_app_active());
    assert!(!terminal.primary_screen_repaint_capable());
}

#[test]
fn claude_like_clear_and_repaint_keeps_command_context_across_resizes() {
    fn repaint(terminal: &mut Terminal, rows: usize) {
        let mut frame = Vec::new();
        frame.extend_from_slice(b"\x1b[H");
        for _ in 0..rows {
            frame.extend_from_slice(b"\x1b[2K\x1b[1B");
        }
        frame.extend_from_slice(
            b"\x1b[H\x1b[2GClaude\x1b[9GCode\r\n\x1b[12Gglm-5.2\r\n\x1b[12G~/project",
        );
        terminal.process(&frame);
    }

    let mut terminal = Terminal::new(50, 160);
    terminal.process(b"\x1b]7;file://localhost/Users/me/project\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    repaint(&mut terminal, 50);

    for (rows, cols) in [(35, 100), (60, 180), (42, 120)] {
        terminal.resize(rows, cols);
        repaint(&mut terminal, rows);
        assert!(terminal.primary_screen_app_active());
        assert_eq!(terminal.cwd(), Some("/Users/me/project"));
        assert_eq!(
            terminal
                .block_tracker()
                .in_flight()
                .map(|live| live.command),
            Some("claude")
        );
        assert!(terminal.grid().row_text(0).contains("Claude"));
    }
}

#[test]
fn zero_width_scalars_do_not_consume_grid_cells() {
    let mut t = term();
    t.process("e\u{0301}X".as_bytes());
    assert_eq!(t.grid().row_text(0), "\u{fffd}X");
    assert_eq!(t.grid().cell(0, 1).character, 'X');
    assert_eq!(t.grid().cursor.col, 2);

    let mut t = term();
    t.process("👩‍🔬Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "\u{ff1f}Y");
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    let mut t = term();
    t.process("*\u{fe0f}Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "\u{ff1f}Y");
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    let mut t = term();
    t.process("👩🏽Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "\u{ff1f}Y");
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    let mut t = term();
    t.process("🇨🇳Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "\u{ff1f}Y");
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    let mut t = term();
    t.process("A\u{200d}\nB".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, '\u{fffd}');
    assert_eq!(t.grid().cell(1, 0).character, 'B');

    let mut t = term();
    t.process("\u{200d}B".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, 'B');

    let mut t = term();
    t.process("A\u{200d}\x1b[2CB".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, '\u{fffd}');
    assert_eq!(t.grid().cell(0, 3).character, 'B');

    let mut t = Terminal::new(4, 2);
    t.process("A*\u{fe0f}Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "A");
    assert_eq!(t.grid().row_text(1), "\u{ff1f}");
    assert_eq!(t.grid().row_text(2), "Y");

    let mut t = Terminal::new(4, 2);
    t.process("A🇨🇳Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "A");
    assert_eq!(t.grid().row_text(1), "\u{ff1f}");
    assert_eq!(t.grid().row_text(2), "Y");

    let mut t = Terminal::new(4, 4);
    t.process("ABC*\u{fe0f}Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "ABC");
    assert_eq!(t.grid().row_text(1), "\u{ff1f}Y");
    assert_eq!(t.grid().cell(1, 2).character, 'Y');

    let mut t = Terminal::new(3, 2);
    t.process(b"\x1b[2;1H\x1b]8;;https://weft.dev/stale\x1b\\X\x1b]8;;\x1b\\");
    assert_eq!(t.hyperlinks().url_at(1, 0), Some("https://weft.dev/stale"));
    t.process("\x1b[1;2H*\u{fe0f}".as_bytes());
    assert_eq!(t.grid().cell(1, 0).character, '\u{ff1f}');
    assert_eq!(t.hyperlinks().url_at(1, 0), None);
    assert_eq!(t.hyperlinks().url_at(1, 1), None);
}

#[test]
fn synchronized_output_watchdog_releases_a_missing_reset() {
    let mut t = term();
    let now = std::time::Instant::now();
    t.synchronized_output_started = now.checked_sub(std::time::Duration::from_secs(1));
    assert!(!t.synchronized_output_at(now));
}

#[test]
fn repeated_synchronized_output_begin_does_not_extend_watchdog() {
    let mut t = term();
    t.process(b"\x1b[?2026h");
    let started = t.synchronized_output_started.unwrap();
    t.process(b"\x1b[?2026h");
    assert_eq!(t.synchronized_output_started, Some(started));
    assert!(!t.synchronized_output_at(started + SYNCHRONIZED_OUTPUT_TIMEOUT));
}

#[test]
fn alt_screen_exit_releases_unclosed_synchronized_frame() {
    let mut t = term();
    t.process(b"\x1b[?1049h\x1b[?2026hpartial\x1b[?1049l");
    assert!(!t.synchronized_output());
    assert!(!t.is_alt_screen_active());
}

#[test]
fn decrqm_reports_synchronized_output_support_and_state() {
    let mut t = term();
    t.process(b"\x1b[?2026$p");
    assert_eq!(t.take_response(), b"\x1b[?2026;2$y");
    t.process(b"\x1b[?2026h\x1b[?2026$p");
    assert_eq!(t.take_response(), b"\x1b[?2026;1$y");
}

#[test]
fn progress_rewrites_are_compacted_in_detached_block_output() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07upgrade\n\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"Upgrading.\rUpgrading..\x1b[K\rUpgrading...\x1b[K");
    t.process(b"\nDone\n\x1b]133;D;0\x07");

    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(block.output.as_ref(), "Upgrading...\nDone\n");
}

fn assert_no_orphaned_wide_cells(t: &Terminal, row: usize) {
    for col in 0..t.grid().num_cols {
        let cell = t.grid().cell(row, col);
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            assert!(col > 0);
            assert_eq!(t.grid().cell(row, col - 1).width, CellWidth::Full);
        }
        if cell.width == CellWidth::Full {
            assert!(col + 1 < t.grid().num_cols);
            assert!(
                t.grid()
                    .cell(row, col + 1)
                    .flags
                    .contains(CellFlags::WIDE_SPACER),
                "orphaned wide lead at {row}:{col}"
            );
        }
    }
}

// ── Print / basic ────────────────────────────────────────────

#[test]
fn print_ascii() {
    let mut t = term();
    t.process(b"Hello");
    assert_eq!(t.grid().cell(0, 0).character, 'H');
    assert_eq!(t.grid().cell(0, 1).character, 'e');
    assert_eq!(t.grid().cell(0, 4).character, 'o');
    assert_eq!(t.grid().cursor.col, 5);
}

#[test]
fn omitted_and_zero_csi_counts_use_one_for_movement_and_editing() {
    // ECMA-48: for these commands an omitted Ps and Ps=0 both mean 1.
    // vte exposes an omitted parameter as `[0]`, so the terminal must
    // normalize it before dispatching to Grid.
    for sequence in [b"\x1b[A".as_slice(), b"\x1b[0A".as_slice()] {
        let mut t = Terminal::new(5, 8);
        t.process(b"\x1b[3;3H");
        t.process(sequence);
        assert_eq!(t.grid().cursor.row, 1, "CUU must move one row");
    }

    for sequence in [b"\x1b[L".as_slice(), b"\x1b[0L".as_slice()] {
        let mut t = Terminal::new(4, 8);
        t.process(b"row0\r\nrow1\r\nrow2\r\nrow3");
        t.process(b"\x1b[1;3r\x1b[1;1H");
        t.process(sequence);
        assert_eq!(t.grid().row_text(0), "", "IL must insert a blank row");
        assert_eq!(t.grid().row_text(1), "row0", "IL must shift row 0 down");
        assert_eq!(t.grid().row_text(2), "row1", "IL must clip at margin");
        assert_eq!(t.grid().row_text(3), "row3", "IL must preserve status row");
    }
}

#[test]
fn omitted_zero_and_explicit_one_match_for_all_csi_count_commands() {
    fn snapshot(prefix: &[u8], action: char, parameter: &str) -> (Vec<String>, usize, usize) {
        let mut t = Terminal::new(5, 12);
        t.process(prefix);
        t.process(format!("\x1b[{parameter}{action}").as_bytes());
        let rows = (0..t.grid().num_rows)
            .map(|row| t.grid().row_text(row))
            .collect();
        (rows, t.grid().cursor.row, t.grid().cursor.col)
    }

    let movement_prefix = b"\x1b[3;5H";
    let edit_prefix = b"abcdefghij\x1b[1;4H";
    let line_prefix = b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4\x1b[2;1H";
    let cases: &[(&[u8], char)] = &[
        (movement_prefix, 'A'),
        (movement_prefix, 'B'),
        (movement_prefix, 'C'),
        (movement_prefix, 'D'),
        (movement_prefix, 'E'),
        (movement_prefix, 'F'),
        (movement_prefix, 'I'),
        (movement_prefix, 'Z'),
        (edit_prefix, '@'),
        (edit_prefix, 'P'),
        (edit_prefix, 'X'),
        (line_prefix, 'L'),
        (line_prefix, 'M'),
        (line_prefix, 'S'),
        (line_prefix, 'T'),
    ];

    for &(prefix, action) in cases {
        let expected = snapshot(prefix, action, "1");
        assert_eq!(
            snapshot(prefix, action, ""),
            expected,
            "CSI {action} must default an omitted count to one"
        );
        assert_eq!(
            snapshot(prefix, action, "0"),
            expected,
            "CSI 0{action} must default a zero count to one"
        );
    }
}

#[test]
fn vim_implicit_insert_line_does_not_leave_old_suffixes() {
    let mut t = Terminal::new(6, 40);
    t.process(b"\x1b[?1049h");
    t.process(b"old line with a very long stale suffix\r\nsecond old row");

    // Real Vim upward scrolling repeatedly uses DECSTBM + CUP + IL with
    // no numeric parameter, then paints only the newly exposed line.
    t.process(b"\x1b[1;5r\x1b[1;1H\x1b[L\x1b[1;6r\x1b[1;1Hnew");

    assert_eq!(t.grid().row_text(0), "new");
    assert_eq!(
        t.grid().row_text(1),
        "old line with a very long stale suffix"
    );
    assert_eq!(t.grid().row_text(5), "");
}

#[test]
fn less_search_prompt_and_repaint_sequence_updates_grid() {
    let mut t = Terminal::new(6, 40);
    t.process(b"\x1b[?1049h\x1b[6;1Hstatus line");

    // Captured from less 668: it clears the prompt row, writes '/', then
    // redraws every query character using BS + CSI K.
    t.process(b"\r\x1b[K/\x1b[KG\x08G\x1b[Kr\x08r\x1b[Ki\x08i\x1b[Kd\x08d\x1b[K");
    assert_eq!(t.grid().row_text(5), "/Grid");

    // Enter clears the prompt and less repaints the result rows with CUP
    // and EL. This verifies the VT/Grid side independently of macOS input.
    t.process(b"\r\x1b[K\x1b[1;1H\x1b[Kmatched Grid row\x1b[6;1H:\x1b[K");
    assert_eq!(t.grid().row_text(0), "matched Grid row");
    assert_eq!(t.grid().row_text(5), ":");
}

#[test]
fn decscusr_accepts_standard_space_intermediate_and_legacy_form() {
    let cases = [
        (b"\x1b[1 q".as_slice(), CursorStyle::BlinkingBlock),
        (b"\x1b[2 q".as_slice(), CursorStyle::Block),
        (b"\x1b[3 q".as_slice(), CursorStyle::BlinkingUnderline),
        (b"\x1b[4 q".as_slice(), CursorStyle::Underline),
        (b"\x1b[5 q".as_slice(), CursorStyle::BlinkingBar),
        (b"\x1b[6 q".as_slice(), CursorStyle::Bar),
        // Keep accepting the no-intermediate form used by some TUIs.
        (b"\x1b[1q".as_slice(), CursorStyle::BlinkingBlock),
    ];
    for (sequence, expected) in cases {
        let mut t = term();
        t.process(sequence);
        assert_eq!(t.cursor_style, expected, "sequence={sequence:?}");
    }
}

#[test]
fn new_output_resets_scroll_offset() {
    let mut t = Terminal::new(3, 8);
    // Scroll several lines into scrollback.
    t.process(b"row0\nrow1\nrow2\nrow3\nrow4\nrow5\n");
    assert!(
        t.grid().scrollback_len() > 0,
        "precondition: history exists"
    );

    // User views older history.
    t.grid_mut().scroll_up_history(2);
    assert!(t.grid().is_scrolled(), "precondition: scrolled up");

    // New PTY output must snap back to the live viewport.
    t.process(b"X");
    assert_eq!(t.grid().scroll_offset, 0, "new output resets offset");
    assert!(!t.grid().is_scrolled());
}

#[test]
fn alt_screen_enter_clear_and_exit_restores() {
    let mut t = Terminal::new(5, 10);
    t.process(b"hello");
    assert_eq!(t.grid().cell(0, 0).character, 'h');

    // DEC 1049h: enter alternate screen, stash main, clear alt view.
    t.process(b"\x1b[?1049h");
    assert!(t.is_alt_screen_active());
    assert_eq!(t.grid().cell(0, 0).character, ' ', "alt screen cleared");

    // Writes land on the alternate screen only.
    t.process(b"world");
    assert_eq!(t.grid().cell(0, 0).character, 'w');

    // DEC 1049l: leave alternate screen, main content restored.
    t.process(b"\x1b[?1049l");
    assert!(!t.is_alt_screen_active());
    assert_eq!(t.grid().cell(0, 0).character, 'h', "main screen restored");
}

#[test]
fn alt_screen_cursor_restored_on_exit() {
    let mut t = Terminal::new(5, 10);
    t.process(b"hello"); // cursor at col 5
    assert_eq!(t.grid().cursor.col, 5);

    t.process(b"\x1b[?1049h");
    // Move around inside the alternate screen.
    t.process(b"\x1b[3;3HXYZ");
    t.process(b"\x1b[?1049l");
    // Main cursor returns to where we left it.
    assert_eq!(t.grid().cursor.col, 5, "cursor restored to main position");
}

#[test]
fn print_with_color() {
    let mut t = term();
    t.process(b"\x1b[31mX");
    assert_eq!(t.grid().cell(0, 0).character, 'X');
    assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(1)); // red = palette[1]
}

#[test]
fn print_with_bold_and_italic() {
    let mut t = term();
    t.process(b"\x1b[1;3mX");
    let flags = t.grid().cell(0, 0).flags;
    assert!(flags.contains(CellFlags::BOLD));
    assert!(flags.contains(CellFlags::ITALIC));
}

#[test]
fn wide_char_splat_clears_orphaned_spacer() {
    // v1.0 regression: overwriting a CJK double-width char's leading cell
    // with a half-width char must clear the trailing WIDE_SPACER, else it
    // lingers as a phantom space (the vim-scroll CJK corruption).
    let mut t = term();
    // Print a CJK char at col 0 → occupies [0]=char, [1]=WIDE_SPACER.
    t.process("中".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, '中');
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    // CUP back to col 0, print a single ASCII char (overwrites the lead).
    t.process(b"\x1b[1;1HA");
    assert_eq!(t.grid().cell(0, 0).character, 'A');
    // The orphaned spacer at col 1 must be cleared (reset to default).
    assert!(!t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.grid().cell(0, 1).character, ' ');
}

#[test]
fn wide_char_splat_clears_orphaned_lead() {
    // v1.0 regression: overwriting a WIDE_SPACER (2nd cell) must clear the
    // orphaned leading Full cell at col-1.
    let mut t = term();
    t.process("中".as_bytes()); // [0]=中, [1]=WIDE_SPACER
                                // CUP to col 2 (1-based) = col index 1, print over the spacer.
    t.process(b"\x1b[1;2HB");
    assert_eq!(t.grid().cell(0, 1).character, 'B');
    assert!(!t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    // The orphaned lead at col 0 must be reset (not '中').
    assert_eq!(t.grid().cell(0, 0).character, ' ');
    assert!(!t.grid().cell(0, 0).flags.contains(CellFlags::WIDE_SPACER));
}

#[test]
fn ascii_fast_path_clears_wide_pair_boundaries() {
    // Keep CUP and the ASCII payload in separate process() calls so the
    // payload takes print_ascii_run rather than vte::Perform::print.
    let mut overwrite_lead = term();
    overwrite_lead.process("中".as_bytes());
    overwrite_lead.process(b"\x1b[1;1H");
    overwrite_lead.process(b"A");
    assert_no_orphaned_wide_cells(&overwrite_lead, 0);

    let mut overwrite_spacer = term();
    overwrite_spacer.process("中".as_bytes());
    overwrite_spacer.process(b"\x1b[1;2H");
    overwrite_spacer.process(b"B");
    assert_no_orphaned_wide_cells(&overwrite_spacer, 0);
    assert_eq!(overwrite_spacer.grid().cell(0, 0).character, ' ');
}

#[test]
fn wide_char_splat_clears_pair_overlapped_by_new_spacer() {
    // A new full-width glyph occupies both its leading cell and the next
    // spacer cell. If that second destination cell is itself the leading
    // half of an older wide glyph, the older glyph's spacer must also be
    // cleared. Vim can produce this one-column overlap while repainting
    // shifted CJK rows after IL/DL.
    let mut t = term();
    t.process("A中".as_bytes()); // [0]=A, [1]=中, [2]=WIDE_SPACER
    assert!(t.grid().cell(0, 2).flags.contains(CellFlags::WIDE_SPACER));

    // CUP to col 1 and print 文 across [0,1], overwriting the old lead at
    // [1]. The old spacer at [2] must not survive.
    t.process(b"\x1b[1;1H");
    t.process("文".as_bytes());

    assert_eq!(t.grid().cell(0, 0).character, '文');
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.grid().cell(0, 2).character, ' ');
    assert!(
        !t.grid().cell(0, 2).flags.contains(CellFlags::WIDE_SPACER),
        "overlapped old wide glyph left an orphaned spacer"
    );
}

#[test]
fn csi_character_edits_preserve_wide_cell_pairs() {
    // Vim uses ECH/ICH/DCH and line erasure while repainting shifted rows.
    // Each operation must preserve the full-width lead/spacer invariant.
    let cases: &[&[u8]] = &[
        b"\x1b[1;2H\x1b[K", // EL from the spacer
        b"\x1b[1;1H\x1b[X", // ECH over the lead
        b"\x1b[1;2H\x1b[@", // ICH between lead/spacer
        b"\x1b[1;1H\x1b[P", // DCH deleting only the lead
    ];
    for edit in cases {
        let mut t = term();
        t.process("中A".as_bytes());
        t.process(edit);
        assert_no_orphaned_wide_cells(&t, 0);
    }
}

#[test]
fn sgr_reset() {
    let mut t = term();
    t.process(b"\x1b[31mX\x1b[0mY");
    assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(1));
    assert_eq!(t.grid().cell(0, 1).fg, CellColor::Default);
}

#[test]
fn sgr_empty_params_means_reset() {
    let mut t = term();
    t.process(b"\x1b[1;31mA\x1b[mB");
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::BOLD));
    assert!(!t.grid().cell(0, 1).flags.contains(CellFlags::BOLD));
    assert_eq!(t.grid().cell(0, 1).fg, CellColor::Default);
}

// ── Cursor movement ──────────────────────────────────────────

#[test]
fn cursor_right() {
    let mut t = term();
    t.process(b"\x1b[5C");
    assert_eq!(t.grid().cursor.col, 5);
}

#[test]
fn cursor_down() {
    let mut t = term();
    t.process(b"\x1b[3B");
    assert_eq!(t.grid().cursor.row, 3);
}

#[test]
fn cursor_up() {
    let mut t = term();
    t.process(b"\x1b[3B\x1b[2A");
    assert_eq!(t.grid().cursor.row, 1);
}

#[test]
fn cursor_position() {
    let mut t = term();
    t.process(b"\x1b[10;20H");
    assert_eq!(t.grid().cursor.row, 9);
    assert_eq!(t.grid().cursor.col, 19);
}

#[test]
fn cursor_horizontal_absolute() {
    let mut t = term();
    t.process(b"\x1b[30G");
    assert_eq!(t.grid().cursor.col, 29);
}

#[test]
fn cursor_vertical_absolute() {
    let mut t = term();
    t.process(b"\x1b[12d");
    assert_eq!(t.grid().cursor.row, 11);
}

// ── Clearing ─────────────────────────────────────────────────

#[test]
fn clear_screen_all() {
    let mut t = term();
    t.process(b"ABC\x1b[2J");
    assert_eq!(t.grid().cell(0, 0).character, ' ');
}

#[test]
fn clear_line_right() {
    let mut t = term();
    t.process(b"ABCDE\x1b[1;1H"); // goto (0,0)
    t.grid_mut().cursor.col = 2;
    t.process(b"\x1b[K");
    assert_eq!(t.grid().cell(0, 1).character, 'B');
    assert_eq!(t.grid().cell(0, 2).character, ' ');
}

// ── Line feed / control ──────────────────────────────────────

#[test]
fn linefeed_moves_down() {
    let mut t = term();
    t.process(b"A\nB");
    assert_eq!(t.grid().cell(0, 0).character, 'A');
    assert_eq!(t.grid().cell(1, 0).character, 'B');
}

#[test]
fn carriage_return() {
    let mut t = term();
    t.process(b"ABC\rX");
    assert_eq!(t.grid().cell(0, 0).character, 'X');
    assert_eq!(t.grid().cell(0, 1).character, 'B');
}

#[test]
fn tab_advances() {
    let mut t = term();
    t.process(b"A\tB");
    assert_eq!(t.grid().cell(0, 0).character, 'A');
    assert_eq!(t.grid().cell(0, 8).character, 'B');
}

// ── Escape sequences ─────────────────────────────────────────

#[test]
fn esc_save_restore() {
    let mut t = term();
    t.process(b"\x1b[5;10H\x1b7\x1b[1;1H\x1b8");
    assert_eq!(t.grid().cursor.row, 4);
    assert_eq!(t.grid().cursor.col, 9);
}

#[test]
fn esc_index() {
    let mut t = term();
    t.grid_mut().cursor.row = 3;
    t.process(b"\x1bD");
    assert_eq!(t.grid().cursor.row, 4);
}

#[test]
fn esc_reverse_index() {
    let mut t = term();
    t.grid_mut().cursor.row = 3;
    t.process(b"\x1bM");
    assert_eq!(t.grid().cursor.row, 2);
}

#[test]
fn print_wrap_at_bottom_row_stays_in_bounds() {
    // Regression: the deferred-wrap path in print() did `cursor.row += 1`
    // without a `num_rows - 1` guard. With a scroll region whose bottom is
    // not the last row, wrapping on the last row indexed past the viewport
    // (panic: index == len). The fix clamps like grid.rs does.
    let mut t = Terminal::new(3, 5);
    // Scroll region bottom = row index 1 (NOT the last row index 2).
    t.grid_mut().set_scroll_region(1, 2); // 1-based → top 0, bottom 1; resets cursor
    t.grid_mut().cursor.row = 2; // last row, outside the scroll region
    t.grid_mut().cursor.col = 4; // last column
    t.grid_mut().cursor.wrap_pending = true;
    t.process(b"x"); // triggers the deferred-wrap path
    assert!(
        t.grid().cursor.row < t.grid().num_rows,
        "cursor row {} escaped the viewport of {} rows",
        t.grid().cursor.row,
        t.grid().num_rows
    );
}

// ── Scroll ───────────────────────────────────────────────────

#[test]
fn scroll_up_csi() {
    let mut t = Terminal::new(5, 4);
    for i in 0..5 {
        t.grid_mut().viewport[i].cells[0].character = char::from_digit(i as u32 + 1, 10).unwrap();
    }
    t.process(b"\x1b[S");
    assert_eq!(t.grid().cell(0, 0).character, '2');
    assert_eq!(t.grid().cell(4, 0).character, ' ');
}

// ── OSC ──────────────────────────────────────────────────────

#[test]
fn osc_set_title() {
    let mut t = term();
    t.process(b"\x1b]0;mytitle\x07");
    assert_eq!(t.title(), "mytitle");
}

#[test]
fn osc_133_marker() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07");
    assert_eq!(t.shell_markers().len(), 1);
    assert_eq!(t.shell_markers()[0], ShellMarker::PromptStart);
}

#[test]
fn osc_133_end_with_exit_code() {
    let mut t = term();
    t.process(b"\x1b]133;D;42\x07");
    assert_eq!(t.shell_markers().len(), 1);
    assert_eq!(
        t.shell_markers()[0],
        ShellMarker::CommandEnd { exit_code: 42 }
    );
}

// ── Command blocks (OSC 133 → BlockTracker) ───────────────────

#[test]
fn osc133_lifecycle_produces_block() {
    let mut t = term();
    // Prompt start → AtPrompt + integration ready.
    t.process(b"\x1b]133;A\x07");
    assert!(t.block_tracker().bootstrap_ready());
    assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);

    // Prompt + command render during AtPrompt → NOT captured as output.
    t.process(b"$ ls -la\r");
    // Command start (preexec): snapshot the command row.
    t.process(b"\x1b]133;B\x07");
    assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
    // Output start + streaming output.
    t.process(b"\x1b]133;C\x07");
    t.process(b"file1\nfile2\n");
    // Command end → finalize.
    t.process(b"\x1b]133;D;0\x07");

    let blocks = t.block_tracker().blocks();
    assert_eq!(blocks.len(), 1);
    let b = &blocks[0];
    assert_eq!(b.command, "$ ls -la", "command = prompt row at 133;B");
    assert_eq!(b.output.as_ref(), "file1\nfile2\n", "output captured B..D");
    assert_eq!(b.exit_code, Some(0));
    assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);
}

#[test]
fn alt_screen_output_is_not_captured() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07");
    t.process(b"$ run vim\r");
    t.process(b"\x1b]133;B\x07"); // CommandExecuting — capture active
                                  // Enter the alternate screen (DEC 1049): a full-screen app takes over.
    t.process(b"\x1b[?1049h");
    assert!(t.is_alt_screen_active());
    // This content belongs to the full-screen app — it must NOT leak into
    // the block's output snapshot.
    t.process(b"VIM FULLSCREEN CONTENT\nmore lines\n");
    // Leave the alternate screen and end the command.
    t.process(b"\x1b[?1049l");
    t.process(b"\x1b]133;D;0\x07");

    let b = &t.block_tracker().blocks()[0];
    assert!(
        !b.output.contains("VIM FULLSCREEN CONTENT"),
        "alt-screen content leaked into block output: {:?}",
        b.output
    );
    assert_eq!(b.exit_code, Some(0));
}

// ── Terminal query responses (DA/DSR/size) ──────────────────

#[test]
fn dsr_reports_cursor_position_1_based() {
    let mut t = term();
    t.grid_mut().cursor.row = 4;
    t.grid_mut().cursor.col = 9;
    t.process(b"\x1b[6n");
    // 1-based → row 5, col 10.
    assert_eq!(t.take_response(), b"\x1b[5;10R");
}

#[test]
fn da1_and_text_area_size_responses() {
    let mut t = term(); // 24×80
    t.process(b"\x1b[c"); // DA1
    t.process(b"\x1b[18t"); // text-area size in chars
    let resp = t.take_response();
    let s = String::from_utf8_lossy(&resp);
    assert!(
        s.contains("\x1b[?62") && s.contains('c'),
        "DA1 missing: {s}"
    );
    assert!(s.contains("\x1b[8;24;80t"), "size report missing: {s}");
}

#[test]
fn da2_secondary_device_attributes() {
    let mut t = term();
    t.process(b"\x1b[>c");
    let resp = t.take_response();
    let s = String::from_utf8_lossy(&resp);
    assert!(s.starts_with("\x1b[>") && s.ends_with('c'), "DA2: {s}");
}

// ── Colors ───────────────────────────────────────────────────

#[test]
fn truecolor_fg() {
    let mut t = term();
    t.process(b"\x1b[38;2;255;128;0mX");
    assert_eq!(
        t.grid().cell(0, 0).fg,
        CellColor::Rgb(Color::rgb(255, 128, 0))
    );
}

#[test]
fn indexed_256_color() {
    let mut t = term();
    t.process(b"\x1b[38;5;196mX");
    assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(196));
}

#[test]
fn bright_foreground() {
    let mut t = term();
    t.process(b"\x1b[91mX");
    assert_eq!(t.grid().cell(0, 0).fg, CellColor::Palette(9)); // SGR 91 → palette[9]
}

#[test]
fn background_color() {
    let mut t = term();
    t.process(b"\x1b[44mX");
    assert_eq!(t.grid().cell(0, 0).bg, CellColor::Palette(4)); // SGR 44 → palette[4]
}

// ── Insert/delete ────────────────────────────────────────────

#[test]
fn delete_chars_csi() {
    let mut t = term();
    t.process(b"ABCDE\x1b[1;1H\x1b[1P");
    assert_eq!(t.grid().cell(0, 0).character, 'B');
    assert_eq!(t.grid().cell(0, 1).character, 'C');
    assert_eq!(t.grid().cell(0, 4).character, ' ');
}

// ── Private modes ────────────────────────────────────────────

#[test]
fn dec_private_cursor_keys() {
    let mut t = term();
    t.process(b"\x1b[?1h");
    assert!(t.app_cursor_keys);
    t.process(b"\x1b[?1l");
    assert!(!t.app_cursor_keys);
}

#[test]
fn bracketed_paste_mode() {
    let mut t = term();
    t.process(b"\x1b[?2004h");
    assert!(t.bracketed_paste);
    t.process(b"\x1b[?2004l");
    assert!(!t.bracketed_paste);
}

#[test]
fn vim_mouse_a_sequence_enables_sgr_button_event_reporting() {
    let mut t = term();
    // Captured from macOS Vim 9.1 after `:set mouse=a`.
    t.process(b"\x1b[?1049h\x1b[?1006;1000h\x1b[?1002h");
    assert!(t.is_alt_screen_active());
    assert_eq!(t.mouse_protocol, MouseProtocol::ButtonEvent);
    assert!(t.sgr_mouse);

    t.process(b"\x1b[?1006;1000l\x1b[?1002l");
    assert_eq!(t.mouse_protocol, MouseProtocol::Off);
    assert!(!t.sgr_mouse);
}

// ── Full reset ───────────────────────────────────────────────

#[test]
fn ris_full_reset() {
    let mut t = term();
    t.process(b"\x1b[31mX\x1b[?1h");
    assert!(t.app_cursor_keys);
    t.process(b"\x1bc");
    assert!(!t.app_cursor_keys);
    assert_eq!(t.grid().cell(0, 0).character, ' ');
}

// ── Palette ──────────────────────────────────────────────────

#[test]
fn palette_init_has_256_colors() {
    let t = term();
    assert_eq!(t.palette[0], Color::rgb(0, 0, 0));
    assert_eq!(t.palette[7], Color::rgb(229, 229, 229));
    assert_eq!(t.palette[16], Color::rgb(0, 0, 0));
    assert_eq!(t.palette[232], Color::rgb(8, 8, 8));
}

// ── Wrap behavior ────────────────────────────────────────────

#[test]
fn wrap_at_line_end() {
    let mut t = Terminal::new(5, 4);
    t.process(b"ABCD"); // fills 4 cols, sets wrap_pending
    assert!(t.grid().cursor.wrap_pending);
    t.process(b"E"); // should wrap to next line
    assert_eq!(t.grid().cursor.row, 1);
    assert_eq!(t.grid().cursor.col, 1);
    assert_eq!(t.grid().cell(1, 0).character, 'E');
}

#[test]
fn print_after_narrowing_resize_does_not_drop_chars() {
    // Simulates the resize race: shell wrote a full-width row at 10 cols,
    // then the grid was narrowed to 5 while the PTY SIGWINCH is still in
    // flight. Force the cursor past the new last column and print more —
    // those chars must wrap onto the next line, not be discarded.
    let mut t = Terminal::new(5, 10);
    t.process(b"0123456789"); // fills row 0 at width 10
    assert!(t.grid().cursor.wrap_pending);
    // Narrow the grid (rewrap merges the single logical line into two).
    t.resize(5, 5);
    // The shell has NOT learned the new size yet and keeps printing at
    // the cursor position, which now points past the last column.
    // Force cursor to column 7 (past the new num_cols=5) as the old
    // shell output would, then print — must wrap, not drop.
    t.grid_mut().cursor.col = 7;
    t.grid_mut().cursor.wrap_pending = false;
    t.process(b"XY");
    // No character should be lost: both 'X' and 'Y' must appear.
    let found_x = (0..t.grid().num_rows)
        .any(|r| (0..t.grid().num_cols).any(|c| t.grid().cell(r, c).character == 'X'));
    let found_y = (0..t.grid().num_rows)
        .any(|r| (0..t.grid().num_cols).any(|c| t.grid().cell(r, c).character == 'Y'));
    assert!(found_x, "'X' must not be dropped on resize race");
    assert!(found_y, "'Y' must not be dropped on resize race");
}

// ── Scroll region ────────────────────────────────────────────

#[test]
fn scroll_region_set_and_reset() {
    let mut t = term();
    t.process(b"\x1b[5;20r");
    assert_eq!(t.grid().scroll_region(), (4, 19));
    t.process(b"\x1b[r"); // reset
    assert_eq!(t.grid().scroll_region(), (0, 23));
}

// ── v0.5 editor takeover: OSC 7 + effective mode + submit ──────

use crate::input::{build_submit_bytes, InputMode};

#[test]
fn osc7_sets_cwd() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]7;file://macbook.local/Users/me/proj\x1b\\");
    assert_eq!(t.cwd(), Some("/Users/me/proj"));
}

#[test]
fn osc7_localhost_host_strips_correctly() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]7;file://localhost/tmp\x1b\\");
    assert_eq!(t.cwd(), Some("/tmp"));
}

#[test]
fn osc7_malformed_is_ignored() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]7;not-a-uri\x1b\\");
    assert_eq!(t.cwd(), None);
}

#[test]
fn osc8_hyperlink_tags_cells_and_resolves_url() {
    // OSC 8 ; ; URI ST → start hyperlink. Subsequent printed cells get
    // the HYPERLINK flag and resolve to URI via the registry.
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]8;;https://weft.dev/a\x1b\\");
    t.process(b"link");
    t.process(b"\x1b]8;;\x1b\\"); // close
    t.process(b"plain");

    // The four cells of "link" should be tagged; "plain" should not.
    let g = t.grid();
    for (i, _) in "link".chars().enumerate() {
        assert!(
            g.cell(0, i)
                .flags
                .contains(crate::grid::CellFlags::HYPERLINK),
            "cell {i} of 'link' should be HYPERLINK"
        );
    }
    // 'p' of "plain" is at col 4 (after 4 chars of "link").
    assert!(
        !g.cell(0, 4)
            .flags
            .contains(crate::grid::CellFlags::HYPERLINK),
        "cell after link close should NOT be HYPERLINK"
    );

    // Cmd+Click resolution: cell (0, 0) → URL.
    assert_eq!(t.hyperlinks().url_at(0, 0), Some("https://weft.dev/a"));
    assert_eq!(t.hyperlinks().url_at(0, 3), Some("https://weft.dev/a"));
    // After close, the plain cell has no URL.
    assert_eq!(t.hyperlinks().url_at(0, 4), None);
}

#[test]
fn osc8_dedups_identical_urls() {
    let mut t = Terminal::new(24, 80);
    // Two consecutive links to the same URL — registry dedups.
    t.process(b"\x1b]8;;https://weft.dev/x\x1b\\");
    t.process(b"a");
    t.process(b"\x1b]8;;\x1b\\");
    t.process(b"\x1b]8;;https://weft.dev/x\x1b\\");
    t.process(b"b");
    t.process(b"\x1b]8;;\x1b\\");

    assert_eq!(t.hyperlinks().url_at(0, 0), Some("https://weft.dev/x"));
    assert_eq!(t.hyperlinks().url_at(0, 1), Some("https://weft.dev/x"));
}

#[test]
fn osc8_cell_map_clears_on_scroll() {
    // When content scrolls the viewport, the (row, col) → id map is
    // invalidated. The HYPERLINK flag stays on cells (visual underline
    // persists) but click resolution returns None — MVP trade-off.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/s\x1b\\");
    t.process(b"link\n");
    t.process(b"\x1b]8;;\x1b\\");
    // Emit enough lines to force a scroll.
    t.process(b"line1\nline2\nline3");
    // After scrolling, no cells should resolve to URLs.
    for row in 0..3 {
        for col in 0..10 {
            assert!(
                t.hyperlinks().url_at(row, col).is_none(),
                "hyperlink at ({row},{col}) should be cleared after scroll"
            );
        }
    }
}

#[test]
fn osc9_git_branch_set_then_cleared_on_prompt_start() {
    // Shell hook emits OSC 9;git=<branch> only inside a repo.
    let mut t = Terminal::new(24, 80);
    // First prompt inside a git repo: hook sends OSC 9;git=main.
    t.process(b"\x1b]9;git=main\x07");
    assert_eq!(t.git_branch(), Some("main"));
    // Next prompt: precmd runs again. PromptStart (133;A) must clear
    // the branch first; if the cwd is still a repo the hook re-emits,
    // but if it's now a non-git dir nothing arrives and the label
    // correctly disappears.
    t.process(b"\x1b]133;A\x07"); // no OSC 9 this time (non-git dir)
    assert_eq!(t.git_branch(), None, "branch cleared on PromptStart");
    // Returning to a git repo re-establishes it.
    t.process(b"\x1b]9;git=develop\x07");
    assert_eq!(t.git_branch(), Some("develop"));
}

#[test]
fn editor_takes_over_only_after_bootstrap_at_prompt() {
    let mut t = Terminal::new(24, 80);
    // Not integrated yet.
    assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
    // Bootstrap + AtPrompt.
    t.process(b"\x1b]133;A\x07");
    assert_eq!(t.effective_input_mode(), InputMode::Editor);
}

#[test]
fn editor_hidden_in_alt_screen() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    assert_eq!(t.effective_input_mode(), InputMode::Editor);
    t.process(b"\x1b[?1049h"); // enter alt screen
    assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
}

#[test]
fn submit_command_builds_bytes_and_blocks_editor() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    for c in "ls -la".chars() {
        t.editor_mut().buffer.insert_char(c);
    }
    let bytes = t.submit_command();
    assert_eq!(bytes, build_submit_bytes("ls -la", false));
    // Editor cleared and blocked until 133;B.
    assert_eq!(t.editor().text(), "");
    assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
}

#[test]
fn tab_in_command_output_is_captured_as_spaces() {
    // Regression: macOS `ls` separates columns with tabs, which are C0
    // controls (handled by `execute`, not `print`). Without mirroring the
    // tab advance into the block's captured output, the block view showed
    // filenames concatenated (`Cargo.lockCargo.toml...`).
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07"); // bootstrap + AtPrompt
    t.process(b"\x1b]133;B\x07"); // command start — capture on
    t.process(b"a\tb\tc");
    t.process(b"\x1b]133;D;0\x07"); // command end — finalize
    let blocks = t.block_tracker().blocks();
    assert_eq!(blocks.len(), 1);
    let out = &blocks[0].output;
    assert!(!out.contains('\t'), "tab leaked into output: {out:?}");
    let a = out.find('a').unwrap();
    let b = out.find('b').unwrap();
    assert!(
        b > a + 1,
        "a and b are adjacent (tabs not expanded): {out:?}"
    );
}

#[test]
fn command_133b_uses_editor_command_not_grid_snapshot() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    for c in "real-cmd".chars() {
        t.editor_mut().buffer.insert_char(c);
    }
    t.submit_command();
    // Shell "executes": emits 133;B. The tracker should record the editor
    // command, not the (empty) grid prompt row.
    t.process(b"\x1b]133;B\x07");
    t.process(b"\x1b]133;D;0\x07");
    let blocks = t.block_tracker().blocks();
    assert_eq!(blocks.last().unwrap().command, "real-cmd");
}

#[test]
fn passthrough_133b_uses_grid_snapshot() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    // No editor submit → passthrough path → command from grid snapshot.
    // Print a fake prompt+command line, then 133;B.
    t.process(b"$ echo hi");
    t.process(b"\x1b]133;B\x07");
    t.process(b"\x1b]133;D;0\x07");
    let cmd = t.block_tracker().blocks().last().unwrap().command.clone();
    assert!(cmd.contains("echo hi"), "got {cmd:?}");
}

#[cfg(test)]
mod reflow_cjk_tests {
    use super::super::*;

    /// v1.0 fix: verify CJK characters render correctly after ASCII fast path.
    /// Regression test: `ls -l` with Chinese filenames showed `????` because
    /// the ASCII fast path scanned 0x20..=0x7E and left CJK bytes (≥0x80) for
    /// vte's per-byte path. This test ensures the handoff preserves UTF-8.
    #[test]
    fn ascii_fast_path_preserves_cjk() {
        let mut t = Terminal::new(24, 80);
        // Mix ASCII + CJK in one line: `-rw-r--r-- 1 user wheel 0 Jan 1 12:00 中文文件.txt\n`
        let line = "-rw-r--r-- 1 user wheel 0 Jan 1 12:00 中文文件.txt\n";
        t.process(line.as_bytes());
        let g = t.grid();
        // The CJK chars should land at their correct column positions.
        // Find them by scanning the first row.
        let row: String = (0..g.num_cols).map(|c| g.cell(0, c).character).collect();
        assert!(row.contains('中'), "expected '中' in row 0, got: {:?}", row);
        assert!(row.contains('文'), "expected '文' in row 0, got: {:?}", row);
    }

    /// v1.0 fix: verify CJK survives a byte-boundary split that mimics
    /// `tab.rs` message chunking. A UTF-8 multi-byte sequence split across
    /// two `process()` calls must not corrupt into `?` / replacement chars.
    #[test]
    fn cjk_survives_byte_boundary_split() {
        let mut t = Terminal::new(24, 80);
        // "中文" = [0xE4, 0xB8, 0xAD, 0xE6, 0x96, 0x87] (6 bytes)
        let bytes = "中文".as_bytes();
        // Split in the middle of the first char (after byte 1).
        t.process(&bytes[..1]); // 0xE4 alone — incomplete UTF-8 start
        t.process(&bytes[1..]); // 0xB8 0xAD 0xE6 0x96 0x87 — rest
        let g = t.grid();
        let row: String = (0..g.num_cols).map(|c| g.cell(0, c).character).collect();
        assert!(
            row.contains('中'),
            "expected '中' after split, got: {:?}",
            row
        );
        assert!(
            row.contains('文'),
            "expected '文' after split, got: {:?}",
            row
        );
    }

    /// A full-width char that would straddle the right margin makes the print
    /// path wrap before placing it, leaving the last cell as a never-written
    /// default. Reflow must NOT bake that trailing blank into the logical line
    /// as a real space — otherwise each resize inserts a phantom space between
    /// CJK characters (compounding). See Grid::resize content_end trimming.
    #[test]
    fn wide_wrap_blank_is_not_baked_into_logical_line() {
        let mut t = Terminal::new(40, 94);
        // Long line with CJK that lands at a wrap boundary when narrowed.
        let echo = "andylee@host dir % cd /tmp && rm -f 待产手册_v1.0.md && touch 待产手册_v1.0.md && ls -lt 待产手册_v1.0.md\n";
        t.process(echo.as_bytes());
        let ls = "-rw-r--r--@ 1 user  wheel  0 Jun 16 12:00 待产手册_v1.0.md\n";
        t.process(ls.as_bytes());
        t.process(b"andylee@host /tmp % ");

        // Wrap (narrow) then unwrap (wide). CJK runs must stay contiguous —
        // no phantom space between characters.
        for w in [50, 64, 94, 40, 94, 55, 94] {
            t.resize(40, w);
        }

        let g = t.grid();
        for row in 0..g.num_rows {
            let mut run = String::new();
            let mut in_cjk = false;
            for col in 0..g.num_cols {
                let c = g.cell(row, col);
                if c.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                let wide = unicode_width::UnicodeWidthChar::width(c.character).unwrap_or(0) > 1;
                if wide {
                    run.push(c.character);
                    in_cjk = true;
                } else if in_cjk && c.character == ' ' {
                    // A space immediately after/within a CJK run is the bug.
                    panic!("phantom space in CJK run {run:?} at row {row}");
                } else if in_cjk {
                    break;
                }
            }
        }
    }
}

/// v1.0 P1.5: Performance benchmarks for the VT parse + grid write pipeline.
///
/// Run with: `cargo test -p weft_core --lib -- --ignored --nocapture`
///
/// These feed realistic PTY byte streams through `Terminal::process()` and
/// measure wall-clock time. They cover the scenarios from the v1.0 plan's
/// Phase 1.5 validation table so we can decide whether the optional C2
/// (custom VT parser) / C3 (multithreading) tasks are still needed after
/// the high-ROI B0-B3 + C1 optimizations.
///
/// All benchmarks use a 24×80 terminal (the v1.0 default) with 10K-line
/// scrollback, matching the plan's test conditions.
#[cfg(test)]
mod perf_benchmarks {
    use super::super::*;
    use std::time::Instant;

    /// Build a realistic `seq 1 N` byte stream, wrapped in OSC 133 shell-
    /// integration markers the way zsh would emit them. The marker prefix
    /// forces the parser through the escape path once per command, then the
    /// bulk numeric output hits the ASCII fast path.
    fn seq_output(n: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n as usize * 8);
        // 133;A (prompt start) + 133;B (command start) + 133;C (output start)
        buf.extend_from_slice(b"\x1b]133;A\x07andylee@host ~ % \x1b]133;B\x07seq 1 ");
        buf.extend_from_slice(n.to_string().as_bytes());
        buf.extend_from_slice(b"\n\x1b]133;C\x07");
        for i in 1..=n {
            buf.extend_from_slice(i.to_string().as_bytes());
            buf.push(b'\n');
        }
        // 133;D;0 (command end, exit 0) + 133;A (next prompt start)
        buf.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07andylee@host ~ % ");
        buf
    }

    /// Build a realistic `ls -la /usr/bin` byte stream: ~1000 entries with
    /// file-mode / owner / size / date / name columns. Mixes ASCII fast path
    /// (the columns) with occasional ANSI color escapes (like `ls --color`).
    fn ls_output(entry_count: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(entry_count * 80);
        buf.extend_from_slice(
            b"\x1b]133;A\x07andylee@host ~ % \x1b]133;B\x07ls -la /usr/bin\n\x1b]133;C\x07",
        );
        buf.extend_from_slice(b"total 12345\n");
        for i in 0..entry_count {
            // Mode owner group size date name — printable ASCII bulk.
            // Insert a color SGR every 10 lines to exercise escape handling.
            if i % 10 == 0 {
                buf.extend_from_slice(b"\x1b[1;32m"); // bold green
            }
            let line = format!(
                "-rwxr-xr-x  1 root  wheel  {:>6} Jan  1 12:00 bin_tool_{:04}\n",
                10000 + i,
                i
            );
            buf.extend_from_slice(line.as_bytes());
            if i % 10 == 0 {
                buf.extend_from_slice(b"\x1b[0m"); // reset
            }
        }
        buf.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07andylee@host ~ % ");
        buf
    }

    /// Measure `Terminal::process` throughput for a given byte stream.
    /// Returns (elapsed_ms, bytes, rows_written).
    fn bench(label: &str, bytes: &[u8]) -> (f64, usize) {
        let mut t = Terminal::with_scrollback(24, 80, 10_000);
        let start = Instant::now();
        t.process(bytes);
        let elapsed = start.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        let bytes_len = bytes.len();
        // Throughput in MB/s.
        let mbps = (bytes_len as f64 / 1_048_576.0) / (elapsed.as_secs_f64().max(1e-9));
        println!("  {label:<28} {ms:>8.2} ms  | {bytes_len:>8} bytes | {mbps:>7.1} MB/s");
        (ms, bytes_len)
    }

    /// `seq 1 10000` — plan target: < 50ms (Warp ~10ms).
    #[test]
    #[ignore]
    fn bench_seq_10000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 10000 (target < 50ms) ===");
        let bytes = seq_output(10_000);
        let (ms, _) = bench("seq 1 10000", &bytes);
        assert!(ms < 50.0, "seq 1 10000 took {ms:.2}ms, target < 50ms");
    }

    /// `seq 1 100000` — plan target: < 300ms (Warp ~50ms).
    #[test]
    #[ignore]
    fn bench_seq_100000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 100000 (target < 300ms) ===");
        let bytes = seq_output(100_000);
        let (ms, _) = bench("seq 1 100000", &bytes);
        assert!(ms < 300.0, "seq 1 100000 took {ms:.2}ms, target < 300ms");
    }

    /// `ls -la /usr/bin` style (~1000 entries) — plan target: < 20ms (Warp ~5ms).
    #[test]
    #[ignore]
    fn bench_ls_usr_bin() {
        println!("\n=== Phase 1.5 benchmark: ls -la /usr/bin (target < 20ms) ===");
        let bytes = ls_output(1000);
        let (ms, _) = bench("ls -la /usr/bin (1000 entries)", &bytes);
        assert!(ms < 20.0, "ls output took {ms:.2}ms, target < 20ms");
    }

    /// Pure ASCII bulk (no escapes) — measures the C1 fast-path ceiling.
    #[test]
    #[ignore]
    fn bench_pure_ascii_100k() {
        println!("\n=== Phase 1.5 benchmark: pure ASCII bulk (no escapes) ===");
        let bytes: Vec<u8> = (0..100_000)
            .flat_map(|i| format!("{i}\n").into_bytes())
            .collect();
        let (ms, _) = bench("pure ASCII 100k lines", &bytes);
        // No escape overhead at all — should be faster than seq_output which
        // has OSC 133 markers. Use as a ceiling reference.
        let _ = ms;
    }

    /// Color-heavy output (SGR every line) — measures escape-sequence overhead.
    #[test]
    #[ignore]
    fn bench_color_output() {
        println!("\n=== Phase 1.5 benchmark: colored output (SGR per line) ===");
        let mut bytes = Vec::with_capacity(80_000);
        bytes.extend_from_slice(b"\x1b]133;A\x07% \x1b]133;B\x07color-test\n\x1b]133;C\x07");
        for i in 0..5000 {
            // Alternate colors to exercise SGR parsing.
            let color = match i % 6 {
                0 => b"\x1b[31m", // red
                1 => b"\x1b[32m", // green
                2 => b"\x1b[33m", // yellow
                3 => b"\x1b[34m", // blue
                4 => b"\x1b[35m", // magenta
                _ => b"\x1b[36m", // cyan
            };
            bytes.extend_from_slice(color);
            bytes.extend_from_slice(format!("line {i:04} with color\n").as_bytes());
            bytes.extend_from_slice(b"\x1b[0m");
        }
        let (ms, _) = bench("colored 5k lines", &bytes);
        let _ = ms;
    }
}
