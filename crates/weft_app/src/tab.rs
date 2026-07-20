//! Per-tab session state (v0.9 H1).
//!
//! A `Tab` owns the per-shell-session state: the VT `Terminal`, the `Pty`,
//! the input/selection handlers, the IME preedit, and the block-scroll
//! offset. The `App` holds a `Vec<Tab>` and an `active_tab` index; the
//! renderer and find-state stay app-level (shared across tabs) for now —
//! per-tab find state is Stage 4.

use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use weft_core::input::InputHandler;
use weft_core::persistence::TabSnapshot;
use weft_core::pty::{Pty, PtyError, PtyEvent};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

use crate::{AppEvent, AppMsg};

mod lifecycle;
mod primary_history;
mod scroll;

use primary_history::PrimaryHistoryRefresh;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct PendingTuiScroll {
    rows: i32,
    col: usize,
    row: usize,
    mods: weft_core::input::Modifiers,
    resolve_at: std::time::Instant,
}

pub enum TuiScrollResolution {
    PtyBytes(Vec<u8>),
    LocalRows(i32),
}

/// A single shell session (one PTY + one Terminal + per-session UI state).
pub struct Tab {
    pub session_id: u64,
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
    pending_pty_output: Option<Vec<u8>>,
    /// M3.5: private — use the block-scroll API methods below instead.
    block_scroll_offset: usize,
    /// Original persisted state retained while a restored shell is starting.
    /// Until OSC 7 supplies an authoritative cwd, this prevents autosave from
    /// replacing the saved cwd with a transient `None`. It also preserves the
    /// complete tab when PTY creation fails and no live snapshot is possible.
    restored_snapshot: Option<TabSnapshot>,
    /// Wheel rows received while a TUI command is starting but before its
    /// alternate-screen sequence has reached the parser. Replayed once the
    /// alt screen becomes active, so the first trackpad gesture is not lost.
    pending_tui_scroll: Option<PendingTuiScroll>,
    /// Short deadline started at editor submission. This bounds the ambiguous
    /// interval before a TUI's alternate-screen modes reach the parser.
    tui_scroll_deadline: Option<std::time::Instant>,
    tui_scroll_wake_scheduled: bool,
    primary_history_refresh: PrimaryHistoryRefresh,
}

impl Tab {
    /// Create a new tab with an PTY + Terminal pair at the given size.
    ///
    /// `cwd` — if `Some(path)`, the shell starts in that directory (via
    /// `chdir` in the child process before exec). Pass `None` to inherit
    /// the weft process's cwd. Using `chdir` instead of sending a `cd`
    /// command keeps the initial tab clean — no `cd` appears in the
    /// terminal, shell history, or block tracker.
    pub fn new(
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
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
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
            block_scroll_offset: 0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    /// Empty tab (used when PTY spawn fails — terminal/pty stay None).
    pub(crate) fn empty() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
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
            block_scroll_offset: 0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    /// Keep the in-memory Grid and the next PTY `TIOCSWINSZ` inseparable.
    /// Overwriting (rather than preserving) an older pending size is required
    /// when several window/font/sidebar changes coalesce before the throttle
    /// flushes them, including for background tabs.
    pub(crate) fn resize_terminal_and_queue(&mut self, rows: usize, cols: usize) -> bool {
        let Some(terminal) = &mut self.terminal else {
            return false;
        };
        terminal.resize(rows, cols);
        self.pending_pty_resize = Some((rows, cols));
        true
    }

    /// Non-blocking drain of PTY events into channel. Capped per frame.
    pub fn pump_pty(&mut self) {
        let Some(pty) = &mut self.pty else { return };
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
        let delivered = self.pty.as_ref().is_some_and(Pty::send_interrupt);
        if !delivered {
            if let Some(terminal) = &mut self.terminal {
                terminal.cancel_primary_screen_interrupt_capture();
            }
        }
        delivered
    }

    /// Write input initiated by the user to this tab's PTY.
    ///
    /// A first Ctrl+C may freeze the current primary-screen transcript while
    /// an interactive program decides whether to exit. Any later user input
    /// means the program continued, so that frozen transcript is stale and
    /// must not be reused by a future interrupt. Keeping this cancellation at
    /// the PTY boundary covers keyboard, paste, mouse, wheel, workflow, and
    /// delayed TUI-scroll input uniformly.
    pub fn write_user_input(&mut self, data: &[u8]) -> weft_core::pty::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.cancel_primary_screen_interrupt_capture();
        }
        let pty = self.pty.as_ref().ok_or_else(|| {
            PtyError::Write(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "tab has no PTY",
            ))
        })?;
        pty.write_sync(data)
    }

    /// Process queued messages into the terminal and drain finished blocks.
    /// v1.0 P1.5-C3: Time-bounded processing. The loop drains messages until
    /// either the channel is empty OR a ~8ms wall-clock budget is exhausted.
    /// This keeps the render thread responsive during huge PTY bursts (e.g.
    /// `cat huge.log`): instead of blocking for 100ms+ processing a full read
    /// buffer, we process ~8ms worth, yield to the renderer, then resume next
    /// frame. The byte budget (`MAX_BYTES_PER_MESSAGE`) only governs splitting
    /// a single oversized message so one message can't monopolize a frame.
    pub fn process_messages(&mut self) -> (bool, Vec<weft_core::blocks::Block>, bool) {
        let mut need_redraw = false;
        // Split threshold for a single oversized message (matches the PTY
        // read buffer size). Messages larger than this are split: head is
        // processed now, tail is re-queued for the next frame.
        const MAX_BYTES_PER_MESSAGE: usize = 256 * 1024;
        // 8ms leaves ~8ms for rendering at 60fps. We check the clock at most
        // every MIN_BYTES_FOR_TIME_CHECK bytes to avoid Instant::now() overhead
        // dominating for tiny messages.
        const FRAME_TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(8);
        const MIN_BYTES_FOR_TIME_CHECK: usize = 32 * 1024;
        let frame_start = std::time::Instant::now();
        let mut bytes_since_check = 0usize;
        let mut processed_pty_output = false;
        let mut alive = true;

        let mut carried = self.pending_pty_output.take().map(AppMsg::PtyOutput);
        loop {
            let msg = match carried.take() {
                Some(msg) => msg,
                None => match self.msg_rx.try_recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                },
            };
            match msg {
                AppMsg::PtyOutput(mut data) => {
                    if data.len() > MAX_BYTES_PER_MESSAGE {
                        // Keep the remainder ahead of every later AppMsg,
                        // especially PtyExit. Re-queueing it at the channel
                        // tail would invert the original PTY byte order.
                        let tail = data.split_off(MAX_BYTES_PER_MESSAGE);
                        self.pending_pty_output = Some(tail);
                        need_redraw |= self.process_pty_output(&data);
                        processed_pty_output = true;
                        break;
                    }
                    bytes_since_check += data.len();
                    need_redraw |= self.process_pty_output(&data);
                    processed_pty_output = true;
                    // Cooperative yield: if we've spent the frame's time
                    // budget, stop draining and let the renderer draw. The
                    // remaining messages stay queued for next frame.
                    if bytes_since_check >= MIN_BYTES_FOR_TIME_CHECK
                        && frame_start.elapsed() >= FRAME_TIME_BUDGET
                    {
                        break;
                    }
                }
                AppMsg::PtyExit(code) => {
                    tracing::info!("Shell exited: {:?}", code);
                    alive = false;
                    break;
                }
            }
        }

        need_redraw |= self.refresh_primary_history_snapshot(processed_pty_output);
        let mut drained = Vec::new();
        let mut reset_scroll = false;
        if let Some(terminal) = &mut self.terminal {
            let settled = if alive {
                terminal.settle_primary_screen_exit_if_idle(std::time::Instant::now())
            } else {
                terminal.settle_primary_screen_exit()
            };
            if settled {
                need_redraw = true;
            }
            drained = terminal.block_tracker_mut().drain_unpersisted();
            reset_scroll = !drained.is_empty() && !terminal.primary_history_view();
            if terminal.synchronized_output() {
                need_redraw = false;
            }
        }
        if reset_scroll {
            self.snap_to_bottom();
        }

        (alive, drained, need_redraw)
    }

    /// Open a bounded window in which an early wheel gesture may belong to a
    /// TUI whose alternate-screen sequence has not reached the parser yet.
    pub fn arm_tui_scroll_window(&mut self) {
        self.pending_tui_scroll = None;
        self.tui_scroll_deadline = Some(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(2))
                .unwrap_or_else(std::time::Instant::now),
        );
    }

    pub fn tui_scroll_window_active(&self) -> bool {
        self.tui_scroll_deadline
            .is_some_and(|deadline| std::time::Instant::now() <= deadline)
    }

    /// Accumulate a wheel gesture while a launched TUI is taking ownership.
    pub fn queue_tui_scroll(
        &mut self,
        rows: i32,
        col: usize,
        row: usize,
        mods: weft_core::input::Modifiers,
    ) -> bool {
        if rows == 0 || !self.tui_scroll_window_active() {
            return false;
        }
        if let Some(pending) = &mut self.pending_tui_scroll {
            pending.rows = pending.rows.saturating_add(rows).clamp(-100, 100);
            pending.col = col;
            pending.row = row;
            pending.mods = mods;
        } else {
            self.pending_tui_scroll = Some(PendingTuiScroll {
                rows: rows.clamp(-100, 100),
                col,
                row,
                mods,
                resolve_at: std::time::Instant::now() + std::time::Duration::from_millis(50),
            });
            self.tui_scroll_wake_scheduled = false;
        }
        true
    }

    /// Return the delay for the single wake needed to resolve the current
    /// ambiguous startup gesture. Repeated wheel events share the same wake.
    pub fn take_tui_scroll_wake_delay(&mut self) -> Option<std::time::Duration> {
        let pending = self.pending_tui_scroll.as_ref()?;
        if self.tui_scroll_wake_scheduled {
            return None;
        }
        self.tui_scroll_wake_scheduled = true;
        Some(
            pending
                .resolve_at
                .saturating_duration_since(std::time::Instant::now()),
        )
    }

    /// Resolve an early gesture after the 50ms protocol grace period. If the
    /// command entered alt screen, encode against its final mouse modes;
    /// otherwise return rows for normal local block/grid scrolling.
    pub fn resolve_pending_tui_scroll(&mut self) -> Option<TuiScrollResolution> {
        let pending = self.pending_tui_scroll.as_ref()?;
        if std::time::Instant::now() < pending.resolve_at {
            return None;
        }
        let pending = self.pending_tui_scroll.take()?;
        self.tui_scroll_wake_scheduled = false;
        let terminal = self.terminal.as_ref()?;
        if !terminal.is_alt_screen_active() {
            // The first ambiguous gesture has now been classified as ordinary
            // local scrolling. Consume the launch window so subsequent
            // trackpad events are immediate instead of paying 50ms each.
            self.tui_scroll_deadline = None;
            return Some(TuiScrollResolution::LocalRows(pending.rows));
        }

        self.input_handler.app_cursor_keys = terminal.app_cursor_keys;
        self.input_handler.mouse_protocol = terminal.mouse_protocol;
        self.input_handler.sgr_mouse = terminal.sgr_mouse;
        self.tui_scroll_deadline = None;
        let count = pending.rows.unsigned_abs() as usize;
        let mut bytes = Vec::new();

        if terminal.mouse_protocol != weft_core::input::MouseProtocol::Off {
            for _ in 0..count {
                if let Some(encoded) = self.input_handler.encode_scroll(
                    pending.rows > 0,
                    pending.col,
                    pending.row,
                    pending.mods,
                ) {
                    bytes.extend_from_slice(&encoded);
                }
            }
            return Some(TuiScrollResolution::PtyBytes(bytes));
        }

        let key = if pending.rows > 0 {
            weft_core::input::KeyCode::Up
        } else {
            weft_core::input::KeyCode::Down
        };
        let single = self
            .input_handler
            .encode_key(key, pending.mods & weft_core::input::Modifiers::SHIFT);
        bytes.reserve(single.len() * count);
        for _ in 0..count {
            bytes.extend_from_slice(&single);
        }
        Some(TuiScrollResolution::PtyBytes(bytes))
    }

    /// Whether this tab's terminal + pty are initialized.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        self.terminal.is_some() && self.pty.is_some()
    }

    /// Directory inherited by a newly-created sibling tab. Prefer live OSC 7
    /// state, retaining a restored cwd until the shell reports one.
    pub fn launch_cwd(&self) -> Option<&str> {
        self.terminal
            .as_ref()
            .and_then(Terminal::cwd)
            .or_else(|| self.restored_snapshot.as_ref()?.cwd.as_deref())
    }

    /// v1.0 H4: Serialize this tab's UI state to a [`TabSnapshot`] for
    /// SQLite persistence. A restored tab whose PTY failed keeps its loaded
    /// snapshot; only a fresh empty tab with no recovery state returns `None`.
    ///
    /// The PTY itself is NOT serialized (impossible to revive). On
    /// restore, the tab shows the saved editor draft + block history;
    /// the user presses Enter to spawn a fresh shell in the saved cwd.
    pub fn to_snapshot(&self, position: usize, active: bool) -> Option<TabSnapshot> {
        let Some(terminal) = self.terminal.as_ref() else {
            let mut snapshot = self.restored_snapshot.clone()?;
            snapshot.position = position;
            snapshot.active = active;
            snapshot.block_scroll_offset = self.block_scroll();
            return Some(snapshot);
        };
        let cwd = terminal.cwd().map(str::to_owned).or_else(|| {
            self.restored_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.cwd.clone())
        });
        let editor_buffer =
            weft_core::persistence::TabSnapshot::encode_editor_buffer(&terminal.editor().buffer);
        let shell_phase = match terminal.block_tracker().phase() {
            weft_core::blocks::ShellPhase::NotIntegrated => "NotIntegrated",
            weft_core::blocks::ShellPhase::AtPrompt => "AtPrompt",
            weft_core::blocks::ShellPhase::CommandExecuting => "CommandExecuting",
        };
        Some(TabSnapshot {
            position,
            active,
            cwd,
            block_scroll_offset: self.block_scroll(),
            editor_buffer,
            shell_phase: shell_phase.to_string(),
        })
    }

    /// v1.0 H4: Restore the editor draft + block-scroll offset from a
    /// [`TabSnapshot`]. Called after [`Tab::new`] to apply the saved
    /// state. The PTY + terminal are already initialized by `new`;
    /// this just rehydrates the editor buffer and scroll offset.
    ///
    /// Returns `false` if the snapshot's editor buffer JSON was invalid
    /// (the tab stays usable with an empty editor — same as a fresh tab).
    pub fn restore_from_snapshot(&mut self, snap: &TabSnapshot) -> bool {
        self.restored_snapshot = Some(snap.clone());
        self.set_block_scroll(snap.block_scroll_offset);
        let Some(terminal) = self.terminal.as_mut() else {
            return false;
        };
        match weft_core::persistence::TabSnapshot::decode_editor_buffer(&snap.editor_buffer) {
            Some(buf) => {
                terminal.editor_mut().buffer = buf;
                true
            }
            None => {
                tracing::warn!("failed to deserialize editor buffer; using empty");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tab_is_not_alive() {
        let t = Tab::empty();
        assert!(!t.is_alive());
    }

    #[test]
    fn empty_tab_has_no_terminal() {
        let t = Tab::empty();
        assert!(t.terminal.is_none());
        assert!(t.pty.is_none());
    }

    #[test]
    fn empty_tab_pump_pty_is_noop() {
        let mut t = Tab::empty();
        // Should not panic — just returns early since pty is None.
        t.pump_pty();
    }

    #[test]
    fn empty_tab_process_messages_returns_empty() {
        let mut t = Tab::empty();
        let (alive, drained, need_redraw) = t.process_messages();
        assert!(alive);
        assert!(drained.is_empty());
        assert!(!need_redraw);
    }

    #[test]
    fn empty_tab_has_zero_block_scroll() {
        let t = Tab::empty();
        assert_eq!(t.block_scroll(), 0);
    }

    #[test]
    fn queued_scroll_replays_after_alt_screen_entry() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));
        assert!(t.resolve_pending_tui_scroll().is_none());

        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        assert!(t.resolve_pending_tui_scroll().is_none());
        expire_pending_scroll(&mut t);
        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected queued arrow bytes");
        };
        assert_eq!(bytes, b"\x1b[B\x1b[B");
        assert!(t.resolve_pending_tui_scroll().is_none());
    }

    #[test]
    fn queued_scroll_falls_back_to_local_rows_for_normal_command() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(3, 5, 10, weft_core::input::Modifiers::empty()));
        t.terminal.as_mut().unwrap().process(b"\x1b]133;A\x07");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::LocalRows(rows)) = t.resolve_pending_tui_scroll() else {
            panic!("expected local scroll fallback");
        };
        assert_eq!(rows, 3);
        assert!(t.pending_tui_scroll.is_none());
        assert!(!t.tui_scroll_window_active());
        assert!(!t.queue_tui_scroll(1, 5, 10, weft_core::input::Modifiers::empty()));
    }

    #[test]
    fn expired_startup_window_does_not_queue_long_running_command_scroll() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        t.tui_scroll_deadline =
            std::time::Instant::now().checked_sub(std::time::Duration::from_millis(1));

        assert!(!t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));

        assert!(!t.tui_scroll_window_active());
        assert!(t.pending_tui_scroll.is_none());
    }

    #[test]
    fn queued_scroll_uses_mouse_protocol_when_tui_enables_it() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));
        t.terminal
            .as_mut()
            .unwrap()
            .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected queued mouse bytes");
        };
        assert_eq!(bytes, b"\x1b[<65;6;11M");
    }

    #[test]
    fn queued_scroll_waits_for_mouse_mode_in_next_pty_chunk() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));

        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        assert!(t.resolve_pending_tui_scroll().is_none());
        assert!(t.resolve_pending_tui_scroll().is_none());

        t.terminal
            .as_mut()
            .unwrap()
            .process(b"\x1b[?1000h\x1b[?1006h");
        expire_pending_scroll(&mut t);
        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected mouse bytes after split mode sequence");
        };
        assert_eq!(bytes, b"\x1b[<65;6;11M");
    }

    #[test]
    fn queued_arrow_scroll_preserves_shift_modifier() {
        let mut t = tab_with_terminal(100);
        t.arm_tui_scroll_window();
        assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::SHIFT));
        t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
        expire_pending_scroll(&mut t);

        let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
            panic!("expected shifted arrow bytes");
        };
        assert_eq!(bytes, b"\x1b[1;2B");
    }

    // ── v1.0 H4: TabsAutoSave snapshot roundtrip ─────────────────────

    /// Build a Tab with a live Terminal but no PTY — enough for
    /// `to_snapshot` / `restore_from_snapshot` without spawning a shell.
    fn tab_with_terminal(scrollback_lines: usize) -> Tab {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Tab {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
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
            block_scroll_offset: 0,
            restored_snapshot: None,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
            primary_history_refresh: PrimaryHistoryRefresh::default(),
        }
    }

    fn expire_pending_scroll(tab: &mut Tab) {
        tab.pending_tui_scroll.as_mut().unwrap().resolve_at = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap();
    }

    #[test]
    fn snapshot_is_none_when_no_terminal() {
        // Tab::empty has no terminal → to_snapshot returns None.
        let t = Tab::empty();
        assert!(t.to_snapshot(0, false).is_none());
    }

    #[test]
    fn snapshot_roundtrip_preserves_scroll_offset_and_editor() {
        let mut t = tab_with_terminal(1000);
        t.set_block_scroll(7);
        t.terminal
            .as_mut()
            .unwrap()
            .editor_mut()
            .buffer
            .set_text("echo hi");

        let snap = t.to_snapshot(2, true).expect("snapshot with terminal");
        assert_eq!(snap.position, 2);
        assert!(snap.active);
        assert_eq!(snap.block_scroll_offset, 7);
        assert!(!snap.editor_buffer.is_empty(), "editor buffer encoded");

        // Restore into a fresh tab.
        let mut restored = tab_with_terminal(1000);
        assert!(restored.restore_from_snapshot(&snap));
        assert_eq!(restored.block_scroll(), 7);
        let editor_text = restored.terminal.as_ref().unwrap().editor().buffer.text();
        assert_eq!(editor_text, "echo hi");
    }

    #[test]
    fn snapshot_default_shell_phase_is_not_integrated() {
        // A fresh Terminal has NotIntegrated phase → snapshot encodes that.
        let t = tab_with_terminal(1000);
        let snap = t.to_snapshot(0, false).unwrap();
        assert_eq!(snap.shell_phase, "NotIntegrated");
    }

    #[test]
    fn snapshot_cwd_none_when_unset() {
        // No OSC 7 received → cwd is None in the snapshot.
        let t = tab_with_terminal(1000);
        let snap = t.to_snapshot(0, false).unwrap();
        assert!(snap.cwd.is_none());
    }

    #[test]
    fn restore_from_invalid_editor_buffer_keeps_empty() {
        // Garbage JSON → restore returns false, editor stays empty.
        let mut t = tab_with_terminal(1000);
        let snap = weft_core::persistence::TabSnapshot {
            position: 0,
            active: false,
            cwd: None,
            block_scroll_offset: 3,
            editor_buffer: "{not valid json".to_string(),
            shell_phase: "AtPrompt".to_string(),
        };
        assert!(!t.restore_from_snapshot(&snap));
        // scroll offset still applied even if editor restore failed.
        assert_eq!(t.block_scroll(), 3);
        let editor_text = t.terminal.as_ref().unwrap().editor().buffer.text();
        assert!(editor_text.is_empty());
    }
}

#[cfg(test)]
#[path = "tab/snapshot_tests.rs"]
mod snapshot_tests;
