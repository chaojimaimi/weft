//! Typed side effects emitted by input controllers.
//!
//! Controllers return these values instead of directly performing PTY,
//! clipboard, persistence, exit or redraw side effects.

use weft_core::pane_layout::PaneId;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effect {
    WritePty {
        tab: usize,
        bytes: Vec<u8>,
    },
    InterruptPty {
        tab: usize,
    },
    /// Resize a specific pane's PTY. `pane_id` targets an individual pane
    /// within the tab's split tree (v1.3 multi-pane); pre-v1.3 callers pass
    /// the active pane's id.
    ResizePty {
        tab: usize,
        pane_id: PaneId,
        rows: usize,
        cols: usize,
    },
    CopyClipboard {
        text: String,
    },
    /// Persist all live tab snapshots to the SQLite store (best-effort).
    PersistTabs,
    /// Persist a batch of command blocks to the BlockStore.
    PersistBlocks {
        blocks: Vec<weft_core::blocks::Block>,
    },
    /// Read the system clipboard and apply the text to `tab` (Editor inserts
    /// into the prompt buffer; Passthrough writes to the PTY with optional
    /// bracketed-paste wrapping). Synchronous on the main thread because
    /// NSPasteboard has AppKit thread affinity.
    Paste {
        tab: usize,
    },
    /// Request application exit. Sets `should_exit`; the actual
    /// `event_loop.exit()` still happens in the winit callback tail.
    Exit,
    /// A tab was closed. `removed_idx` is the old position; `new_active` is
    /// the now-active tab; `is_last` signals the last tab was closed (app
    /// should exit). The synchronous mutation already happened in
    /// SessionManager; this effect lets the app shell run post-close hooks
    /// (IME reset, find refresh, tab bar scroll) in one place.
    TabClosed {
        removed_idx: usize,
        new_active: usize,
        is_last: bool,
    },
    /// The active tab switched. `new_idx` / `prev_idx` are the tab positions.
    /// The synchronous `active_tab` mutation already happened in
    /// SessionManager; this effect lets the app shell run post-switch hooks.
    TabSwitched {
        new_idx: usize,
        prev_idx: usize,
    },
    RequestRedraw,
}

pub(crate) fn close_tab_effects(
    removed_idx: usize,
    new_active: usize,
    last_tab: bool,
) -> Vec<Effect> {
    let mut effects = vec![Effect::TabClosed {
        removed_idx,
        new_active,
        is_last: last_tab,
    }];
    if last_tab {
        effects.push(Effect::Exit);
    } else {
        effects.push(Effect::RequestRedraw);
    }
    effects.push(Effect::PersistTabs);
    effects
}

pub(crate) fn process_message_effects(
    exit_requested: bool,
    blocks: Vec<weft_core::blocks::Block>,
    redraw: bool,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    if exit_requested {
        effects.push(Effect::Exit);
    }
    if !blocks.is_empty() {
        effects.push(Effect::PersistBlocks { blocks });
    }
    if redraw {
        effects.push(Effect::RequestRedraw);
    }
    effects
}

pub(crate) fn passthrough_key_effects(tab: usize, bytes: Vec<u8>) -> Vec<Effect> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let is_ctrl_c = bytes.as_slice() == [0x03];
    if is_ctrl_c {
        // InterruptPty performs one atomic delivery by writing ETX to the PTY.
        // A failed write remains a failure; signal fallback would bypass raw
        // mode and remote-session semantics.
        // Emitting WritePty as well would deliver two interrupts and makes
        // double-Ctrl+C TUIs (Claude Code, OpenCode) exit on the first press.
        vec![Effect::InterruptPty { tab }, Effect::RequestRedraw]
    } else {
        vec![Effect::WritePty { tab, bytes }]
    }
}

pub(crate) fn ime_commit_effects(tab: usize, text: &str) -> Vec<Effect> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![
            Effect::WritePty {
                tab,
                bytes: text.as_bytes().to_vec(),
            },
            Effect::RequestRedraw,
        ]
    }
}

pub(crate) fn pending_resize_effects(
    pending: &[Vec<(PaneId, (usize, usize))>],
    active_tab: usize,
    active_ready: bool,
    cascade_settled: bool,
) -> Vec<Effect> {
    pending
        .iter()
        .enumerate()
        .flat_map(|(tab, panes)| {
            let ready = if tab == active_tab {
                active_ready
            } else {
                cascade_settled
            };
            panes.iter().filter_map(move |(pane_id, (rows, cols))| {
                ready.then_some(Effect::ResizePty {
                    tab,
                    pane_id: *pane_id,
                    rows: *rows,
                    cols: *cols,
                })
            })
        })
        .collect()
}

pub(crate) fn copy_clipboard_effects(text: Option<String>) -> Vec<Effect> {
    text.filter(|text| !text.is_empty())
        .map(|text| vec![Effect::CopyClipboard { text }])
        .unwrap_or_default()
}

pub(crate) fn context_clipboard_effects(text: Option<String>) -> Vec<Effect> {
    text.map(|text| vec![Effect::CopyClipboard { text }])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        close_tab_effects, context_clipboard_effects, copy_clipboard_effects, ime_commit_effects,
        passthrough_key_effects, pending_resize_effects, process_message_effects, Effect,
    };

    #[test]
    fn slash_and_question_mark_each_emit_exactly_one_raw_pty_write() {
        for byte in [b'/', b'?'] {
            assert_eq!(
                passthrough_key_effects(2, vec![byte]),
                [Effect::WritePty {
                    tab: 2,
                    bytes: vec![byte],
                }]
            );
        }
    }

    #[test]
    fn ime_commit_is_raw_utf8_write_followed_by_redraw() {
        assert_eq!(
            ime_commit_effects(1, "搜索"),
            [
                Effect::WritePty {
                    tab: 1,
                    bytes: "搜索".as_bytes().to_vec(),
                },
                Effect::RequestRedraw,
            ]
        );
    }

    #[test]
    fn ctrl_c_uses_one_atomic_interrupt_before_redraw() {
        assert_eq!(
            passthrough_key_effects(0, vec![0x03]),
            [Effect::InterruptPty { tab: 0 }, Effect::RequestRedraw,]
        );
    }

    #[test]
    fn active_resize_flushes_before_background_tabs() {
        use weft_core::pane_layout::PaneId;
        let pending = [
            vec![(PaneId(1), (30, 100))],
            vec![(PaneId(2), (40, 120))],
            vec![],
        ];
        assert_eq!(
            pending_resize_effects(&pending, 1, true, false),
            [Effect::ResizePty {
                tab: 1,
                pane_id: PaneId(2),
                rows: 40,
                cols: 120,
            }]
        );
        assert_eq!(pending_resize_effects(&pending, 1, false, false), []);
    }

    #[test]
    fn settled_resize_emits_only_latest_pending_dimensions_per_tab() {
        use weft_core::pane_layout::PaneId;
        let pending = [vec![(PaneId(1), (44, 132))], vec![(PaneId(2), (36, 90))]];
        assert_eq!(
            pending_resize_effects(&pending, 0, true, true),
            [
                Effect::ResizePty {
                    tab: 0,
                    pane_id: PaneId(1),
                    rows: 44,
                    cols: 132,
                },
                Effect::ResizePty {
                    tab: 1,
                    pane_id: PaneId(2),
                    rows: 36,
                    cols: 90,
                },
            ]
        );
    }

    #[test]
    fn multi_pane_resize_emits_one_effect_per_pane() {
        use weft_core::pane_layout::PaneId;
        // Tab 0 has two panes pending resize; tab 1 has one.
        let pending = [
            vec![(PaneId(1), (30, 80)), (PaneId(2), (30, 40))],
            vec![(PaneId(3), (40, 100))],
        ];
        let effects = pending_resize_effects(&pending, 0, true, true);
        assert_eq!(effects.len(), 3);
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::ResizePty {
                pane_id: PaneId(2),
                rows: 30,
                cols: 40,
                ..
            }
        )));
    }

    #[test]
    fn clipboard_effect_rejects_absent_or_empty_selection() {
        assert!(copy_clipboard_effects(None).is_empty());
        assert!(copy_clipboard_effects(Some(String::new())).is_empty());
        assert_eq!(
            copy_clipboard_effects(Some("selected".into())),
            [Effect::CopyClipboard {
                text: "selected".into(),
            }]
        );
    }

    #[test]
    fn context_copy_preserves_empty_text_as_clear_clipboard_action() {
        assert_eq!(
            context_clipboard_effects(Some(String::new())),
            [Effect::CopyClipboard {
                text: String::new(),
            }]
        );
        assert!(context_clipboard_effects(None).is_empty());
    }

    #[test]
    fn close_tab_effects_preserve_exit_persist_and_redraw_order() {
        // Last tab: TabClosed (is_last=true) + Exit + PersistTabs.
        assert_eq!(
            close_tab_effects(0, 0, true),
            [
                Effect::TabClosed {
                    removed_idx: 0,
                    new_active: 0,
                    is_last: true
                },
                Effect::Exit,
                Effect::PersistTabs,
            ]
        );
        // Non-last tab: TabClosed (is_last=false) + RequestRedraw + PersistTabs.
        assert_eq!(
            close_tab_effects(2, 1, false),
            [
                Effect::TabClosed {
                    removed_idx: 2,
                    new_active: 1,
                    is_last: false
                },
                Effect::RequestRedraw,
                Effect::PersistTabs,
            ]
        );
    }

    #[test]
    fn shell_exit_keeps_completed_blocks_before_redraw() {
        use std::time::SystemTime;
        use weft_core::blocks::{Block, BlockId};

        let completed = Block {
            id: BlockId(9),
            command: "exit".into(),
            cwd: None,
            output: "done".into(),
            styled_output: None,
            exit_code: Some(0),
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: Some(SystemTime::UNIX_EPOCH),
            collapsed: false,
        };
        assert_eq!(
            process_message_effects(true, vec![completed.clone()], true),
            [
                Effect::Exit,
                Effect::PersistBlocks {
                    blocks: vec![completed]
                },
                Effect::RequestRedraw,
            ]
        );
    }
}
