//! v1.3: Pane — a single shell session inside a tab.
//!
//! A [`Tab`](super::tab::Tab) owns one or more `Pane`s. Each pane carries
//! its own PTY, VT `Terminal`, input/selection handlers, IME preedit, and
//! block-scroll state — everything that is per-shell-session lives here.
//! The tab-level concerns (split tree, active pane index, stable tab
//! `session_id` for tab-bar / context-menu ownership) stay on `Tab`.
//!
//! `Tab` implements `Deref<Target=Pane>` / `DerefMut` so existing call
//! sites that read `tab.pty` / `tab.input_handler` /
//! `tab.selection_handler` / `tab.ime_preedit` / … keep compiling
//! unchanged: the field access auto-dereferences to the active pane. New
//! code that needs to operate on a non-active pane should go through
//! `tab.pane(id)` / `tab.pane_mut(id)`.
//!
//! v1.13.6 T10 P1 (PLAN_v1136 §1 D1/D9): the VT `Terminal` moved behind a
//! fair mutex (`Option<Arc<FairMutex<Terminal>>>`) as the parse-thread
//! shared model. The field is **private** (D9 rule 1); access goes through
//! the controlled accessors [`Pane::lock_terminal`] (owned guard for
//! multi-statement scopes such as a full draw frame) and
//! [`Pane::with_terminal`] (short closure reads/writes). The guard is the
//! *owned* `ArcMutexGuard` (`lock_arc`) — it holds an `Arc` clone instead of
//! borrowing the pane, so pre-existing disjoint-field-borrow sites (e.g.
//! `(&terminal, &mut pane.selection_handler)` in the draw path, or
//! `terminal ∥ pty` in `interrupt_pty`) keep compiling unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{ArcMutexGuard, FairMutex, RawFairMutex};
use weft_core::input::{new_flag, InputHandler, MouseSuppressFlag};
use weft_core::persistence::TabSnapshot;
use weft_core::pty::{Pty, PtyError, PtyWriter};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

/// Owned terminal lock guard (v1.13.6 T10 P1). `lock_arc` keeps an `Arc`
/// clone inside the guard, so it carries no borrow of the `Pane` — callers
/// may mutate sibling pane fields while holding it. Still bound by D9
/// rules 2–4: never nested, never held across channel send/recv, fd writes,
/// or file IO.
pub(crate) type TerminalGuard = ArcMutexGuard<RawFairMutex, Terminal>;

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
    /// v1.13.6 T10 P1 (D9 rule 1): private — access only via
    /// [`Pane::lock_terminal`] / [`Pane::with_terminal`]. `Arc<FairMutex>`
    /// is the T10 shared-ownership model: the P2 parse worker will clone
    /// the Arc; the main thread short-scope locks.
    terminal: Option<Arc<FairMutex<Terminal>>>,
    pub pty: Option<Pty>,
    /// v1.13.6 T10 P2 (D4): the shared PTY write half. The main thread
    /// (user input / paste / Ctrl+C) and the parse worker (VT query
    /// replies) each hold an `Arc` clone; the single write mutex lives
    /// inside [`PtyWriter`]. `None` together with `pty` (spawn failure /
    /// test panes).
    pub writer: Option<Arc<PtyWriter>>,
    /// v1.13.6 T10 P2 (D6): lock-free "the parse worker produced output
    /// for this pane since the main pump last looked". The retired
    /// `need_redraw` signal fell out of the main thread draining the bytes
    /// itself; with parsing in the worker, the pump observes this flag
    /// instead — it feeds `had_output` (frame trace, the running-output
    /// follow snap, TUI-scroll resolution) without a lock or a frame of
    /// artificial delay. Worker sets / `Tab::process_messages` take-clears.
    pub(crate) had_output: Arc<AtomicBool>,
    /// v1.11.15 (FIX A, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §1.2): reader-side
    /// mouse-suppression flag — shared with this pane's PTY read loop and
    /// Terminal parser. While it is set, weft must not write hover/wheel
    /// bytes into a session whose TUI already emitted its disable sequences
    /// (or whose PTY hit EOF). Per-pane by design: only THIS session's own
    /// mouse sends are suppressed, never a sibling pane's.
    pub mouse_suppress: MouseSuppressFlag,
    pub msg_rx: Receiver<AppMsg>,
    /// Worker → main control channel. Since T10 P2 the WORKER holds the
    /// producing clone; the pane's own handle is the test-injection seam
    /// (the production main thread only consumes `msg_rx`).
    #[cfg_attr(not(test), allow(dead_code))]
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
    /// v1.13.6 T10 P2 review P1-1 (close watchdog): stamped when a close
    /// initiation drops this pane's PTY (`Tab::begin_close` /
    /// `close_active_pane`). A SIGHUP-immune child (nohup/setsid) holding the
    /// slave fd keeps the reader alive forever — no EOF, no `Exit`, no
    /// `PtyExited` — so the pump finalizes the pane itself once the deadline
    /// passes (same tail as the exit arm). `None` for every non-closing
    /// pane; the dedicated `weft-close-watchdog` wake thread guarantees the
    /// check runs even with zero further output.
    pub(crate) close_deadline: Option<std::time::Instant>,
    /// Watchdog/exit double-settle guard: set the moment a pane is finalized
    /// (watchdog path, or a late real `PtyExited` after the watchdog) so the
    /// second arrival is consumed as a no-op. Natural exits never need it
    /// (the pane leaves the map), but the flag makes the race explicit and
    /// testable.
    pub(crate) close_settled: bool,
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

        // v1.11.15 (FIX A): one flag per pane — the PTY reader thread and the
        // Terminal parser share it (see the field doc).
        let mouse_suppress = new_flag();
        // v1.13.6 T10 P2 (D3): no wake closure any more — the parse worker
        // owns UI wakes; this proxy clone is handed to it below.
        let mut pty = match Pty::spawn_with_args(
            &shell,
            &[],
            (rows as u16, cols as u16),
            &env_refs,
            cwd,
            mouse_suppress.clone(),
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to spawn PTY: {e}");
                return Self::empty();
            }
        };
        // D2 split: the receive half belongs to the parse worker.
        let event_rx = pty
            .take_event_rx()
            .expect("a freshly spawned Pty holds its receiver");
        let writer = pty.writer();

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
        let terminal = Arc::new(FairMutex::new(terminal));
        let pane_session_id = NEXT_PANE_SESSION.fetch_add(1, Ordering::Relaxed);
        let had_output = Arc::new(AtomicBool::new(false));
        // D2: spawn this pane's parse worker (Pane::empty — PTY spawn
        // failure — reaches neither this code nor a worker: no receiver
        // ⇒ no worker).
        let wake_proxy = proxy.clone();
        crate::app::parse_worker::spawn(
            pane_session_id,
            event_rx,
            Arc::clone(&terminal),
            Arc::clone(&writer),
            msg_tx.clone(),
            Arc::clone(&had_output),
            Box::new(move || {
                let _ = wake_proxy.send_event(AppEvent::Wake);
            }),
        );
        Self {
            pane_session_id,
            input_seq: NEXT_INPUT_SEQ.fetch_add(1, Ordering::Relaxed),
            terminal: Some(terminal),
            pty: Some(pty),
            writer: Some(writer),
            had_output,
            mouse_suppress,
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            close_deadline: None,
            close_settled: false,
            last_resize_commit: None,
            last_sent_winsize: None,
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
            // T10 P2 (D2): PTY spawn failure — no receiver ⇒ no parse worker.
            writer: None,
            had_output: Arc::new(AtomicBool::new(false)),
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            close_deadline: None,
            close_settled: false,
            last_resize_commit: None,
            last_sent_winsize: None,
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
            terminal: Some(Arc::new(FairMutex::new(Terminal::with_scrollback(
                24,
                80,
                scrollback_lines,
            )))),
            pty: None,
            writer: None,
            had_output: Arc::new(AtomicBool::new(false)),
            mouse_suppress: new_flag(),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            ime_preedit_cursor: None,
            pending_pty_resize: None,
            close_deadline: None,
            close_settled: false,
            last_resize_commit: None,
            last_sent_winsize: None,
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

    // ── v1.13.6 T10 P1/P2: controlled terminal accessors (D9) ────────────
    //
    // D9 RULE 5 — RE-ENTRANCY ROSTER (rust-reviewer P2-1; keep in sync).
    // Every method below internally locks this pane's terminal. Calling any
    // of them from inside a guard scope (`lock_terminal` guard live, or a
    // `with_terminal` closure) DEADLOCKS AT RUNTIME — parking_lot re-locking
    // is not a compile error. The roster:
    //   Tab:  set_block_scroll / snap_to_bottom / scroll_up_by /
    //         scroll_down_by / enter_primary_history_if_active /
    //         sync_primary_history_view / clamp_block_scroll /
    //         scroll_block_fractional / sync_mouse_modes /
    //         resolve_pending_tui_scroll / active_pane_views /
    //         clear_background_grid_dirty / any_synchronized_output /
    //         to_snapshot / write_user_input / interrupt_pty /
    //         process_messages / finish_pending_blocks
    //   Pane: refresh_primary_history_snapshot(_now) /
    //         resize_terminal_and_queue / set_blocks_retained_limit /
    //         set_blocks_output_cap / set_tui_render_mode
    //   App:  compute_block_view_rows / block_view_active /
    //         terminal_content_contains / pixel_to_grid
    // New re-entrant methods MUST be registered here.
    //
    // D9 RULE 4 — BLOCKING-OP EXEMPTIONS (rust-reviewer P3-1): channel
    // `try_send`/`try_recv` (and other non-blocking forms) ARE allowed in a
    // guard scope; blocking send/recv, fd writes, file IO, and joins are
    // NOT. The parse worker's control-event send and reply write happen
    // strictly after its unlock (D2 step 3).

    /// Lock this pane's terminal for a multi-statement scope (full draw
    /// frame, post-processing pass, resize commit, `write_user_input`, …).
    /// The returned guard is *owned* (`lock_arc` — holds an `Arc` clone), so
    /// sibling pane fields stay mutable while it is held. D9 discipline:
    /// never re-lock the same pane inside the guard scope (see the roster
    /// above); never hold it across channel send/recv, fd writes, or file IO.
    pub(crate) fn lock_terminal(&self) -> Option<TerminalGuard> {
        self.terminal.as_ref().map(|t| t.lock_arc())
    }

    /// Short-scope terminal access for point reads/writes (mode state, cwd,
    /// dims, editor text, …). Returns `None` when the pane has no terminal.
    /// The closure must not itself call `with_terminal`/`lock_terminal` on
    /// the same pane (no nested locks — D9 rule 2, roster above) and must
    /// not perform blocking operations (D9 rule 4).
    pub(crate) fn with_terminal<R>(&self, f: impl FnOnce(&mut Terminal) -> R) -> Option<R> {
        let mut guard = self.lock_terminal()?;
        Some(f(&mut guard))
    }

    /// v1.13.6 T10 P2 (D6): take-and-clear the worker's had_output flag —
    /// the per-frame "this pane parsed output" signal that replaced the
    /// main thread's own byte-drain observation.
    pub(crate) fn take_had_output(&self) -> bool {
        self.had_output.swap(false, Ordering::Relaxed)
    }

    /// Whether this pane has a live terminal (PTY-spawned or test-built).
    pub(crate) fn has_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    /// Test-only terminal injection for the former `pane.terminal = Some(…)`
    /// direct-field assignments.
    #[cfg(test)]
    pub(crate) fn set_terminal_for_test(&mut self, terminal: Terminal) {
        self.terminal = Some(Arc::new(FairMutex::new(terminal)));
    }

    /// Test-only Arc clone of this pane's terminal — lets the REAL parse
    /// worker thread be wired to a pane without a real PTY (the worker e2e
    /// test). D9 rule 1 stands for production consumers: nothing outside
    /// the test builds may obtain the Arc.
    #[cfg(test)]
    pub(crate) fn terminal_arc_for_test(&self) -> Option<Arc<FairMutex<Terminal>>> {
        self.terminal.clone()
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
        self.with_terminal(|t| t.set_blocks_retained_limit(limit));
    }

    /// PLAN_v11217 §3.5 (T4): apply the `[blocks] output_cap_mib` config to
    /// this pane's terminal. Mirror of `set_blocks_retained_limit` — same
    /// creation-site chokepoints. `cap_bytes` arrives pre-converted (and
    /// clamped) from the config_controller helper.
    pub(crate) fn set_blocks_output_cap(&mut self, cap_bytes: usize) {
        self.with_terminal(|t| t.set_block_output_cap(cap_bytes));
    }

    /// v1.11.7 (PLAN_v1117 §三 M1.2, P2-3): inject the user's
    /// `[experimental] tui_render_mode` into this pane's terminal (the
    /// factory default is `noninteractive`; `Terminal::new` itself stays
    /// Classic so weft_core tests keep the v1.11.6 baseline). Mirror of
    /// `set_blocks_retained_limit` — same chokepoints.
    pub(crate) fn set_tui_render_mode(&mut self, mode: weft_core::vt::TuiRenderMode) {
        self.with_terminal(|t| t.set_tui_render_mode(mode));
    }

    /// Queue a geometry transaction without resizing the Grid ahead of its PTY.
    /// Overwriting an older pending size coalesces resize cascades. The effect
    /// dispatcher applies `TIOCSWINSZ` first and only then commits the Grid,
    /// preventing old-width progress output from wrapping in a new-width Grid.
    pub(crate) fn resize_terminal_and_queue(&mut self, rows: usize, cols: usize) -> bool {
        let current = self.with_terminal(|t| (t.grid().num_rows, t.grid().num_cols));
        if current == Some((rows, cols)) && self.pending_pty_resize.is_none() {
            return false;
        }
        if current.is_none() {
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
    /// P1-1 close watchdog delay: how long a closing pane waits for its
    /// worker's `PtyExited` before the pump finalizes it anyway. Generous
    /// enough for a full 8 MiB channel backlog to drain at parse speed
    /// (~200 ms typical), short enough that a hung child cannot leave a
    /// visible zombie pane.
    pub(crate) const CLOSE_WATCHDOG_DELAY: std::time::Duration =
        std::time::Duration::from_millis(500);

    /// Close-initiation teardown: stamp the watchdog deadline, then drop the
    /// PTY (SIGHUP + master fd close → reader EOF → worker drains the
    /// backlog → `PtyExited`). The single chokepoint for both close
    /// initiators (`Tab::begin_close`, `Tab::close_active_pane`).
    pub(crate) fn begin_close_teardown(&mut self) {
        self.close_deadline = Some(std::time::Instant::now() + Self::CLOSE_WATCHDOG_DELAY);
        self.release_pty();
    }

    pub(crate) fn release_pty(&mut self) {
        self.pending_pty_resize = None;
        // T10 P2: drop the pane's own write half too — post-release writes
        // fail fast, and the worker's clone keeps the fd alive only until
        // the event channel closes (then the worker exits and the last
        // Arc drops the fd).
        if let Some(pty) = self.pty.take() {
            drop(pty);
        }
        self.writer = None;
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

    /// Deliver one PTY-native interrupt without discarding command output.
    ///
    /// Ctrl+C is always one ETX byte, regardless of whether the foreground app
    /// uses the alternate screen, primary-screen cursor addressing, raw mode,
    /// SSH, or a conventional shell command. Output is intentionally retained
    /// (interactive tools write confirmation/resume tails after the first or
    /// second Ctrl+C) — with the T10 Flush-marker caveat that a FULL event
    /// channel skips the marker and degrades to tcflush-only, weakening the
    /// tail retention under flood (see `Pty::flush_input`).
    pub fn interrupt_pty(&mut self) -> bool {
        self.with_terminal(|t| t.begin_primary_screen_interrupt_capture());
        // T10 P2 (D4): the ETX goes through the shared writer (it takes the
        // write lock, so it can never split a worker reply mid-write);
        // `flush_input` stays on the Pty (tcflush + the Flush marker).
        let Some(writer) = self.writer.as_ref() else {
            self.with_terminal(|t| t.cancel_primary_screen_interrupt_capture());
            return false;
        };
        if !writer.send_interrupt() {
            self.with_terminal(|t| t.cancel_primary_screen_interrupt_capture());
            return false;
        }
        // Best-effort flush of pending input so a half-typed line doesn't
        // leak past the interrupt. `tcflush(TCIFLUSH)` on the master side
        // clears the kernel's input queue; the app-side half is the parse
        // worker's Flush-marker sweep (D2). Review P2-1, exact semantics:
        // output queued AHEAD of the marker was already parsed into the
        // terminal (NOT dropped — today's Ctrl+C dropped it instantly);
        // only what is still queued behind it is discarded; and when the
        // bounded channel is full the marker is skipped entirely, so up to
        // ~8 MiB of backlog keeps parsing past the interrupt — the v1.11.15
        // resume-tail retention weakens in that arm (P3 probe tracks the
        // worst-case truncation delay: channel capacity ÷ parse rate).
        if let Some(pty) = self.pty.as_ref() {
            pty.flush_input();
        }
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
