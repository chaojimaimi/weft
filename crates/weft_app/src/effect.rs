//! Typed side effects emitted by input controllers.
//!
//! Controllers return these values instead of directly performing PTY,
//! clipboard, persistence, exit or redraw side effects.

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effect {
    WritePty {
        tab: usize,
        bytes: Vec<u8>,
    },
    InterruptPty {
        tab: usize,
    },
    FlushPtyOutput {
        tab: usize,
    },
    ResizePty {
        tab: usize,
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
    RequestRedraw,
}

pub(crate) fn close_tab_effects(last_tab: bool) -> Vec<Effect> {
    if last_tab {
        vec![Effect::Exit, Effect::PersistTabs]
    } else {
        vec![Effect::RequestRedraw, Effect::PersistTabs]
    }
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
    let mut effects = vec![Effect::WritePty { tab, bytes }];
    if is_ctrl_c {
        effects.extend([
            Effect::InterruptPty { tab },
            Effect::FlushPtyOutput { tab },
            Effect::RequestRedraw,
        ]);
    }
    effects
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
    pending: &[Option<(usize, usize)>],
    active_tab: usize,
    active_ready: bool,
    cascade_settled: bool,
) -> Vec<Effect> {
    pending
        .iter()
        .enumerate()
        .filter_map(|(tab, dimensions)| {
            let (rows, cols) = (*dimensions)?;
            let ready = if tab == active_tab {
                active_ready
            } else {
                cascade_settled
            };
            ready.then_some(Effect::ResizePty { tab, rows, cols })
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
    fn ctrl_c_preserves_interrupt_flush_and_redraw_order() {
        assert_eq!(
            passthrough_key_effects(0, vec![0x03]),
            [
                Effect::WritePty {
                    tab: 0,
                    bytes: vec![0x03],
                },
                Effect::InterruptPty { tab: 0 },
                Effect::FlushPtyOutput { tab: 0 },
                Effect::RequestRedraw,
            ]
        );
    }

    #[test]
    fn active_resize_flushes_before_background_tabs() {
        let pending = [Some((30, 100)), Some((40, 120)), None];
        assert_eq!(
            pending_resize_effects(&pending, 1, true, false),
            [Effect::ResizePty {
                tab: 1,
                rows: 40,
                cols: 120,
            }]
        );
        assert_eq!(pending_resize_effects(&pending, 1, false, false), []);
    }

    #[test]
    fn settled_resize_emits_only_latest_pending_dimensions_per_tab() {
        let pending = [Some((44, 132)), Some((36, 90))];
        assert_eq!(
            pending_resize_effects(&pending, 0, true, true),
            [
                Effect::ResizePty {
                    tab: 0,
                    rows: 44,
                    cols: 132,
                },
                Effect::ResizePty {
                    tab: 1,
                    rows: 36,
                    cols: 90,
                },
            ]
        );
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
        assert_eq!(close_tab_effects(true), [Effect::Exit, Effect::PersistTabs]);
        assert_eq!(
            close_tab_effects(false),
            [Effect::RequestRedraw, Effect::PersistTabs]
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
