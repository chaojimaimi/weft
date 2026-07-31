use super::*;
use weft_core::editor::EditorBuffer;
use weft_core::persistence::TabSnapshot;

fn tab_with_terminal() -> Tab {
    let mut tab = Tab::empty();
    tab.terminal = Some(Terminal::with_scrollback(24, 80, 1000));
    tab
}

#[test]
fn empty_tab_starts_without_transient_pending_state() {
    let tab = Tab::empty();
    assert!(tab.pending_pty_resize.is_none());
    assert!(tab.pending_pty_output.is_none());
    assert!(tab.ime_preedit.is_empty());
}

#[test]
fn terminal_resize_replaces_stale_pending_size_for_background_convergence() {
    let mut tab = tab_with_terminal();
    tab.pending_pty_resize = Some((40, 120));

    assert!(tab.resize_terminal_and_queue(22, 78));
    assert_eq!(tab.pending_pty_resize, Some((22, 78)));
    let grid = tab.terminal.as_ref().unwrap().grid();
    assert_eq!((grid.num_rows, grid.num_cols), (22, 78));
}

fn snapshot(cwd: &str, editor: &EditorBuffer) -> TabSnapshot {
    TabSnapshot {
        position: 0,
        active: true,
        cwd: Some(cwd.to_string()),
        block_scroll_offset: 0,
        editor_buffer: TabSnapshot::encode_editor_buffer(editor),
        shell_phase: "AtPrompt".to_string(),
        block_ids: Vec::new(),
    }
}

#[test]
fn restored_cwd_survives_until_terminal_reports_authoritative_cwd() {
    let mut tab = tab_with_terminal();
    let restored = snapshot("/restored/project", &EditorBuffer::new());

    assert!(tab.restore_from_snapshot(&restored));
    assert_eq!(tab.terminal.as_ref().unwrap().cwd(), None);
    assert_eq!(
        tab.to_snapshot(0, true).unwrap().cwd.as_deref(),
        Some("/restored/project")
    );
}

#[test]
fn restored_snapshot_survives_when_terminal_and_pty_are_missing() {
    let mut tab = Tab::empty();
    let mut editor = EditorBuffer::new();
    editor.set_text("unfinished");
    let mut restored = snapshot("/missing/project", &editor);
    restored.position = 2;
    restored.block_scroll_offset = 9;

    assert!(!tab.restore_from_snapshot(&restored));
    let saved = tab
        .to_snapshot(1, false)
        .expect("restored fallback must survive a failed PTY restore");
    assert_eq!(saved.position, 1);
    assert!(!saved.active);
    assert_eq!(saved.cwd.as_deref(), Some("/missing/project"));
    assert_eq!(saved.block_scroll_offset, 9);
    assert_eq!(saved.editor_buffer, restored.editor_buffer);
    assert_eq!(saved.shell_phase, restored.shell_phase);
}

#[test]
fn live_osc7_cwd_overrides_restored_fallback() {
    let mut tab = tab_with_terminal();
    let restored = snapshot("/restored/project", &EditorBuffer::new());

    assert!(tab.restore_from_snapshot(&restored));
    tab.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b]7;file://localhost/live/project\x1b\\");

    assert_eq!(
        tab.to_snapshot(0, true).unwrap().cwd.as_deref(),
        Some("/live/project")
    );
}

#[test]
fn new_tab_launch_cwd_prefers_live_then_restored_state() {
    let mut tab = tab_with_terminal();
    let restored = snapshot("/restored/project", &EditorBuffer::new());
    assert!(tab.restore_from_snapshot(&restored));
    assert_eq!(tab.launch_cwd(), Some("/restored/project"));

    tab.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b]7;file://localhost/live/project\x1b\\");
    assert_eq!(tab.launch_cwd(), Some("/live/project"));
}

#[test]
fn queued_alt_screen_teardown_output_is_preserved() {
    let mut tab = tab_with_terminal();
    tab.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
    tab.msg_tx
        .send(AppMsg::PtyOutput(b"\x1b[?1049lresume hint".to_vec()))
        .unwrap();

    let (_, _, need_redraw) = tab.process_messages();

    assert!(need_redraw);
    assert!(!tab.terminal.as_ref().unwrap().is_alt_screen_active());
    assert!(tab
        .terminal
        .as_ref()
        .unwrap()
        .grid()
        .row_text(0)
        .contains("resume hint"));
}

#[test]
fn queued_primary_tui_teardown_becomes_a_screen_snapshot() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07old linear output\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());

    tab.msg_tx
        .send(AppMsg::PtyOutput(
            b"\x1b[2J\x1b[HResume this session with:\x1b[2;1Hscreen-app --resume abc\x1b]133;D;130\x07\x1b]133;A\x07"
                .to_vec(),
        ))
        .unwrap();

    let (_, blocks, need_redraw) = tab.process_messages();

    assert!(need_redraw);
    assert!(
        blocks.is_empty(),
        "the screen tail must settle before freezing"
    );
    assert!(tab.terminal.as_ref().unwrap().primary_screen_exit_pending());
    tab.terminal.as_mut().unwrap().settle_primary_screen_exit();
    let (_, blocks, _) = tab.process_messages();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command, "screen-app");
    assert_eq!(
        blocks[0].output.as_ref(),
        "Resume this session with:\nscreen-app --resume abc"
    );
    assert!(!blocks[0].output.contains("old linear output"));
}

#[test]
fn failed_interrupt_does_not_drop_queued_output_or_reset_shell_phase() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert_eq!(
        terminal.block_tracker().phase(),
        weft_core::blocks::ShellPhase::CommandExecuting
    );
    tab.msg_tx
        .send(AppMsg::PtyOutput(b"still-running-tail".to_vec()))
        .unwrap();

    assert!(
        !tab.interrupt_pty(),
        "a tab without a PTY cannot deliver ETX"
    );
    let (_, _, need_redraw) = tab.process_messages();

    assert!(need_redraw);
    assert_eq!(
        tab.terminal.as_ref().unwrap().block_tracker().phase(),
        weft_core::blocks::ShellPhase::CommandExecuting
    );
    assert!(tab
        .terminal
        .as_ref()
        .unwrap()
        .block_tracker()
        .in_flight()
        .is_some_and(|live| live.output.contains("still-running-tail")));
}

#[test]
fn failed_interrupt_rolls_back_primary_screen_freeze() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"old frame before failed interrupt");

    assert!(!tab.interrupt_pty(), "tab intentionally has no PTY");
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[Hnew frame after failed interrupt\x1b[?2026l");
    terminal.process(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    terminal.settle_primary_screen_exit();

    let output = terminal
        .block_tracker()
        .blocks()
        .last()
        .unwrap()
        .output
        .as_ref();
    assert!(output.contains("new frame after failed interrupt"));
    assert!(!output.contains("old frame before failed interrupt"));
}

#[test]
fn pty_exit_force_settles_the_late_primary_tui_resume_tail() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());

    tab.msg_tx
        .send(AppMsg::PtyOutput(
            b"\x1b]133;D;130\x07\x1b]133;A\x07\x1b[2J\x1b[HPress Ctrl-C again to exit\x1b[2;1HResume this session with:\x1b[3;1Hscreen-app --resume late"
                .to_vec(),
        ))
        .unwrap();
    tab.msg_tx.send(AppMsg::PtyExit(Ok(130))).unwrap();

    let (alive, blocks, need_redraw) = tab.process_messages();

    assert!(!alive);
    assert!(need_redraw);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command, "screen-app");
    assert_eq!(
        blocks[0].output.as_ref(),
        "Press Ctrl-C again to exit\n\nResume this session with:\nscreen-app --resume late"
    );
}

#[test]
fn oversized_output_remainder_stays_ahead_of_queued_pty_exit() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H");

    let mut output = vec![b'x'; 256 * 1024 + 1];
    output.extend_from_slice(
        b"\x1b]133;D;130\x07\x1b]133;A\x07\x1b[2J\x1b[HResume this session with:\x1b[2;1Hscreen-app --resume ordered",
    );
    tab.msg_tx.send(AppMsg::PtyOutput(output)).unwrap();
    tab.msg_tx.send(AppMsg::PtyExit(Ok(130))).unwrap();

    let (alive, blocks, _) = tab.process_messages();
    assert!(alive);
    assert!(blocks.is_empty());
    assert!(tab.pending_pty_output.is_some());

    let (alive, blocks, _) = tab.process_messages();
    assert!(!alive);
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0]
        .output
        .ends_with("Resume this session with:\nscreen-app --resume ordered"));
}

#[test]
fn closing_a_tab_force_settles_its_pending_primary_tui_block() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal
        .process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    tab.msg_tx
        .send(AppMsg::PtyOutput(b"\x1b[2J\x1b[Hlate close tail".to_vec()))
        .unwrap();

    let blocks = tab.finish_pending_blocks();

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command, "screen-app");
    assert_eq!(blocks[0].output.as_ref(), "late close tail");
    assert!(!tab.terminal.as_ref().unwrap().primary_screen_exit_pending());
}

#[test]
fn closing_a_primary_tui_does_not_chase_an_unbounded_output_producer() {
    let mut tab = tab_with_terminal();
    let terminal = tab.terminal.as_mut().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal
        .process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    for _ in 0..=super::lifecycle::MAX_CLOSE_TAIL_EVENTS {
        tab.msg_tx.send(AppMsg::PtyOutput(b"x".to_vec())).unwrap();
    }

    let blocks = tab.finish_pending_blocks();

    assert_eq!(blocks.len(), 1);
    assert_eq!(
        tab.msg_rx.len(),
        1,
        "close must process a fixed queue snapshot"
    );
}

#[test]
fn synchronized_output_suppresses_partial_frame_until_commit() {
    let mut tab = tab_with_terminal();
    tab.msg_tx
        .send(AppMsg::PtyOutput(b"\x1b[?2026hpartial".to_vec()))
        .unwrap();
    let (_, _, partial_redraw) = tab.process_messages();
    assert!(!partial_redraw);

    tab.msg_tx
        .send(AppMsg::PtyOutput(b" frame\x1b[?2026l".to_vec()))
        .unwrap();
    let (_, _, committed_redraw) = tab.process_messages();
    assert!(committed_redraw);
    assert!(tab
        .terminal
        .as_ref()
        .unwrap()
        .grid()
        .row_text(0)
        .contains("partial frame"));
}
