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
use weft_core::input::{new_flag, InputHandler, MouseSuppressFlag};
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
    /// v1.11.15 (FIX A, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §1.2): reader-side
    /// mouse-suppression flag — shared with this pane's PTY read loop and
    /// Terminal parser. While it is set, weft must not write hover/wheel
    /// bytes into a session whose TUI already emitted its disable sequences
    /// (or whose PTY hit EOF). Per-pane by design: only THIS session's own
    /// mouse sends are suppressed, never a sibling pane's.
    pub mouse_suppress: MouseSuppressFlag,
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
    /// FIX-α (docs/FIX_DRAG_RESIZE_STUTTER.md): instant of the last
    /// SUCCESSFUL resize commit. Stamped only by the apply point after
    /// `commit_pty_resize_result` succeeds — an ioctl failure keeps
    /// `pending_pty_resize` for retry and must never stamp (the retry would
    /// be interval-suppressed). `pending_resize_effects` gates re-emission
    /// on `now - stamp >= RESIZE_COMMIT_MIN_INTERVAL` so a live drag pays at
    /// most one grid reflow per pane per interval instead of one per frame.
    /// `None` until the first commit; a stamp left by an old resize is stale
    /// by the time a new cascade starts (monotonic clock), so non-drag
    /// resizes commit immediately.
    pub(crate) last_resize_commit: Option<std::time::Instant>,
    /// v1.10.19: winsize `(rows, cols)` last actually sent to the PTY via
    /// TIOCSWINSZ. `apply_winsize_ioctl` dedups against it: re-requesting a
    /// size the PTY already has must not re-issue the ioctl (each redundant
    /// one SIGWINCHes the foreground app — the feedback that keeps a resize
    /// loop alive). `None` until the first successful ioctl.
    pub(crate) last_sent_winsize: Option<(usize, usize)>,
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
    /// Set only by the REAL restore paths (`restore_from_snapshot`,
    /// `attach_recovery_snapshot`) — never as a stub. The pre-OSC-7 cwd
    /// lives in [`Pane::restored_cwd`]; the v1.8.9 bug was writing a stub
    /// here from `set_restored_cwd_fallback`, which made the
    /// `attach_recovery_snapshot` `is_some()` guard skip every real attach.
    pub restored_snapshot: Option<TabSnapshot>,
    /// v1.10.24 B1: cwd to report (via `launch_cwd` / `to_snapshot`) until
    /// the freshly-spawned shell emits its own OSC 7. Written by
    /// [`Pane::set_restored_cwd_fallback`]. Lives on its own field — the
    /// v1.8.9 stub-snapshot overload blocked the real recovery snapshot
    /// attach (block_ids never injected, scroll offset never applied).
    pub restored_cwd: Option<String>,
    /// Whether `restored_cwd` has ever been assigned. Preserves the set-once
    /// contract even when the first call carried `None`.
    pub(crate) restored_cwd_set: bool,
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
    /// v1.10.21: alt-screen history-peek entry gate (400ms re-entry lockout
    /// and ≥2-row net-travel threshold; see `crate::alt_peek`). Lives on the
    /// pane, alongside the peek flag it guards on this pane's Terminal.
    pub(crate) alt_peek_gate: crate::alt_peek::PeekEntryGate,
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
        // v1.11.15 (FIX A): one flag per pane — the PTY reader thread and the
        // Terminal parser share it (see the field doc).
        let mouse_suppress = new_flag();
        let pty = match Pty::spawn_with_args(
            &shell,
            &[],
            (rows as u16, cols as u16),
            &env_refs,
            cwd,
            mouse_suppress.clone(),
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

        // v1.11.2 X3 defensive clamp (PLAN_v1112 §5): config values are
        // normalized at load time, but Pane can also be built from programmatic
        // paths — never let an out-of-range depth reach the Terminal.
        let scrollback_lines = scrollback_lines.clamp(
            weft_core::config::SCROLLBACK_MIN_LINES,
            weft_core::config::SCROLLBACK_MAX_LINES,
        );
        let mut terminal = Terminal::with_scrollback(rows, cols, scrollback_lines);
        terminal.set_mouse_suppress_flag(mouse_suppress.clone());
        tracing::info!(rows, cols, "initial terminal size");

        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: Some(terminal),
            pty: Some(pty),
            mouse_suppress,
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            last_resize_commit: None,
            last_sent_winsize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            restored_cwd: None,
            restored_cwd_set: false,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
            alt_peek_gate: crate::alt_peek::PeekEntryGate::default(),
        }
    }

    /// Empty pane — terminal/pty stay None. Production fallback when PTY
    /// spawn fails (see `spawn`); also the base of the test-only
    /// `Tab::empty` constructor.
    pub(crate) fn empty() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            pane_session_id: NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed),
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: None,
            pty: None,
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            last_resize_commit: None,
            last_sent_winsize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            restored_cwd: None,
            restored_cwd_set: false,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
            alt_peek_gate: crate::alt_peek::PeekEntryGate::default(),
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
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            last_resize_commit: None,
            last_sent_winsize: None,
            pending_pty_output: None,
            block_scroll_anchor: BlockScrollAnchor::FollowBottom,
            block_scroll_fraction: 0.0,
            restored_snapshot: None,
            restored_cwd: None,
            restored_cwd_set: false,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
            alt_peek_gate: crate::alt_peek::PeekEntryGate::default(),
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

    /// v1.11.2 X4 (PLAN_v1112 §1.2): apply the `[blocks] retained_limit`
    /// config to this pane's terminal. Called by every tab/pane creation
    /// site that already reads config (same chokepoints as scrollback).
    pub(crate) fn set_blocks_retained_limit(&mut self, limit: usize) {
        if let Some(t) = self.terminal.as_mut() {
            t.set_blocks_retained_limit(limit);
        }
    }

    /// PLAN_v11217 §3.5 (T4): apply the `[blocks] output_cap_mib` config to
    /// this pane's terminal. Mirror of `set_blocks_retained_limit` — same
    /// creation-site chokepoints. `cap_bytes` arrives pre-converted (and
    /// clamped) from the config_controller helper.
    pub(crate) fn set_blocks_output_cap(&mut self, cap_bytes: usize) {
        if let Some(t) = self.terminal.as_mut() {
            t.set_block_output_cap(cap_bytes);
        }
    }

    /// v1.11.7 (PLAN_v1117 §三 M1.2, P2-3): inject the user's
    /// `[experimental] tui_render_mode` into this pane's terminal (the
    /// factory default is `noninteractive`; `Terminal::new` itself stays
    /// Classic so weft_core tests keep the v1.11.6 baseline). Mirror of
    /// `set_blocks_retained_limit` — same chokepoints.
    pub(crate) fn set_tui_render_mode(&mut self, mode: weft_core::vt::TuiRenderMode) {
        if let Some(t) = self.terminal.as_mut() {
            t.set_tui_render_mode(mode);
        }
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

    /// v1.10.19: True when `requested` differs from the winsize last sent to
    /// the PTY. Coalesced `pending_pty_resize` overwrites (alt-screen flips
    /// 102→99→102…) flush sizes the PTY already has; re-issuing TIOCSWINSZ
    /// for those would SIGWINCH the foreground app for no state change —
    /// each redundant ioctl is one more turn of the resize feedback loop.
    pub(crate) fn should_send_winsize_ioctl(
        last_sent_winsize: Option<(usize, usize)>,
        requested: (usize, usize),
    ) -> bool {
        last_sent_winsize != Some(requested)
    }

    /// FIX (field run, v1.12.6): release the PTY's reaper BEFORE the tokio
    /// runtime tears down. The waitpid thread lives in the runtime's
    /// blocking pool and parks in `wait4(shell)`; if the shell outlives the
    /// app teardown (a full-screen TUI like top does not exit from a late
    /// SIGHUP, and nobody else delivers one once the event loop is gone),
    /// the runtime's `BlockingPool` teardown waits for it on the main
    /// thread -- the sampled "not responding" stall. Dropping the Pty sends
    /// SIGHUP and closes the master fd, which lets the wait4 return.
    pub(crate) fn release_pty(&mut self) {
        self.pending_pty_resize = None;
        if let Some(pty) = self.pty.take() {
            drop(pty);
        }
    }

    /// v1.10.19: Send a queued winsize to the PTY via TIOCSWINSZ, deduping
    /// against the last size actually sent (see [`should_send_winsize_ioctl`]).
    ///
    /// Returns `Ok(true)` when the ioctl was issued, `Ok(false)` when it was
    /// deduped (the PTY already has this size — the caller still commits the
    /// in-memory Grid dimensions, which may have drifted), and `Err` when the
    /// ioctl failed or the pane has no PTY (the caller skips the Grid commit
    /// and retries on the next flush).
    pub(crate) fn apply_winsize_ioctl(&mut self, rows: usize, cols: usize) -> Result<bool, String> {
        let winsize = (rows, cols);
        if !Self::should_send_winsize_ioctl(self.last_sent_winsize, winsize) {
            return Ok(false);
        }
        let Some(pty) = &self.pty else {
            return Err("no pty attached to pane".to_string());
        };
        pty.resize(rows as u16, cols as u16)
            .map(|()| {
                self.last_sent_winsize = Some(winsize);
                true
            })
            .map_err(|error| error.to_string())
    }

    /// v1.10.24 B1: Set the pane's cwd fallback — the directory reported by
    /// `launch_cwd` / `to_snapshot` until the freshly-spawned shell emits its
    /// own OSC 7.
    ///
    /// Previously this wrote a full stub `TabSnapshot` into
    /// `restored_snapshot`, which made `attach_recovery_snapshot`'s
    /// `is_some()` guard skip every real attach — block_ids were never
    /// injected and the scroll offset never applied (v1.8.9 no-op, see
    /// FIX_RECOVERY_DESIGN_ALIGNMENT B1). The fallback now lives on its own
    /// field so the real snapshot can attach freely.
    ///
    /// Set-once: the first value wins, even over a later restore step
    /// (e.g. `build_subtree` re-setting the root leaf).
    pub(crate) fn set_restored_cwd_fallback(&mut self, cwd: Option<String>) {
        if self.restored_cwd_set || self.restored_snapshot.is_some() {
            return;
        }
        self.restored_cwd_set = true;
        self.restored_cwd = cwd;
    }

    /// v1.10.24 B1: Effective restored cwd for the pre-OSC-7 interval — the
    /// real attached snapshot's cwd wins, then the workspace-restore
    /// fallback.
    pub(crate) fn restored_cwd_fallback(&self) -> Option<&str> {
        self.restored_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.cwd.as_deref())
            .or(self.restored_cwd.as_deref())
    }

    /// Non-blocking drain of PTY events into channel. Capped per frame.
    pub fn pump_pty(&mut self) {
        let Some(pty) = &mut self.pty else {
            return;
        };
        let tx = self.msg_tx.clone();
        const MAX_EVENTS_PER_FRAME: usize = 64;
        // AUDIT_v1.10.39: stop feeding the bounded(1024) channel before it
        // fills. Producer (this pump) and consumer (process_messages) run on
        // the SAME main thread in a fixed pump→process order, so a blocking
        // `tx.send` on a full channel would wait for "ourselves" and hang the
        // UI thread forever. The high-water check is race-free for exactly
        // that reason — nothing else sends between check and send. Events
        // left behind simply stay in the PTY's own queue until next frame.
        const CHANNEL_HIGH_WATER: usize = 768;
        let mut count = 0usize;
        loop {
            if tx.len() >= CHANNEL_HIGH_WATER {
                break;
            }
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

/// Decide the chdir target for a restored tab's fresh shell.
///
/// Only an absent/empty snapshot cwd yields `None` (inherit the Weft
/// process's cwd). Everything else is honored verbatim — including `$HOME`.
/// The pre-v1.10.36 filter also dropped `$HOME`, so a Finder-launched Weft
/// (process cwd `/`) restored home-directory tabs at the filesystem root.
pub(crate) fn restore_spawn_cwd(saved_cwd: Option<&str>) -> Option<String> {
    saved_cwd.filter(|c| !c.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_cwd_fallback_is_set_once_and_visible_to_snapshot_logic() {
        let mut pane = Pane::with_terminal_only(1000);
        pane.set_restored_cwd_fallback(Some("/saved".into()));
        assert_eq!(pane.restored_cwd.as_deref(), Some("/saved"));
        assert_eq!(
            pane.restored_cwd_fallback(),
            Some("/saved"),
            "the fallback must be visible to the snapshot cwd logic"
        );
        assert!(
            pane.restored_snapshot.is_none(),
            "v1.8.9 no-op regression: the cwd fallback must not write a stub snapshot"
        );

        pane.set_restored_cwd_fallback(Some("/replacement".into()));
        assert_eq!(
            pane.restored_cwd.as_deref(),
            Some("/saved"),
            "a later fallback must not replace the original recovery state"
        );
    }

    // ── v1.10.19: PTY winsize dedup (resize-loop defense layer 1) ────────

    #[test]
    fn should_send_winsize_ioctl_skips_sizes_already_sent() {
        assert!(!Pane::should_send_winsize_ioctl(Some((24, 80)), (24, 80)));
        assert!(Pane::should_send_winsize_ioctl(Some((24, 80)), (24, 100)));
        assert!(Pane::should_send_winsize_ioctl(Some((24, 80)), (25, 80)));
        // Before any ioctl was ever sent, every request is a real send.
        assert!(Pane::should_send_winsize_ioctl(None, (24, 80)));
    }

    #[test]
    fn apply_winsize_ioctl_dedups_and_records_the_last_sent_size() {
        // with_terminal_only has no PTY — the dedup decision still applies:
        // a re-request of the last-sent size must short-circuit before any
        // PTY access (Ok(false)), while a genuinely new size takes the
        // ioctl path and fails with the "no pty" error.
        let mut pane = Pane::with_terminal_only(1000);
        pane.last_sent_winsize = Some((24, 80));
        assert_eq!(pane.apply_winsize_ioctl(24, 80), Ok(false));
        assert_eq!(
            pane.last_sent_winsize,
            Some((24, 80)),
            "dedup does not touch last_sent_winsize"
        );
        assert!(
            pane.apply_winsize_ioctl(24, 100).is_err(),
            "a changed size must reach the ioctl path (no pty here → error)"
        );
        assert_eq!(
            pane.last_sent_winsize,
            Some((24, 80)),
            "failed ioctl must not record the requested size"
        );
    }

    // ── v1.10.36: restored-tab chdir target filter ──────────────────────

    #[test]
    fn restore_spawn_cwd_none_and_empty_inherit_weft_cwd() {
        assert_eq!(restore_spawn_cwd(None), None);
        assert_eq!(restore_spawn_cwd(Some("")), None);
    }

    #[test]
    fn restore_spawn_cwd_honors_non_home_paths_verbatim() {
        assert_eq!(
            restore_spawn_cwd(Some("/Users/me")),
            Some("/Users/me".to_string())
        );
    }

    #[test]
    fn restore_spawn_cwd_keeps_home_verbatim() {
        // Regression guard for the production bug: a saved cwd equal to
        // $HOME was filtered out, so a Finder-launched Weft (process cwd `/`)
        // restored home tabs at the filesystem root. The pure function takes
        // a literal so the guard runs even in a HOME-less CI environment.
        assert_eq!(
            restore_spawn_cwd(Some("/Users/andy")),
            Some("/Users/andy".to_string()),
            "a saved cwd equal to $HOME must be honored, not dropped"
        );
    }
}
