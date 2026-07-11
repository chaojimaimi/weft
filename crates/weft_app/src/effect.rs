//! Typed side effects emitted by input controllers.

#[derive(Clone, Debug, PartialEq, Eq)]
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
    RequestRedraw,
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

#[cfg(test)]
mod tests {
    use super::{
        copy_clipboard_effects, ime_commit_effects, passthrough_key_effects,
        pending_resize_effects, Effect,
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
}
