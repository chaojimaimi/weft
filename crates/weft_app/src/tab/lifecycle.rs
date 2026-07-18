use super::Tab;
use crate::AppMsg;
use weft_core::pty::PtyEvent;

const MAX_CLOSE_TAIL_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_CLOSE_TAIL_EVENTS: usize = 256;

impl Tab {
    pub(super) fn process_pty_output(&mut self, data: &[u8]) -> bool {
        let Some(terminal) = &mut self.terminal else {
            return false;
        };
        terminal.process(data);
        let response = terminal.take_response();
        if !response.is_empty() {
            if let Some(pty) = &self.pty {
                if let Err(error) = pty.write_sync(&response) {
                    tracing::warn!(%error, "failed to write terminal response");
                }
            }
        }
        true
    }

    pub fn finish_pending_blocks(&mut self) -> Vec<weft_core::blocks::Block> {
        let preserve_screen_tail = self.terminal.as_ref().is_some_and(|terminal| {
            terminal.primary_screen_app_active() || terminal.primary_screen_exit_pending()
        });
        if preserve_screen_tail {
            self.drain_bounded_close_tail();
        }
        let Some(terminal) = &mut self.terminal else {
            return Vec::new();
        };
        terminal.settle_primary_screen_exit();
        terminal.block_tracker_mut().drain_unpersisted()
    }

    fn drain_bounded_close_tail(&mut self) {
        let mut bytes = 0_usize;
        let mut events = 0_usize;
        if let Some(data) = self.pending_pty_output.take() {
            if !self.consume_close_output(&data, &mut bytes, &mut events) {
                return;
            }
        }

        let queued_messages = self.msg_rx.len().min(MAX_CLOSE_TAIL_EVENTS - events);
        let mut exited = false;
        for _ in 0..queued_messages {
            let Ok(message) = self.msg_rx.try_recv() else {
                break;
            };
            match message {
                AppMsg::PtyOutput(data) => {
                    if !self.consume_close_output(&data, &mut bytes, &mut events) {
                        return;
                    }
                }
                AppMsg::PtyExit(code) => {
                    tracing::info!(?code, "shell exited while closing tab");
                    exited = true;
                    break;
                }
            }
        }
        if exited || events >= MAX_CLOSE_TAIL_EVENTS {
            return;
        }

        let queued_pty_events = self
            .pty
            .as_ref()
            .map(weft_core::pty::Pty::queued_event_count)
            .unwrap_or(0)
            .min(MAX_CLOSE_TAIL_EVENTS - events);
        for _ in 0..queued_pty_events {
            let Some(event) = self.pty.as_mut().and_then(|pty| pty.try_recv().ok()) else {
                break;
            };
            match event {
                PtyEvent::Output(data) => {
                    if !self.consume_close_output(&data, &mut bytes, &mut events) {
                        return;
                    }
                }
                PtyEvent::Exit(code) => {
                    tracing::info!(?code, "shell exited while closing tab");
                    break;
                }
            }
        }
    }

    fn consume_close_output(&mut self, data: &[u8], bytes: &mut usize, events: &mut usize) -> bool {
        if data.len() > MAX_CLOSE_TAIL_BYTES.saturating_sub(*bytes)
            || *events >= MAX_CLOSE_TAIL_EVENTS
        {
            tracing::warn!(
                bytes = *bytes,
                events = *events,
                "bounded close-time TUI tail drain reached its limit"
            );
            return false;
        }
        *bytes += data.len();
        *events += 1;
        self.process_pty_output(data);
        true
    }
}
