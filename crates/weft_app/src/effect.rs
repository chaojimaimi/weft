//! Typed side effects emitted by input controllers.
//!
//! Controllers return these values instead of directly performing PTY,
//! clipboard, persistence, exit or redraw side effects.

use std::time::{Duration, Instant};
use weft_core::pane_layout::PaneId;

/// FIX-α (docs/FIX_DRAG_RESIZE_STUTTER.md): minimum spacing between
/// successful PTY resize commits per pane. Live window dragging used to turn
/// every redraw's ready pending resize into an `Effect::ResizePty`, paying
/// the main-thread grid reflow once per frame (~17.7ms each at 10k
/// scrollback); this interval caps it at one commit per pane per 30ms.
///
/// Why 30ms is safe (margin invariant): interval (30ms) + frame budget
/// (~16ms) sits far inside the active-redraw window (`App::about_to_wait`
/// requests redraws for 100ms after the last `Resized`), so an expired
/// interval is guaranteed to be observed by a redraw that is still
/// happening. winit 0.30 has no drag-end event; the 100ms window is the
/// established proxy. The throttle only delays — the pane's single pending
/// slot keeps overwriting to the newest size and only a successful apply
/// clears it, so the final size always lands (first resize is never delayed:
/// the stamp is `None`/stale outside a drag). Single-switch rollback: set
/// this to `Duration::ZERO`.
pub(crate) const RESIZE_COMMIT_MIN_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Effect {
    WritePty {
        session_id: u64,
        bytes: Vec<u8>,
    },
    InterruptPty {
        session_id: u64,
    },
    /// Resize a specific pane's PTY. `pane_id` targets an individual pane
    /// within the tab's split tree (v1.3 multi-pane); pre-v1.3 callers pass
    /// the active pane's id.
    ResizePty {
        session_id: u64,
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
    /// v1.11.2 X4 (PLAN_v1112 §1.3): load one page of pre-retention history
    /// from SQLite and prepend it to the active tab's in-memory block list.
    /// Dispatch queries `BlockStore::older_than`, feeds
    /// `BlockTracker::load_older_to_front`, and reports via toast.
    LoadOlderBlocks,
    /// Read the system clipboard and apply the text to the session identified
    /// by `session_id` (Editor inserts into the prompt buffer; Passthrough
    /// writes to the PTY with optional bracketed-paste wrapping). Synchronous
    /// on the main thread because NSPasteboard has AppKit thread affinity.
    /// v1.11.11 (PLAN_v11111 M-B): the Effect family carries the stable
    /// `session_id` — a drained effect for a closed tab is dropped with a
    /// warn instead of silently targeting a shifted index.
    Paste {
        session_id: u64,
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

pub(crate) fn passthrough_key_effects(session_id: u64, bytes: Vec<u8>) -> Vec<Effect> {
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
        vec![Effect::InterruptPty { session_id }, Effect::RequestRedraw]
    } else {
        vec![Effect::WritePty { session_id, bytes }]
    }
}

pub(crate) fn ime_commit_effects(session_id: u64, text: &str) -> Vec<Effect> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![
            Effect::WritePty {
                session_id,
                bytes: text.as_bytes().to_vec(),
            },
            Effect::RequestRedraw,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingPaneResize {
    pane_id: PaneId,
    rows: usize,
    cols: usize,
    synchronized: bool,
    /// FIX-α: the pane is on the alternate screen. Its resize takes the
    /// cheap dimension-only path (`Grid::resize_dims`, no reflow), so the
    /// commit interval never applies — alt resizes pass straight through.
    alt_active: bool,
    /// FIX-α: when this pane last SUCCESSFULLY committed a resize. Stamped
    /// at the apply point only (`App::apply_pty_resize_effect` after
    /// `commit_pty_resize_result` succeeds) — an ioctl failure keeps the
    /// request pending with a stale stamp, so retries are never suppressed.
    /// `None` = never committed → always due.
    last_resize_commit: Option<Instant>,
}

impl PendingPaneResize {
    pub(crate) fn new(
        pane_id: PaneId,
        (rows, cols): (usize, usize),
        synchronized: bool,
        alt_active: bool,
        last_resize_commit: Option<Instant>,
    ) -> Self {
        Self {
            pane_id,
            rows,
            cols,
            synchronized,
            alt_active,
            last_resize_commit,
        }
    }

    /// FIX-α emission gate: due when the pane never committed or the last
    /// successful commit is at least [`RESIZE_COMMIT_MIN_INTERVAL`] ago.
    /// `saturating_duration_since` keeps a `now` earlier than the stamp
    /// (synthetic test clocks) from panicking.
    fn commit_due(&self, now: Instant) -> bool {
        // map_or(true, ...) instead of `is_none_or`: MSRV is 1.75
        // (Cargo.toml), is_none_or only stabilized in 1.82 — same precedent
        // as theme_import.rs.
        self.last_resize_commit.map_or(true, |stamp| {
            now.saturating_duration_since(stamp) >= RESIZE_COMMIT_MIN_INTERVAL
        })
    }

    #[cfg(test)]
    pub(crate) fn dimensions(self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    #[cfg(test)]
    pub(crate) fn is_synchronized(self) -> bool {
        self.synchronized
    }
}

/// v1.11.11 (PLAN_v11111 M-B, architect P1-1): the input pairs each
/// session's stable id with its pending pane resizes (redraw_controller
/// zips `Tab::session_id` in tab order). The ready gate compares session ids
/// instead of the old index equality, so a closed-tab Effect carries an id
/// the drain resolves (or drops with a warn) via reverse lookup.
///
/// FIX-α (docs/FIX_DRAG_RESIZE_STUTTER.md): `now` is injected so tests can
/// drive the gate with synthetic `Instant`s (no real sleeps). Emission
/// additionally requires the pane's per-commit interval to be due — except
/// for alt-screen panes, whose dimension-only resize is cheap enough to
/// always pass. Throttling delays emission only; it never drops a pending
/// resize (see [`RESIZE_COMMIT_MIN_INTERVAL`]).
pub(crate) fn pending_resize_effects(
    pending: &[(u64, Vec<PendingPaneResize>)],
    active_session_id: u64,
    cascade_settled: bool,
    now: Instant,
) -> Vec<Effect> {
    pending
        .iter()
        .flat_map(|(session_id, panes)| {
            let ready = if *session_id == active_session_id {
                true
            } else {
                cascade_settled
            };
            panes.iter().filter_map(move |resize| {
                let due = resize.alt_active || resize.commit_due(now);
                (ready && !resize.synchronized && due).then_some(Effect::ResizePty {
                    session_id: *session_id,
                    pane_id: resize.pane_id,
                    rows: resize.rows,
                    cols: resize.cols,
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
        PendingPaneResize, RESIZE_COMMIT_MIN_INTERVAL,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn slash_and_question_mark_each_emit_exactly_one_raw_pty_write() {
        for byte in *b"/?" {
            assert_eq!(
                passthrough_key_effects(2, vec![byte]),
                [Effect::WritePty {
                    session_id: 2,
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
                    session_id: 1,
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
            [
                Effect::InterruptPty { session_id: 0 },
                Effect::RequestRedraw,
            ]
        );
    }

    #[test]
    fn active_resize_flushes_immediately_while_background_tabs_wait() {
        use weft_core::pane_layout::PaneId;
        let pending = [
            (
                1,
                vec![PendingPaneResize::new(
                    PaneId(1),
                    (30, 100),
                    false,
                    false,
                    None,
                )],
            ),
            (
                2,
                vec![PendingPaneResize::new(
                    PaneId(2),
                    (40, 120),
                    false,
                    false,
                    None,
                )],
            ),
            (3, vec![]),
        ];
        assert_eq!(
            pending_resize_effects(&pending, 2, false, Instant::now()),
            [Effect::ResizePty {
                session_id: 2,
                pane_id: PaneId(2),
                rows: 40,
                cols: 120,
            }]
        );
    }

    #[test]
    fn settled_resize_emits_only_latest_pending_dimensions_per_tab() {
        use weft_core::pane_layout::PaneId;
        let pending = [
            (
                0,
                vec![PendingPaneResize::new(
                    PaneId(1),
                    (44, 132),
                    false,
                    false,
                    None,
                )],
            ),
            (
                1,
                vec![PendingPaneResize::new(
                    PaneId(2),
                    (36, 90),
                    false,
                    false,
                    None,
                )],
            ),
        ];
        assert_eq!(
            pending_resize_effects(&pending, 0, true, Instant::now()),
            [
                Effect::ResizePty {
                    session_id: 0,
                    pane_id: PaneId(1),
                    rows: 44,
                    cols: 132,
                },
                Effect::ResizePty {
                    session_id: 1,
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
            (
                0,
                vec![
                    PendingPaneResize::new(PaneId(1), (30, 80), false, false, None),
                    PendingPaneResize::new(PaneId(2), (30, 40), false, false, None),
                ],
            ),
            (
                1,
                vec![PendingPaneResize::new(
                    PaneId(3),
                    (40, 100),
                    false,
                    false,
                    None,
                )],
            ),
        ];
        let effects = pending_resize_effects(&pending, 0, true, Instant::now());
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
    fn synchronized_panes_defer_resize_without_blocking_ready_siblings() {
        use weft_core::pane_layout::PaneId;
        let pending = [(
            0,
            vec![
                PendingPaneResize::new(PaneId(1), (30, 80), true, false, None),
                PendingPaneResize::new(PaneId(2), (30, 40), false, false, None),
            ],
        )];

        assert_eq!(
            pending_resize_effects(&pending, 0, false, Instant::now()),
            [Effect::ResizePty {
                session_id: 0,
                pane_id: PaneId(2),
                rows: 30,
                cols: 40,
            }]
        );

        let committed_after_frame = [(
            0,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 80),
                false,
                false,
                None,
            )],
        )];
        assert_eq!(
            pending_resize_effects(&committed_after_frame, 0, false, Instant::now()),
            [Effect::ResizePty {
                session_id: 0,
                pane_id: PaneId(1),
                rows: 30,
                cols: 80,
            }]
        );
    }

    // ── FIX-α (FIX_DRAG_RESIZE_STUTTER): per-pane commit-interval throttle ──
    // All four states use synthetic `Instant` construction (same style as
    // tab/resize.rs) — no real sleeps.

    /// State 1 — a commit inside the interval is filtered: during a live
    /// drag the pending slot is refreshed every frame, but at most one
    /// reflow per pane per `RESIZE_COMMIT_MIN_INTERVAL` may reach the apply
    /// point.
    #[test]
    fn resize_inside_commit_interval_is_filtered() {
        use weft_core::pane_layout::PaneId;
        let now = Instant::now();
        let pending = [(
            1,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 100),
                false,
                false,
                Some(now - Duration::from_millis(10)),
            )],
        )];
        assert!(
            pending_resize_effects(&pending, 1, false, now).is_empty(),
            "a commit 10ms after the last one must wait out the 30ms interval"
        );
    }

    /// State 2 — once the interval expires the resize is admitted again
    /// (`>=` boundary: exactly `RESIZE_COMMIT_MIN_INTERVAL` old is due).
    #[test]
    fn resize_admitted_once_commit_interval_expires() {
        use weft_core::pane_layout::PaneId;
        let now = Instant::now();
        let pending = [(
            1,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 100),
                false,
                false,
                Some(now - RESIZE_COMMIT_MIN_INTERVAL),
            )],
        )];
        assert_eq!(
            pending_resize_effects(&pending, 1, false, now),
            [Effect::ResizePty {
                session_id: 1,
                pane_id: PaneId(1),
                rows: 30,
                cols: 100,
            }]
        );
    }

    /// State 3 (review P3) — the ioctl-failure retry must never get stuck.
    /// The stamp is taken only at APPLY SUCCESS, so a pending whose stamp is
    /// stale (or absent) re-emits on every drain, however frequent. The
    /// function is side-effect free: repeated calls keep emitting, which is
    /// exactly what keeps a failed ioctl's retained request alive.
    #[test]
    fn failed_ioctl_retry_keeps_re_emitting_every_drain() {
        use weft_core::pane_layout::PaneId;
        let start = Instant::now();
        let pending = [(
            1,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 100),
                false,
                false,
                Some(start - Duration::from_secs(3600)),
            )],
        )];
        for drain in 0..5u32 {
            // Drains 1ms apart — rapid enough that a stamp-on-emission design
            // would suppress everything after the first.
            let now = start + Duration::from_millis(u64::from(drain));
            assert_eq!(
                pending_resize_effects(&pending, 1, false, now).len(),
                1,
                "drain {drain}: a stale-stamped pending must keep flowing — retries are never throttled"
            );
        }
    }

    /// State 4 — alt-screen panes bypass the interval entirely: their resize
    /// takes the dimension-only `resize_dims` path (no reflow), so even a
    /// brand-new stamp must not delay them.
    #[test]
    fn alt_screen_resize_bypasses_commit_interval() {
        use weft_core::pane_layout::PaneId;
        let now = Instant::now();
        let pending = [(
            1,
            vec![PendingPaneResize::new(
                PaneId(1),
                (30, 100),
                false,
                true,
                Some(now),
            )],
        )];
        assert_eq!(
            pending_resize_effects(&pending, 1, false, now),
            [Effect::ResizePty {
                session_id: 1,
                pane_id: PaneId(1),
                rows: 30,
                cols: 100,
            }]
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
            screen_origin: false,
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
