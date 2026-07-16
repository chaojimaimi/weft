use super::*;
use weft_core::editor::EditorBuffer;
use weft_core::persistence::TabSnapshot;

fn tab_with_terminal() -> Tab {
    let mut tab = Tab::empty();
    tab.terminal = Some(Terminal::with_scrollback(24, 80, 1000));
    tab
}

fn snapshot(cwd: &str, editor: &EditorBuffer) -> TabSnapshot {
    TabSnapshot {
        position: 0,
        active: true,
        cwd: Some(cwd.to_string()),
        block_scroll_offset: 0,
        editor_buffer: TabSnapshot::encode_editor_buffer(editor),
        shell_phase: "AtPrompt".to_string(),
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
fn interrupt_cleanup_preserves_alt_screen_teardown_output() {
    let mut tab = tab_with_terminal();
    tab.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
    tab.msg_tx
        .send(AppMsg::PtyOutput(b"\x1b[?1049lresume hint".to_vec()))
        .unwrap();

    tab.flush_pty_output();
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
