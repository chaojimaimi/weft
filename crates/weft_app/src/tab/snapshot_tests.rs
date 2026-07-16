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
