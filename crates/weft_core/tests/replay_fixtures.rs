//! R1-4: Deterministic PTY byte-fixture replay tests.
//!
//! These tests drive `Terminal::process(bytes)` with inline byte literals
//! (no real PTY, no wall-clock dependency) to cover four high-value scenarios
//! that were previously only exercisable via manual GUI testing:
//!
//! 1. CJK / ANSI / Block Elements rendering
//! 2. Claude double-Ctrl+C primary-screen exit + late resume tail
//! 3. Late resume tail replacing stale row suffixes
//! 4. Primary-screen TUI resize survival
//!
//! The byte shapes mirror real TUI output (OSC 133 markers, DEC 2026 sync,
//! absolute cursor moves) and are discovered via the `WEFT_PTY_CAPTURE`
//! env-gated tee in `pty.rs`. Fixtures use inline literals (not binary
//! recordings) so they are auditable and diff-friendly.

use weft_core::grid::{CellFlags, CellWidth};
use weft_core::vt::Terminal;

/// Assert no orphaned wide cells: every `WIDE_SPACER` must follow a `Full`
/// width cell, and every `Full` width cell must be followed by a `WIDE_SPACER`.
fn assert_no_orphaned_wide_cells(terminal: &Terminal) {
    let grid = terminal.grid();
    for row in 0..grid.num_rows {
        for col in 0..grid.num_cols {
            let cell = grid.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                assert!(col > 0, "wide spacer at left edge: {row}:{col}");
                assert_eq!(
                    grid.cell(row, col - 1).width,
                    CellWidth::Full,
                    "orphaned wide spacer at {row}:{col}"
                );
            }
            if cell.width == CellWidth::Full {
                assert!(
                    col + 1 < grid.num_cols,
                    "wide lead at right edge: {row}:{col}"
                );
                assert!(
                    grid.cell(row, col + 1)
                        .flags
                        .contains(CellFlags::WIDE_SPACER),
                    "orphaned wide lead at {row}:{col}"
                );
            }
        }
    }
}

/// Fixture 1: CJK full-width characters, ANSI 256-color sequences, and
/// Unicode box-drawing / block elements all render correctly through
/// `Terminal::process`. This mirrors the `opentui_drawing_symbols` test in
/// `vt/tests.rs` but is structured as a replay fixture.
#[test]
fn fixture_cjk_ansi_block_elements() {
    let mut t = Terminal::new(6, 40);

    // ANSI 256-color + CJK full-width text.
    t.process(b"\x1b[38;5;208m\xe6\xb5\x8b\xe8\xaf\x95\x1b[0m");
    assert_eq!(t.grid().row_text(0), "测试");
    assert_eq!(t.grid().cursor.col, 4);
    assert_eq!(t.grid().cell(0, 0).width, CellWidth::Full);
    assert_eq!(t.grid().cell(0, 2).width, CellWidth::Full);
    assert_no_orphaned_wide_cells(&t);

    // Box-drawing and block elements (half-width, no wide pairs).
    t.process(b"\x1b[2;1H\xe2\x94\x83\xe2\x96\x84\xe2\x94\x82");
    let row1 = t.grid().row_text(1);
    assert!(row1.contains('┃'), "box drawing: {row1}");
    assert!(row1.contains('▄'), "block element: {row1}");
    assert_no_orphaned_wide_cells(&t);

    // DEC 2026 synchronized output with CJK inside.
    t.process(b"\x1b[?2026h\x1b[3;1H\xe5\x85\xa8\xe8\xa7\x92\x1b[?2026l");
    assert_eq!(t.grid().row_text(2), "全角");
    assert_no_orphaned_wide_cells(&t);
}

/// Fixture 2: Claude double-Ctrl+C primary-screen exit with a late resume
/// tail. The TUI paints a primary screen, emits OSC 133;D on exit, then
/// streams "Press Ctrl-C again to exit" + "claude --resume" before the
/// shell settles. `settle_primary_screen_exit()` is called explicitly
/// (no 200ms wall-clock wait).
#[test]
fn fixture_clude_double_ctrl_c_exit() {
    let mut terminal = Terminal::new(6, 64);
    terminal.process(b"\x1b]7;file://localhost/Users/me/project\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // TUI paints a colored answer, then exits (OSC 133;D).
    terminal.process(b"\x1b[2J\x1b[H\x1b[38;2;222;120;80manswer\x1b[0m");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    assert!(terminal.block_tracker().blocks().is_empty());

    // Late resume tail arrives before settle.
    terminal.process(
        b"\x1b[3;1HPress Ctrl-C again to exit\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume late-id",
    );
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert_eq!(block.cwd.as_deref(), Some("/Users/me/project"));
    assert!(block.output.contains("answer"));
    assert!(block.output.contains("Press Ctrl-C again to exit"));
    assert!(block.output.contains("claude --resume late-id"));

    // Styled output preserves the RGB color from the TUI paint.
    let styled = block
        .styled_output
        .as_ref()
        .expect("screen snapshot styles");
    assert!(styled
        .line(0)
        .and_then(|line| line.foreground_at(0))
        .is_some_and(|color| matches!(color, weft_core::grid::CellColor::Rgb(_))));
}

/// Fixture 3: Late resume tail replaces stale row suffixes. The TUI exits,
/// a stale suffix lingers on the resume line, then the real resume command
/// overwrites it. `settle_primary_screen_exit()` confirms the stale suffix
/// is gone and the clean resume command is captured.
#[test]
fn fixture_late_resume_tail_replaces_stale_suffix() {
    let mut terminal = Terminal::new(7, 72);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // Stale suffix on the resume line.
    terminal.process(b"\x1b[2J\x1b[Hanswer\x1b[4;1HResume this session with: stale answer suffix");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());

    // Clean resume command overwrites the stale suffix.
    terminal.process(b"\x1b[4;1HResume this session with:\x1b[5;1Hclaude --resume clean-id");
    assert_eq!(terminal.grid().row_text(3), "Resume this session with:");
    terminal.settle_primary_screen_exit();

    let block = terminal.block_tracker().blocks().last().unwrap();
    assert!(
        !block.output.contains("stale answer suffix"),
        "stale suffix survived: {:?}",
        block.output
    );
    assert!(block.output.contains("claude --resume clean-id"));
}

/// Fixture 4: Primary-screen TUI resize survival. A primary-screen app is
/// active, then the terminal is resized. The ownership mask and primary
/// screen state must survive the resize so subsequent output is rendered
/// correctly (not treated as shell output).
#[test]
fn fixture_primary_screen_tui_resize_survival() {
    let mut terminal = Terminal::new(6, 40);
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(terminal.primary_screen_app_active());

    // Ownership mask exists before resize.
    let ownership_before = terminal
        .primary_screen_viewport_ownership()
        .map(|slice| slice.to_vec());
    assert!(
        ownership_before.is_some(),
        "ownership mask should exist before resize"
    );

    // Resize: the terminal must sync the ownership mask to the new dimensions.
    terminal.resize(10, 60);
    assert!(
        terminal.primary_screen_app_active(),
        "primary screen must still be active after resize"
    );

    // Ownership mask survives resize (resized to new row count).
    let ownership_after = terminal
        .primary_screen_viewport_ownership()
        .map(|slice| slice.to_vec());
    assert!(
        ownership_after.is_some(),
        "ownership mask should survive resize"
    );
    assert_eq!(
        ownership_after.as_ref().unwrap().len(),
        10,
        "ownership mask must match new row count"
    );

    // Post-resize output is still processed correctly.
    terminal.process(b"\x1b[5;1Hpost-resize line");
    assert!(
        terminal.grid().row_text(4).contains("post-resize line"),
        "post-resize output should be rendered: {:?}",
        terminal.grid().row_text(4)
    );
    assert!(terminal.primary_screen_app_active());
}

/// Fixture 5 (v1.7.4 Phase A): Verify vte 0.13.1 safely handles APC sequences
/// (ESC _ ... ST) without breaking OSC/DCS/UTF-8/print.
///
/// Kitty Graphics protocol uses `ESC _ G <payload> ESC \` (APC with 'G'
/// identifier). vte 0.13.1 routes ESC _ into the `SosPmApcString` state,
/// which uses `Ignore` action for all payload bytes — they are silently
/// dropped, NOT dispatched to any handler. This fixture confirms:
///
/// 1. APC payload does not leak onto the grid as text
/// 2. APC does not break subsequent OSC 133 / print / CSI sequences
/// 3. APC does not corrupt CJK UTF-8 handling
/// 4. BEL (0x07) does NOT terminate APC in vte 0.13.1 (only ST does)
///
/// This is the Phase A decision-gate test per V17_IMPLEMENTATION_PLAN §6.
#[test]
fn fixture_apc_kitty_graphics_safety() {
    let mut t = Terminal::new(6, 40);

    // --- Test 1: APC with Kitty-style payload, ST-terminated ---
    // ESC _ G a=T,f=24,s=1,v=1;<base64-payload> ESC \
    // The entire payload must be silently dropped — no text on grid.
    t.process(b"\x1b_G a=T,f=24,s=1,v=1;iVBORw0KGgoAAAANS\x1b\\");
    assert_eq!(
        t.grid().row_text(0),
        "",
        "APC payload must not leak onto grid"
    );
    assert_eq!(t.grid().cursor.col, 0, "cursor unchanged by APC");

    // --- Test 2: APC does not break subsequent OSC 133 ---
    t.process(b"\x1b]133;A\x07echo hello\x1b]133;B\x07");
    assert_eq!(t.grid().row_text(0), "echo hello");

    // --- Test 3: APC does not break CJK UTF-8 ---
    t.process(b"\x1b[2;1H");
    t.process(b"\x1b_G q=1\x1b\\\xe6\xb5\x8b\xe8\xaf\x95");
    assert_eq!(
        t.grid().row_text(1),
        "测试",
        "CJK after APC renders correctly"
    );
    assert_no_orphaned_wide_cells(&t);

    // --- Test 4: APC does not break CSI cursor movement ---
    t.process(b"\x1b[3;1H");
    t.process(b"\x1b_G t=d\x1b\\\x1b[5GABC");
    assert_eq!(t.grid().row_text(2), "    ABC");

    // --- Test 5: Multiple consecutive APCs are all dropped ---
    t.process(b"\x1b[4;1H");
    t.process(b"\x1b_G a=T\x1b\\\x1b_G a=T\x1b\\\x1b_G a=T\x1b\\text");
    assert_eq!(t.grid().row_text(3), "text");

    // --- Test 6: APC with BEL termination ---
    // vte 0.13.1's SosPmApcString state treats 0x07 as Ignore, NOT as a
    // terminator. So BEL does NOT end the APC — the payload continues
    // until ST (ESC \). Verify this by checking that text after BEL-but-
    // before-ST is still consumed by the APC state.
    t.process(b"\x1b[5;1H");
    t.process(b"\x1b_G payload\x07still-inside-apc\x1b\\visible");
    assert_eq!(
        t.grid().row_text(4),
        "visible",
        "BEL does not terminate APC; only ST does"
    );

    // --- Test 7: APC interspersed with DCS (ESC P) ---
    // Both must be independently handled.
    t.process(b"\x1b[6;1H");
    t.process(b"\x1b_G apc_payload\x1b\\\x1bPdcs_payload\x1b\\after");
    assert_eq!(t.grid().row_text(5), "after");
}

/// Fixture 6 (v1.7.4 Phase A): Verify APC with large payload (exceeding
/// any reasonable limit) does not cause panic or unbounded memory growth
/// in vte 0.13.1. Since vte uses `Ignore` action for APC bytes, no buffer
/// accumulates — the payload is discarded byte-by-byte.
#[test]
fn fixture_apc_large_payload_no_panic() {
    let mut t = Terminal::new(4, 40);

    // Construct a 256 KiB APC payload — far larger than any real Kitty
    // Graphics image chunk. vte should discard all bytes without panic.
    let mut bytes = Vec::with_capacity(256 * 1024 + 4);
    bytes.extend_from_slice(b"\x1b_G a=T;");
    bytes.extend(std::iter::repeat(b'X').take(256 * 1024));
    bytes.extend_from_slice(b"\x1b\\");
    t.process(&bytes);

    // Grid must be clean — no payload leaked.
    for row in 0..4 {
        assert_eq!(t.grid().row_text(row), "", "row {row} leaked APC payload");
    }

    // Subsequent output must work normally.
    t.process(b"after-apc");
    assert_eq!(t.grid().row_text(0), "after-apc");
}
