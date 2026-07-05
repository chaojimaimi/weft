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
    pub fn new(
        rows: usize,
        cols: usize,
        scrollback_lines: usize,
        proxy: &winit::event_loop::EventLoopProxy<AppEvent>,
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
}
