//! v1.3: Pane — a single shell session inside a tab.
//!
//! A [`Tab`](super::tab::Tab) owns one or more `Pane`s. Each pane carries
//! its own PTY, VT `Terminal`, input/selection handlers, IME preedit, and
//! block-scroll state — everything that is per-shell-session lives here.
//! The tab-level concerns (split tree, active pane index, stable tab
//! `session_id` for tab-bar / context-menu ownership) stay on `Tab`.
//!
//! `Tab` implements `Deref<Target=Pane>` / `DerefMut` so existing call
//! sites that read `tab.terminal` / `tab.pty` / `tab.input_handler` /
//! `tab.selection_handler` / `tab.ime_preedit` / … keep compiling
//! unchanged: the field access auto-dereferences to the active pane. New
//! code that needs to operate on a non-active pane should go through
//! `tab.pane(id)` / `tab.pane_mut(id)`.

use std::sync::atomic::Ordering;

use crossbeam_channel::{Receiver, Sender};
use weft_core::input::InputHandler;
use weft_core::persistence::TabSnapshot;
use weft_core::pty::{Pty, PtyError, PtyEvent};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

use crate::tab::{BlockScrollAnchor, PendingTuiScroll, PrimaryHistoryRefresh, TuiScrollResolution};
use crate::{AppEvent, AppMsg};

/// Monotonic per-process pane-session counter. Each pane gets a fresh id
/// at spawn time; the id is stable for the pane's lifetime and is reused
/// across tabs (the tab-level `session_id` is the externally visible one).
static NEXT_PANE_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Per-pane input-event sequence counter. Bumped at the keyboard-encode
/// chokepoint so trace records can correlate a normalized input event
/// with its Effect dispatch. See `Tab::next_input_seq` for the public API.
static NEXT_INPUT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A single shell session: one PTY + one Terminal + per-session UI state.
///
/// Multiple panes live inside a tab when the user splits (`Cmd+D` /
/// `Cmd+Shift+D`). Each pane runs an independent shell — closing one pane
/// does not affect the others; closing the last pane closes the tab.
pub struct Pane {
    /// Stable per-pane id used for PTY / trace correlation. Distinct from
    /// the tab-level `session_id` (which stays constant for the tab's
    /// lifetime regardless of how many panes are open).
    pub pane_session_id: u64,
    /// Per-pane monotonic input-event sequence. Bumped by `next_input_seq`
    /// at the keyboard-encode chokepoint. Read from `drain_effects` /
    /// dispatch traces so a single log line answers "which physical key
    /// press produced this Effect".
    pub input_seq: u64,
    pub terminal: Option<Terminal>,
    pub pty: Option<Pty>,
    pub msg_rx: Receiver<AppMsg>,
    pub msg_tx: Sender<AppMsg>,
    pub input_handler: InputHandler,
    pub selection_handler: SelectionHandler,
    pub ime_preedit: String,
    /// F2 P1-4: Preedit cursor byte range from `Ime::Preedit(text, cursor)`.
    /// `None` when the IME has no cursor (should be hidden). `Some((s, e))`
    /// gives the byte range `[s, e)` within `ime_preedit` to highlight or
    /// position the caret at.
    pub ime_preedit_cursor: Option<(usize, usize)>,
    pub pending_pty_resize: Option<(usize, usize)>,
    pub(crate) pending_pty_output: Option<Vec<u8>>,
    /// R2-1: private — use the block-scroll API methods below instead.
    /// Replaces the raw `usize` offset with a `BlockScrollAnchor` so the
    /// snap-to-bottom caller can tell follow-tail apart from detached-read.
    pub(crate) block_scroll_anchor: BlockScrollAnchor,
    /// Fractional visual-row offset used for pixel-precise trackpad scrolling.
    /// Persistence remains row-based; this transient fraction resets on
    /// programmatic jumps and session restore.
    pub(crate) block_scroll_fraction: f32,
    /// Original persisted state retained while a restored shell is starting.
    /// Until OSC 7 supplies an authoritative cwd, this prevents autosave
    /// from replacing the saved cwd with a transient `None`. It also
    /// preserves the complete pane when PTY creation fails and no live
    /// snapshot is possible.
    pub restored_snapshot: Option<TabSnapshot>,
    /// Wheel rows received while a TUI command is starting but before its
    /// alternate-screen sequence has reached the parser. Replayed once the
    /// alt screen becomes active, so the first trackpad gesture is not lost.
    pub(crate) pending_tui_scroll: Option<PendingTuiScroll>,
    /// Short deadline started at editor submission. This bounds the
    /// ambiguous interval before a TUI's alternate-screen modes reach the
    /// parser.
    pub(crate) tui_scroll_deadline: Option<std::time::Instant>,
    pub(crate) tui_scroll_wake_scheduled: bool,
    pub(crate) primary_history_refresh: PrimaryHistoryRefresh,
}

impl Pane {
    /// Create a new pane with a PTY + Terminal pair at the given size.
    ///
    /// `cwd` — if `Some(path)`, the shell starts in that directory (via
    /// `chdir` in the child process before exec). Pass `None` to inherit
    /// the weft process's cwd. Using `chdir` instead of sending a `cd`
    /// command keeps the pane clean — no `cd` appears in the terminal,
    /// shell history, or block tracker.
    pub(crate) fn spawn(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
        cwd: Option<&str>,
    ) -> Self {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let env = crate::shell_integration_env(&shell);
        let env_refs: Vec<(&str, &str)> =
            env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let wake_proxy = proxy.clone();
        let pty = match Pty::spawn_with_args(
            &shell,
            &[],
            (rows as u16, cols as u16),
            &env_refs,
            cwd,
            move || {
                let _ = wake_proxy.send_event(AppEvent::Wake);
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to spawn PTY: {e}");
                return Self::empty();
            }
        };

        let terminal = Terminal::with_scrollback(rows, cols, scrollback_lines);
        tracing::info!(rows, cols, "initial terminal size");

        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: Some(terminal),
            pty: Some(pty),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    /// Empty pane (used when PTY spawn fails — terminal/pty stay None).
    pub(crate) fn empty() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: None,
            pty: None,
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    /// Build a pane with a live Terminal but no PTY — used by snapshot
    /// roundtrip tests (`tab_with_terminal`). The pane has a fresh
    /// `pane_session_id` and the given scrollback depth.
    #[cfg(test)]
    pub(crate) fn with_terminal_only(scrollback_lines: usize) -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: Some(Terminal::with_scrollback(24, 80, scrollback_lines)),
            pty: None,
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    /// Bump and return the next per-pane input-event sequence number.
    /// Currently called at the keyboard-encode chokepoint only. IME commit,
    /// mouse gestures, and paste are not yet wired — extending coverage to
    /// those sources is a follow-up if cross-source trace correlation is
    /// needed. The counter is per-process monotonic (the static allocator
    /// never resets), so two panes never share a sequence number.
    pub(crate) fn next_input_seq(&mut self) -> u64 {
        let next = NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed);
        self.input_seq = next;
        next
    }

    /// Queue a geometry transaction without resizing the Grid ahead of its PTY.
    /// Overwriting an older pending size coalesces resize cascades. The effect
    /// dispatcher applies `TIOCSWINSZ` first and only then commits the Grid,
    /// preventing old-width progress output from wrapping in a new-width Grid.
    pub(crate) fn resize_terminal_and_queue(&mut self, rows: usize, cols: usize) -> bool {
        let Some(terminal) = &self.terminal else {
            return false;
        };
        if (terminal.grid().num_rows, terminal.grid().num_cols) == (rows, cols)
            && self.pending_pty_resize.is_none()
        {
            return false;
        }
        self.pending_pty_resize = Some((rows, cols));
        true
    }

    pub(crate) fn set_restored_cwd_fallback(&mut self, cwd: Option<String>) {
        if self.restored_snapshot.is_some() {
            return;
        }
        self.restored_snapshot = Some(TabSnapshot {
            position: 0,
            active: false,
            cwd,
            block_scroll_offset: 0,
            editor_buffer: String::new(),
            shell_phase: "AtPrompt".to_string(),
            block_ids: Vec::new(),
        });
    }

    /// Non-blocking drain of PTY events into channel. Capped per frame.
    pub fn pump_pty(&mut self) {
        let Some(pty) = &mut self.pty else {
            return;
        };
        let tx = self.msg_tx.clone();
        const MAX_EVENTS_PER_FRAME: usize = 64;
        let mut count = 0usize;
        loop {
            match pty.try_recv() {
                Ok(PtyEvent::Output(data)) => {
                    if tx.send(AppMsg::PtyOutput(data)).is_err() {
                        break;
                    }
                }
                Ok(PtyEvent::Exit(code)) => {
                    let _ = tx.send(AppMsg::PtyExit(code));
                    break;
                }
                Err(_) => break,
            }
            count += 1;
            if count >= MAX_EVENTS_PER_FRAME {
                break;
            }
        }
    }

    /// Deliver one PTY-native interrupt without discarding command output.
    ///
    /// Ctrl+C is always one ETX byte, regardless of whether the foreground app
    /// uses the alternate screen, primary-screen cursor addressing, raw mode,
    /// SSH, or a conventional shell command. Output is intentionally retained:
    /// interactive tools write confirmation/resume tails after the first or
    /// second Ctrl+C, and a post-write `tcflush` can race away the ETX itself.
    pub fn interrupt_pty(&mut self) -> bool {
        if let Some(terminal) = &mut self.terminal {
            terminal.begin_primary_screen_interrupt_capture();
        }
        // `send_interrupt` only needs `&self`, but `flush_input` needs `&mut`,
        // so take a single `&mut` borrow of the pty field and keep it alive
        // across both calls. `self.terminal` is a disjoint field, so the
        // borrow checker permits the `&mut self.terminal` accesses below.
        let Some(pty) = self.pty.as_mut() else {
            if let Some(terminal) = &mut self.terminal {
                terminal.cancel_primary_screen_interrupt_capture();
            }
            return false;
        };
        if !pty.send_interrupt() {
            if let Some(terminal) = &mut self.terminal {
                terminal.cancel_primary_screen_interrupt_capture();
            }
            return false;
        }
        // Best-effort flush of pending input so a half-typed line doesn't
        // leak past the interrupt. `tcflush(TCIFLUSH)` on the master side
        // clears the kernel's input queue; the app-side reader is the
        // pump_pty loop, which keeps running and will drain the rest.
        pty.flush_input();
        true
    }

    // Silence the unused-import warning for PtyError — it's part of the
    // public API surface (returned by Pty::spawn_with_args) but the pane
    // spawn path converts it into a log line before it can escape.
    #[allow(dead_code)]
    fn _pty_error_link(_: PtyError) {}

    // TuiScrollResolution is re-exported by tab.rs; keep the import live so
    // future pane-level scroll helpers can use it without re-importing.
    #[allow(dead_code)]
    fn _tui_scroll_resolution_link(_: TuiScrollResolution) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_cwd_fallback_is_set_once_and_visible_to_snapshot_logic() {
        let mut pane = Pane::with_terminal_only(1000);
        pane.set_restored_cwd_fallback(Some("/saved".into()));
        assert_eq!(
            pane.restored_snapshot
                .as_ref()
                .and_then(|s| s.cwd.as_deref()),
            Some("/saved")
        );

        pane.set_restored_cwd_fallback(Some("/replacement".into()));
        assert_eq!(
            pane.restored_snapshot
                .as_ref()
                .and_then(|s| s.cwd.as_deref()),
            Some("/saved"),
            "a later fallback must not replace the original recovery state"
        );
    }
}
