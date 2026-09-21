use super::Tab;
use crate::AppMsg;
use weft_core::pty::PtyEvent;

const MAX_CLOSE_TAIL_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_CLOSE_TAIL_EVENTS: usize = 256;

impl Tab {
    /// Active-pane entry to the PTY output processor. FIX_background_pane_pump
    /// §2.2: the body moved to `tab/pane_pump.rs`
    /// ([`Tab::process_pty_output_for_pane`]) so the per-pane consume pass and
    /// this path share one implementation; the alt-flip recording there now
    /// carries the pane id (`record_alt_flip_instants(pane_id, flips)`).
    /// Kept for the tab-close tail drain (`finish_pending_blocks` →
    /// `drain_bounded_close_tail`) and the tab tests, which all operate on
    /// the active pane.
    pub(super) fn process_pty_output(&mut self, data: &[u8]) -> bool {
        self.process_pty_output_for_pane(self.active_pane, data)
    }

    pub fn finish_pending_blocks(&mut self) -> Vec<weft_core::blocks::Block> {
        let preserve_screen_tail = self.terminal.as_ref().is_some_and(|terminal| {
            terminal.primary_screen_app_active() || terminal.primary_screen_exit_pending()
        });
        if preserve_screen_tail {
            self.drain_bounded_close_tail();
        }
        let split_heads;
        let blocks = {
            let Some(terminal) = &mut self.terminal else {
                return Vec::new();
            };
            terminal.settle_primary_screen_exit();
            // v1.10.26 Batch D (D-3): the settle above can split 1MiB TUI
            // history heads — surface their count so the detached
            // block-scroll anchor compensates for the inserted chrome rows.
            split_heads = terminal.take_pending_screen_split_heads().unwrap_or(0);
            terminal.block_tracker_mut().drain_unpersisted()
        };
        if split_heads > 0 {
            self.compensate_anchor_for_split(split_heads);
        }
        blocks
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
                    // v1.11.4 (PLAN_v1114 §1.3): close-tail reset — a dying
                    // shell must not leave negotiated kitty flags behind.
                    if let Some(t) = &mut self.terminal {
                        t.kitty_reset();
                    }
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
                    // v1.11.4 (PLAN_v1114 §1.3): close-tail reset (PtyEvent
                    // side — same contract as AppMsg::PtyExit above).
                    if let Some(t) = &mut self.terminal {
                        t.kitty_reset();
                    }
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
