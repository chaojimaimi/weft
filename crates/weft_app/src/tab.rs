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
use weft_core::pty::{Pty, PtyEvent};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

use crate::{AppEvent, AppMsg};

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
    /// M3.5: private — use the block-scroll API methods below instead.
    block_scroll_offset: usize,
    /// Wheel rows received while a TUI command is starting but before its
    /// alternate-screen sequence has reached the parser. Replayed once the
    /// alt screen becomes active, so the first trackpad gesture is not lost.
    pending_tui_scroll: Option<PendingTuiScroll>,
    /// Short deadline started at editor submission. This bounds the ambiguous
    /// interval before a TUI's alternate-screen modes reach the parser.
    tui_scroll_deadline: Option<std::time::Instant>,
    tui_scroll_wake_scheduled: bool,
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
            block_scroll_offset: 0,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
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
            block_scroll_offset: 0,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
        }
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

    /// v1.0 fix: Flush all stale PTY output after Ctrl+C.
    ///
    /// Discards pending output in both the kernel PTY buffer and the
    /// internal message channel. Called after `send_interrupt()` so the UI
    /// can respond immediately instead of spending many frames processing
    /// the interrupted command's remaining output (e.g. ~7MB from
    /// `seq 1 10000000`).
    ///
    /// `PtyExit` events are preserved — they signal shell exit and must not
    /// be lost.
    pub fn flush_pty_output(&mut self) {
        // Flush kernel PTY read buffer + drain Pty's internal event channel.
        if let Some(pty) = &mut self.pty {
            pty.flush_input();
        }
        // Drain queued AppMsg::PtyOutput messages from msg_rx.
        // PtyExit is re-queued — must not be lost.
        while let Ok(msg) = self.msg_rx.try_recv() {
            if let AppMsg::PtyExit(code) = msg {
                let _ = self.msg_tx.send(AppMsg::PtyExit(code));
            }
            // PtyOutput → discard (stale output from interrupted command)
        }
        // v1.0 fix: Reset shell phase to AtPrompt — the flush may have
        // discarded the OSC 133;A marker the shell emits after interruption.
        // Without this, phase stays CommandExecuting and the prompt/editor
        // never reappears (user must press Enter to recover).
        if let Some(terminal) = &mut self.terminal {
            terminal.block_tracker_mut().reset_to_prompt();
        }
    }

    /// Process queued messages into the terminal. Returns
    /// `(alive, drained_blocks, need_redraw)`.
    /// `alive=false` when the shell exited. `drained_blocks` are finished
    /// command blocks ready for persistence (the caller inserts them into
    /// the shared BlockStore). `need_redraw` is true when any output was
    /// processed (so the caller can call `request_redraw`).
    ///
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

        while let Ok(msg) = self.msg_rx.try_recv() {
            match msg {
                AppMsg::PtyOutput(data) => {
                    if data.len() > MAX_BYTES_PER_MESSAGE {
                        // Oversized message: process the head, re-queue the
                        // tail, then yield to the renderer this frame.
                        let head = data[..MAX_BYTES_PER_MESSAGE].to_vec();
                        let tail = data[MAX_BYTES_PER_MESSAGE..].to_vec();
                        let _ = self.msg_tx.send(AppMsg::PtyOutput(tail));
                        let mut response = Vec::new();
                        if let Some(terminal) = &mut self.terminal {
                            terminal.process(&head);
                            response = terminal.take_response();
                            need_redraw = true;
                        }
                        if !response.is_empty() {
                            if let Some(pty) = &self.pty {
                                if let Err(e) = pty.write_sync(&response) {
                                    tracing::warn!(error = %e, "failed to write terminal response");
                                }
                            }
                        }
                        break;
                    }
                    bytes_since_check += data.len();
                    let mut response = Vec::new();
                    if let Some(terminal) = &mut self.terminal {
                        terminal.process(&data);
                        response = terminal.take_response();
                        need_redraw = true;
                    }
                    if !response.is_empty() {
                        if let Some(pty) = &self.pty {
                            if let Err(e) = pty.write_sync(&response) {
                                tracing::warn!(error = %e, "failed to write terminal response");
                            }
                        }
                    }
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
                    return (false, Vec::new(), need_redraw);
                }
            }
        }

        let mut drained = Vec::new();
        if let Some(terminal) = &mut self.terminal {
            drained = terminal.block_tracker_mut().drain_unpersisted();
        }

        (true, drained, need_redraw)
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

    /// v1.0 H4: Serialize this tab's UI state to a [`TabSnapshot`] for
    /// SQLite persistence. Returns `None` when the terminal is missing
    /// (PTY spawn failed — nothing worth persisting).
    ///
    /// The PTY itself is NOT serialized (impossible to revive). On
    /// restore, the tab shows the saved editor draft + block history;
    /// the user presses Enter to spawn a fresh shell in the saved cwd.
    pub fn to_snapshot(
        &self,
        position: usize,
        active: bool,
    ) -> Option<weft_core::persistence::TabSnapshot> {
        let terminal = self.terminal.as_ref()?;
        let cwd = terminal.cwd().map(|s| s.to_string());
        let editor_buffer =
            weft_core::persistence::TabSnapshot::encode_editor_buffer(&terminal.editor().buffer);
        let shell_phase = match terminal.block_tracker().phase() {
            weft_core::blocks::ShellPhase::NotIntegrated => "NotIntegrated",
            weft_core::blocks::ShellPhase::AtPrompt => "AtPrompt",
            weft_core::blocks::ShellPhase::CommandExecuting => "CommandExecuting",
        };
        Some(weft_core::persistence::TabSnapshot {
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
    pub fn restore_from_snapshot(&mut self, snap: &weft_core::persistence::TabSnapshot) -> bool {
        self.block_scroll_offset = snap.block_scroll_offset;
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

    // ── Block-scroll API (M3.5) ──────────────────────────────────────
    // Encapsulates block_scroll_offset writes so controllers don't set
    // the field directly. Each method encodes one intent.

    /// Current block-scroll offset (0 = bottom / most recent).
    pub fn block_scroll(&self) -> usize {
        self.block_scroll_offset
    }

    /// Set the block-scroll offset to an exact value (clamped to 0).
    pub fn set_block_scroll(&mut self, offset: usize) {
        self.block_scroll_offset = offset;
    }

    /// Scroll to bottom (offset = 0, the most recent content).
    pub fn snap_to_bottom(&mut self) {
        self.block_scroll_offset = 0;
    }

    /// Scroll up by `rows` (away from bottom, offset increases).
    pub fn scroll_up_by(&mut self, rows: usize) {
        self.block_scroll_offset = self.block_scroll_offset.saturating_add(rows);
    }

    /// Scroll down by `rows` (toward bottom, offset decreases).
    pub fn scroll_down_by(&mut self, rows: usize) {
        self.block_scroll_offset = self.block_scroll_offset.saturating_sub(rows);
    }

    /// Clamp the offset to `max_scroll` (called after the terminal updates
    /// its block count so offset can't point past the last block).
    pub fn clamp_block_scroll(&mut self, max_scroll: usize) {
        if self.block_scroll_offset > max_scroll {
            self.block_scroll_offset = max_scroll;
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

    #[test]
    fn empty_tab_has_no_pending_resize() {
        let t = Tab::empty();
        assert!(t.pending_pty_resize.is_none());
    }

    #[test]
    fn empty_tab_has_empty_ime_preedit() {
        let t = Tab::empty();
        assert!(t.ime_preedit.is_empty());
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
            block_scroll_offset: 0,
            pending_tui_scroll: None,
            tui_scroll_deadline: None,
            tui_scroll_wake_scheduled: false,
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
