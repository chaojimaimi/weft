//! v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M2 / §四 测试矩阵): headless
//! app-side tests for the screen-owned-session render policy (M2.3).
//!
//! The app's real stdin 挂点 are `App::forward_key_to_pty` (keyboard outbound,
//! app/keyboard.rs) and `apply_paste_text` (app/effect_dispatch.rs); both call
//! `Terminal::note_interactive_stdin` exactly like these tests do through the
//! pane. Driving the full `App` would need a winit event loop, so the tab +
//! Terminal level is the repo's established headless harness (`tab_with_terminal`).

use super::*;
use weft_core::vt::TuiRenderMode;

fn tab_with_terminal_ni(scrollback_lines: usize) -> Tab {
    let mut tab = Tab::with_single_pane(Pane::with_terminal_only(scrollback_lines));
    // The factory injects `[experimental] tui_render_mode` — mirror the
    // `config_controller::apply_tui_render_mode` chokepoint.
    crate::config_controller::apply_tui_render_mode(&mut tab, TuiRenderMode::Noninteractive);
    tab
}

/// The F6 "uv class" fixture: shell integration + multi-frame in-place
/// progress repaint (two `\x1b[2A` ops cross the >= 2 detection threshold).
fn uv_progress_bytes(terminal: &mut Terminal) {
    terminal.process(b"\x1b]133;A\x07uv sync\n\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"Resolving packages...\nResolving packages... [1/2]\n");
    terminal.process("\x1b[2A\r\x1b[KPreparing… (1/3)\n\x1b[KPreparing… (2/3)".as_bytes());
    terminal.process("\x1b[2A\r\x1b[KPreparing… (2/3)\n\x1b[KPreparing… (3/3)".as_bytes());
}

#[test]
fn uv_class_byte_stream_keeps_block_view_with_block_head() {
    // M2.3 scenario 1: a noninteractive-tier progress command runs entirely
    // as a block — the block head (command string) stays intact and the
    // view never flips to the live grid mid-session.
    let mut tab = tab_with_terminal_ni(1000);
    let terminal = tab.terminal.as_mut().unwrap();
    uv_progress_bytes(terminal);
    assert!(
        terminal.show_block_view(),
        "screen-owned progress session must keep the block view (noninteractive)"
    );
    let live = terminal
        .block_tracker()
        .in_flight()
        .expect("command in flight");
    assert_eq!(live.command, "uv sync", "block head command string intact");
    // Neither an interactive-stdin note nor settle window dropped the block.
    terminal.note_interactive_stdin();
    assert!(
        !terminal.show_block_view(),
        "stdin exemption flips the session back to the classic takeover"
    );
}

#[test]
fn typed_key_while_screen_owned_returns_to_grid_view() {
    // M2.3 scenario 2: the first real keystroke of an interactive TUI
    // (openclaw pattern) switches the renderer back to the classic live-grid
    // takeover — the keyboard exit calls note_interactive_stdin on the pane
    // exactly like `App::forward_key_to_pty` does.
    let mut tab = tab_with_terminal_ni(1000);
    {
        let terminal = tab.terminal.as_mut().unwrap();
        terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07openclaw\x1b]133;C\x07");
        terminal.process("\x1b[999D\x1b[915A\x1b[1A".as_bytes());
        assert!(
            terminal.show_block_view(),
            "screen-owned, no stdin yet — blocks"
        );
        assert_eq!(
            terminal.screen_owner(),
            weft_core::vt::ScreenOwner::PrimaryScreenApp
        );
    }
    // The keystroke (any non-empty encoding — the pane-level call that
    // `App::forward_key_to_pty` makes).
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .note_interactive_stdin();
    assert!(
        !tab.terminal.as_ref().unwrap().show_block_view(),
        "first keystroke returns the TUI to the classic live-grid takeover"
    );
    // The flag lives only for this command; the next command starts clean.
    tab.terminal.as_mut().unwrap().process(b"\x1b]133;D;0\x07");
    tab.terminal.as_mut().unwrap().settle_primary_screen_exit();
    assert!(
        !tab.terminal.as_ref().unwrap().interactive_stdin_seen(),
        "command boundary clears the interactive-stdin flag"
    );
}

#[test]
fn mouse_protocol_enabled_uses_classic_takeover_without_stdin() {
    // M2.3 scenario 3 (P1-3): mouse reporting is an interaction capability
    // declaration — a screen-owned session with DEC 1002 armed goes straight
    // to the classic takeover even with zero stdin bytes (the block view
    // would swallow wheel events meant for the TUI).
    let mut tab = tab_with_terminal_ni(1000);
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07claude\x1b]133;C\x07");
    terminal.process(b"\x1b[H\x1b[2;1H");
    assert!(
        terminal.show_block_view(),
        "screen-owned, no mouse yet — blocks"
    );
    terminal.process(b"\x1b[?1002h");
    assert_eq!(
        terminal.mouse_protocol(),
        weft_core::input::MouseProtocol::ButtonEvent
    );
    assert!(
        !terminal.show_block_view(),
        "mouse reporting exempts the session back to classic immediately"
    );
    assert!(
        !terminal.interactive_stdin_seen(),
        "the mouse exemption is orthogonal to the stdin flag"
    );
}

#[test]
fn history_scroll_into_and_back_keeps_block_view_semantics() {
    // M2.3 scenario 4: scrolling into primary-history view and back to the
    // bottom keeps the block view in both states (history browsing always
    // shows blocks; back at the live bottom the screen-owned session resumes
    // its block view under the noninteractive tier).
    let mut tab = tab_with_terminal_ni(1000);
    let terminal = tab.terminal.as_mut().unwrap();
    uv_progress_bytes(terminal);
    assert!(terminal.show_block_view());
    terminal.set_primary_history_view(true);
    assert!(
        terminal.show_block_view(),
        "history browsing shows the block document"
    );
    terminal.set_primary_history_view(false);
    assert!(
        terminal.show_block_view(),
        "back at the live bottom — screen-owned session still renders blocks"
    );
}

#[test]
fn decrqss_probe_does_not_set_interactive_stdin() {
    // M2.3 / M1.1 exclusion list (P1-1): automatic replies never touch the
    // stdin flag. A DECRQSS probe is answered by replies.rs via the PTY
    // write layer — exactly the traffic that must NOT exempt a progress
    // command back to classic.
    let mut terminal = Terminal::new(24, 80);
    terminal.set_tui_render_mode(TuiRenderMode::Noninteractive);
    uv_progress_bytes(&mut terminal);
    assert!(terminal.show_block_view());
    // Probe the current SGR attrs (the DECRQSS request bytes as an app
    // would send them) — the reply is generated, the flag must stay false.
    terminal.process(b"\x1bP$q1;m\x1b\\");
    assert!(
        !terminal.interactive_stdin_seen(),
        "DECRQSS auto-answer must not count as interactive stdin"
    );
    assert!(
        terminal.show_block_view(),
        "progress session still renders as a block after the probe"
    );
}

#[test]
fn terminal_defaults_to_classic_without_config_injection() {
    // P2-3 baseline anchor: `Terminal::new` (and a bare Pane) stay Classic —
    // the ~40 existing show_block_view assertions and the 2750-test baseline
    // depend on it. The noninteractive tier only exists after the config
    // chokepoint injects it.
    let tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
    let terminal = tab.terminal.as_ref().unwrap();
    assert_eq!(terminal.tui_render_mode(), TuiRenderMode::Classic);
}
