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
/// scrollback); this interval caps it at one commit per pane per 8ms.
///
/// Why 8ms (v1.12.2, PLAN_S2_render B1 downgrade from 30ms): 30ms was sized
/// when a resize commit cost 15-42ms — throttling to one commit per ~2 frames
/// matched the work's own cost. After S3 (reflow buffer reuse + incremental
/// grid rebuild) a commit is ~3ms at the T4 bench's 50k scrollback, so 30ms
/// had become a pure latency floor: on-device frame probes caught resize
/// bursts spaced exactly 32.1-32.7ms apart (the 30ms floor + one frame),
/// i.e. the window visibly resized at ~30fps while headroom vs the 8ms interval (frame budget from the live RESIZE_PROBE sessions; see PLAN_S2_render B1) existed.
/// 8ms keeps commit spacing at ~every frame at 120Hz without letting a
/// single frame carry two commits.
///
/// Why 8ms is safe (margin invariant): interval (8ms) + frame budget
/// (~16ms) sits far inside the active-redraw window (`App::about_to_wait`
/// requests redraws for 100ms after the last `Resized`), so an expired
/// interval is guaranteed to be observed by a redraw that is still
/// happening. winit 0.30 has no drag-end event; the 100ms window is the
/// established proxy. The throttle only delays — the pane's single pending
/// slot keeps overwriting to the newest size and only a successful apply
/// clears it, so the final size always lands (first resize is never delayed:
/// the stamp is `None`/stale outside a drag). Single-switch rollback: set
/// this to `Duration::ZERO`.
pub(crate) const RESIZE_COMMIT_MIN_INTERVAL: Duration = Duration::from_millis(8);

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
///
/// F6 (v1.18.3, PLAN_v1183_F6_CASCADE_TAB_SCOPE §2-F6-1): the third
/// parameter is the bare `elapsed_debounce` leg — the zoom-cascade force
/// bit is gone, so cross-tab sessions settle once (see the branch comment
/// below) instead of committing per animation step.
pub(crate) fn pending_resize_effects(
    pending: &[(u64, Vec<PendingPaneResize>)],
    active_session_id: u64,
    elapsed_debounce: bool,
    now: Instant,
) -> Vec<Effect> {
    pending
        .iter()
        .flat_map(|(session_id, panes)| {
            let ready = if *session_id == active_session_id {
                true
                // Phase C（PLAN_zoom_drawable_stall）的 per-step commit 由本分支承载：
                // 分屏 pane 与活动 pane 同 session_id，split+top field bug 仍被覆盖。
            } else {
                // F6 (2026-11-02): cross-tab sessions NEVER follow the zoom cascade
                // per step — a large background document pays O(document) re-layout
                // plus a SIGWINCH prompt repaint per step, all billed to the active
                // tab's zoom frame (root cause of the v1.18.3 field report). They
                // commit ONCE via the 100ms debounce after the cascade settles
                // (last_resize_instant is refreshed by the final Resized, so the
                // debounce starts at animation end).
                elapsed_debounce
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
mod tests;
