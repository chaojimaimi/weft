use super::*;
use weft_core::editor::EditorBuffer;
use weft_core::persistence::TabSnapshot;
use weft_core::vt::Terminal;

fn tab_with_terminal() -> Tab {
    let mut tab = Tab::empty();
    tab.set_terminal_for_test(Terminal::with_scrollback(24, 80, 1000));
    tab
}

/// v1.12.28 (P1-02 ②): single-pane tabs must NOT write the `panes` field —
/// the single-pane snapshot path is byte-identical to v1.12.27 apart from
/// the new (absent) field, so old readers and the single-pane restore path
/// see no change.
#[test]
fn to_snapshot_single_pane_omits_panes_field() {
    let tab = tab_with_terminal();
    let snap = tab.to_snapshot(0, true).unwrap();
    assert!(
        snap.panes.is_none(),
        "single-pane tabs must not persist a pane tree"
    );
}

/// v1.12.28 (P1-02 ②): a multi-pane tab persists the split structure tree —
/// direction/ratio at the split node, one Pane leaf per live pane in DFS
/// order, and the active leaf's DFS index (the freshly split pane becomes
/// active, so index 1 of [root, new]).
#[test]
fn to_snapshot_multi_pane_writes_split_tree_with_active_leaf_index() {
    use weft_core::persistence::SnapshotPaneNode;
    let mut tab = tab_with_terminal();
    tab.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .expect("test split succeeds");
    let snap = tab.to_snapshot(0, true).expect("multi-pane tab serializes");
    let panes = snap
        .panes
        .expect("a multi-pane tab must persist its pane tree");
    assert_eq!(panes.active_leaf, 1, "the new pane is active (DFS index 1)");
    match panes.tree {
        SnapshotPaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            assert_eq!(direction, SplitDirection::Vertical);
            assert!((ratio - 0.5).abs() < 1e-6);
            match *first {
                SnapshotPaneNode::Pane {
                    cwd,
                    editor_buffer,
                    block_ids,
                } => {
                    assert_eq!(cwd, None, "no OSC 7 yet → cwd None");
                    // A fresh terminal's empty EditorBuffer still encodes as
                    // a JSON object — assert it decodes back cleanly.
                    assert!(TabSnapshot::decode_editor_buffer(&editor_buffer).is_some());
                    assert!(block_ids.is_empty());
                }
                other => panic!("expected a Pane leaf, got {other:?}"),
            }
            assert!(matches!(*second, SnapshotPaneNode::Pane { .. }));
        }
        other => panic!("expected a Split root, got {other:?}"),
    }
}

/// v1.12.28 (P1-02 ②): the PaneTree → SnapshotPaneNode fold pairs leaves in
/// DFS order (first child before second) and records the active pane's DFS
/// index — here a 3-leaf tree with a nested Split whose ACTIVE pane is the
/// second child's first leaf (index 1 of [A, B, C]).
#[test]
fn snapshot_tree_conversion_pairs_leaves_in_dfs_order() {
    use weft_core::persistence::SnapshotPaneNode;
    let leaf = |id: u64, cwd: &str| {
        (
            PaneId(id),
            SnapshotPaneNode::Pane {
                cwd: Some(cwd.to_string()),
                editor_buffer: String::new(),
                block_ids: Vec::new(),
            },
        )
    };
    // Split { first: Leaf(A), second: Split { first: Leaf(B), second: Leaf(C) } }
    let tree = PaneTree::Split {
        direction: SplitDirection::Vertical,
        ratio: 0.5,
        first: Box::new(PaneTree::Leaf(leaf(1, "/a"))),
        second: Box::new(PaneTree::Split {
            direction: SplitDirection::Horizontal,
            ratio: 0.4,
            first: Box::new(PaneTree::Leaf(leaf(2, "/b"))),
            second: Box::new(PaneTree::Leaf(leaf(3, "/c"))),
        }),
    };
    let mut index = 0usize;
    let mut active_leaf = 0usize;
    let node = snapshot_node_from_tree(tree, PaneId(2), &mut index, &mut active_leaf);
    assert_eq!(
        active_leaf, 1,
        "DFS order is A=0, B=1, C=2 (first before second)"
    );
    assert_eq!(index, 3, "all three leaves folded");
    match node {
        SnapshotPaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            assert_eq!(direction, SplitDirection::Vertical);
            assert!((ratio - 0.5).abs() < 1e-6);
            match *first {
                SnapshotPaneNode::Pane { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/a")),
                other => panic!("expected leaf A, got {other:?}"),
            }
            match *second {
                SnapshotPaneNode::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    assert_eq!(direction, SplitDirection::Horizontal);
                    assert!((ratio - 0.4).abs() < 1e-6);
                    match (*first, *second) {
                        (
                            SnapshotPaneNode::Pane { cwd: b, .. },
                            SnapshotPaneNode::Pane { cwd: c, .. },
                        ) => {
                            assert_eq!(b.as_deref(), Some("/b"));
                            assert_eq!(c.as_deref(), Some("/c"));
                        }
                        _ => panic!("expected leaves B and C"),
                    }
                }
                other => panic!("expected nested split, got {other:?}"),
            }
        }
        other => panic!("expected split root, got {other:?}"),
    }
}

#[test]
fn empty_tab_starts_without_transient_pending_state() {
    let tab = Tab::empty();
    assert!(tab.pending_pty_resize.is_none());
    assert!(tab.ime_preedit.is_empty());
}

#[test]
fn terminal_resize_queues_latest_size_without_desynchronizing_grid() {
    let mut tab = tab_with_terminal();
    tab.pending_pty_resize = Some((40, 120));

    assert!(tab.active_mut().resize_terminal_and_queue(22, 78));
    assert_eq!(tab.pending_pty_resize, Some((22, 78)));
    let grid_dims = tab.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols));
    assert_eq!(
        grid_dims,
        Some((24, 80)),
        "Grid geometry must stay paired with the old PTY until ResizePty commits"
    );
}

#[test]
fn pending_resize_reports_each_panes_synchronized_frame_state() {
    let mut tab = tab_with_terminal();
    tab.pending_pty_resize = Some((30, 100));
    tab.lock_terminal().unwrap().process(b"\x1b[?2026hpartial");

    assert!(tab.any_synchronized_output());
    let pending = tab.pending_pane_resizes();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].dimensions(), (30, 100));
    assert!(pending[0].is_synchronized());

    tab.lock_terminal().unwrap().process(b" frame\x1b[?2026l");
    assert!(!tab.any_synchronized_output());
    assert!(!tab.pending_pane_resizes()[0].is_synchronized());
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
        panes: None,
    }
}

/// v1.12.24 (N-3): a tab that loaded a previous generation's history and then
/// produced new blocks must serialize BOTH generations into its snapshot —
/// the v1.7.6 session-only snapshot dropped the loaded ids, so every
/// restart-restore cycle lost one ↑ recall generation (user-reproduced).
#[test]
fn to_snapshot_block_ids_carry_full_lineage_across_generations() {
    use std::time::{Duration, SystemTime};
    use weft_core::blocks::{Block, BlockId};

    let tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    let mk = |id: u64, cmd: &str| Block {
        id: BlockId(id),
        command: cmd.to_string(),
        cwd: None,
        output: String::new().into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        finished_at: None,
        collapsed: false,
        screen_origin: false,
    };
    // Previous generation loaded at startup/Restore (ids 1-3)...
    terminal
        .block_tracker_mut()
        .load_blocks(vec![mk(1, "one"), mk(2, "two"), mk(3, "three")]);
    // ...then this session produces "four" (id 4 via the observed sequence).
    let tracker = terminal.block_tracker_mut();
    tracker.on_prompt_start();
    tracker.on_command_start("four".to_string());
    tracker.on_command_end(0);
    // T10 P1 (D9 rule 2): release the guard before to_snapshot re-locks.
    drop(terminal);

    let snap = tab.to_snapshot(0, true).unwrap();
    assert_eq!(snap.block_ids, vec![1, 2, 3, 4]);
}

#[test]
fn restored_cwd_survives_until_terminal_reports_authoritative_cwd() {
    let mut tab = tab_with_terminal();
    let restored = snapshot("/restored/project", &EditorBuffer::new());

    assert!(tab.restore_from_snapshot(&restored));
    assert_eq!(tab.lock_terminal().unwrap().cwd(), None);
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
    tab.lock_terminal()
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
    assert_eq!(tab.launch_cwd().as_deref(), Some("/restored/project"));

    tab.lock_terminal()
        .unwrap()
        .process(b"\x1b]7;file://localhost/live/project\x1b\\");
    assert_eq!(tab.launch_cwd().as_deref(), Some("/live/project"));
}

/// v1.10.24 B1: the workspace-restore cwd fallback (`restored_cwd`) drives
/// `launch_cwd` before the shell reports OSC 7 — the same semantics the old
/// stub-snapshot fallback provided (regression guard for the fallback path).
#[test]
fn launch_cwd_uses_restored_cwd_fallback_before_osc7() {
    let mut tab = tab_with_terminal();
    tab.set_restored_cwd_fallback(Some("/restored/project".into()));
    assert_eq!(
        tab.launch_cwd().as_deref(),
        Some("/restored/project"),
        "fallback cwd must drive launch_cwd before OSC 7"
    );

    tab.lock_terminal()
        .unwrap()
        .process(b"\x1b]7;file://localhost/live/project\x1b\\");
    assert_eq!(tab.launch_cwd().as_deref(), Some("/live/project"));
}

/// v1.10.24 B1: a PTY-dead workspace-restored pane (terminal None, no real
/// snapshot, only the cwd fallback) must still serialize via `to_snapshot`.
/// Before the fix this branch relied on the stub snapshot being present.
#[test]
fn to_snapshot_keeps_pty_dead_pane_serializable_with_cwd_fallback_only() {
    let mut tab = Tab::empty();
    tab.set_restored_cwd_fallback(Some("/workspace/dir".into()));

    let saved = tab
        .to_snapshot(1, false)
        .expect("the cwd fallback must keep a PTY-dead pane serializable");
    assert_eq!(saved.position, 1);
    assert!(!saved.active);
    assert_eq!(saved.cwd.as_deref(), Some("/workspace/dir"));
    assert!(saved.block_ids.is_empty());
    assert_eq!(saved.shell_phase, "AtPrompt");
}

#[test]
fn queued_alt_screen_teardown_output_is_preserved() {
    let mut tab = tab_with_terminal();
    tab.lock_terminal().unwrap().process(b"\x1b[?1049h");
    // T10 P2: output is injected through the worker-shaped test seam.
    tab.feed_pty_output_for_test(tab.active_pane_id(), b"\x1b[?1049lresume hint");

    let (_, _, need_redraw, _) = tab.process_messages();

    assert!(need_redraw);
    assert!(!tab.lock_terminal().unwrap().is_alt_screen_active());
    assert!(tab
        .lock_terminal()
        .unwrap()
        .grid()
        .row_text(0)
        .contains("resume hint"));
}

#[test]
fn queued_primary_tui_teardown_becomes_a_screen_snapshot() {
    let mut tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07old linear output\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());
    // T10 P1 (D9 rule 2): release the guard before the tab-level pump —
    // process_messages re-locks this pane's terminal.
    drop(terminal);

    tab.feed_pty_output_for_test(
        tab.active_pane_id(),
        b"\x1b[2J\x1b[HResume this session with:\x1b[2;1Hscreen-app --resume abc\x1b]133;D;130\x07\x1b]133;A\x07",
    );

    let (_, blocks, need_redraw, _) = tab.process_messages();

    assert!(need_redraw);
    assert!(
        blocks.is_empty(),
        "the screen tail must settle before freezing"
    );
    assert!(tab.lock_terminal().unwrap().primary_screen_exit_pending());
    tab.lock_terminal().unwrap().settle_primary_screen_exit();
    let (_, blocks, _, _) = tab.process_messages();
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
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
    assert_eq!(
        terminal.block_tracker().phase(),
        weft_core::blocks::ShellPhase::CommandExecuting
    );
    // T10 P1 (D9 rule 2): release the guard before interrupt_pty/process_messages.
    drop(terminal);
    tab.feed_pty_output_for_test(tab.active_pane_id(), b"still-running-tail");

    assert!(
        !tab.interrupt_pty(),
        "a tab without a PTY cannot deliver ETX"
    );
    let (_, _, need_redraw, _) = tab.process_messages();

    assert!(need_redraw);
    assert_eq!(
        tab.lock_terminal().unwrap().block_tracker().phase(),
        weft_core::blocks::ShellPhase::CommandExecuting
    );
    assert!(tab
        .lock_terminal()
        .unwrap()
        .block_tracker()
        .in_flight()
        .is_some_and(|live| live.output.contains("still-running-tail")));
}

#[test]
fn failed_interrupt_rolls_back_primary_screen_freeze() {
    let mut tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[H\x1b[2;1H");
    terminal.process(b"old frame before failed interrupt");
    // T10 P1 (D9 rule 2): release the guard before interrupt_pty re-locks.
    drop(terminal);

    assert!(!tab.interrupt_pty(), "tab intentionally has no PTY");
    let mut terminal = tab.lock_terminal().unwrap();
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
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H");
    assert!(terminal.primary_screen_app_active());
    // T10 P1 (D9 rule 2): release the guard before process_messages re-locks.
    drop(terminal);

    tab.feed_pty_output_for_test(
        tab.active_pane_id(),
        b"\x1b]133;D;130\x07\x1b]133;A\x07\x1b[2J\x1b[HPress Ctrl-C again to exit\x1b[2;1HResume this session with:\x1b[3;1Hscreen-app --resume late",
    );
    tab.msg_tx.send(AppMsg::PtyExited(Ok(130))).unwrap();

    let (alive, blocks, need_redraw, _) = tab.process_messages();

    assert!(!alive);
    assert!(need_redraw);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command, "screen-app");
    assert_eq!(
        blocks[0].output.as_ref(),
        "Press Ctrl-C again to exit\n\nResume this session with:\nscreen-app --resume late"
    );
}

/// v1.11.4 (PLAN_v1114 §1.3/§4.8): AppMsg::PtyExited (the worker→main
/// control event consumed by Tab::process_messages) clears BOTH kitty
/// keyboard stacks — a dead shell must never leave negotiated flags behind
/// for whatever respawns.
#[test]
fn pty_exit_resets_kitty_keyboard_flags() {
    let mut tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b[>1u\x1b[?1049h\x1b[>11u"); // main=1, alt=11→masked 3
    assert_eq!(terminal.keyboard_protocol_flags(), 3);
    // T10 P1 (D9 rule 2): release the guard before process_messages re-locks.
    drop(terminal);

    tab.msg_tx.send(AppMsg::PtyExited(Ok(0))).unwrap();
    let (alive, _, _, _) = tab.process_messages();
    assert!(!alive, "PtyExited must be processed");
    let mut terminal = tab.lock_terminal().unwrap();
    assert_eq!(terminal.keyboard_protocol_flags(), 0, "main stack cleared");
    terminal.process(b"\x1b[?1049l");
    assert_eq!(
        terminal.keyboard_protocol_flags(),
        0,
        "alt stack cleared too"
    );
}

/// v1.11.4 (PLAN_v1114 §1.3): the close contract (T10 P2: the PtyExited arm
/// in pane_pump — a dying shell while a tab closes) applies the same kitty
/// reset a natural shell exit triggers.
#[test]
fn close_tail_pty_exit_resets_kitty_keyboard_flags() {
    let mut tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal.process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H");
    assert!(
        terminal.primary_screen_app_active(),
        "close-tail precondition"
    );
    terminal.process(b"\x1b[>27u");
    assert_eq!(terminal.keyboard_protocol_flags(), 27 & 0b1_0011);
    // T10 P1 (D9 rule 2): release the guard before finish_pending_blocks.
    drop(terminal);

    // T10 P2 (D2): the close contract is async — the PtyExited ARM owns the
    // close-tail kitty reset now (the synchronous channel drain that used to
    // do it is retired with the main-thread byte pump).
    tab.msg_tx.send(AppMsg::PtyExited(Ok(0))).unwrap();
    tab.process_messages();
    let terminal = tab.lock_terminal().unwrap();
    assert_eq!(
        terminal.keyboard_protocol_flags(),
        0,
        "close-tail PtyExited must clear kitty flags"
    );
}

#[test]
fn closing_a_tab_force_settles_its_pending_primary_tui_block() {
    let mut tab = tab_with_terminal();
    let mut terminal = tab.lock_terminal().unwrap();
    terminal.process(b"\x1b]133;A\x07");
    terminal.editor_mut().buffer.set_text("screen-app");
    terminal.submit_command();
    terminal
        .process(b"\x1b]133;B\x07\x1b]133;C\x07\x1b[2;1H\x1b[3;1H\x1b]133;D;0\x07\x1b]133;A\x07");
    assert!(terminal.primary_screen_exit_pending());
    // T10 P1 (D9 rule 2): release the guard before finish_pending_blocks.
    drop(terminal);
    // T10 P2 (D2): the late tail is parsed by the worker (test seam here)
    // BEFORE the close finishes it — the async contract keeps it in the
    // block.
    tab.feed_pty_output_for_test(tab.active_pane_id(), b"\x1b[2J\x1b[Hlate close tail");

    let blocks = tab.finish_pending_blocks();

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].command, "screen-app");
    assert_eq!(blocks[0].output.as_ref(), "late close tail");
    assert!(!tab.lock_terminal().unwrap().primary_screen_exit_pending());
}

#[test]
fn synchronized_output_suppresses_partial_frame_until_commit() {
    let mut tab = tab_with_terminal();
    tab.feed_pty_output_for_test(tab.active_pane_id(), b"\x1b[?2026hpartial");
    let (_, _, partial_redraw, _) = tab.process_messages();
    assert!(!partial_redraw);

    tab.feed_pty_output_for_test(tab.active_pane_id(), b" frame\x1b[?2026l");
    let (_, _, committed_redraw, _) = tab.process_messages();
    assert!(committed_redraw);
    assert!(tab
        .lock_terminal()
        .unwrap()
        .grid()
        .row_text(0)
        .contains("partial frame"));
}
