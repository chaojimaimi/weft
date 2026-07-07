//! Per-tab session state (v0.9 H1).
//!
//! A `Tab` owns the per-shell-session state: the VT `Terminal`, the `Pty`,
//! the input/selection handlers, the IME preedit, and the block-scroll
//! offset. The `App` holds a `Vec<Tab>` and an `active_tab` index; the
//! renderer and find-state stay app-level (shared across tabs) for now —
//! per-tab find state is Stage 4.

use crossbeam_channel::{Receiver, Sender};
use weft_core::input::InputHandler;
use weft_core::pty::{Pty, PtyEvent};
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

use crate::{AppEvent, AppMsg};

/// A single shell session (one PTY + one Terminal + per-session UI state).
pub struct Tab {
    pub terminal: Option<Terminal>,
    pub pty: Option<Pty>,
    pub msg_rx: Receiver<AppMsg>,
    pub msg_tx: Sender<AppMsg>,
    pub input_handler: InputHandler,
    pub selection_handler: SelectionHandler,
    pub ime_preedit: String,
    pub pending_pty_resize: Option<(usize, usize)>,
    pub block_scroll_offset: usize,
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
            terminal: Some(terminal),
            pty: Some(pty),
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            pending_pty_resize: None,
            block_scroll_offset: 0,
        }
    }

    /// Empty tab (used when PTY spawn fails — terminal/pty stay None).
    fn empty() -> Self {
        let (msg_tx, msg_rx) = crossbeam_channel::bounded(1024);
        Self {
            terminal: None,
            pty: None,
            msg_rx,
            msg_tx,
            input_handler: InputHandler::new(),
            selection_handler: SelectionHandler::new(),
            ime_preedit: String::new(),
            pending_pty_resize: None,
            block_scroll_offset: 0,
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
    }

    /// Process queued messages into the terminal. Returns
    /// `(alive, drained_blocks, need_redraw)`.
    /// `alive=false` when the shell exited. `drained_blocks` are finished
    /// command blocks ready for persistence (the caller inserts them into
    /// the shared BlockStore). `need_redraw` is true when any output was
    /// processed (so the caller can call `request_redraw`).
    pub fn process_messages(&mut self) -> (bool, Vec<weft_core::blocks::Block>, bool) {
        let mut need_redraw = false;
        const MAX_BYTES_PER_FRAME: usize = 64 * 1024;
        let mut bytes_this_frame = 0usize;

        while bytes_this_frame < MAX_BYTES_PER_FRAME {
            let msg = match self.msg_rx.try_recv() {
                Ok(m) => m,
                Err(_) => break,
            };
            match msg {
                AppMsg::PtyOutput(data) => {
                    let remaining_budget = MAX_BYTES_PER_FRAME - bytes_this_frame;
                    if data.len() > remaining_budget {
                        let head = data[..remaining_budget].to_vec();
                        let tail = data[remaining_budget..].to_vec();
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
                    bytes_this_frame += data.len();
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
    pub fn to_snapshot(&self, position: usize) -> Option<weft_core::persistence::TabSnapshot> {
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
            cwd,
            block_scroll_offset: self.block_scroll_offset,
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
        assert_eq!(t.block_scroll_offset, 0);
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
}
