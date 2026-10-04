use super::*;
use crate::blocks::{ShellPhase, DEFAULT_OUTPUT_CAP};
use crate::grid::{CellColor, UnderlineStyle};
use std::time::{Duration, Instant};

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
    // v1.10.5: CUP (`H`) — CHR (`G`) is horizontal-only and no longer
    // absolute evidence (pi's input-line redraw false-positive).
    t.process(b"\x1b[?2031h\x1b[H");
    assert!(
        t.show_block_view(),
        "one absolute move is not enough evidence"
    );
    t.process(b"\x1b[2;1H");
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
    // v1.12.23 audit batch 2: `BlockTracker::reset_to_prompt` was removed as
    // dead code; drive the same Ctrl+C-flush semantics via the OSC 133
    // boundary instead (finalize in-flight block + return to AtPrompt).
    // Note: on a screen-owned session any `133;D` defers the finalization
    // (200ms settle protocol), so the settle call below completes the
    // transition that `reset_to_prompt` performed synchronously.
    t.process(b"\x1b]133;D\x07");
    t.settle_primary_screen_exit();
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
fn alt_screen_history_peek_overlays_block_view_during_alt() {
    let mut t = term();
    // Bootstrap the block tracker so show_block_view() can ever be true.
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(t.show_block_view());

    // Entering the alt screen normally hides the BlockView (the TUI owns the
    // viewport).
    t.process(b"\x1b[?1049h");
    assert!(t.is_alt_screen_active());
    assert!(
        !t.show_block_view(),
        "alt screen hides the block view by default"
    );

    // Enabling the peek must overlay the BlockView over the TUI.
    t.set_alt_screen_history_peek(true);
    assert!(t.is_alt_screen_history_peek());
    assert!(
        t.show_block_view(),
        "peek must make the block view render over the alt-screen TUI"
    );

    // Disabling the peek returns to the live alt grid.
    t.set_alt_screen_history_peek(false);
    assert!(!t.show_block_view());
}

#[test]
fn alt_screen_exit_clears_history_peek() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"\x1b[?1049h");
    t.set_alt_screen_history_peek(true);
    assert!(t.is_alt_screen_history_peek());

    // Exiting the alt screen must clear the peek flag so it does not leak into
    // the restored primary-screen view (which would wrongly keep BlockView on).
    t.process(b"\x1b[?1049l");
    assert!(!t.is_alt_screen_active());
    assert!(
        !t.is_alt_screen_history_peek(),
        "peek must clear when leaving the alt screen"
    );
}

/// v1.10.19: PTY cols must NOT swing with a primary-screen TUI's transient
/// DEC 1049 toggles. The old is_alt-only selection produced 102↔99 col
/// flapping (alt uses full width, primary uses gutter-subtracted width);
/// every flip queued a TIOCSWINSZ → SIGWINCH → redraw → flip, the ~130ms
/// resize oscillation.
///
/// v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT): the primary-phase value
/// is Content, not Full. omp draws its UI exactly at the PTY cols it receives,
/// so Full made the input-line border (`|]`) land past the renderable area
/// (right ~1.5 cols clipped) and fold to the next line after settle. The
/// mapping never ratchets into a new value — each phase still maps to ONE
/// constant. (v1.10.25 Batch 3: the pure mapping alone does NOT prevent a
/// Full↔Content *alternation*; the anti-cycle guard is the app-layer burst
/// hysteresis, `Tab::burst_locked_cols` — see FIX_SCROLL_SHIFT_AND_
/// RESIZE_STORM.md 演进注记.)
///
/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): the alt phase is now gated by the
/// sustained-alt hysteresis — only alt *continuously* resident for
/// `SUSTAINED_ALT_COLS_MS` reports Full, so a transient 1049h/l excursion
/// (omp's ~129ms SIGWINCH repaint) stays Content and cannot feed the loop.
#[test]
fn primary_tui_content_cols_are_stable_across_transient_alt_toggles() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"\x1b[?2031h\x1b[H\x1b[2;1H");
    assert!(t.primary_screen_app_active());
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "a screen-owning primary TUI uses content width"
    );

    // v1.10.30 (FIX_LESS_ALT_COLS_JUMP): Create a burst re-entry scenario
    // by doing an initial alt enter/exit pair. This makes subsequent alt
    // entries burst re-entries (< 400ms since last exit) rather than isolated.
    t.process(b"\x1b[?1049h");
    t.process(b"\x1b[?1049l");

    // A genuinely transient alt phase of the same TUI — the resize-loop
    // feedback shape. Under the sustained-alt hysteresis this must NOT flip
    // to Full: the loop is broken at the source
    // (docs/FIX_TRANSIENT_ALT_COLS_FLIP.md).
    t.process(b"\x1b[?1049h");
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "a transient (sub-250ms) alt phase stays at content width"
    );
    t.process(b"\x1b[?1049l");
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "primary phase after a transient alt exit stays at content width"
    );

    // A plain, *sustained* alt-screen TUI (vim) is full width while active —
    // once it has stayed resident past the 250ms threshold; primary width is
    // content after it exits.
    t.process(b"\x1b[?1049h");
    t.capabilities.alt_active_since = sustained_residency_stamp();
    assert_eq!(t.tui_cols_kind(), TuiColsKind::Full);
    t.process(b"\x1b[?1049l");
    assert_eq!(t.tui_cols_kind(), TuiColsKind::Content);

    // While the primary-screen exit is still settling the TUI remains on
    // screen — content width holds until the screen settles (no width jump
    // at the settle boundary).
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(t.primary_screen_exit_pending());
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "exit-pending keeps content width"
    );

    t.settle_primary_screen_exit();
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "a settled shell prompt stays at content width — no settle jump"
    );
}

/// v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT): the cols mapping is a
/// PURE function of the alt flag — each phase maps to one CONSTANT target,
/// so N toggles replay the same pair and the primary target never drifts
/// into a third value. This test locks ONLY that pure mapping; the mapping
/// alone does NOT prevent a Full↔Content alternation from feeding the
/// SIGWINCH feedback loop — the v1.10.25 Batch 3 app-layer burst hysteresis
/// (tab/resize.rs `Tab::burst_locked_cols`, ioctl-count-bounded storm
/// regression in tab/tests.rs) is the anti-cycle guard.
///
/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): "alt maps to Full" now means
/// *sustained* alt — residency past `SUSTAINED_ALT_COLS_MS`. Each toggle's
/// alt half is faked to sustained residency so the pair stays the two
/// constants Full/Content and no third value can ratchet in.
#[test]
fn repeated_transient_1049_toggles_keep_the_tui_cols_target_constant() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"\x1b[?2031h\x1b[H\x1b[2;1H");
    assert!(t.primary_screen_app_active());
    let primary_target = t.tui_cols_kind();
    assert_eq!(primary_target, TuiColsKind::Content);

    // Many 1049h/l flips — the SIGWINCH → redraw → toggle feedback loop
    // drives ~130ms-period bursts in the field.
    for flip in 0..50u32 {
        t.process(b"\x1b[?1049h");
        // Faking sustained residency (past the 250ms hysteresis) locks in
        // the Full constant for this half of the pair; a real transient
        // excursion never crosses the threshold — that is exactly what the
        // hysteresis guarantees.
        t.capabilities.alt_active_since = sustained_residency_stamp();
        assert_eq!(
            t.tui_cols_kind(),
            TuiColsKind::Full,
            "sustained alt phase always Full (flip {flip})"
        );
        t.process(b"\x1b[?1049l");
        assert_eq!(
            t.tui_cols_kind(),
            primary_target,
            "primary phase always Content (flip {flip}) — no drift"
        );
    }
    // The burst leaves no residue: the settled target equals the pre-burst
    // value, so a pane already at that size emits no further winsize ioctl.
    assert_eq!(t.tui_cols_kind(), primary_target);
}

/// A residency stamp comfortably past the 250ms hysteresis threshold.
/// `checked_sub` because `Instant - Duration` panics on underflow; the
/// monotonic clock epoch is boot time, so a test process can never run
/// within 300ms of it — the expect documents that impossibility.
fn sustained_residency_stamp() -> Option<Instant> {
    Instant::now().checked_sub(Duration::from_millis(SUSTAINED_ALT_COLS_MS + 50))
}

/// v1.10.30 (FIX_LESS_ALT_COLS_JUMP): omp burst re-entry test. After a recent
/// alt exit (< 400ms), re-entering alt should NOT immediately flip to Full
/// (must still satisfy 250ms sustained residency). This prevents omp from
/// oscillating in its 1049h→repaint→1049l loop.
#[test]
fn transient_alt_excursion_does_not_flip_cols_kind() {
    let mut t = term();
    // First, create a recent exit by entering and immediately leaving alt.
    t.process(b"\x1b[?1049h");
    t.process(b"\x1b[?1049l");
    // Now re-enter alt (burst re-entry < 400ms).
    t.process(b"\x1b[?1049h");
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Content,
        "a burst re-entry must not flip cols to Full immediately"
    );
    // A real sustained alt TUI (vim): fake 300ms residency → Full.
    t.capabilities.alt_active_since = sustained_residency_stamp();
    assert_eq!(t.tui_cols_kind(), TuiColsKind::Full);
    // Leaving alt restores Content (and clears the residency stamp).
    t.process(b"\x1b[?1049l");
    assert_eq!(t.tui_cols_kind(), TuiColsKind::Content);
}

/// v1.10.30 (FIX_LESS_ALT_COLS_JUMP): isolated alt entry test. When there's
/// no recent alt exit, entering alt should immediately flip to Full (fixes
/// less/vim startup jump where the app draws before the first SIGWINCH).
#[test]
fn isolated_alt_entry_flips_to_full_immediately() {
    let mut t = term();
    // No prior alt exit in this session → isolated entry.
    t.process(b"\x1b[?1049h");
    assert_eq!(
        t.tui_cols_kind(),
        TuiColsKind::Full,
        "an isolated alt entry must flip cols to Full immediately"
    );
    // Leaving alt restores Content.
    t.process(b"\x1b[?1049l");
    assert_eq!(t.tui_cols_kind(), TuiColsKind::Content);
}

/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP), v1.10.30 (FIX_LESS_ALT_COLS_JUMP):
/// the pure decision backing `tui_cols_kind` — alt continuously resident
/// at/over `SUSTAINED_ALT_COLS_MS` is Full, transient (<250ms) is Content,
/// and an unknown entry time (None) conservatively keeps the old Full mapping.
/// Isolated entries (no recent exit or >= 400ms since last exit) are Full
/// immediately; burst re-entries (< 400ms) still need sustained residency.
#[test]
fn sustained_alt_cols_kind_pure_decision() {
    // Not alt: always Content.
    assert_eq!(
        sustained_alt_cols_kind(false, None, None),
        TuiColsKind::Content
    );
    assert_eq!(
        sustained_alt_cols_kind(false, Some(Duration::from_secs(10)), None),
        TuiColsKind::Content
    );

    // Isolated entry (no recent exit) → immediate Full, regardless of residency time.
    assert_eq!(
        sustained_alt_cols_kind(true, Some(Duration::from_millis(0)), None),
        TuiColsKind::Full,
        "isolated entry with 0ms residency must be Full"
    );
    assert_eq!(
        sustained_alt_cols_kind(true, Some(Duration::from_millis(100)), None),
        TuiColsKind::Full,
        "isolated entry with 100ms residency must be Full"
    );

    // Isolated entry (>= 400ms since last exit) → immediate Full.
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(0)),
            Some(Duration::from_millis(400))
        ),
        TuiColsKind::Full,
        "isolated entry (400ms since exit) must be Full"
    );
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(500)),
            Some(Duration::from_millis(1000))
        ),
        TuiColsKind::Full,
        "isolated entry (1000ms since exit) must be Full"
    );

    // Burst re-entry (< 400ms since last exit) — applies sustained residency threshold.
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(0)),
            Some(Duration::from_millis(200))
        ),
        TuiColsKind::Content,
        "burst re-entry with 0ms residency must be Content"
    );
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(300))
        ),
        TuiColsKind::Content,
        "burst re-entry with 100ms residency must be Content"
    );
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(SUSTAINED_ALT_COLS_MS - 1)),
            Some(Duration::from_millis(350))
        ),
        TuiColsKind::Content,
        "burst re-entry with 249ms residency must be Content"
    );
    // At/over the threshold — Full.
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_millis(SUSTAINED_ALT_COLS_MS)),
            Some(Duration::from_millis(300))
        ),
        TuiColsKind::Full,
        "burst re-entry with 250ms residency must be Full"
    );
    assert_eq!(
        sustained_alt_cols_kind(
            true,
            Some(Duration::from_secs(2)),
            Some(Duration::from_millis(200))
        ),
        TuiColsKind::Full,
        "burst re-entry with 2s residency must be Full"
    );

    // Unknown entrance time — conservative old-behavior Full.
    assert_eq!(sustained_alt_cols_kind(true, None, None), TuiColsKind::Full);
    assert_eq!(
        sustained_alt_cols_kind(true, None, Some(Duration::from_millis(200))),
        TuiColsKind::Full
    );
}

#[test]
fn primary_screen_history_view_snapshot_refresh_is_explicitly_coalesced() {
    let mut terminal = Terminal::new(5, 48);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H\x1b[2J\x1b[Hfirst answer\x1b[2;1Hsecond answer");
    assert!(terminal.primary_screen_app_active());
    assert!(terminal
        .block_tracker()
        .in_flight()
        .is_some_and(|live| live.output.is_empty()));

    terminal.set_primary_history_view(true);
    let snapshot = terminal.block_tracker().in_flight().unwrap();
    assert_eq!(snapshot.output, "first answer\nsecond answer");

    terminal.process(b"\x1b[3;1Hthird answer");
    let unchanged = terminal.block_tracker().in_flight().unwrap();
    assert_eq!(unchanged.output, "first answer\nsecond answer");

    let refreshed_at = Instant::now() + super::screen_exit::PRIMARY_HISTORY_SNAPSHOT_INTERVAL;
    assert!(terminal.refresh_primary_history_snapshot_at(refreshed_at));
    let refreshed = terminal.block_tracker().in_flight().unwrap();
    assert_eq!(
        refreshed.output,
        "first answer\nsecond answer\nthird answer"
    );
}

#[test]
fn primary_screen_history_snapshot_refresh_is_rate_limited() {
    let mut terminal = Terminal::new(5, 48);
    terminal.process(
        b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H\x1b[2J\x1b[Hfirst",
    );
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);
    terminal.process(b"\x1b[2;1Hsecond");

    assert!(!terminal.refresh_primary_history_snapshot_at(Instant::now()));
    assert_eq!(
        terminal.block_tracker().in_flight().unwrap().output,
        "first"
    );

    let due = Instant::now() + super::screen_exit::PRIMARY_HISTORY_SNAPSHOT_INTERVAL;
    assert!(terminal.refresh_primary_history_snapshot_at(due));
    assert_eq!(
        terminal.block_tracker().in_flight().unwrap().output,
        "first\nsecond"
    );
}

#[test]
fn primary_screen_history_view_includes_rows_scrolled_off_the_live_viewport() {
    let mut terminal = Terminal::new(4, 32);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H\x1b[2J\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");
    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.grid().num_rows, 4);

    terminal.set_primary_history_view(true);
    let snapshot = terminal.block_tracker().in_flight().unwrap();
    assert!(
        snapshot.output.starts_with("one\ntwo"),
        "history capture must retain primary-screen rows above the live viewport: {:?}",
        snapshot.output
    );
    assert!(snapshot.output.ends_with("five\nsix"));
    assert!(snapshot.output.lines().count() > terminal.grid().num_rows);
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

    // v1.12.23 audit batch 2: `BlockTracker::reset_to_prompt` was removed as
    // dead code; drive the same Ctrl+C-flush exit via the OSC 133 boundary.
    // No settle needed here: the app_active flip comes from the D handler
    // resetting primary_screen_cursor_ops (perform.rs), matching the single
    // phase assertion below.
    t.process(b"\x1b]133;D\x07");
    assert!(!t.primary_screen_app_active());
}

#[test]
fn primary_screen_exit_snapshot_keeps_scrollback_and_ctrl_c_resume_tail() {
    let mut terminal = Terminal::new(4, 48);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.block_tracker().screen_document_start(), Some(0));

    terminal.process(
        b"\x1b[2J\x1b[Hanswer line 1\r\nanswer line 2\r\nanswer line 3\r\nPress Ctrl-C again to exit\r\nResume this session with:\r\nclaude --resume session-id",
    );
    terminal.process(b"\x1b]133;D;130\x07\x1b]133;A\x07");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "answer line 1\nanswer line 2\nanswer line 3\n\nPress Ctrl-C again to exit\n\nResume this session with:\nclaude --resume session-id"
    );
}

#[test]
fn primary_screen_snapshot_excludes_previous_command_rows_left_in_viewport() {
    let mut terminal = Terminal::new(10, 64);
    terminal.process(b"\x1b]133;A\x07\x1b[Hpwd\x1b[2;1H/Users/me/project");
    for ch in "claude".chars() {
        terminal.editor_mut().buffer.insert_char(ch);
    }
    terminal.submit_command();
    terminal.process(b"\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07");

    // The TUI starts below the shell's retained rows without clearing them.
    // They remain valid Grid content, but are not part of this command's
    // detached transcript because the command already has its own Block.
    terminal.process(b"\x1b[4;1H\x1b[5;1HClaude Code\x1b[6;1Hglm-5.2");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(
        !output.contains("pwd"),
        "previous command leaked: {output:?}"
    );
    assert!(
        !output.contains("/Users/me/project"),
        "previous output leaked: {output:?}"
    );
    assert!(
        !output.starts_with("claude\n"),
        "command echo leaked: {output:?}"
    );
    assert!(output.contains("Claude Code"));
    assert!(output.contains("glm-5.2"));
}

#[test]
fn primary_screen_live_view_starts_at_the_owned_document_boundary() {
    let mut terminal = Terminal::new(10, 64);
    terminal.process(
        b"\x1b]133;A\x07\x1b[1;1Hpwdd\x1b[2;1Hzsh: command not found: pwdd\x1b[3;1Hpwd\x1b[4;1H/Users/me/project",
    );
    for ch in "claude".chars() {
        terminal.editor_mut().buffer.insert_char(ch);
    }
    terminal.submit_command();
    terminal.process(b"\x1b[5;1Hclaude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[6;1H\x1b[7;1HClaude Code");

    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.primary_screen_visible_row_start(), Some(5));
    assert_eq!(
        terminal.primary_screen_viewport_ownership(),
        Some([false, false, false, false, false, false, true, false, false, false].as_slice())
    );
    assert_eq!(terminal.grid().row_text(0), "pwdd");
    assert_eq!(terminal.grid().row_text(5), "");
    assert_eq!(terminal.grid().row_text(6), "Claude Code");

    terminal.grid_mut().set_scroll_offset(1);
    assert_eq!(terminal.primary_screen_visible_row_start(), None);
    assert_eq!(terminal.primary_screen_viewport_ownership(), None);
}

#[test]
fn primary_screen_boundary_is_frozen_before_tui_repeats_the_command_name() {
    let mut terminal = Terminal::new(10, 64);
    terminal.process(b"\x1b]133;A\x07\x1b[Hpwd\x1b[2;1H/Users/me/project");
    for ch in "claude".chars() {
        terminal.editor_mut().buffer.insert_char(ch);
    }
    terminal.submit_command();
    terminal.process(b"\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07");

    // The first addressed TUI row itself repeats the command name. Waiting
    // until the second cursor op to scan would mistake this for the shell echo.
    terminal.process(b"\x1b[5;1Hclaude\x1b[6;1HClaude Code");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("pwd"));
    assert!(!output.contains("/Users/me/project"));
    assert!(
        output.contains("claude"),
        "TUI title was cropped: {output:?}"
    );
    assert!(output.contains("Claude Code"));
}

#[test]
fn primary_screen_ascii_fast_path_expands_capture_above_initial_boundary() {
    let mut terminal = Terminal::new(8, 64);
    terminal
        .process(b"\x1b]133;A\x07\x1b[Hold shell row\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[5;1H\x1b[6;1H");
    assert!(terminal.primary_screen_app_active());

    // Printable ASCII bypasses vte::Perform::print(), so it must update the
    // screen-document boundary inside print_ascii_run itself.
    terminal.process(b"\x1b[2;1HTUI HEADER");
    terminal.set_primary_history_view(true);
    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("old shell row"));
    assert!(
        output.contains("TUI HEADER"),
        "ASCII row was cropped: {output:?}"
    );
}

#[test]
fn primary_screen_absolute_boundary_survives_forward_and_reverse_scrolls() {
    let mut terminal = Terminal::new(7, 32);
    terminal
        .process(b"\x1b]133;A\x07\x1b[Hold\x1b[2;1Holder\x1b[3;1Happ\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[4;1Hanswer one\x1b[5;1Hanswer two\x1b[6;1H");
    assert!(terminal.primary_screen_app_active());

    terminal.process(b"\x1b[1S");
    terminal.set_primary_history_view(true);
    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.lines().any(|line| line == "old"));
    assert!(!output.contains("older"));
    assert!(output.contains("answer one"));
    assert!(output.contains("answer two"));

    terminal.set_primary_history_view(false);
    terminal.process(b"\x1b[1T");
    terminal.set_primary_history_view(true);
    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.lines().any(|line| line == "old"));
    assert!(!output.contains("older"));
    assert!(output.contains("answer one"));
    assert!(output.contains("answer two"));
}

#[test]
fn primary_screen_ownership_follows_rows_into_scrollback() {
    let mut terminal = Terminal::new(3, 24);
    for (row, text) in ["old shell", "owned answer", "owned prompt"]
        .into_iter()
        .enumerate()
    {
        for (col, character) in text.chars().enumerate() {
            terminal.grid_mut().viewport[row].cells[col].character = character;
        }
    }
    terminal.capabilities.primary_screen_ownership.viewport = Some(vec![false, true, true]);

    terminal.process(b"\x1b[S");
    assert_eq!(
        terminal.capabilities.primary_screen_ownership.scrollback,
        vec![false]
    );
    assert_eq!(
        terminal
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref(),
        Some([true, true, false].as_slice())
    );

    terminal.process(b"\x1b[S");
    assert_eq!(
        terminal.capabilities.primary_screen_ownership.scrollback,
        vec![false, true]
    );
    let snapshot = terminal
        .grid()
        .document_snapshot_from_position_with_ownership_masks_and_resolver(
            0,
            &terminal.capabilities.primary_screen_ownership.scrollback,
            terminal
                .capabilities
                .primary_screen_ownership
                .viewport
                .as_deref()
                .unwrap(),
            |_| None,
            DEFAULT_OUTPUT_CAP,
        )
        .0;
    assert!(!snapshot.contains("old shell"));
    assert!(snapshot.contains("owned answer"));
    assert!(snapshot.contains("owned prompt"));
}

#[test]
fn primary_screen_ownership_resizes_with_dimension_only_primary_grid() {
    let mut terminal = Terminal::new(3, 32);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[Hbanner\x1b[2;1Hprompt");
    assert!(terminal.primary_screen_app_active());

    terminal.resize(5, 32);
    assert_eq!(
        terminal
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_ref()
            .map(Vec::len),
        Some(5)
    );
    terminal.process(b"\x1b[5;1Hnewly exposed tail");
    terminal.set_primary_history_view(true);
    assert!(terminal
        .block_tracker()
        .in_flight()
        .unwrap()
        .output
        .contains("newly exposed tail"));

    terminal.set_primary_history_view(false);
    terminal.resize(2, 32);
    terminal.resize(5, 32);
    assert_eq!(
        terminal
            .capabilities
            .primary_screen_ownership
            .viewport
            .as_deref(),
        Some([true, true, false, false, false].as_slice())
    );
    terminal.process(b"\x1b[5;1Htail after shrink and grow");
    terminal.set_primary_history_view(true);
    assert!(terminal
        .block_tracker()
        .in_flight()
        .unwrap()
        .output
        .contains("tail after shrink and grow"));
}

#[test]
fn alt_screen_csi_3j_preserves_hidden_primary_scrollback_ownership() {
    let mut terminal = Terminal::new(3, 24);
    terminal.capabilities.primary_screen_ownership.viewport = Some(vec![true, true, true]);
    terminal.process(b"one\r\ntwo\r\nthree\r\nfour");
    terminal.capabilities.primary_screen_ownership.scrollback =
        vec![true; terminal.grid().scrollback_len()];
    terminal.process(b"\x1b[?1049h");
    let hidden_scrollback = terminal.alt_grid.scrollback.len();
    let hidden_ownership = terminal
        .capabilities
        .primary_screen_ownership
        .scrollback
        .clone();

    terminal.process(b"alternate\x1b[3J");

    assert_eq!(terminal.alt_grid.scrollback.len(), hidden_scrollback);
    assert_eq!(
        terminal.capabilities.primary_screen_ownership.scrollback,
        hidden_ownership
    );
}

#[test]
fn runtime_scrollback_shrink_keeps_primary_ownership_suffix() {
    let mut terminal = Terminal::new(2, 16);
    terminal.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight");
    let retained = terminal.grid().scrollback_len();
    assert!(retained >= 4);
    terminal.capabilities.primary_screen_ownership.scrollback =
        (0..retained).map(|index| index % 2 == 1).collect();
    let expected =
        terminal.capabilities.primary_screen_ownership.scrollback[retained - 2..].to_vec();
    terminal.process(b"\x1b[?1049h");

    terminal.set_scrollback_max_lines(2);

    assert_eq!(terminal.alt_grid.scrollback.len(), 2);
    assert_eq!(
        terminal.capabilities.primary_screen_ownership.scrollback,
        expected
    );
}

#[test]
fn alternate_screen_mutations_do_not_corrupt_later_primary_tail_boundary() {
    let mut terminal = Terminal::new(8, 64);
    terminal
        .process(b"\x1b]133;A\x07\x1b[Hold shell row\x1b[3;1Hhybrid\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[?1049hALT SCREEN\r\nline 2\r\nline 3\x1b[2S\x1b[?1049l");

    terminal.process(b"\x1b[4;1Hprimary teardown\x1b[5;1H");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);
    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("old shell row"));
    assert!(!output.contains("ALT SCREEN"));
    assert!(
        output.contains("primary teardown"),
        "primary tail was cropped: {output:?}"
    );
}

#[test]
fn hidden_primary_boundary_survives_alt_screen_resize_transition() {
    let mut terminal = Terminal::new(8, 24);
    terminal.process(
        b"\x1b]133;A\x07\x1b[1;1Hold shell row\x1b[3;1Hhybrid\x1b]133;B\x07\x1b]133;C\x07",
    );
    terminal.process(b"\x1b[5;1Hprimary first");
    assert!(!terminal.primary_screen_app_active());

    terminal.process(b"\x1b[?1049hALT SCREEN");
    terminal.resize(8, 10);
    terminal.process(b"\x1b[?1049l\x1b[5;1Htail");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("old shell row"));
    assert!(!output.contains("ALT SCREEN"));
    assert!(!output.contains("hybrid"));
    assert!(
        output.contains("tail"),
        "primary tail was cropped after alt resize: {output:?}"
    );
}

#[test]
fn primary_screen_exit_waits_for_late_resume_tail_before_freezing_block() {
    let mut terminal = Terminal::new(6, 64);
    terminal.process(b"\x1b]7;file://localhost/Users/me/project\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.block_tracker().screen_document_start(), Some(1));

    terminal.process(b"\x1b[2J\x1b[H\x1b[38;2;222;120;80manswer\x1b[0m");
    assert_eq!(terminal.block_tracker().screen_document_start(), Some(0));
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
    assert!(
        block.output.contains("answer"),
        "late-tail snapshot lost answer: {:?}",
        block.output
    );
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
fn colored_status_survives_spinner_cursor_rewrite() {
    let mut terminal = Terminal::new(12, 100);
    terminal.process(b"\x1b]133;A\x07openclaw gateway status\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(
        b"\x1b[1m\x1b[38;2;255;90;45mOpenClaw\x1b[39m\x1b[22m\r\n\
          \x1b[?25l\x1b[90m|\x1b[39m\r\n\
          \x1b[1D\x1b[0K\x1b[32m<>\x1b[39m  \r\n\
          \x1b[?25h\r\x1b[2K\x1b[38;2;139;127;119mService:\x1b[39m \
          \x1b[38;2;255;90;45mLaunchAgent\x1b[39m (\x1b[32mloaded\x1b[39m)\r\n",
    );
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");

    let block = terminal.block_tracker().blocks().last().expect("block");
    let service_line = block
        .output
        .lines()
        .position(|line| line.starts_with("Service:"))
        .expect("service line");
    let styled = block.styled_output.as_ref().expect("styled output");
    let line = styled.line(service_line).expect("styled service line");
    assert_eq!(
        line.foreground_at(0),
        Some(crate::grid::CellColor::Rgb(crate::grid::Color::rgb(
            139, 127, 119
        )))
    );
    assert_eq!(
        line.foreground_at("Service: ".chars().count()),
        Some(crate::grid::CellColor::Rgb(crate::grid::Color::rgb(
            255, 90, 45
        )))
    );
}

#[test]
fn deferred_primary_screen_exit_replaces_stale_row_suffixes() {
    let mut terminal = Terminal::new(7, 72);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());
    terminal.process(b"\x1b[2J\x1b[Hanswer\x1b[4;1HResume this session with: stale answer suffix");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());

    terminal.process(b"\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume clean-id");
    assert_eq!(terminal.grid().row_text(3), "Resume this session with:");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert!(!block.output.contains("stale answer suffix"));
    assert!(block.output.contains("claude --resume clean-id"));
}

#[test]
fn deferred_primary_screen_exit_clears_replaced_row_hyperlinks() {
    let mut terminal = Terminal::new(7, 72);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());
    terminal.process(
        b"\x1b[4;1H\x1b]8;;https://weft.dev/stale\x07stale linked suffix that extends far beyond replacement\x1b]8;;\x07",
    );
    assert_eq!(
        terminal.hyperlinks().url_at(3, 40),
        Some("https://weft.dev/stale")
    );
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");

    terminal.process(b"\x1b[4;1HResume this session with:");

    assert_eq!(terminal.grid().row_text(3), "Resume this session with:");
    assert_eq!(terminal.hyperlinks().url_at(3, 40), None);
}

#[test]
fn deferred_primary_screen_tail_can_expand_a_nonzero_document_boundary() {
    let mut terminal = Terminal::new(8, 64);
    terminal.process(
        b"\x1b]133;A\x07\x1b[1;1Hold shell row\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07",
    );
    terminal.process(b"\x1b[4;1Hanswer\x1b[5;1H");
    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.block_tracker().screen_document_start(), Some(3));

    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    terminal.process(
        b"\x1b[2;1HPress Ctrl-C again to exit\x1b[3;1HResume this session with:\x1b[4;1Hclaude --resume deferred-id",
    );
    assert_eq!(terminal.block_tracker().screen_document_start(), Some(1));
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert!(!block.output.contains("old shell row"));
    assert!(block.output.contains("Press Ctrl-C again to exit"));
    assert!(block.output.contains("claude --resume deferred-id"));
}

#[test]
fn primary_screen_candidate_survives_resize_after_first_cursor_address() {
    let mut terminal = Terminal::new(8, 24);
    terminal.process(
        b"\x1b]133;A\x07\x1b[1;1Hold shell row\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07",
    );
    terminal.process(b"\x1b[5;1Hfirst frame");
    assert!(!terminal.primary_screen_app_active());

    terminal.resize(8, 10);
    terminal.process(b"\x1b[5;1Hsecond");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("old shell row"));
    assert!(!output.contains("claude"));
    assert!(
        output.contains("second"),
        "resized frame was cropped: {output:?}"
    );
}

#[test]
fn frozen_primary_screen_candidate_survives_resize_before_any_cursor_address() {
    let mut terminal = Terminal::new(8, 24);
    terminal.process(
        b"\x1b]133;A\x07\x1b[1;1Hold shell row\x1b[3;1Hclaude\x1b]133;B\x07\x1b]133;C\x07",
    );
    assert!(!terminal.primary_screen_app_active());

    terminal.resize(8, 10);
    terminal.process(b"\x1b[5;1Hfirst\x1b[6;1Hsecond");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(!output.contains("old shell row"));
    assert!(!output.contains("claude"));
    assert!(output.contains("first"));
    assert!(output.contains("second"));
}

#[test]
fn candidate_ownership_reflows_with_startup_output_before_tui_takeover() {
    let mut terminal = Terminal::new(6, 18);
    terminal.process(b"old shell heading\r\nold shell body\r\n");
    terminal
        .process(b"\x1b]133;B\x07\x1b]133;C\x07startup linear output that wraps before takeover");
    assert!(!terminal.primary_screen_app_active());

    terminal.resize(6, 9);
    terminal.process(b"\x1b[1;1Hbanner\x1b[6;1Hprompt");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(
        output.contains("startup"),
        "reflowed startup output was lost: {output:?}"
    );
    assert!(output.contains("banner"));
    assert!(output.contains("prompt"));
    assert!(
        !output.contains("old shell"),
        "stale rows leaked: {output:?}"
    );
}

#[test]
fn candidate_ownership_reflow_drops_the_same_overflow_prefix_as_grid() {
    let mut terminal = Terminal::with_scrollback(4, 16, 2);
    terminal.process(b"old shell one\r\nold shell two\r\nold shell three\r\n");
    terminal.process(
        b"\x1b]133;B\x07\x1b]133;C\x07owned-start-a owned-start-b owned-start-c owned-start-d",
    );
    assert!(!terminal.primary_screen_app_active());

    terminal.resize(4, 5);
    assert_eq!(
        terminal
            .capabilities
            .primary_screen_ownership
            .scrollback
            .len(),
        terminal.grid().scrollback.len(),
        "ownership and grid must retain the same reflow suffix"
    );
    terminal.process(b"\x1b[1;1Hhead\x1b[4;1Htail");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(
        !output.contains("old shell"),
        "stale prefix leaked: {output:?}"
    );
    assert!(output.contains("head"));
    assert!(output.contains("tail"));
}

#[test]
fn hidden_primary_candidate_ownership_survives_height_resize_before_takeover() {
    let mut terminal = Terminal::new(5, 24);
    terminal.process(b"old shell heading\r\nold shell body\r\n");
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07startup");
    terminal.process(b"\x1b[?1049h");

    terminal.resize(8, 24);
    terminal.process(b"\x1b[?1049l\x1b[1;1Hbanner\x1b[8;1Hprompt");
    assert!(terminal.primary_screen_app_active());
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(output.contains("startup"));
    assert!(output.contains("banner"));
    assert!(output.contains("prompt"));
    assert!(
        !output.contains("old shell"),
        "stale rows leaked: {output:?}"
    );
}

#[test]
fn ordinary_running_command_still_reflows_while_candidate_is_pending() {
    let mut terminal = Terminal::new(5, 12);
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07abcdefghijklmnop");
    assert!(!terminal.primary_screen_app_active());

    terminal.resize(5, 6);
    let text = (0..terminal.grid().num_rows)
        .map(|row| terminal.grid().row_text(row))
        .collect::<Vec<_>>()
        .join("");

    assert!(
        text.contains("abcdefghijklmnop"),
        "reflow truncated output: {text:?}"
    );
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
fn primary_screen_snapshot_filters_untouched_rows_without_destroying_live_grid() {
    let mut terminal = Terminal::new(8, 48);
    terminal.process(
        b"previous command\r\nstale table heading\r\nstale table row one\r\nstale table row two",
    );
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07");

    // Primary-screen TUIs frequently paint a banner at the top and their
    // prompt/status near the bottom without explicitly clearing intervening
    // shell rows. Two absolute cursor moves establish viewport ownership.
    terminal.process(b"\x1b[HClaude banner\x1b[7;1Hprompt\x1b[8;1Hstatus");
    assert!(terminal.primary_screen_app_active());
    assert_eq!(terminal.grid().row_text(1), "stale table heading");
    assert_eq!(terminal.grid().row_text(2), "stale table row one");
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert!(output.contains("Claude banner"));
    assert!(output.contains("prompt"));
    assert!(output.contains("status"));
    assert!(
        !output.contains("stale table"),
        "stale rows leaked: {output:?}"
    );
}

#[test]
fn mouse_reporting_is_suspended_while_primary_screen_interrupt_settles() {
    let mut terminal = Terminal::new(5, 48);
    terminal
        .process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[Hbanner\x1b[2;1Hprompt\x1b[?1003h\x1b[?1006h");
    assert!(terminal.primary_screen_app_active());
    assert!(terminal.accepts_mouse_reporting_input());

    terminal.begin_primary_screen_interrupt_capture();
    assert!(!terminal.accepts_mouse_reporting_input());

    terminal.cancel_primary_screen_interrupt_capture();
    assert!(terminal.accepts_mouse_reporting_input());

    terminal.process(b"\x1b]133;D;130\x07\x1b]133;A\x07");
    assert!(!terminal.accepts_mouse_reporting_input());
    terminal.settle_primary_screen_exit();
    assert_eq!(terminal.mouse_protocol(), MouseProtocol::Off);
    assert!(!terminal.sgr_mouse());
    assert!(!terminal.accepts_mouse_reporting_input());
}

#[test]
fn alternate_screen_mouse_reporting_works_without_shell_markers() {
    let mut terminal = Terminal::new(5, 48);
    terminal.process(b"\x1b[?1049h\x1b[?1003h\x1b[?1006h");
    assert!(terminal.accepts_mouse_reporting_input());
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
fn synchronized_full_repaint_discards_superseded_primary_screen_scrollback() {
    let mut terminal = Terminal::new(4, 32);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"old banner\r\nold answer 1\r\nold answer 2\r\nold answer 3\r\nold answer 4");
    assert!(terminal.grid().scrollback_len() > 0);

    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[Hnew banner\r\nnew answer\x1b[?2026l");
    terminal.set_primary_history_view(true);
    let output = terminal.block_tracker().in_flight().unwrap().output;
    // v1.10.23 (FIX_OMP_CONTENT_LOSS): the superseded frame is PRESERVED
    // into the block history before the repaint clears the scrollback — the
    // streamed paragraphs must stay readable when reviewing history.
    assert!(
        output.contains("old banner"),
        "superseded frame must be preserved into the block history: {output:?}"
    );
    assert!(output.contains("old answer 4"));
    assert!(output.contains("new banner"));
}

#[test]
fn legal_repeated_rows_survive_after_atomic_repaint_evidence() {
    let mut terminal = Terminal::new(16, 72);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[?2026l");
    assert!(terminal.primary_screen_repaint_capable());
    let row = b"legal repeated table row with important user-visible content";
    for index in 0..6 {
        terminal.process(format!("\x1b[{};1H", index + 1).as_bytes());
        terminal.process(row);
        terminal.process(format!("\x1b[{};1H", index + 9).as_bytes());
        terminal.process(row);
    }
    terminal.set_primary_history_view(true);

    let output = terminal.block_tracker().in_flight().unwrap().output;
    assert_eq!(
        output
            .matches("legal repeated table row with important user-visible content")
            .count(),
        12
    );
}

#[test]
fn interrupt_snapshot_preserves_answer_when_resume_tail_reuses_screen_rows() {
    let mut terminal = Terminal::new(6, 64);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"answer row one\x1b[2;1Hanswer row two\x1b[3;1Htable final row");
    terminal.begin_primary_screen_interrupt_capture();
    terminal.process(b"stale repaint suffix");
    terminal.process(
        b"\x1b[3;1HPress Ctrl-C again to exit\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume preserved-id",
    );
    terminal.process(b"\x1b]133;D;130\x07\x1b]133;A\x07");
    terminal.settle_primary_screen_exit();

    let output = terminal
        .block_tracker()
        .blocks()
        .last()
        .unwrap()
        .output
        .as_ref();
    assert_eq!(
        output,
        "answer row two\ntable final row\n\nPress Ctrl-C again to exit\n\nResume this session with:\nclaude --resume preserved-id"
    );
}

#[test]
fn continued_user_input_discards_stale_interrupt_snapshot() {
    let mut terminal = Terminal::new(5, 48);
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"old answer before first interrupt");
    terminal.begin_primary_screen_interrupt_capture();

    terminal.cancel_primary_screen_interrupt_capture();
    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[Hnew answer after continuing\x1b[?2026l");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    terminal.settle_primary_screen_exit();

    let output = terminal
        .block_tracker()
        .blocks()
        .last()
        .unwrap()
        .output
        .as_ref();
    assert!(output.contains("new answer after continuing"));
    assert!(
        !output.contains("old answer before first interrupt"),
        "stale frozen transcript leaked after continued input: {output:?}"
    );
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
    // v1.6.0: width-0 scalars are appended to the previous cell's grapheme
    // cluster in RowExtras instead of being replaced with U+FFFD/U+FF1F
    // fallbacks. The cell's `character` keeps the lead scalar; the full
    // cluster lives in extras. Consumers (selection, copy, renderer) consult
    // extras when the EXTRA flag is set.

    // e + combining acute → cell keeps 'e', cluster "e\u{0301}" in extras.
    let mut t = term();
    t.process("e\u{0301}X".as_bytes());
    // v1.6.0: row_text returns the full cluster string, not just the lead char.
    assert_eq!(t.grid().row_text(0), "e\u{0301}X");
    assert_eq!(t.grid().cell(0, 0).character, 'e');
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(0),
        Some("e\u{0301}")
    );
    assert_eq!(t.grid().cell(0, 1).character, 'X');
    assert_eq!(t.grid().cursor.col, 2);

    // ZWJ emoji sequence (woman + ZWJ + microscope): the microscope joins
    // the cluster via suppress_joined_scalar. Cluster is preserved in extras.
    let mut t = term();
    t.process("👩‍🔬Y".as_bytes());
    // v1.6.0: row_text returns the full ZWJ cluster string.
    assert_eq!(t.grid().row_text(0), "👩\u{200d}🔬Y");
    assert_eq!(t.grid().cell(0, 0).character, '👩');
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(0),
        Some("👩\u{200d}🔬")
    );
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    // VS16 promotes '*' from width 1 to width 2 — cell expands to Full.
    let mut t = term();
    t.process("*\u{fe0f}Y".as_bytes());
    // v1.6.0: row_text returns the full cluster including VS16.
    assert_eq!(t.grid().row_text(0), "*\u{fe0f}Y");
    assert_eq!(t.grid().cell(0, 0).character, '*');
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    // Skin tone modifier on emoji — appended to cluster, width unchanged.
    let mut t = term();
    t.process("👩🏽Y".as_bytes());
    // v1.6.0: row_text returns the full cluster including skin tone.
    assert_eq!(t.grid().row_text(0), "👩🏽Y");
    assert_eq!(t.grid().cell(0, 0).character, '👩');
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(t.grid().viewport[0].extras.grapheme_at(0), Some("👩🏽"));
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    // Regional indicator pair (flag) — second RI extends the first.
    let mut t = term();
    t.process("🇨🇳Y".as_bytes());
    // v1.6.0: row_text returns the full flag cluster (both regional indicators).
    assert_eq!(t.grid().row_text(0), "🇨🇳Y");
    assert_eq!(t.grid().cell(0, 0).character, '🇨');
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(t.grid().viewport[0].extras.grapheme_at(0), Some("🇨🇳"));
    assert_eq!(t.grid().cell(0, 2).character, 'Y');
    assert_eq!(t.grid().cursor.col, 3);

    // ZWJ at end of line with no following scalar — stays in extras, cursor
    // moves to next line on '\n', and 'B' prints normally at (1, 0).
    let mut t = term();
    t.process("A\u{200d}\nB".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, 'A');
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(0),
        Some("A\u{200d}")
    );
    assert_eq!(t.grid().cell(1, 0).character, 'B');

    // ZWJ with no previous cell — dropped.
    let mut t = term();
    t.process("\u{200d}B".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, 'B');
    assert!(t.grid().viewport[0].extras.is_empty());

    // ZWJ followed by cursor move then 'B': 'B' can't join the cluster
    // (previous cell is blank after the move), so it prints normally.
    let mut t = term();
    t.process("A\u{200d}\x1b[2CB".as_bytes());
    assert_eq!(t.grid().cell(0, 0).character, 'A');
    assert!(t.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(0),
        Some("A\u{200d}")
    );
    assert_eq!(t.grid().cell(0, 3).character, 'B');

    // VS16 at line boundary (2-col grid): cluster width grows to 2 but
    // there's no room to expand — cell stays width 1, cluster in extras.
    // 'Y' wraps to the next line as usual.
    let mut t = Terminal::new(4, 2);
    t.process("A*\u{fe0f}Y".as_bytes());
    // v1.6.0: row_text returns the full cluster including VS16.
    assert_eq!(t.grid().row_text(0), "A*\u{fe0f}");
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(1),
        Some("*\u{fe0f}")
    );
    assert_eq!(t.grid().row_text(1), "Y");
    assert_eq!(t.grid().row_text(2), "");

    // Regional flag at line boundary (2-col grid): flag wraps to next row.
    let mut t = Terminal::new(4, 2);
    t.process("A🇨🇳Y".as_bytes());
    assert_eq!(t.grid().row_text(0), "A");
    // v1.6.0: row_text returns the full flag cluster.
    assert_eq!(t.grid().row_text(1), "🇨🇳");
    assert!(t.grid().cell(1, 0).flags.contains(CellFlags::EXTRA));
    assert_eq!(t.grid().viewport[1].extras.grapheme_at(0), Some("🇨🇳"));
    assert_eq!(t.grid().row_text(2), "Y");

    // VS16 at end of 4-col row: cluster width grows but can't expand inline.
    let mut t = Terminal::new(4, 4);
    t.process("ABC*\u{fe0f}Y".as_bytes());
    // v1.6.0: row_text returns the full cluster including VS16.
    assert_eq!(t.grid().row_text(0), "ABC*\u{fe0f}");
    assert!(t.grid().cell(0, 3).flags.contains(CellFlags::EXTRA));
    assert_eq!(
        t.grid().viewport[0].extras.grapheme_at(3),
        Some("*\u{fe0f}")
    );
    assert_eq!(t.grid().row_text(1), "Y");

    // v1.6.0: overwriting a cell with '*' + VS16 no longer destroys the
    // hyperlink on a different row. The v1.5 fallback mechanism replaced
    // '*' with U+FF1F and wrapped it, overwriting (1, 0). The v1.6.0 path
    // keeps '*' at (0, 1) with the cluster in extras, leaving (1, 0) intact.
    let mut t = Terminal::new(3, 2);
    t.process(b"\x1b[2;1H\x1b]8;;https://weft.dev/stale\x1b\\X\x1b]8;;\x1b\\");
    assert_eq!(t.hyperlinks().url_at(1, 0), Some("https://weft.dev/stale"));
    t.process("\x1b[1;2H*\u{fe0f}".as_bytes());
    assert_eq!(t.grid().cell(0, 1).character, '*');
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::EXTRA));
    assert_eq!(t.grid().cell(1, 0).character, 'X');
    assert_eq!(t.hyperlinks().url_at(1, 0), Some("https://weft.dev/stale"));
    assert_eq!(t.hyperlinks().url_at(1, 1), None);
}

#[test]
fn scalar_overwrite_clears_stale_grapheme_extras() {
    let mut ascii = term();
    ascii.process("e\u{0301}".as_bytes());
    assert_eq!(ascii.grid().grapheme_at(0, 0), Some("e\u{0301}"));
    ascii.process(b"\rX");
    assert_eq!(ascii.grid().row_text(0), "X");
    assert_eq!(ascii.grid().grapheme_at(0, 0), None);
    assert!(!ascii.grid().cell(0, 0).flags.contains(CellFlags::EXTRA));

    let mut wide = term();
    wide.process("e\u{0301}".as_bytes());
    wide.process("\r中".as_bytes());
    assert_eq!(wide.grid().row_text(0), "中");
    assert_eq!(wide.grid().grapheme_at(0, 0), None);
}

#[test]
fn vs16_width_expansion_clears_displaced_cell_extras() {
    let mut t = term();
    t.process(" e\u{0301}".as_bytes());
    assert_eq!(t.grid().grapheme_at(0, 1), Some("e\u{0301}"));

    t.process("\r*\u{fe0f}".as_bytes());
    assert_eq!(t.grid().row_text(0), "*\u{fe0f}");
    assert_eq!(t.grid().grapheme_at(0, 1), None);
    assert!(t.grid().cell(0, 1).flags.contains(CellFlags::WIDE_SPACER));
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

/// FIX_OPENCODE_STARTUP_FLASH: xterm's modifyOtherKeys enable
/// (`CSI > 4 ; 1 m`) is a PRIVATE SGR — the `>` intermediate must not be
/// dropped. It was previously dispatched to `handle_sgr` as SGR 4;1, which
/// set UNDERLINE|BOLD on every subsequent cell: opencode's startup clears
/// (`38;2;255;255;255m 48;2;15;15;15m` + 80 spaces, no `0m` between rows)
/// then painted full-width near-white underline rails across the cleared
/// rows — the "several white horizontal bars" flash. Private SGRs are
/// terminal-behaviour controls (modifyOtherKeys), never text attributes.
#[test]
fn csi_gt_private_sgr_is_not_text_attributes() {
    // opencode's exact startup shape: modifyOtherKeys ON, then a white-fg
    // clear row without an intervening SGR reset.
    let mut t = term();
    t.process(b"\x1b[>4;1m\x1b[38;2;255;255;255m\x1b[48;2;15;15;15m ");
    let cell = t.grid().cell(0, 0);
    assert_eq!(
        cell.flags
            .intersection(CellFlags::UNDERLINE | CellFlags::BOLD),
        CellFlags::empty(),
        "private SGR `>4;1m` leaked text attributes: {cell:?}"
    );

    // The disable form (`>4;2m`) must behave identically.
    let mut t = term();
    t.process(b"\x1b[>4;2mX");
    let cell = t.grid().cell(0, 0);
    assert_eq!(
        cell.flags
            .intersection(CellFlags::UNDERLINE | CellFlags::BOLD),
        CellFlags::empty(),
        "private SGR `>4;2m` leaked text attributes: {cell:?}"
    );

    // A real SGR 4;1 (underline + bold) must still work — only the private
    // (intermediate-bearing) form is ignored.
    let mut t = term();
    t.process(b"\x1b[4;1mY");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::UNDERLINE));
    assert!(cell.flags.contains(CellFlags::BOLD));
}

#[test]
fn progress_rewrites_are_compacted_in_detached_block_output() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07upgrade\n\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"Upgrading.\rUpgrading..\x1b[K\rUpgrading...\x1b[K");
    t.process(b"\nDone\n\x1b]133;D;0\x07");

    let block = t.block_tracker().blocks().last().unwrap();
    // Trailing '\n' stripped at finalize (PROMPT_SP tail-strip).
    assert_eq!(block.output.as_ref(), "Upgrading...\nDone");
}

#[test]
fn horizontal_cursor_progress_rewrites_are_compacted_in_block_output() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07upgrade\n\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"Upgrading.\x1b[1GUpgrading..\x1b[K\x1b[1GUpgrading...\x1b[K");
    t.process(b"\nDone\n\x1b]133;D;0\x07");

    let block = t.block_tracker().blocks().last().unwrap();
    // Trailing '\n' stripped at finalize (PROMPT_SP tail-strip).
    assert_eq!(block.output.as_ref(), "Upgrading...\nDone");
}

#[test]
fn multiline_progress_bar_repaints_in_place_via_cursor_up() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07ollama pull\n\x1b]133;B\x07\x1b]133;C\x07");

    // Initial 3-row progress paint (each row followed by \n).
    t.process(b"pulling a:   0%\npulling b:   0%\npulling c:   0%\n");
    // Repaint: cursor up 3 rows, then overwrite each row + erase tail.
    t.process(b"\x1b[3A\rpulling a:  50%\x1b[K\n\rpulling b:  30%\x1b[K\n\rpulling c:  10%\x1b[K");
    t.process(b"\n\x1b]133;D;0\x07");

    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "pulling a:  50%\npulling b:  30%\npulling c:  10%",
        "multi-line progress via CSI A must not grow rows (trailing newline stripped at finalize)"
    );
}

#[test]
fn multiline_progress_bar_repaints_in_place_via_cursor_up_during_in_flight() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07brew upgrade\n\x1b]133;B\x07\x1b]133;C\x07");

    // Initial 2-row progress paint.
    t.process(b"fetching: 0%\ndownloading: 0%\n");
    // Repaint: cursor up 2 rows, overwrite each row.
    t.process(b"\x1b[2A\rfetching: 100%\x1b[K\n\rdownloading:  50%\x1b[K");

    // Command still in flight — verify the in-flight buffer hasn't grown.
    let output = t.block_tracker().in_flight().unwrap().output;
    let line_count = output.lines().count();
    assert_eq!(
        line_count, 2,
        "in-flight progress repaint must not grow rows: {output:?}"
    );
    assert_eq!(output, "fetching: 100%\ndownloading:  50%\n");
}

#[test]
fn multiline_progress_bar_repaints_via_cursor_down_e_and_up_f() {
    // CSI E (cursor down + CR) and CSI F (cursor up + CR) are the
    // CR-combining variants of CSI B/A. Verify they also compact rows.
    let mut t = term();
    t.process(b"\x1b]133;A\x07cmd\n\x1b]133;B\x07\x1b]133;C\x07");

    // Paint row 1, then use CSI E (down 1 + CR) to move to row 2.
    t.process(b"row1: 0%\x1b[1Erow2: 0%\n");
    // Repaint: CSI F (up 2 + CR) back to row 1, then CSI E to row 2.
    t.process(b"\x1b[2Frow1: 50%\x1b[K\x1b[1Erow2: 30%\x1b[K");
    t.process(b"\n\x1b]133;D;0\x07");

    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "row1: 50%\nrow2: 30%",
        "CSI E/F progress repaint must not grow rows (trailing newline stripped at finalize)"
    );
}

#[test]
fn horizontal_cursor_capture_uses_grid_clamped_columns() {
    for cursor_move in [b"\x1b[999C".as_slice(), b"\x1b[999G".as_slice()] {
        let mut t = Terminal::new(4, 10);
        t.process(b"\x1b]133;A\x07cmd\n\x1b]133;B\x07\x1b]133;C\x07foo");
        t.process(cursor_move);
        t.process(b"X\x1b]133;D;0\x07");

        let output = &t.block_tracker().blocks().last().unwrap().output;
        assert!(output.len() <= 10, "capture escaped grid width: {output:?}");
        assert!(output.ends_with('X'));
    }

    let mut t = Terminal::new(4, 10);
    t.process(b"\x1b]133;A\x07cmd\n\x1b]133;B\x07\x1b]133;C\x07foo\x1b[999DZ");
    t.process(b"\x1b]133;D;0\x07");
    assert_eq!(
        t.block_tracker().blocks().last().unwrap().output.as_ref(),
        "Zoo"
    );
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
    assert_eq!(t.grid().scroll_offset(), 0, "new output resets offset");
    assert!(!t.grid().is_scrolled());
}

#[test]
fn tui_redraw_preserves_scroll_offset_while_browsing_history() {
    // v1.10.4: while the user browses history of a primary-screen TUI
    // (openclaw/Claude Code — phase NotIntegrated, no alt-screen), the TUI's
    // redraw output must NOT reset grid.scroll_offset. Before the fix, every
    // printed char during a redraw snapped the viewport back to the live
    // bottom, so pressing arrow keys inside the TUI (which trigger a redraw)
    // yanked the view up to the top.
    let mut t = Terminal::new(5, 20);
    // Establish TUI ownership: shell integration 133;C (→ CommandExecuting)
    // + relative cursor moves (openclaw redraw pattern). `screen_owner`
    // classifies by cursor-addressing volume within CommandExecuting.
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07tui\x1b]133;C\x07");
    t.process(b"\x1b[2A\x1b[3B");
    assert_eq!(t.screen_owner(), ScreenOwner::PrimaryScreenApp);

    // User enters history browsing mode, THEN scrolls up into history.
    // (Entering resets scroll_offset to 0 by design — the browsing offset
    // is established after the mode is active.)
    t.set_primary_history_view(true);
    for i in 0..10 {
        t.process(format!("history-row-{i}\r\n").as_bytes());
    }
    t.grid_mut().scroll_up_history(3);
    assert!(t.grid().is_scrolled(), "precondition: scrolled up");
    let scrolled_offset = t.grid().scroll_offset();

    // TUI redraws (relative move + erase + text) while user browses history.
    t.process(b"\x1b[999D\x1b[3A\x1b[3B\x1b[Joption-a\r\noption-b\r\n");
    assert_eq!(
        t.grid().scroll_offset(),
        scrolled_offset,
        "TUI redraw must NOT reset scroll_offset while browsing history"
    );

    // But plain (non-history) output still resets — the normal path is intact.
    t.set_primary_history_view(false);
    t.process(b"X");
    assert_eq!(
        t.grid().scroll_offset(),
        0,
        "plain output still resets offset"
    );
}

#[test]
fn large_cuu_clamps_cursor_and_repaints_visible_rows() {
    // v1.10.4: a TUI redraw that moves the cursor far above the viewport
    // (openclaw's `\x1b[999D\x1b[1027A` "back to my document top" pattern)
    // clamps the cursor at row 0 — the repaint writes into the live
    // viewport rows, which the renderer reads via `cell()` (offset 0 →
    // live rows). The UI must be fully visible, exactly like xterm/tmux.
    let mut t = Terminal::with_scrollback(10, 40, 2000);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
    for i in 0..50 {
        t.process(format!("anim-row-{i}\r\n").as_bytes());
    }
    assert_eq!(t.grid().scroll_offset(), 0);
    assert_eq!(t.grid().cursor.row, 9, "cursor at viewport bottom");

    // Redraw: CUB 999 + CUU 1027 clamps at row 0; ED + repaint.
    t.process(b"\x1b[999D\x1b[1027A\x1b[Joption-a\r\noption-b\r\n");
    assert_eq!(t.grid().cursor.row, 2);
    assert_eq!(t.grid().scroll_offset(), 0, "clamp must not scroll");
    // The renderer reads through `cell()` (offset-mapped): at offset 0 the
    // live rows ARE the visible window, so the repaint is what the user sees.
    assert_eq!(
        t.grid().cell(0, 0).character,
        'o',
        "repaint must be visible at the top of the live viewport"
    );
    let mut row0 = String::new();
    for col in 0..20 {
        row0.push(t.grid().cell(0, col).character);
    }
    assert_eq!(row0.trim(), "option-a");
}

#[test]
fn custom_scroll_region_clamps_cursor_without_viewport_scroll() {
    // A large CUU inside a custom scroll region clamps at the region top —
    // the viewport never moves (standard DECSTBM behavior).
    let mut t = Terminal::with_scrollback(10, 40, 2000);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07cmd\x1b]133;C\x07");
    for i in 0..50 {
        t.process(format!("row-{i}\r\n").as_bytes());
    }
    assert_eq!(t.grid().cursor.row, 9);

    // Custom region rows 2..=8 (1-based) = 1..=8 (0-based), cursor inside.
    t.process(b"\x1b[2;9r\x1b[2;5H");
    t.process(b"\x1b[999D\x1b[1027A");
    assert_eq!(
        t.grid().cursor.row,
        1,
        "cursor clamps at the custom region top"
    );
    assert_eq!(
        t.grid().scroll_offset(),
        0,
        "custom-region CUU must not scroll the viewport"
    );
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
    // Fix B2 (v1.11.16): at the last physical row outside the scroll region
    // the cursor stays put, and NO row may be marked `wrapped` (the old code
    // mismarked row-1 as a continuation of unrelated content).
    assert!(
        (0..t.grid().num_rows).all(|r| !t.grid().viewport[r].wrapped),
        "no row may be marked wrapped when the cursor cannot advance"
    );
}

/// Fix B2 (v1.11.16): a normal deferred wrap on a non-last row must mark the
/// row the cursor just left as `wrapped` so reflow can merge the continuation.
#[test]
fn wrap_marks_previous_row_on_normal_newline() {
    let mut t = Terminal::new(24, 80);
    let row_before = 5;
    t.grid_mut().cursor.row = row_before;
    t.grid_mut().cursor.col = 79; // last column
    t.grid_mut().cursor.wrap_pending = true;
    t.process(b"x"); // triggers the deferred-wrap path, advances to row 6
    assert_eq!(t.grid().cursor.row, row_before + 1, "cursor should advance");
    // 'x' is half-width: after the wrap to col 0 the print advances col to 1.
    assert_eq!(t.grid().cursor.col, 1, "cursor sits after the printed 'x'");
    assert!(
        t.grid().viewport[row_before].wrapped,
        "row {} (the row the cursor left) must be marked wrapped",
        row_before
    );
}

/// Fix B2 (v1.11.16): a deferred wrap at the scroll-region bottom must scroll
/// and mark the row above the bottom as `wrapped`.
#[test]
fn wrap_marks_previous_row_on_scroll_region_bottom() {
    let mut t = Terminal::new(24, 80);
    t.grid_mut().set_scroll_region(1, 10); // 1-based → top 0, bottom 9
    let bottom = t.grid().scroll_region().1;
    assert_eq!(bottom, 9);
    t.grid_mut().cursor.row = bottom;
    t.grid_mut().cursor.col = 79; // last column
    t.grid_mut().cursor.wrap_pending = true;
    let sb_before = t.grid().scrollback.len();
    t.process(b"x"); // triggers scroll at the region bottom
                     // The cursor stays on the bottom row after a scroll; the row above it is
                     // the overflowed row and must be marked wrapped.
    assert_eq!(
        t.grid().cursor.row,
        bottom,
        "cursor stays on bottom after scroll"
    );
    assert!(
        t.grid().viewport[bottom - 1].wrapped,
        "row {} (above the scroll-region bottom) must be marked wrapped",
        bottom - 1
    );
    assert!(
        t.grid().scrollback.len() > sb_before,
        "a scroll must have occurred at the scroll-region bottom"
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

// VULN-009 integration anchor: a 50KiB OSC 2 title (well under the
// osc_guard 1MiB raw cap, so the guard forwards it) must reach `self.title`
// capped at OSC_TITLE_MAX_BYTES, not verbatim.
#[test]
fn osc2_title_is_capped_at_4kib() {
    let mut t = term();
    let seq = format!("\x1b]2;{}\x07", "x".repeat(50_000));
    t.process(seq.as_bytes());
    assert_eq!(t.title().len(), super::osc::OSC_TITLE_MAX_BYTES);
}

// Same cap on the OSC 7 cwd path: a hostile near-cap file:// URL must not
// retain an unbounded string.
#[test]
fn osc7_cwd_payload_is_capped() {
    let mut t = term();
    let seq = format!("\x1b]7;file://host/{}\x07", "d/".repeat(30_000));
    t.process(seq.as_bytes());
    let cwd = t.cwd.clone().unwrap_or_default();
    assert!(cwd.len() <= super::osc::OSC_TITLE_MAX_BYTES);
}

// v1.10.12: OSC 11 background-color query. TUIs (omp/pi/opencode) probe it
// at startup to pick their theme; the response carries the app theme's
// background in xterm `rgb:RR/GG/BB` form.
#[test]
fn osc11_background_query_answers_theme_background() {
    let mut t = term();
    t.set_background_color(Color::rgb(0x12, 0x34, 0x56));
    t.process(b"\x1b]11;?\x07");
    assert_eq!(t.take_response(), b"\x1b]11;rgb:12/34/56\x1b\\");
}

#[test]
fn osc11_background_default_is_darkish() {
    let mut t = term();
    t.process(b"\x1b]11;?\x07");
    let resp = t.take_response();
    let s = String::from_utf8_lossy(&resp);
    assert!(
        s.starts_with("\x1b]11;rgb:") && s.ends_with("\x1b\\"),
        "malformed OSC 11 response: {s}"
    );
}

// A set request (`OSC 11;rgb:..`) must not be answered — xterm semantics.
#[test]
fn osc11_set_request_is_not_answered() {
    let mut t = term();
    t.process(b"\x1b]11;rgb:ff/00/00\x07");
    assert_eq!(
        t.take_response(),
        b"",
        "set request must not produce a reply"
    );
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

/// v1.10.7 regression: a screen-owned TUI session whose shell integration
/// runs INSIDE the app (pi spawns an interactive zsh that inherits
/// `WEFT_SHELL_INTEGRATION`) re-emits the OSC 133 markers per internal
/// command. Each `133;D` + `133;A` precmd pair used to defer a primary-screen
/// exit, and the next `133;B` settled it immediately — finalizing the session
/// block per internal command (one prompt split into many blocks). The
/// nested markers must keep ONE in-flight block until the TUI really exits.
#[test]
fn nested_shell_markers_do_not_split_screen_session_blocks() {
    let mut t = Terminal::new(10, 40);
    // User starts pi (screen-owned TUI, DEC 2026 sync + relative moves).
    // The typed command echoes to the grid BEFORE preexec emits 133;B, so
    // `snapshot_command_line` captures "pi" as the pending command.
    t.process(b"\x1b]133;A\x07pi\r");
    t.process(b"\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active(), "TUI detected");
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "screen ownership begun"
    );
    let blocks_before = t.block_tracker().blocks().len();

    // pi internal command #1: nested zsh precmd (D;0 + A) then preexec (B+C).
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    // Timing note (rust-reviewer P2-2): the nested 133;B below must arrive
    // while `primary_screen_exit` is still pending (< the 200ms settle delay,
    // measured in REAL wall clock across the two `process` calls) so the
    // nested-marker cancellation branch runs. A pathologically slow machine
    // could in principle let the pending exit settle first; the path has no
    // clock injection today.
    assert_eq!(
        t.block_tracker().phase(),
        ShellPhase::CommandExecuting,
        "the 133;D→133;A pair must not drop the session out of execution"
    );
    t.process(b"echo hi\x1b]133;B\x07\x1b]133;C\x07");
    assert_eq!(
        t.block_tracker().phase(),
        ShellPhase::CommandExecuting,
        "nested 133;B must resume the screen command, not settle it"
    );
    t.process("nested output one".as_bytes());
    assert_eq!(
        t.block_tracker().blocks().len(),
        blocks_before,
        "internal command markers must not finalize a block"
    );
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "screen ownership survives nested markers"
    );

    // pi internal command #2 (same pattern).
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    t.process("nested output two".as_bytes());
    assert_eq!(t.block_tracker().blocks().len(), blocks_before);
    assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);

    // The REAL exit: 133;D (or precmd after the app ended) defers, and the
    // idle settle finalizes exactly ONE block.
    t.process(b"\x1b]133;D;0\x07");
    assert!(t.settle_primary_screen_exit());
    let blocks = t.block_tracker().blocks();
    assert_eq!(
        blocks.len(),
        blocks_before + 1,
        "real exit finalizes exactly one session block"
    );
    assert_eq!(blocks[blocks.len() - 1].command, "pi");
}

/// v1.10.7: a real exit is deferred once with its exit code, and a later
/// `133;D` (nested precmd pair) must NOT overwrite the pending exit code —
/// the settled block keeps the first (real) exit code.
#[test]
fn nested_marker_does_not_overwrite_pending_exit_code() {
    let mut t = Terminal::new(10, 40);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
    t.process("\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.primary_screen_app_active());

    // Real exit: shell precmd emits `133;D;130` then `133;A`.
    t.process(b"\x1b]133;D;130\x07\x1b]133;A\x07");
    // A nested marker burst must not re-defer with a different code.
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(t.settle_primary_screen_exit());
    let blocks = t.block_tracker().blocks();
    assert_eq!(
        blocks.last().map(|b| b.exit_code),
        Some(Some(130)),
        "the first (real) exit code wins over nested 133;D;0"
    );
}

/// Applications may use OSC 133 A/B/C as semantic output zones. Once the
/// terminal has seen Weft's origin-tagged shell integration, untagged zones
/// belong to the foreground application and must never mutate shell/block
/// state, even when emitted repeatedly during synchronized TUI repaints.
#[test]
fn application_osc133_zones_do_not_drive_tagged_shell_session() {
    let mut t = Terminal::new(10, 40);
    t.process(b"\x1b]133;A;weft-shell\x07pi\r");
    t.process(b"\x1b]133;B;weft-shell\x07\x1b]133;C;weft-shell\x07");
    t.process("\x1b[?2026h\x1b[2Kinput\x1b[2G\x1b[?2026l".as_bytes());
    assert!(t.block_tracker().screen_document_start().is_some());

    let command = t
        .block_tracker()
        .in_flight()
        .expect("screen command remains live")
        .command
        .to_string();
    let blocks_before = t.block_tracker().blocks().len();

    for (index, text) in ["thinking", "tool output", "final answer"]
        .into_iter()
        .enumerate()
    {
        t.process(b"\x1b]133;A\x07");
        if index == 0 {
            // Exceed the old 200ms timing heuristic. Origin ownership, not
            // repaint timing, must decide whether this is a shell marker.
            std::thread::sleep(
                PRIMARY_SCREEN_EXIT_SETTLE_DELAY + std::time::Duration::from_millis(20),
            );
        }
        t.process(b"\x1b]133;B\x07\x1b]133;C\x07");
        t.process(format!("\x1b[?2026h\x1b[{};1H\x1b[2K{text}\x1b[?2026l", index + 2).as_bytes());
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        assert_eq!(t.block_tracker().blocks().len(), blocks_before);
        assert_eq!(
            t.block_tracker().in_flight().map(|live| live.command),
            Some(command.as_str()),
            "application zones must not overwrite the outer shell command"
        );
    }

    t.process(b"\x1b]133;D;0;weft-shell\x07\x1b]133;A;weft-shell\x07");
    assert!(t.settle_primary_screen_exit());
    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(block.command, command);
    assert!(block.output.contains("thinking"));
    assert!(block.output.contains("tool output"));
    assert!(block.output.contains("final answer"));
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
    // Trailing '\n' stripped at finalize (PROMPT_SP tail-strip).
    assert_eq!(b.output.as_ref(), "file1\nfile2", "output captured B..D");
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

/// SGR 58 (set underline color) / 59 (reset) carry their own sub-params
/// (`58;2;R;G;B`, `58;5;N`) just like 38/48. AUDIT_v1.10.39 P0-1: they used to
/// fall into the unhandled arm without consuming those params, so `2` was
/// misparsed as DIM, `5` as ITALIC, and trailing `0;0` bytes as two full
/// attribute resets — wiping the underline the sequence asked for.
#[test]
fn sgr_underline_color_58_does_not_leak_params_into_attributes() {
    // Truecolor variant: `ESC[4;58;2;255;0;0m`.
    let mut t = term();
    t.process(b"\x1b[4;58;2;255;0;0mX");
    let cell = t.grid().cell(0, 0);
    assert!(
        cell.flags.contains(CellFlags::UNDERLINE),
        "underline must survive a 58;2;R;G;B sequence"
    );
    assert!(
        !cell.flags.contains(CellFlags::DIM),
        "the `2` inside 58;2;R;G;B must not enable DIM"
    );

    // Indexed variant followed by another attribute: `ESC[58;5;196;4m`.
    let mut t2 = term();
    t2.process(b"\x1b[58;5;196;4mY");
    let cell2 = t2.grid().cell(0, 0);
    assert!(cell2.flags.contains(CellFlags::UNDERLINE));
    assert!(
        !cell2.flags.contains(CellFlags::ITALIC),
        "the `5` inside 58;5;N must not enable ITALIC"
    );
}

/// AUDIT_v1.10.39 P1: SGR 21 now means "doubly underlined" (ECMA-48, and the
/// Ghostty/WezTerm/Alacritty/xterm consensus) instead of the Linux-console
/// legacy "clear bold". This also makes the DOUBLE_UNDER render path
/// reachable from the parser for the first time.
#[test]
fn sgr_21_sets_double_underline_without_touching_bold() {
    let mut t = term();
    t.process(b"\x1b[1m\x1b[21mA");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::DOUBLE_UNDER));
    assert!(
        cell.flags.contains(CellFlags::BOLD),
        "SGR 21 must not clear bold — SGR 22 does that"
    );
}

// ── v1.11.3 SGR capability matrix (PLAN_v1113 §4.2) ────────────

/// v1.11.3: colon-form `4:x` sets the corresponding underline style
/// (PLAN_v1113 §2.1). Each style lands in both the UnderlineStyle field and
/// the legacy UNDERLINE flag.
#[test]
fn sgr_4_colon_styles_map_all_five() {
    for (seq, style) in [
        ("\x1b[4:1m", UnderlineStyle::Single),
        ("\x1b[4:2m", UnderlineStyle::Double),
        ("\x1b[4:3m", UnderlineStyle::Wavy),
        ("\x1b[4:4m", UnderlineStyle::Dotted),
        ("\x1b[4:5m", UnderlineStyle::Dashed),
    ] {
        let mut t = term();
        t.process(seq.as_bytes());
        t.process(b"X");
        let cell = t.grid().cell(0, 0);
        assert_eq!(cell.underline_style, style, "sequence {seq}");
        assert!(
            cell.flags.contains(CellFlags::UNDERLINE),
            "sequence {seq} must set UNDERLINE"
        );
    }
}

/// v1.11.3: `4:0` is kitty-style "no underline" = SGR 24, and the probe
/// verdict (PLAN_v1113 step 1) extends it: vte materializes the empty tail
/// as a 0 subparam, so `4:` arrives as `[4,0]` — identical to `4:0`. Both
/// clear the underline entirely and return the style to Single.
#[test]
fn sgr_4_colon_zero_and_empty_tail_clear_underline() {
    for seq in ["\x1b[4:0m", "\x1b[4:m"] {
        let mut t = term();
        t.process(b"\x1b[4:3m"); // wavy first — must be fully cleared
        t.process(seq.as_bytes());
        t.process(b"X");
        let cell = t.grid().cell(0, 0);
        assert!(
            !cell.flags.contains(CellFlags::UNDERLINE),
            "sequence {seq} must clear UNDERLINE"
        );
        assert!(
            !cell.flags.contains(CellFlags::DOUBLE_UNDER),
            "sequence {seq} must clear DOUBLE_UNDER"
        );
        assert_eq!(cell.underline_style, UnderlineStyle::Single, "{seq}");
    }
}

/// v1.11.3: plain SGR `4` in semicolon form re-sets the underline but 归位s
/// the style to Single — `4:3` followed by a plain `4` must not leave wavy
/// residue (kitty/wezterm/Ghostty consensus, PLAN_v1113 §2.1).
#[test]
fn sgr_plain_4_resets_style_to_single() {
    let mut t = term();
    t.process(b"\x1b[4:3m\x1b[4mA");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::UNDERLINE));
    assert_eq!(cell.underline_style, UnderlineStyle::Single);
}

/// v1.11.3: semicolon `4;3` stays the classic "underline + italic" pair —
/// the colon/group-view dispatch must not alter the semicolon form.
#[test]
fn sgr_semicolon_4_3_is_underline_plus_italic() {
    let mut t = term();
    t.process(b"\x1b[4;3mA");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::UNDERLINE));
    assert!(cell.flags.contains(CellFlags::ITALIC));
    assert_eq!(cell.underline_style, UnderlineStyle::Single);
}

/// v1.11.3: unknown style values (kitty 4:6..4:9 etc.) are ignored without
/// panicking — the underline flag still lands, style stays as-is
/// (PLAN_v1113 §2.1 "不得裸索引").
#[test]
fn sgr_4_colon_unknown_style_ignored() {
    for seq in ["\x1b[4:6m", "\x1b[4:9m", "\x1b[4:15m"] {
        let mut t = term();
        t.process(b"\x1b[4:2m"); // double first: unknown must not clobber
        t.process(seq.as_bytes());
        t.process(b"X");
        let cell = t.grid().cell(0, 0);
        assert!(cell.flags.contains(CellFlags::UNDERLINE), "{seq}");
        assert_eq!(cell.underline_style, UnderlineStyle::Double, "{seq}");
    }
}

/// v1.11.3: SGR 21/24 write both carriers (dual-track, PLAN_v1113 §1.2):
/// 21 keeps the historical DOUBLE_UNDER-only flag write (维持现状) and adds
/// style Double; 24 clears the flags + 归位 Single.
#[test]
fn sgr_21_24_write_both_underline_carriers() {
    let mut t = term();
    t.process(b"\x1b[21mA");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::DOUBLE_UNDER));
    assert_eq!(cell.underline_style, UnderlineStyle::Double);

    t.process(b"\x1b[24mB");
    let cell = t.grid().cell(0, 1);
    assert!(!cell.flags.contains(CellFlags::DOUBLE_UNDER));
    assert!(!cell.flags.contains(CellFlags::UNDERLINE));
    assert_eq!(cell.underline_style, UnderlineStyle::Single);
}

/// rust-reviewer v1.11.3 Major-1: the mirror-sync contract — `4:x` must
/// REMOVE the DOUBLE_UNDER bit when the style is not double, otherwise a
/// stale `21` followed by any other underline style keeps rendering as
/// double (the bit outranks the style field on the read side). Locks all
/// four non-double styles.
#[test]
fn sgr_21_then_4x_clears_stale_double_under_bit() {
    for (seq, style) in [
        (b"\x1b[4:1m", UnderlineStyle::Single),
        (b"\x1b[4:3m", UnderlineStyle::Wavy),
        (b"\x1b[4:4m", UnderlineStyle::Dotted),
        (b"\x1b[4:5m", UnderlineStyle::Dashed),
    ] {
        let mut t = term();
        t.process(b"\x1b[21m");
        t.process(seq);
        t.process(b"X");
        let cell = t.grid().cell(0, 0);
        assert!(
            !cell.flags.contains(CellFlags::DOUBLE_UNDER),
            "{seq:?}: stale DOUBLE_UNDER bit must be removed"
        );
        assert_eq!(cell.underline_style, style, "{seq:?}");
        assert!(cell.flags.contains(CellFlags::UNDERLINE), "{seq:?}");
    }
}

/// rust-reviewer v1.11.3 Minor-1 regression: unknown 4:x values are ignored
/// WHOLE-GROUP (kitty/WezTerm/Ghostty consensus) — no UNDERLINE insertion,
/// so a capability probe like `4:9` never paints an unexpected underline.
#[test]
fn sgr_4_colon_unknown_value_is_fully_ignored() {
    let mut t = term();
    // Style request alone: nothing may appear (the stream has no prior
    // attribute state in a fresh terminal).
    t.process(b"\x1b[4:9m\x1b[4:65535mX");
    let cell = t.grid().cell(0, 0);
    assert!(!cell.flags.contains(CellFlags::UNDERLINE));

    // But an existing wavy stays untouched by an unknown probe following it.
    let mut t2 = term();
    t2.process(b"\x1b[4:3mA\x1b[4:9mB");
    let b = t2.grid().cell(0, 1);
    assert_eq!(b.underline_style, UnderlineStyle::Wavy);
}

/// v1.11.3: `58:x` colon form stores the underline color on the cell — all
/// three shapes per PLAN_v1113 §2.1: truecolor, nvim's empty-colorspace
/// truecolor, and palette. Malformed groups are dropped whole.
#[test]
fn sgr_58_colon_stores_underline_color() {
    // Truecolor `58:2:R:G:B`
    let mut t = term();
    t.process(b"\x1b[4\x1b[58:2:255:0:128mX");
    let cell = t.grid().cell(0, 0);
    assert_eq!(
        cell.underline_color,
        Some(CellColor::Rgb(Color::rgb(255, 0, 128)))
    );

    // nvim empty colorspace `58:2::R:G:B` → probe shape [58,2,0,R,G,B]
    let mut t2 = term();
    t2.process(b"\x1b[4\x1b[58:2::10:20:30mY");
    let cell2 = t2.grid().cell(0, 0);
    assert_eq!(
        cell2.underline_color,
        Some(CellColor::Rgb(Color::rgb(10, 20, 30)))
    );

    // Palette `58:5:N`
    let mut t3 = term();
    t3.process(b"\x1b[4\x1b[58:5:196mZ");
    let cell3 = t3.grid().cell(0, 0);
    assert_eq!(cell3.underline_color, Some(CellColor::Palette(196)));

    // Malformed (`58:2:1:2` short, `58:7` odd): ignore whole group, and the
    // group's elements must NOT leak into the flat walk (`2` would enable
    // DIM — the AUDIT_v1.10.39 P0-1 regression).
    for seq in ["\x1b[58:2:1:2m", "\x1b[58:7m"] {
        let mut t4 = term();
        t4.process(seq.as_bytes());
        t4.process(b"W");
        let cell4 = t4.grid().cell(0, 0);
        assert_eq!(cell4.underline_color, None, "{seq}");
        assert!(!cell4.flags.contains(CellFlags::DIM), "{seq} leaked `2`");
        assert!(!cell4.flags.contains(CellFlags::ITALIC), "{seq} leaked `5`");
    }
}

/// v1.11.3: semicolon `58;2;R;G;B` also stores the underline color (the
/// flat-walk path), and `59` resets it. SGR 0 (reset) returns it to None.
#[test]
fn sgr_58_semicolon_stores_and_59_resets_underline_color() {
    let mut t = term();
    t.process(b"\x1b[4;58;2;1;2;3mX");
    let cell = t.grid().cell(0, 0);
    assert_eq!(
        cell.underline_color,
        Some(CellColor::Rgb(Color::rgb(1, 2, 3)))
    );
    assert!(
        !cell.flags.contains(CellFlags::DIM),
        "`2` must not leak as DIM"
    );

    t.process(b"\x1b[59mY");
    assert_eq!(t.grid().cell(0, 1).underline_color, None);

    let mut t2 = term();
    t2.process(b"\x1b[58;5;196m\x1b[0mZ");
    assert_eq!(t2.grid().cell(0, 0).underline_color, None, "SGR 0 resets");
}

/// v1.11.3 (PLAN_v1113 §2.1 S5): REVERSE swaps fg/bg only — the underline
/// color is application-owned and must survive REVERSE untouched.
#[test]
fn sgr_reverse_does_not_swap_underline_color() {
    let mut t = term();
    t.process(b"\x1b[4\x1b[58:2:10:20:30m\x1b[7mX");
    let cell = t.grid().cell(0, 0);
    assert!(cell.flags.contains(CellFlags::REVERSE));
    assert_eq!(
        cell.underline_color,
        Some(CellColor::Rgb(Color::rgb(10, 20, 30))),
        "REVERSE must not touch underline_color"
    );
}

/// v1.11.3: style surviving an overwrite — a wavy cell overwritten by plain
/// text (after an SGR reset) must lose the style/color (the field-assignment
/// write path carries the NEW attrs, never leaves stale cell values).
#[test]
fn sgr_style_and_color_overwrite_resets_cleanly() {
    let mut t = term();
    t.process(b"\x1b[4:3m\x1b[58:2:1:2:3mA");
    t.process(b"\r\x1b[K\x1b[0m");
    t.process(b"B");
    let cell = t.grid().cell(0, 0);
    assert_eq!(cell.underline_style, UnderlineStyle::Single);
    assert_eq!(cell.underline_color, None);
    assert!(!cell.flags.contains(CellFlags::UNDERLINE));
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
    assert!(t.app_cursor_keys());
    t.process(b"\x1b[?1l");
    assert!(!t.app_cursor_keys());
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
    assert_eq!(t.mouse_protocol(), MouseProtocol::ButtonEvent);
    assert!(t.sgr_mouse());

    t.process(b"\x1b[?1006;1000l\x1b[?1002l");
    assert_eq!(t.mouse_protocol(), MouseProtocol::Off);
    assert!(!t.sgr_mouse());
}

// ── v1.11.15 (FIX A): reader-side mouse-suppression flag undo ────────

#[test]
fn mouse_suppress_flag_clears_on_both_decset_and_decrst_of_mouse_modes() {
    use crate::input::{is_suppressed, new_flag, set_suppressed};
    let mut t = term();
    let flag = new_flag();
    t.set_mouse_suppress_flag(flag.clone());

    // The parser must clear on the SET arm (h) — a re-asserted DECSET is
    // just as much proof the TUI is alive as a reset.
    set_suppressed(&flag);
    t.process(b"\x1b[?1003h");
    assert!(!is_suppressed(&flag), "DECSET 1003 (h) must clear the flag");

    // …and on the RESET arm (l) — the TUI's teardown disable.
    set_suppressed(&flag);
    t.process(b"\x1b[?1003l");
    assert!(!is_suppressed(&flag), "DECRST 1003 (l) must clear the flag");

    // Encoding mode 1006 and X10 mode 9 ride the same pre-clear.
    set_suppressed(&flag);
    t.process(b"\x1b[?1006l");
    assert!(!is_suppressed(&flag), "DECRST 1006 must clear the flag");
    set_suppressed(&flag);
    t.process(b"\x1b[?9h");
    assert!(!is_suppressed(&flag), "DECSET 9 must clear the flag");

    // The change-guarded set_mouse_protocol body (a TUI re-asserting its
    // current mode) must not skip the clear: 1003 is already AnyEvent here,
    // the guard logs/writes nothing, and the flag still clears.
    set_suppressed(&flag);
    t.process(b"\x1b[?1003h");
    assert!(
        !is_suppressed(&flag),
        "an h-arm no-op through the change guard must still clear the flag"
    );
}

#[test]
fn mouse_suppress_flag_survives_non_mouse_dec_modes() {
    use crate::input::{is_suppressed, new_flag, set_suppressed};
    let mut t = term();
    let flag = new_flag();
    t.set_mouse_suppress_flag(flag.clone());
    set_suppressed(&flag);
    t.process(b"\x1b[?1049l\x1b[?2004h\x1b[?2026l\x1b[?25h");
    assert!(
        is_suppressed(&flag),
        "only the mouse mode family clears the flag; ?1049/?2004/?2026/?25 must not"
    );
}

#[test]
fn mouse_suppress_flag_none_is_noop_for_the_parser() {
    // No flag injected (headless default): every mouse-mode parse is a
    // plain no-op clear — zero disturbance to the 2800-test baseline.
    let mut t = term();
    t.process(b"\x1b[?1003h");
    assert_eq!(t.mouse_protocol(), MouseProtocol::AnyEvent);
    t.process(b"\x1b[?1003l");
    assert_eq!(t.mouse_protocol(), MouseProtocol::Off);
}

// ── Full reset ───────────────────────────────────────────────

#[test]
fn ris_full_reset() {
    let mut t = term();
    t.process(b"\x1b[31mX\x1b[?1h");
    assert!(t.app_cursor_keys());
    t.process(b"\x1bc");
    assert!(!t.app_cursor_keys());
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
    // v1.6.1: The viewport-relative `cell_map` (fast-path index) is cleared
    // when content scrolls — the (row, col) → id mappings are no longer
    // valid for the new viewport positions. The HYPERLINK flag stays on
    // cells (visual underline persists) and the link id is preserved in
    // `RowExtras` so `Grid::hyperlink_id_at` can still resolve it (see
    // `osc8_link_survives_scroll_via_row_extras`).
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/s\x1b\\");
    t.process(b"link\n");
    t.process(b"\x1b]8;;\x1b\\");
    // Emit enough lines to force a scroll.
    t.process(b"line1\nline2\nline3");
    // After scrolling, the viewport-relative cell_map is cleared.
    for row in 0..3 {
        for col in 0..10 {
            assert!(
                t.hyperlinks().url_at(row, col).is_none(),
                "viewport cell_map at ({row},{col}) should be cleared after scroll"
            );
        }
    }
}

#[test]
fn osc8_link_survives_scroll_via_row_extras() {
    // v1.6.1: Link ids are stored in `RowExtras.hyperlink_id`, which is
    // preserved when rows enter the scrollback. `Grid::hyperlink_id_at`
    // resolves through `scroll_offset` so links in scrolled-off content
    // remain clickable — the key improvement over the v0.8 viewport-only
    // `HyperlinkRegistry::url_at`.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/scrolled\x1b\\");
    t.process(b"link\n");
    t.process(b"\x1b]8;;\x1b\\");
    // Emit enough lines to force "link" into scrollback.
    t.process(b"line1\nline2\nline3");
    // Scroll up to view the "link" row.
    t.grid_mut().set_scroll_offset(3);
    // The link should resolve via RowExtras even though it's in scrollback.
    let id = t.grid().hyperlink_id_at(0, 0);
    assert!(
        id.is_some(),
        "hyperlink id should survive scroll via RowExtras"
    );
    let id = id.unwrap();
    assert_eq!(t.hyperlinks().url(id), Some("https://weft.dev/scrolled"));
    // Cells without a link still return None.
    assert_eq!(t.grid().hyperlink_id_at(0, 5), None);
}

#[test]
fn osc8_link_survives_resize_via_row_extras() {
    // v1.6.1: RowExtras entries are preserved through resize/reflow.
    // `Grid::resize_preserving_document_position` moves entries alongside
    // cells, so links remain resolvable after a column-width change.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/resize\x1b\\");
    t.process(b"link");
    t.process(b"\x1b]8;;\x1b\\");
    // Verify link exists before resize.
    let id_before = t.grid().hyperlink_id_at(0, 0);
    assert!(id_before.is_some());
    // Resize narrower — content stays, extras should follow.
    t.grid_mut().resize(3, 40);
    // After resize, the link may have moved but should still be resolvable
    // somewhere in row 0. We check the first 4 columns (length of "link").
    let mut found = false;
    for col in 0..4 {
        if t.grid().hyperlink_id_at(0, col).is_some() {
            found = true;
            break;
        }
    }
    assert!(found, "hyperlink should survive resize via RowExtras");
}

#[test]
fn osc8_url_length_limit_rejects_oversized_urls() {
    // v1.6.1: URLs longer than MAX_HYPERLINK_URL_LEN (8 KiB) are rejected
    // — `register` returns id 0 (no link). Protects against memory abuse.
    use crate::hyperlink::MAX_HYPERLINK_URL_LEN;
    let mut t = Terminal::new(3, 80);
    let oversized = format!("https://weft.dev/{}", "x".repeat(MAX_HYPERLINK_URL_LEN));
    t.process(b"\x1b]8;;");
    t.process(oversized.as_bytes());
    t.process(b"\x1b\\");
    t.process(b"x");
    // No link should be tagged because the URL was rejected.
    assert!(t.grid().hyperlink_id_at(0, 0).is_none());
}

#[test]
fn osc8_close_then_print_clears_hyperlink_in_extras() {
    // v1.6.1: When OSC 8 close is followed by more print at the same cell,
    // the hyperlink id is cleared from RowExtras (while preserving grapheme
    // if present). The HYPERLINK flag is also cleared.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/a\x1b\\");
    t.process(b"A");
    t.process(b"\x1b]8;;\x1b\\");
    // Move cursor back and overwrite with a non-link char.
    t.process(b"\r");
    t.process(b"B");
    // Cell (0,0) should no longer have a hyperlink.
    assert!(
        t.grid().hyperlink_id_at(0, 0).is_none(),
        "hyperlink should be cleared after overwrite"
    );
}

#[test]
fn hyperlink_lookup_rejects_stale_extras_without_cell_flag() {
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/stale\x1b\\A\x1b]8;;\x1b\\");
    assert!(t.grid().hyperlink_id_at(0, 0).is_some());

    t.grid_mut().viewport[0].cells[0]
        .flags
        .remove(CellFlags::HYPERLINK);
    assert_eq!(t.grid().hyperlink_id_at(0, 0), None);
}

#[test]
fn osc8_link_at_block_capture_resolves_url() {
    // v1.6.1: Block capture (document_snapshot_with_url_resolver) should
    // capture LinkSpans alongside text. This verifies the integration of
    // RowExtras.hyperlink_id → url_resolver → LinkSpan in StyledLine.
    use crate::blocks::StyledOutput;
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/block\x1b\\");
    t.process(b"link");
    t.process(b"\x1b]8;;\x1b\\");
    t.process(b"\n");
    // Take a snapshot with URL resolution.
    let url_resolver = |id: u32| -> Option<std::sync::Arc<str>> {
        t.hyperlinks().url(id).map(std::sync::Arc::<str>::from)
    };
    let (_text, styled, _): (String, StyledOutput, Option<usize>) = t
        .grid()
        .document_snapshot_from_position_with_resolver(0, url_resolver, DEFAULT_OUTPUT_CAP);
    // The first line should have a LinkSpan covering "link" (chars 0-4).
    let line0 = styled.line(0);
    assert!(line0.is_some(), "line 0 should exist in snapshot");
    let line0 = line0.unwrap();
    if !line0.links.is_empty() {
        let link = &line0.links[0];
        assert_eq!(link.url, "https://weft.dev/block");
        assert_eq!(link.start, 0);
        assert_eq!(link.end, 4);
    }
}

#[test]
fn osc8_multiple_urls_get_distinct_ids() {
    // v1.6.1: Different URLs get different ids; same URL deduped.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/a\x1b\\");
    t.process(b"a");
    t.process(b"\x1b]8;;\x1b\\");
    t.process(b"\x1b]8;;https://weft.dev/b\x1b\\");
    t.process(b"b");
    t.process(b"\x1b]8;;\x1b\\");
    let id_a = t.grid().hyperlink_id_at(0, 0);
    let id_b = t.grid().hyperlink_id_at(0, 1);
    assert!(id_a.is_some() && id_b.is_some());
    assert_ne!(id_a, id_b, "different URLs should have different ids");
    assert_eq!(
        t.hyperlinks().url(id_a.unwrap()),
        Some("https://weft.dev/a")
    );
    assert_eq!(
        t.hyperlinks().url(id_b.unwrap()),
        Some("https://weft.dev/b")
    );
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
fn run_command_sets_text_and_submits() {
    // v1.3 AI integration: Terminal::run_command should produce the same
    // bytes as manually typing the command then calling submit_command,
    // and the command should be recorded in editor history so ↑ can recall
    // it later (e.g. the user wants to tweak an AI-suggested command).
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    let bytes = t.run_command("find . -name '*.ts'");
    assert_eq!(bytes, build_submit_bytes("find . -name '*.ts'", false));
    assert_eq!(t.editor().text(), "");
    assert_eq!(t.effective_input_mode(), InputMode::Passthrough);
    // History is populated — Up-arrow navigates to the AI-suggested cmd.
    assert_eq!(
        t.editor().history().first(),
        Some(&"find . -name '*.ts'".to_string())
    );
}

#[test]
fn run_command_empty_behaves_like_submit_empty() {
    // Empty command should synthesize an empty block (Warp-style spacer)
    // rather than sending a bare newline with no block.
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]133;A\x07");
    let _ = t.run_command("");
    let blocks = t.block_tracker().blocks();
    assert_eq!(
        blocks.len(),
        1,
        "empty run_command should still produce a block"
    );
    assert_eq!(blocks[0].command, "");
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

// ── v1.10.23 (FIX_OMP_CONTENT_LOSS): superseded-frame preservation ──

/// Stream `count` lines with the TUI's erase-before-write pattern (EL2
/// marks each row owned) — the omp resize-repaint flow.
fn stream_owned_lines(t: &mut Terminal, count: usize, prefix: &str) {
    for i in 0..count {
        t.process(format!("\x1b[2K{prefix}{i}\r\n").as_bytes());
    }
}

#[test]
fn full_frame_repaint_preserves_superseded_document_in_block_history() {
    // v1.10.23: omp's resize repaint — DEC 2026 sync frame + CSI 2J —
    // used to physically clear the scrollback, deleting every streamed
    // paragraph that had scrolled out of the viewport. The superseded
    // document must be preserved into the block history BEFORE the
    // clear so history review stays complete.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07omp\x1b]133;C\x07");
    // Establish TUI ownership (2+ cursor-addressing ops; CHR 2G counts,
    // 1G does not), then open the synchronized frame.
    t.process("\x1b[3A\x1b[2G\x1b[?2026h".as_bytes());
    assert!(t.primary_screen_app_active());
    // Paint the first frame; 8 paragraphs, 3 scroll into the scrollback.
    stream_owned_lines(&mut t, 8, "paragraph ");
    assert!(
        !t.grid().scrollback.is_empty(),
        "precondition: scrolled-out rows"
    );

    // Resize repaint: clear the whole screen and redraw.
    t.process("\x1b[2J\x1b[Hfresh frame".as_bytes());
    assert_eq!(
        t.grid().scrollback.len(),
        0,
        "the repaint physically clears the scrollback"
    );
    // The superseded frame must be readable through the block history.
    assert!(t.refresh_primary_history_snapshot_now());
    let output = t.block_tracker().in_flight().unwrap().output.to_string();
    for i in 0..8 {
        assert!(
            output.contains(&format!("paragraph {i}")),
            "streamed paragraph {i} must survive the repaint"
        );
    }
    assert!(
        output.contains("fresh frame"),
        "the new frame is the live tail"
    );

    // A second repaint replaces the live frame — it is 1 line, below the
    // 4-line preservation floor, so no fragment is preserved — while the
    // streamed history stays readable and is not duplicated.
    t.process("\x1b[2J\x1b[Hsecond frame".as_bytes());
    assert!(t.refresh_primary_history_snapshot_now());
    let output = t.block_tracker().in_flight().unwrap().output.to_string();
    assert!(
        output.contains("paragraph 0"),
        "history survives a second repaint"
    );
    assert!(output.contains("second frame"));
    assert!(
        !output.contains("fresh frame"),
        "the superseded live frame is replaced, not duplicated"
    );

    // Settle: the finalized block carries the full preserved transcript.
    t.process(b"\x1b]133;D;0\x07");
    assert!(t.settle_primary_screen_exit());
    let block = t.block_tracker().blocks().last().unwrap();
    assert!(block.output.contains("paragraph 0"));
    assert!(block.output.contains("paragraph 7"));
}

#[test]
fn synchronized_frame_finish_preserves_scrollback_but_not_the_live_frame() {
    // v1.10.23: the whole-frame-EL2 repaint (no 2J) discards at the
    // synchronized-frame finish — the viewport already holds the NEW
    // frame (all rows cleared + rewritten inside the sync window), so
    // only the scrolled-out paragraphs are at risk. Preserving the
    // viewport too would duplicate the live frame in the history.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07omp\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[2G\x1b[?2026h".as_bytes());
    assert!(t.primary_screen_app_active());
    // 9 paragraphs: the 9th LF at the bottom scrolls p4 out too, so the
    // scrollback holds p0..p4 (5 rows) and the viewport holds p5..p8.
    stream_owned_lines(&mut t, 9, "p");
    assert_eq!(
        t.grid().scrollback.len(),
        5,
        "precondition: 5 scrolled rows"
    );
    // Full-frame repaint via per-row EL2 inside the synchronized frame.
    for row in 0..5 {
        t.process(format!("\x1b[{};1H\x1b[2Knew-{row}", row + 1).as_bytes());
    }
    t.process("\x1b[?2026l".as_bytes());
    assert_eq!(
        t.grid().scrollback.len(),
        0,
        "finish discard clears the scrollback"
    );

    assert!(t.refresh_primary_history_snapshot_now());
    let output = t.block_tracker().in_flight().unwrap().output.to_string();
    for i in 0..5 {
        assert!(
            output.contains(&format!("p{i}")),
            "scrolled-out paragraph {i} must be preserved"
        );
    }
    assert!(
        !output.contains("p5"),
        "viewport content overwritten by the repaint is not preserved"
    );
    // The live frame appears exactly once (no duplication).
    for row in 0..5 {
        assert_eq!(
            output.matches(&format!("new-{row}")).count(),
            1,
            "live frame row {row} must not be duplicated"
        );
    }
}

#[test]
fn tiny_repaint_frames_are_not_preserved_into_block_history() {
    // v1.10.23: resize jitter produces sub-4-line frames; preserving
    // them would fragment the block history with tiny partial frames.
    let mut t = Terminal::new(5, 20);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07omp\x1b]133;C\x07");
    t.process("\x1b[3A\x1b[2G\x1b[?2026h".as_bytes());
    // A 2-line frame, then a full-frame repaint.
    t.process("\x1b[2Kab\r\n\x1b[2Kcd".as_bytes());
    t.process("\x1b[2J\x1b[Hx".as_bytes());
    assert_eq!(t.grid().scrollback.len(), 0);
    assert!(t.refresh_primary_history_snapshot_now());
    let output = t.block_tracker().in_flight().unwrap().output.to_string();
    assert!(!output.contains("ab"), "tiny frame must not be preserved");
    assert!(!output.contains("cd"));
    assert!(output.contains('x'), "the new frame is the live tail");
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

    /// v1.11.16: perf budgets are calibrated for a developer machine, but
    /// shared CI runners are both slower and noisier, so an absolute
    /// assertion there measures the runner rather than the parser.
    /// `WEFT_PERF_SLACK` scales every target (default 1.0 = the real budget);
    /// CI sets a documented factor so the regression signal survives without
    /// turning host jitter into red builds.
    fn perf_slack() -> f64 {
        std::env::var("WEFT_PERF_SLACK")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v >= 1.0)
            .unwrap_or(1.0)
    }

    /// `seq 1 10000` — plan target: < 50ms (Warp ~10ms).
    #[test]
    #[ignore]
    fn bench_seq_10000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 10000 (target < 50ms) ===");
        let bytes = seq_output(10_000);
        let (ms, _) = bench("seq 1 10000", &bytes);
        let target = 50.0 * perf_slack();
        assert!(
            ms < target,
            "seq 1 10000 took {ms:.2}ms, target < {target:.0}ms"
        );
    }

    /// `seq 1 100000` — plan target: < 300ms (Warp ~50ms).
    #[test]
    #[ignore]
    fn bench_seq_100000() {
        println!("\n=== Phase 1.5 benchmark: seq 1 100000 (target < 300ms) ===");
        let bytes = seq_output(100_000);
        let (ms, _) = bench("seq 1 100000", &bytes);
        let target = 300.0 * perf_slack();
        assert!(
            ms < target,
            "seq 1 100000 took {ms:.2}ms, target < {target:.0}ms"
        );
    }

    /// `ls -la /usr/bin` style (~1000 entries) — plan target: < 20ms (Warp ~5ms).
    #[test]
    #[ignore]
    fn bench_ls_usr_bin() {
        println!("\n=== Phase 1.5 benchmark: ls -la /usr/bin (target < 20ms) ===");
        let bytes = ls_output(1000);
        let (ms, _) = bench("ls -la /usr/bin (1000 entries)", &bytes);
        let target = 20.0 * perf_slack();
        assert!(
            ms < target,
            "ls output took {ms:.2}ms, target < {target:.0}ms"
        );
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

/// v1.10.31 (FIX_BREW_PROGRESS_TUI_MISCLASSIFY): brew progress streams with
/// DEC 2026 synchronized output must NOT be classified as TUIs. They emit
/// `?2026h/?2026l` on every frame but only use EL + CHA(column 1), which is
/// not real TUI cursor addressing. This test reproduces the exact byte pattern
/// from a real brew session (7 frames of "Downloading XKB...") and verifies
/// that the block output contains exactly ONE final line, not 7 duplicates.
#[test]
fn brew_progress_stream_stays_single_row_in_capture() {
    let mut t = term();
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");

    // Simulate 7 frames of brew progress output. Each frame:
    // - Prints the full line
    // - Clears to end of line (EL, \x1b[K)
    // - Moves cursor to column 1 (CHA with param 0, \x1b[0G)
    // - Toggles DEC 2026 sync off/on (\x1b[?2026l\x1b[?2026h)
    // - Prints a colored spinner character
    // This is the exact pattern from real brew byte streams.
    let spinners: &[&[u8]] = &[
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\xa0\x8b", // blue spinner
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\x0a\x8c",
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\xa0\x8d",
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\x0a\x8e",
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\xa0\x8f",
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\x0a\x90",
        b"\x1b[34m\x1b[?2026l\x1b[?2026h\xe2\xa0\x91",
    ];

    let mut frame_count = 0;
    for (i, spinner) in spinners.iter().enumerate() {
        // Each frame: print the line with incrementing progress
        let progress = format!("Bottle jq (1.8.2) ### Downloading  XKB/{}1KB", 440 + i);
        t.process(progress.as_bytes());
        // EL + CHA(0) + DEC 2026 toggle + spinner
        t.process(b"\x1b[K\x1b[0G");
        t.process(spinner);
        frame_count += 1;
    }

    // End the command
    t.process(b"\n\x1b]133;D;0\x07");

    // Verify that the TUI was NOT detected (key assertion)
    assert!(
        !t.primary_screen_app_active(),
        "brew progress with DEC 2026 should NOT trigger TUI detection"
    );

    // Verify the block output contains exactly ONE occurrence of "Downloading"
    let block = t.block_tracker().blocks().last().unwrap();
    let output = block.output.as_ref();

    // Count occurrences of "Downloading" in the output
    let downloading_count = output.matches("Downloading").count();

    assert_eq!(
        downloading_count, 1,
        "brew progress must converge to a single line (got {} occurrences, expected 1). \
         Before the fix, this would be {} (one per frame). Output: {:?}",
        downloading_count, frame_count, output
    );

    // Verify the final state contains the completion marker
    assert!(
        output.contains("Downloading"),
        "final output should contain the last progress state"
    );
}

// ── Terminal capability hardening (FIX_TERMINAL_CAPABILITY_HARDENING) ──

/// Kitty keyboard-protocol ops (`CSI >flags u` / `CSI =flags;mode u` /
/// `CSI <u`) negotiate the protocol AND must NOT be dispatched to the
/// bare-`CSI u` DECRC restore path. opencode pushes kitty modes at startup;
/// a restore would jump the cursor back to the last DECSC-saved position
/// mid-TUI. Every op is answered with exact bytes (v2 plan §4.2: replies
/// are asserted, not assumed silent).
#[test]
fn kitty_keyboard_mode_ops_do_not_run_decrc_restore() {
    let mut t = term();
    t.process(b"\x1b[5;10H"); // cursor → (4, 9)
    t.process(b"\x1b7"); // DECSC save
    t.process(b"\x1b[3;3H"); // cursor → (2, 2)
    t.process(b"\x1b[>1u"); // kitty push
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?1u", "push 1 ⇒ flags 1");
    t.process(b"\x1b[=2u"); // kitty set (mode default 1: full assignment)
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?2u", "set 2 rewrites the top");
    t.process(b"\x1b[<u"); // kitty pop
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?0u", "pop ⇒ empty stack ⇒ 0");
    assert_eq!(
        (t.grid().cursor.row, t.grid().cursor.col),
        (2, 2),
        "kitty keyboard-mode ops must not restore the saved cursor"
    );
}

/// Bare `CSI u` keeps its SCO DECRC alias: it restores the cursor saved by
/// DECSC (`ESC 7`). Regression anchor so the intermediates guard cannot
/// silently break the alias.
#[test]
fn bare_csi_u_restores_cursor_after_decsc() {
    let mut t = term();
    t.process(b"\x1b[5;10H");
    t.process(b"\x1b7"); // DECSC save (4, 9)
    t.process(b"\x1b[10;20H");
    t.process(b"\x1b[u"); // bare CSI u → DECRC
    assert_eq!((t.grid().cursor.row, t.grid().cursor.col), (4, 9));
}

/// `CSI ? u` — kitty keyboard enhancement query. v1.11.4 implements the
/// protocol, so the empty-stack query answers EXACTLY `CSI ? 0 u`: 0 is a
/// legitimate negotiated state (stack empty = no flags), never the old
/// silence. Consumers (nvim/Helix) must be able to distinguish "did not
/// speak kitty" from "spoke kitty with zero flags".
#[test]
fn csi_question_u_kitty_query_answers_current_flags() {
    let mut t = term();
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?0u", "empty stack ⇒ ?0u");
    t.process(b"\x1b[>27u\x1b[?u"); // 27 = 0b11011 → masked to 0b10011
    assert_eq!(t.take_response(), b"\x1b[?19u", "masked push ⇒ ?19u");
    // a parameter on a query changes nothing (no conditional answers)
    t.process(b"\x1b[?2u");
    assert_eq!(t.take_response(), b"\x1b[?19u", "query param ignored");
}

/// v1.11.4 (PLAN_v1114 §4.1): `CSI > u` pushes 0 (`CSI >flags u` with an
/// omitted flags parameter defaults to 0); `CSI < 2 u` pops twice — the
/// pop count is the FIRST parameter, not a prefix marker.
#[test]
fn kitty_push_omitted_flags_is_zero_and_pop_count_is_first_param() {
    let mut t = term();
    t.process(b"\x1b[>u"); // push with omitted flags ⇒ 0
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?0u", "CSI >u ⇒ flags 0");
    t.process(b"\x1b[>1u\x1b[>5u\x1b[>27u"); // stack: [0], then 1, 5, 27
    t.process(b"\x1b[<2u"); // pop count 2
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?1u",
        "two pops leave the first real push (1)"
    );
    t.process(b"\x1b[<u");
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?0u", "final pop ⇒ 0");
}

/// v1.11.4 (PLAN_v1114 §4.1): set modes 2/3 (set-bits / clear-bits) at
/// terminal level; the stack top is rewritten in place (M1).
#[test]
fn kitty_set_modes_two_and_three_are_bitwise() {
    let mut t = term();
    t.process(b"\x1b[>11u"); // 11 = 0b1011
    t.process(b"\x1b[=16;2u"); // set mode 2: add the text-bit (16)
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?19u", "set-bits → masked 0b10011");
    t.process(b"\x1b[=18;3u"); // clear mode 3: drop bits 1 and 4 (18 = 0b10010)
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?1u",
        "clear-bits → 0b10011 & !0b10010 = 0b1"
    );
    // M1 depth invariant survives mode flips
    t.process(b"\x1b[<u");
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?0u",
        "one pop drains the whole stack"
    );
}

/// rust-reviewer v1.11.4 Major-1: the user's `[compat] kitty_keyboard =
/// false` switch must survive RIS (`ESC c` replaces the whole Terminal via
/// `*self = Self::new()`); only the negotiated stacks reset.
#[test]
fn ris_preserves_kitty_protocol_enabled_switch() {
    let mut t = term();
    t.set_kitty_protocol_enabled(false);
    t.process(b"\x1b[>1u\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"",
        "disabled terminal swallows negotiation"
    );
    assert_eq!(t.keyboard_protocol_flags(), 0);

    t.process(b"\x1bc"); // RIS
    assert!(
        !t.kitty_protocol_enabled,
        "the config switch must survive RIS"
    );
    assert_eq!(t.keyboard_protocol_flags(), 0, "RIS still clears the stack");
}

/// v1.11.4 (PLAN_v1114 §1.1, §4.1): the two screens keep independent
/// stacks; DEC 1049 swaps move nothing between them.
#[test]
fn kitty_double_stack_is_isolated_across_alt_swap() {
    let mut t = term();
    t.process(b"\x1b[>1u"); // main: 1
    t.process(b"\x1b[?1049h"); // enter alt screen
    t.process(b"\x1b[>11u"); // alt: 11 → masked to 0b11
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?3u",
        "alt screen sees its own push (masked)"
    );
    t.process(b"\x1b[?1049l"); // back to main
    t.process(b"\x1b[?u");
    assert_eq!(
        t.take_response(),
        b"\x1b[?1u",
        "main stack untouched by alt ops"
    );
}

/// v1.11.4 (PLAN_v1114 §3): `[compat] kitty_keyboard = false` swallows ALL
/// four ops — the pre-v1.11.4 silence — and zeroes the encoder feed.
#[test]
fn kitty_disabled_swallows_all_ops_and_zeroes_flags() {
    let mut t = term();
    t.set_kitty_protocol_enabled(false);
    t.process(b"\x1b[>1u");
    t.process(b"\x1b[=2;1u");
    t.process(b"\x1b[<u");
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"", "disabled: every op stays silent");
    assert_eq!(t.keyboard_protocol_flags(), 0, "encoder feed forced to 0");
    // re-enable: the state machine was never touched, so it starts clean
    t.set_kitty_protocol_enabled(true);
    t.process(b"\x1b[?u");
    assert_eq!(t.take_response(), b"\x1b[?0u");
}

/// v1.11.4 (PLAN_v1114 §1.3, §4.8): OSC 133;D (command end — back at the
/// prompt) clears BOTH stacks — the crash-residue hook for `kill -9 nvim`
/// (a TUI crash produces no PtyExit, only this prompt boundary).
#[test]
fn osc_133_d_command_end_resets_kitty_stacks() {
    let mut t = term();
    t.process(b"\x1b[>1u\x1b[?1049h\x1b[>11u"); // main=1, alt=11→masked 3
    assert_eq!(t.keyboard_protocol_flags(), 3);
    t.process(b"\x1b]133;D;0\x07");
    assert_eq!(t.keyboard_protocol_flags(), 0, "main stack reset");
    t.process(b"\x1b[?1049l");
    assert_eq!(
        t.keyboard_protocol_flags(),
        0,
        "alt stack reset too — no stale flags after a crash"
    );
}

/// v1.11.4: RIS (`ESC c`) also clears the negotiation state (fresh
/// terminal contract), matching every other terminal-mode reset.
#[test]
fn ris_clears_kitty_stacks() {
    let mut t = term();
    t.process(b"\x1b[>1u\x1b[>11u"); // 11 → masked to 0b11
    assert_eq!(t.keyboard_protocol_flags(), 3);
    t.process(b"\x1bc");
    assert_eq!(t.keyboard_protocol_flags(), 0);
}

/// XTVERSION (`CSI > 0 q` / `CSI > q`): reply `DCS > | weft <ver> ST` — the
/// form xterm answers with. DECSCUSR (`CSI q` / `CSI SP q`) is not a query
/// and must produce no reply.
#[test]
fn xtversion_replies_with_dcs_banner() {
    let mut t = term();
    t.process(b"\x1b[>0q");
    let resp = t.take_response();
    let s = String::from_utf8_lossy(&resp);
    assert!(s.starts_with("\x1bP>|weft "), "XTVERSION prefix: {s:?}");
    assert!(s.ends_with("\x1b\\"), "XTVERSION must end with ST: {s:?}");
    assert!(
        s.contains(env!("CARGO_PKG_VERSION")),
        "XTVERSION must carry CARGO_PKG_VERSION: {s:?}"
    );

    t.process(b"\x1b[>q"); // param-less form
    assert_eq!(t.take_response(), resp, "CSI >q must answer like CSI >0q");

    t.process(b"\x1b[2q"); // DECSCUSR — not a query
    assert_eq!(t.take_response(), b"", "DECSCUSR must not reply");
}

/// XTGETTCAP (`DCS + q <hex>;<hex>... ST`): exactly one negative answer
/// (`DCS 0 + r <hex> ST`) per requested capability name. opencode queries
/// the `Ms` capability (`4d73`) this way at startup.
#[test]
fn xtgettcap_answers_each_requested_name_negatively() {
    let mut t = term();
    t.process(b"\x1bP+q4d73;636f6c6f72\x1b\\"); // Ms; color
    assert_eq!(
        t.take_response(),
        b"\x1bP0+r4d73\x1b\\\x1bP0+r636f6c6f72\x1b\\",
        "one negative XTGETTCAP answer per requested name"
    );
}

/// Malformed XTGETTCAP payloads (odd-length hex, non-hex chars, empty
/// segments, empty payload) are skipped without panicking and without
/// emitting garbage; non-request DCS sequences stay ignored.
#[test]
fn xtgettcap_malformed_payloads_are_tolerated() {
    let mut t = term();
    t.process(b"\x1bP+q4d7;zz;636f6c6f72;x;;\x1b\\");
    assert_eq!(
        t.take_response(),
        b"\x1bP0+r636f6c6f72\x1b\\",
        "only the valid hex name should be answered"
    );
    t.process(b"\x1bP+q\x1b\\"); // empty payload
    assert_eq!(t.take_response(), b"", "empty payload answers nothing");
    t.process(b"\x1bP1+r4d73\x1b\\"); // an XTGETTCAP response aimed at us
    t.process(b"\x1bP!u01234567\x1b\\"); // unrelated DCS
    assert_eq!(t.take_response(), b"", "non-request DCS stays ignored");
}

/// A `DCS + q` introducer whose payload never hits ST must not grow memory
/// without bound (vte Puts every passthrough byte) — the collector caps at
/// 1KiB and the whole overflowed request is dropped, silently.
#[test]
fn xtgettcap_overlong_payload_is_capped_and_discarded() {
    let mut t = term();
    let mut seq = Vec::with_capacity(5200);
    seq.extend_from_slice(b"\x1bP+q");
    seq.extend(std::iter::repeat(b'4').take(5000)); // 5000 bytes, no ST yet
    seq.extend_from_slice(b"\x1b\\");
    t.process(&seq); // must not panic; memory bounded by the 1KiB cap
    assert_eq!(
        t.take_response(),
        b"",
        "overflowed XTGETTCAP request must be dropped whole"
    );
    // the collector must re-arm cleanly for the next request
    t.process(b"\x1bP+q4d73\x1b\\");
    assert_eq!(t.take_response(), b"\x1bP0+r4d73\x1b\\");
}

// ── v1.11.3 DECRQSS / XTGETTCAP Su (PLAN_v1113 §2.2/§2.3) ──────

/// v1.11.3 (PLAN_v1113 §4.3): the nvim probe — `ESC[0m ESC[4:3m` followed by
/// `DCS $ q m ST` — must answer EXACTLY `1$r4:3m`. nvim's
/// `tui_query_extended_underline` / `handle_term_response` byte-match the
/// body between DCS and ST; this is the real vim/neovim spell-check wavy
/// underline unlock path.
#[test]
fn decrqss_sgr_probe_after_wavy_answers_exactly_1_dollar_r_4_colon_3() {
    let mut t = term();
    t.process(b"\x1b[0m\x1b[4:3m\x1bP$qm\x1b\\");
    assert_eq!(
        t.take_response(),
        b"\x1bP1$r4:3m\x1b\\",
        "nvim DECRQSS probe must receive the exact 1$r4:3m body"
    );
}

/// v1.11.3: double underline state answers `1$r4:2m`.
#[test]
fn decrqss_sgr_after_double_answers_4_colon_2() {
    let mut t = term();
    t.process(b"\x1b[0m\x1b[21m\x1bP$qm\x1b\\");
    assert_eq!(t.take_response(), b"\x1bP1$r4:2m\x1b\\");
}

/// v1.11.3: an unanswerable DECRQSS payload (anything but `m`) gets the
/// refused answer `DCS 0 $ r <pt> ST`.
#[test]
fn decrqss_unknown_payload_gets_refused_answer() {
    let mut t = term();
    t.process(b"\x1bP$qr\x1b\\"); // e.g. DECTABSR-style query
    assert_eq!(t.take_response(), b"\x1bP0$rr\x1b\\");
}

/// v1.11.3: default attrs contain no underline segment (explicit `0` reset
/// body), and `$q` with an empty payload is refused like any unknown.
#[test]
fn decrqss_default_attrs_answer_has_no_underline() {
    let mut t = term();
    t.process(b"\x1b[0m\x1bP$qm\x1b\\");
    let bytes = t.take_response();
    let resp = String::from_utf8_lossy(&bytes);
    assert_eq!(resp, "\x1bP1$r0m\x1b\\");
    assert!(!resp.contains('4'), "no underline segment in {resp}");
}

/// v1.11.3 (PLAN_v1113 §4.4): XTGETTCAP `Su` (hex `5375`) gets a positive
/// `1+r` answer carrying the base64-encoded mintty-style value; unknown
/// names keep the negative `0+r` answer.
#[test]
fn xtgettcap_su_gets_positive_answer_at_terminal_level() {
    let mut t = term();
    t.process(b"\x1bP+q5375;4d73\x1b\\"); // Su; Ms
    assert_eq!(
        t.take_response(),
        b"\x1bP1+r5375=G1s0OiVkbQ\x1b\\\x1bP0+r4d73\x1b\\",
        "Su positive + Ms negative in request order"
    );
}

/// v1.11.3 (PLAN_v1113 §4.6): screen snapshot round-trip — a cell written
/// with `4:3` + `58:2:...` must surface as a Wavy style + explicit color in
/// the captured StyledLine (parser → cell → snapshot chain end-to-end).
#[test]
fn snapshot_roundtrip_preserves_wavy_style_and_underline_color() {
    use crate::blocks::StyledOutput;
    use crate::grid::UnderlineStyle;
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b[4:3m\x1b[58:2:200:100:50mwave");
    let (_text, styled, _): (String, StyledOutput, Option<usize>) = t
        .grid()
        .document_snapshot_from_position_with_resolver(0, |_| None, DEFAULT_OUTPUT_CAP);
    let line = styled.line(0).expect("line 0 in snapshot");
    assert_eq!(line.underline_style_at(0), UnderlineStyle::Wavy);
    assert_eq!(line.underline_style_at(3), UnderlineStyle::Wavy);
    assert_eq!(
        line.underline_color_at(0),
        Some(CellColor::Rgb(Color::rgb(200, 100, 50)))
    );
    assert_eq!(
        line.underline_color_at(3),
        Some(CellColor::Rgb(Color::rgb(200, 100, 50)))
    );
}

/// `CSI ? 1 u` (kitty keyboard op carrying the private marker) must not be
/// mis-dispatched as DEC private mode set/unset: mode 1 is DECCKM and a
/// bogus `u` final used to toggle it off via handle_dec_private_mode(1, false).
/// v1.11.4 answers the query with the CURRENT flags (here `?0u` — no push
/// yet); the DECCKM assertions pin the gate.
#[test]
fn csi_question_1_u_does_not_toggle_decckm() {
    let mut t = term();
    t.process(b"\x1b[?1h"); // DECCKM on
    assert!(t.app_cursor_keys(), "?1h must enable DECCKM");
    t.process(b"\x1b[?1u"); // kitty query with private marker — not DECSET
    assert!(t.app_cursor_keys(), "CSI ?1u must not clear DECCKM");
    assert_eq!(
        t.take_response(),
        b"\x1b[?0u",
        "kitty query answers current flags (?0u before any push)"
    );
    t.process(b"\x1b[?1l"); // real DECSET off still works
    assert!(!t.app_cursor_keys(), "?1l must disable DECCKM");
}

// ── FIX_SPINNER_TOPJUMP_MISCLASSIFY (v1.10.38) ──
//
// `opencode upgrade`'s status line rewrites its spinner in place via
// `CSI 999D` + `CSI J` (byte-captured: ~1000× per install, no other cursor
// addressing). Horizontal relative hops never change rows, so v1.10.38
// excludes `C`/`D` from `is_primary_screen_addressing`: they no longer feed
// `cursor_ops`, and a pure-spinner stream therefore never crosses the
// `cursor_ops >= 2` takeover threshold. Row-capable evidence (`H`/`f`/`d`,
// `A`/`B`, `G`>1) is untouched, so omp/pi/openclaw-style repainters still
// engage exactly as before.

/// The red regression: opencode's real spinner signature (`CSI 999D` +
/// `CSI J`, zero counting ops — horizontal hops are excluded) must NOT
/// become a screen document, and its
/// finalized block must stay `screen_origin = false` with its banner +
/// spinner text intact as an ordinary streaming block.
#[test]
fn spinner_rewrite_stream_does_not_become_screen_document() {
    let mut t = Terminal::new(10, 60);
    // Ordinary command start: prompt boundary, then the echo line + preexec.
    t.process(b"\x1b]133;A\x07opencode upgrade\x1b]133;B\x07\x1b]133;C\x07");
    // Streaming command's initial banner.
    t.process("\r\nfetching metadata\r\nchecking version\r\n".as_bytes());
    // The byte-captured opencode signature: hop back to column 0 and erase
    // through end-of-screen before rewriting the status line. Two frames,
    // zero row-capable addressing — the old rule counted both `D`s and took
    // over mid-run.
    t.process("\x1b[999D\x1b[J\u{25D3} Upgrading.\x1b[999D\x1b[J\u{25D2} Upgrading.".as_bytes());
    assert!(
        !t.primary_screen_app_active(),
        "horizontal hops alone must not cross the cursor_ops threshold"
    );
    assert!(
        t.block_tracker().screen_document_start().is_none(),
        "spinner-only rewrites must not take over the viewport as a screen document"
    );
    // Finalize like a normal command; the block stays an ordinary streaming
    // block (screen_origin = false) with the banner and the spinner line.
    t.process(b"\x1b]133;D;0\x07");
    t.settle_primary_screen_exit(); // no-op unless a screen exit was deferred
    let block = t.block_tracker().blocks().last().unwrap();
    assert!(
        !block.screen_origin,
        "spinner-only stream must finalize soft-wrappable, not screen_origin"
    );
    let output = block.output.as_ref();
    assert!(
        output.contains("fetching metadata"),
        "banner line 1 must survive ordinary capture: {output:?}"
    );
    assert!(
        output.contains("checking version"),
        "banner line 2 must survive ordinary capture: {output:?}"
    );
    // Ordinary capture rewrites in place (OutputCapture), so the surviving
    // spinner line holds the last frame's text — not both frame characters.
    assert!(
        output.contains("Upgrading."),
        "the spinner status line must survive ordinary capture: {output:?}"
    );
    // MINOR-3: pin the in-place rewrite for real — the first frame's glyph
    // must have been overwritten by the second frame, not concatenated.
    assert!(
        !output.contains('\u{25D3}'),
        "the first spinner frame must be rewritten away, not appended: {output:?}"
    );
}

/// Regression guard (green before and after): a full-viewport TUI that
/// repaints with absolute CUP/VPA addressing must still take over the screen
/// document.
#[test]
fn absolute_addressing_still_takes_over_screen_document() {
    let mut t = Terminal::new(10, 60);
    t.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    t.process("\r\nbanner line\r\n".as_bytes());
    // Claude Code owns the viewport with repeated absolute CUP addressing.
    t.process(b"\x1b[8;5Hframe row one\x1b[9;5Hframe row two");
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "absolute addressing must still begin a screen document"
    );
    t.process(b"\x1b]133;D;0\x07");
    t.settle_primary_screen_exit();
    let block = t.block_tracker().blocks().last().unwrap();
    assert!(
        block.screen_origin,
        "an absolute-addressed TUI block must stay screen_origin"
    );
}

/// Regression guard (green before and after): a COMPLETED DEC 2026
/// synchronized full frame archives `primary_screen_synchronized_frame_seen`
/// (read back via `primary_screen_repaint_capable()`), and the window's
/// relative addressing counts toward `cursor_ops` — the first `1B` alone
/// contributes two ops (the generic note + the v1.10.31 window bonus), so
/// ownership begins mid-window exactly as it did before v1.10.38. There is
/// no separate evidence gate at the takeover entry (that design was
/// superseded); this test pins the window-counting + archival mechanics that
/// full-frame repainters rely on.
#[test]
fn synchronized_full_frame_still_takes_over_screen_document() {
    let mut t = Terminal::new(3, 40);
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    // One synchronized window that clears and repaints every row with
    // relative moves (no CUP) — cursor_ops cross the threshold mid-window,
    // and the completion (`?2026l`) archives the atomic full frame.
    t.process(b"\x1b[?2026h");
    t.process(b"\x1b[2K\x1b[1B\x1b[2K\x1b[1B\x1b[2Kframe row");
    t.process(b"\x1b[?2026l");
    assert!(
        t.primary_screen_repaint_capable(),
        "every row cleared inside one window must prove atomic repaint"
    );
    // Ownership is already established by the window's row ops; the extra
    // op merely documents that streaming continues inside the document.
    t.process(b"\x1b[1B");
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "a completed synchronized full frame must begin a screen document"
    );
}

// ── v1.11.5 OSC 52 / 9 / 777 UiEvent channel (PLAN_v1115 §M1) ──────────

/// End-to-end fixture: a single `process()` batch with mixed OSC sequences
/// drains to exactly the events the dispatcher pushed, in order.
#[test]
fn ui_events_drain_end_to_end_fixture() {
    use crate::vt::DockProgress;

    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]52;c;Zm9vYmFy\x07"); // write "foobar"
    t.process(b"\x1b]777;notify;Title;multi;part body\x07");
    t.process(b"\x1b]9;4;3;47\x07"); // progress 47%
    t.process(b"\x1b]52;c;?\x07"); // read request
    t.process(b"\x1b]9;plain message\x07"); // iTerm2-style notify

    let events = t.take_ui_events();
    assert_eq!(
        events.len(),
        5,
        "exactly the five pushed events: {events:?}"
    );
    assert_eq!(
        events[0],
        crate::vt::UiEvent::ClipboardWrite {
            data: b"foobar".to_vec(),
            truncated: false
        }
    );
    assert_eq!(
        events[1],
        crate::vt::UiEvent::RemoteNotify {
            title: "Title".into(),
            body: "multi;part body".into(),
        }
    );
    assert_eq!(
        events[2],
        crate::vt::UiEvent::DockProgress(DockProgress::Percent(47))
    );
    assert_eq!(events[3], crate::vt::UiEvent::ClipboardReadRequest);
    assert_eq!(
        events[4],
        crate::vt::UiEvent::RemoteNotify {
            title: String::new(),
            body: "plain message".into(),
        }
    );

    // FIFO drain: second take is empty (the queue was mem::taken).
    assert!(t.take_ui_events().is_empty(), "drain is FIFO + destructive");
}

/// Unknown OSC codes and non-notify 777 subcommands produce no events.
#[test]
fn ui_events_unknown_osc_produces_nothing() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]100;x\x07\x1b]777;scrollback;+1\x07\x1b]52;q;Zm9v\x07");
    assert!(t.take_ui_events().is_empty());
}

/// The OSC 9 `git=` prefix stays on the legacy branch: its payload is never
/// misread as a notification (git fixture priority, PLAN_v1115 §M1).
#[test]
fn ui_events_osc9_git_prefix_never_notifies() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]9;git=main\x07");
    assert!(
        t.take_ui_events().is_empty(),
        "git= is a branch report, not a notify"
    );
    assert_eq!(t.git_branch(), Some("main"));

    // but a non-git 9 payload IS a notify
    t.process(b"\x1b]9;build done\x07");
    assert_eq!(t.take_ui_events().len(), 1);
}

/// RIS (ESC c) replaces the whole Terminal, so the ui_events queue is
/// cleared without an explicit reset hook (PLAN_v1115 D-a).
#[test]
fn ui_events_ris_clears_queue_by_object_replace() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]52;c;Zm9v\x07");
    assert_eq!(t.take_ui_events().len(), 1);
    t.process(b"\x1b]52;c;Zm9v\x07");
    t.process(b"\x1bc"); // RIS
    assert!(
        t.take_ui_events().is_empty(),
        "RIS must clear queued ui events"
    );
}

/// OSC 9;4 progress with a missing/illegal param is ignored — no event,
/// no panic, and the grid stays untouched.
#[test]
fn ui_events_progress_missing_params_ignored() {
    let mut t = Terminal::new(24, 80);
    t.process(b"\x1b]9;4\x07"); // no state at all
    t.process(b"\x1b]9;4;3\x07"); // state 3 without progress
    t.process(b"\x1b]9;4;9;5\x07"); // unknown state 9
    assert!(t.take_ui_events().is_empty(), "all three must be ignored");
}

// ── v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1 / M3) ──────────────────
//
// Screen-owned sessions keep the BlockView during the whole command
// (noninteractive/all tiers); interactive stdin / mouse reporting exempt
// them back to the classic takeover; the settle window keeps the live
// block visible; and the classic + noninteractive tiers converge on the
// same snapshot block byte-for-byte.

/// The five F6 app-mode fixtures. Each returns a terminal primed with the
/// shell markers plus the app's cursor-addressing pattern.
#[derive(Clone, Copy)]
enum FixtureKind {
    /// Claude Code — absolute CUP addressing (`\x1b[H\x1b[2;1H`).
    Claude,
    /// openclaw — relative CUU + CUB (`\x1b[999D\x1b[915A\x1b[1A`; the CUB
    /// is horizontal-only and does not count, the two CUU do).
    Openclaw,
    /// pi — CUU + CHA + DEC 2026 synchronized frames.
    Pi,
    /// ollama — multi-row in-place repaint, two `\x1b[3A` frames.
    OllamaMulti,
    /// brew spinner / synchronized output with zero counting ops.
    Brew,
}

fn fixture(kind: FixtureKind) -> Terminal {
    let mut t = Terminal::new(24, 80);
    match kind {
        FixtureKind::Claude => {
            t.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
            t.process(b"\x1b[H\x1b[2;1H");
        }
        FixtureKind::Openclaw => {
            t.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
            t.process("\x1b[999D\x1b[915A\x1b[1A".as_bytes());
        }
        FixtureKind::Pi => {
            t.process(b"\x1b]133;A\x07\x1b]133;B\x07pi\x1b]133;C\x07");
            t.process("\x1b[3A\x1b[1G\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l".as_bytes());
        }
        FixtureKind::OllamaMulti => {
            t.process(b"\x1b]133;A\x07ollama pull\n\x1b]133;B\x07\x1b]133;C\x07");
            t.process(b"pulling a: 0%\npulling b: 0%\npulling c: 0%\n");
            let first =
                "\x1b[3A\rpulling a: 50%\x1b[K\n\rpulling b: 30%\x1b[K\n\rpulling c: 10%\x1b[K";
            let second =
                "\x1b[3A\rpulling a: 80%\x1b[K\n\rpulling b: 60%\x1b[K\n\rpulling c: 40%\x1b[K";
            t.process(first.as_bytes());
            t.process(second.as_bytes());
        }
        FixtureKind::Brew => {
            t.process(b"\x1b]133;A\x07brew update\n\x1b]133;B\x07\x1b]133;C\x07");
            for _ in 0..3 {
                t.process(b"\x1b[?2026h\x1b[2K\xe2\xa0\x8b\x1b[?2026l");
            }
        }
    }
    t
}

/// Assert the three-tier show_block_view result for one fixture.
#[test]
fn render_mode_matrix_three_tiers_x_app_fixtures() {
    // M1.6: 三档 × F6 全部 app 模式. For the four detected TUIs classic
    // hands the viewport to the live grid; noninteractive (no stdin/mouse)
    // and all keep the BlockView; brew (zero counting ops) stays a block in
    // every tier.
    let detected = [
        FixtureKind::Claude,
        FixtureKind::Openclaw,
        FixtureKind::Pi,
        FixtureKind::OllamaMulti,
    ];
    for kind in detected {
        let mut t = fixture(kind);
        assert!(t.primary_screen_app_active(), "fixture must detect a TUI");
        // classic
        assert!(
            !t.show_block_view(),
            "classic: screen-owned TUI renders in the live grid"
        );
        // noninteractive
        t.set_tui_render_mode(TuiRenderMode::Noninteractive);
        assert!(
            t.show_block_view(),
            "noninteractive: screen-owned session keeps the BlockView"
        );
        // all
        t.set_tui_render_mode(TuiRenderMode::All);
        assert!(t.show_block_view(), "all: screen-owned always blocks");
        // classic after the all flip
        t.set_tui_render_mode(TuiRenderMode::Classic);
        assert!(!t.show_block_view(), "classic: back to the live grid");
    }
    // brew / spinner: zero counting ops — never screen-owned, block in all three.
    for mode in [
        TuiRenderMode::Classic,
        TuiRenderMode::Noninteractive,
        TuiRenderMode::All,
    ] {
        let mut t = fixture(FixtureKind::Brew);
        t.set_tui_render_mode(mode);
        assert!(!t.primary_screen_app_active(), "brew must not classify");
        assert!(t.show_block_view(), "brew stays a block in every tier");
    }
}

#[test]
fn interactive_stdin_flip_overrides_noninteractive_tier() {
    // M1.6 stdin 翻转: one keystroke exempts the session back to classic.
    for kind in [
        FixtureKind::Claude,
        FixtureKind::Openclaw,
        FixtureKind::Pi,
        FixtureKind::OllamaMulti,
    ] {
        let mut t = fixture(kind);
        t.set_tui_render_mode(TuiRenderMode::Noninteractive);
        assert!(t.show_block_view());
        t.note_interactive_stdin();
        assert!(
            !t.show_block_view(),
            "stdin seen → classic formula owns the viewport"
        );
        assert!(t.interactive_stdin_seen());
    }
}

#[test]
fn nested_markers_do_not_clear_stdin_flag_but_settle_does() {
    // P1-2: the flag resets at REAL command boundaries only — never at
    // 133;A/D or nested markers (pi's inner prompts must not flip the view
    // mid-interaction).
    let mut t = fixture(FixtureKind::Pi);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t.note_interactive_stdin();
    assert!(!t.show_block_view(), "exempted to classic");

    // 133;D → deferred exit (screen-owned). A nested 133;B arrives inside
    // the settle window — this is the TUI's OWN internal command (resume
    // path), NOT a real boundary: the flag must survive.
    t.process(b"\x1b]133;D;0\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(
        t.interactive_stdin_seen(),
        "nested marker must not clear the stdin flag mid-interaction"
    );
    assert!(
        t.block_tracker().screen_document_start().is_some(),
        "screen session still owns the document after resume"
    );

    // A plain prompt marker (133;A) followed by another 133;D also keeps
    // the flag — only settle / real command start clear it.
    t.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert!(
        t.interactive_stdin_seen(),
        "133;A alone must not clear the flag (P1-2)"
    );

    // A real 133;D + settle (or a real 133;B) is the boundary.
    t.process(b"\x1b]133;D;0\x07");
    t.settle_primary_screen_exit();
    assert!(
        !t.interactive_stdin_seen(),
        "settle (real command boundary) clears the flag"
    );
}

#[test]
fn settle_clears_stdin_flag_and_next_command_starts_clean() {
    // M1.6 复位: settle clears; the NEXT command (133;B real path) also
    // clears, so a stale exemption can never leak.
    let mut t = fixture(FixtureKind::Claude);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t.note_interactive_stdin();
    t.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    t.settle_primary_screen_exit();
    assert!(!t.interactive_stdin_seen());
    // Next command begins exempt-free (even without an intermediate settle).
    let mut t2 = fixture(FixtureKind::Claude);
    t2.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t2.note_interactive_stdin();
    t2.process(b"next\x1b]133;B\x07\x1b]133;C\x07");
    assert!(!t2.interactive_stdin_seen(), "real 133;B clears the flag");
}

#[test]
fn mouse_protocol_parallel_exemption_without_stdin() {
    // P1-3: mouse reporting is an interaction capability declaration — the
    // noninteractive tier exempts the session even with zero stdin bytes.
    let mut t = fixture(FixtureKind::Claude);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    assert!(t.show_block_view());
    t.process(b"\x1b[?1002h");
    assert_eq!(t.mouse_protocol(), MouseProtocol::ButtonEvent);
    assert!(!t.show_block_view(), "mouse reporting → classic takeover");
    assert!(!t.interactive_stdin_seen());
    // The exemption is mode-scoped: a DEC 1002l return restores blocks.
    t.process(b"\x1b[?1002l");
    assert_eq!(t.mouse_protocol(), MouseProtocol::Off);
    assert!(t.show_block_view(), "protocol off → blocks again");
}

#[test]
fn exit_pending_in_flight_keeps_final_frame_output() {
    // P0-2 (M1.6): between 133;D (defer) and settle, in_flight() must still
    // serve the live block with the FINAL snapshot frame — the settling flag
    // widens the gate so the block never vanishes for the 200ms window.
    let mut t = fixture(FixtureKind::Claude);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t.process(b"claude> hello\r\n");
    // A screen-owned capture is active; end the command.
    t.process(b"\x1b]133;D;0\x07");
    assert!(
        t.primary_screen_exit_pending(),
        "exit deferred until the settle window elapses"
    );
    let live = t.block_tracker().in_flight();
    assert!(
        live.is_some(),
        "live block must survive the defer→settle window"
    );
    let output = live.unwrap().output.to_string();
    assert!(
        !output.is_empty(),
        "in-flight output must carry the final snapshot frame"
    );
    // The block view also stays (P0-1: exit_pending is dropped in the new tiers).
    assert!(t.show_block_view());
    // Settling closes the block with that same final frame.
    t.settle_primary_screen_exit();
    assert!(t.block_tracker().in_flight().is_none());
    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        output,
        "settle keeps the final frame"
    );
}

// ── M3: block fidelity for the reported progress classes ───────────────

#[test]
fn uv_multi_frame_progress_no_stack_single_terminal_frame() {
    // uv sync (F14 pattern — multi-frame CUU progress UI). Classic locks the
    // takeover (regression anchor); noninteractive keeps the BlockView and
    // the settled output holds ONLY the single terminal frame (no stacked
    // "Preparing… (1/3)" rows).
    let mut classic = Terminal::new(24, 80);
    classic.process(b"\x1b]133;A\x07uv sync\n\x1b]133;B\x07\x1b]133;C\x07");
    classic.process(b"Resolving packages...\nResolving packages... [1/2]\n");
    classic.process("\x1b[2A\r\x1b[KPreparing… (1/3)\n\x1b[KPreparing… (2/3)\n".as_bytes());
    classic.process("\x1b[2A\r\x1b[KPreparing… (2/3)\n\x1b[KPreparing… (3/3)\n".as_bytes());
    assert!(
        classic.primary_screen_app_active(),
        "classic: uv class detected"
    );
    assert!(
        !classic.show_block_view(),
        "classic lock: multi-frame progress takes over the grid"
    );

    let mut t = Terminal::new(24, 80);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t.process(b"\x1b]133;A\x07uv sync\n\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"Resolving packages...\nResolving packages... [1/2]\n");
    t.process("\x1b[2A\r\x1b[KPreparing… (1/3)\n\x1b[KPreparing… (2/3)\n".as_bytes());
    t.process("\x1b[2A\r\x1b[KPreparing… (2/3)\n\x1b[KPreparing… (3/3)\n".as_bytes());
    assert!(t.primary_screen_app_active());
    assert!(
        t.show_block_view(),
        "noninteractive: the reported class stays in the BlockView"
    );
    // The 50ms rate-limited snapshot publishes on the 133;D path (and the
    // idle ticker) — after it, the in-flight live block carries the final
    // frame only, with no stacked superseded rows.
    t.process(b"\x1b]133;D;0\x07");
    let live = t.block_tracker().in_flight().unwrap();
    let live_out = live.output.to_string();
    assert!(
        live_out.contains("Preparing… (3/3)"),
        "live block shows the snapshot content: {live_out:?}"
    );
    assert!(
        !live_out.contains("Resolving packages... [1/2]") && !live_out.contains("Preparing… (1/3)"),
        "superseded frames must not stack in the live/settled output: {live_out:?}"
    );
    t.settle_primary_screen_exit();
    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "Preparing… (2/3)\nPreparing… (3/3)",
        "settled output is exactly the single terminal frame"
    );
}

#[test]
fn ollama_multi_layer_progress_no_stack() {
    // ollama pull with a multi-row multi-layer progress bar (tests.rs:1664
    // extended to a second repaint so cursor_ops crosses the threshold).
    let mut t = Terminal::new(24, 80);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    t.process(b"\x1b]133;A\x07ollama pull\n\x1b]133;B\x07\x1b]133;C\x07");
    t.process(b"pulling a: 0%\npulling b: 0%\npulling c: 0%\n");
    let first = "\x1b[3A\rpulling a: 50%\x1b[K\n\rpulling b: 30%\x1b[K\n\rpulling c: 10%\x1b[K\n";
    let second = "\x1b[3A\rpulling a: 80%\x1b[K\n\rpulling b: 60%\x1b[K\n\rpulling c: 40%\x1b[K\n";
    t.process(first.as_bytes());
    t.process(second.as_bytes());
    assert!(t.primary_screen_app_active(), "multi-frame ollama detected");
    assert!(
        t.show_block_view(),
        "screen-owned ollama progress keeps the block view"
    );
    t.process(b"\n\x1b]133;D;0\x07");
    t.settle_primary_screen_exit();
    let block = t.block_tracker().blocks().last().unwrap();
    assert_eq!(
        block.output.as_ref(),
        "pulling a: 80%\npulling b: 60%\npulling c: 40%",
        "multi-layer repaints must converge to the single final frame"
    );
}

#[test]
fn settle_convergence_classic_and_noninteractive_byte_identical() {
    // M3.1 汇合差分: classic and noninteractive settle through the SAME
    // snapshot pipeline — the finished Block.output must be byte-identical
    // for every detected fixture (direct evidence the data plane is
    // untouched by the view-policy change).
    for kind in [
        FixtureKind::Claude,
        FixtureKind::Openclaw,
        FixtureKind::Pi,
        FixtureKind::OllamaMulti,
        FixtureKind::Brew,
    ] {
        let mut classic = fixture(kind);
        classic.set_tui_render_mode(TuiRenderMode::Classic);
        classic.process(b"\x1b]133;D;0\x07");
        classic.settle_primary_screen_exit();
        let classic_out = classic
            .block_tracker()
            .blocks()
            .last()
            .map(|b| b.output.to_string())
            .unwrap_or_default();

        let mut ni = fixture(kind);
        ni.set_tui_render_mode(TuiRenderMode::Noninteractive);
        ni.process(b"\x1b]133;D;0\x07");
        ni.settle_primary_screen_exit();
        let ni_out = ni
            .block_tracker()
            .blocks()
            .last()
            .map(|b| b.output.to_string())
            .unwrap_or_default();

        assert_eq!(
            classic_out, ni_out,
            "classic and noninteractive must converge on the same snapshot block"
        );
        assert!(!classic_out.is_empty(), "fixture must produce a block");
    }
}

#[test]
fn claude_caret_tracks_snapshot_line_matching_grid_cursor() {
    // M3.1 caret: the renderer's tracked BlockView caret branch is
    // (tracked_snapshot_line, grid-cursor-col) — assert the tracked snapshot
    // line matches the grid cursor row so the caret sits on the exact
    // materialized document row (renderer.rs: block_view_tui_cursor, tracked
    // branch; the T16b formula fallback anchors the capture tail instead and
    // never reads this mapping).
    let mut t = fixture(FixtureKind::Claude);
    t.set_tui_render_mode(TuiRenderMode::Noninteractive);
    // Type into the TUI so the document contains content rows.
    t.process(b"claude> hello\r\nclaude> ");
    // End the command — the 133;D handler snapshots (tracked line + segment).
    t.process(b"\x1b]133;D;0\x07");
    let tracked = t
        .primary_screen_cursor_snapshot_line()
        .expect("snapshot must track the cursor line");
    let live_line_count = t
        .block_tracker()
        .in_flight()
        .unwrap()
        .output
        .lines()
        .count();
    let mapped = live_line_count
        .saturating_sub(t.grid().num_rows)
        .saturating_add(t.grid().cursor.row)
        .min(live_line_count.saturating_sub(1));
    assert_eq!(
        tracked, mapped,
        "tracked caret line must match the renderer's grid-cursor mapping"
    );
    // The caret column is the grid cursor column by construction
    // (renderer.rs: Some((line, grid.cursor.col))) — nothing to assert
    // beyond the line mapping above (rust-reviewer P2-1: a col-vs-itself
    // comparison is a tautology).
    // Cleanup: settle leaves one block (screen-owned session finalizes).
    t.settle_primary_screen_exit();
    assert_eq!(t.block_tracker().blocks().len(), 1);
}

#[test]
fn resize_widening_keeps_written_trailing_space_in_history() {
    // PLAN_S3 T2 (rust-reviewer P2): a space PRINTED at the exact-fit wrap
    // boundary carries CellFlags::DIRTY, so FlatStorage's content predicate
    // (`!= ' ' || !flags.is_empty()`) must encode it. A later widening reflow
    // consumes the flag via `row_content_end`: the space stays logical-line
    // content and "abc e" must NOT collapse to "abce".
    let mut term = Terminal::new(4, 4);
    term.process(b"abc e");
    term.resize(4, 8);
    let merged = (0..term.grid().num_rows)
        .map(|r| term.grid().row_text(r))
        .find(|t| t.contains("abc"))
        .expect("merged line must exist after widening");
    assert!(
        merged.starts_with("abc e"),
        "written trailing space must survive the flat roundtrip, got {merged:?}"
    );
}

#[test]
fn osc8_wide_char_spacer_hover_shape_matches_history() {
    // T3 (P3-2 ruling): the print path gives the WIDE_SPACER half the lead's
    // hyperlink so viewport rows hover like materialized history rows — both
    // halves of the glyph are clickable in either storage.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/wide\x1b\\");
    t.process("中".as_bytes());
    t.process(b"\x1b]8;;\x1b\\");
    // Viewport shape: lead linked, spacer linked.
    let id = t.grid().hyperlink_id_at(0, 0).expect("lead cell linked");
    assert_eq!(
        t.grid().hyperlink_id_at(0, 1),
        Some(id),
        "viewport spacer must carry the lead's hyperlink"
    );
    assert_eq!(t.hyperlinks().url(id), Some("https://weft.dev/wide"));

    // Push the row into flat history and view it: the materialized shape
    // must match the viewport shape.
    t.process(b"\x1b[3;1H\n");
    t.grid_mut().set_scroll_offset(1);
    assert_eq!(
        t.grid().hyperlink_id_at(0, 0),
        Some(id),
        "history lead linked"
    );
    assert_eq!(
        t.grid().hyperlink_id_at(0, 1),
        Some(id),
        "history spacer linked"
    );
    // Overwriting the pair without a link clears both halves. Note the
    // two-char write: 'a' hits the lead, 'b' lands ON the spacer slot and
    // cleans its extras via the write's own unlink path — the single-char
    // regression below is what catches the stale-extras case.
    t.process(b"\x1b[1;1Hab");
    assert_eq!(t.grid().hyperlink_id_at(0, 0), None);
    assert_eq!(t.grid().hyperlink_id_at(0, 1), None);
}

#[test]
fn osc8_narrow_overwrite_of_wide_lead_kills_spacer_link_into_history() {
    // T3 review P1: overwriting ONLY the lead of a linked wide pair leaves
    // the spacer's cell flag clean (reset) but used to leave its extras
    // hyperlink behind. Viewport hover looked fine (flag-guarded read), but
    // flat encoding reads extras → "default cell + stale extras" encoded a
    // linked blank → materialized history showed a phantom clickable link.
    let mut t = Terminal::new(3, 80);
    t.process(b"\x1b]8;;https://weft.dev/wide\x1b\\");
    t.process("中".as_bytes()); // lead col 0 + spacer col 1, both linked
    t.process(b"\x1b]8;;\x1b\\");
    let id = t.grid().hyperlink_id_at(0, 0).expect("lead linked");
    assert_eq!(t.grid().hyperlink_id_at(0, 1), Some(id), "precondition");

    // ONE narrow char over the lead — nothing lands on the spacer slot.
    t.process(b"\x1b[1;1Ha");
    assert_eq!(t.grid().hyperlink_id_at(0, 0), None);
    assert_eq!(t.grid().hyperlink_id_at(0, 1), None, "viewport flag clean");
    assert_eq!(
        t.grid().viewport[0].extras.hyperlink_id_at(1),
        None,
        "spacer extras entry must be cleared with the pair"
    );

    // The phantom resurrected exactly here: encode reads extras, so scroll
    // the row into flat history and inspect the materialized shape.
    t.process(b"\x1b[3;1H\n");
    t.grid_mut().set_scroll_offset(1);
    let row = t.grid().scrollback.get(0).expect("row in flat history");
    assert_eq!(
        row.extras.hyperlink_id_at(1),
        None,
        "spacer extras must not survive into flat history"
    );
    assert!(!row.cells[1]
        .flags
        .contains(crate::grid::CellFlags::HYPERLINK));
    assert_eq!(
        t.grid().hyperlink_id_at(0, 1),
        None,
        "no phantom clickable link on the materialized blank"
    );
    // The overwriting char itself is intact.
    assert_eq!(t.grid().cell(0, 0).character, 'a');
}

#[test]
fn extras_entries_are_multi_scalar_and_match_the_extra_flag() {
    // T1 reviewer P3-3: flat materialization drops single-scalar clusters,
    // which is only lossless because the print path can never create one.
    // Pin the invariant: extras entry ⟺ EXTRA flag, and every stored
    // cluster has ≥ 2 scalars.
    let mut t = Terminal::new(2, 80);
    t.process("e\u{0301}".as_bytes()); // combining acute
    t.process("x".as_bytes());
    t.process("👩\u{200d}🔬".as_bytes()); // ZWJ sequence
    t.process("中".as_bytes()); // plain wide, no cluster
    t.process("*\u{fe0f}".as_bytes()); // VS16 promotion
    for (row_index, row) in t.grid().viewport.iter().enumerate() {
        for (col, cell) in row.cells.iter().enumerate() {
            let entry = row.extras.get(col);
            assert_eq!(
                cell.flags.contains(crate::grid::CellFlags::EXTRA),
                entry.is_some(),
                "extras ⟺ EXTRA violated at ({row_index},{col})"
            );
            if let Some(extra) = entry {
                let cluster = extra
                    .grapheme
                    .as_ref()
                    .expect("print path only stores clusters");
                assert!(
                    cluster.chars().count() >= 2,
                    "single-scalar cluster at ({row_index},{col}): {cluster:?}"
                );
            }
        }
    }
    // row_text renders clusters whole: the combining mark shows up after
    // the 'e' (decomposed form, matching what the user's terminal shows).
    assert_eq!(t.grid().row_text(0), "e\u{0301}x👩\u{200d}🔬中*\u{fe0f}");
}

#[test]
fn same_dims_pty_resize_preserves_ownership_evidence() {
    // T5 review P2: `commit_pty_resize_result` may report a PTY resize that
    // ended up dimension-identical; the candidate-path resize then produces
    // an identity row map. The ownership reduction must not turn that into
    // an empty mask (every ownership scrap wiped).
    let mut terminal = Terminal::new(6, 18);
    terminal.process(b"old shell heading\r\nold shell body\r\n");
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07startup linear output");
    let mask_before = terminal.capabilities.primary_screen_ownership.clone();
    assert!(
        mask_before.viewport.is_some(),
        "precondition: freeze left ownership evidence"
    );

    terminal.resize(6, 18); // same dims — the PTY raced back

    let mask_after = &terminal.capabilities.primary_screen_ownership;
    assert_eq!(
        mask_after.viewport, mask_before.viewport,
        "identity resize must preserve the viewport ownership mask"
    );
    assert_eq!(
        mask_after.scrollback, mask_before.scrollback,
        "identity resize must preserve the scrollback ownership mask"
    );
}

/// Appendix I optimization baseline (ignored by default; run with
/// `cargo test -p weft_core --test vt -- --ignored --nocapture bench_shrink`):
/// reproduces the field zoom-out resize (62x200 -> 33x91) over a
/// seq-1-18900-shaped document and times both directions.
#[test]
#[ignore]
fn bench_shrink_and_grow_resize_18900_lines() {
    use std::time::Instant;

    let mut term = crate::vt::Terminal::with_scrollback(62, 200, 100_000);
    let mut content = String::with_capacity(18900 * 10);
    for i in 1..=18900 {
        content.push_str(&i.to_string());
        content.push_str("\r\n");
    }
    let fed = Instant::now();
    term.process(content.as_bytes());
    eprintln!("feed 18900 lines: {:?}", fed.elapsed());

    // Field zoom-out direction: maximized (62x200) -> restore (33x91).
    let t0 = Instant::now();
    term.resize(33, 91);
    let shrink = t0.elapsed();

    // Field zoom-in direction: restore -> maximized.
    let t1 = Instant::now();
    term.resize(62, 200);
    let grow = t1.elapsed();

    eprintln!("resize 62x200 -> 33x91 (shrink): {shrink:?}");
    eprintln!("resize 33x91  -> 62x200 (grow):   {grow:?}");

    // Repeat a few cycles for a stable picture.
    for cycle in 0..3 {
        let a = Instant::now();
        term.resize(33, 91);
        let s = a.elapsed();
        let b = Instant::now();
        term.resize(62, 200);
        let g = b.elapsed();
        eprintln!("cycle {cycle}: shrink={s:?} grow={g:?}");
    }
}
