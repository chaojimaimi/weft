use super::Tab;
use crate::AppMsg;
use weft_core::pty::PtyEvent;

const MAX_CLOSE_TAIL_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_CLOSE_TAIL_EVENTS: usize = 256;

impl Tab {
    pub(super) fn process_pty_output(&mut self, data: &[u8]) -> bool {
        // v1.10.4: detect alt-screen (DEC 1049) enter/exit. When a TUI
        // toggles between alt-screen and primary screen, the PTY cols must
        // switch between full-width (alt-screen: TUI needs every column to
        // paint borders/layout) and gutter-subtracted (primary screen:
        // BlockView reserves breathing room).
        //
        // v1.10.26 Batch D (D-2): alt toggles are detected by the terminal's
        // u64 flip-counter diff across the `process()` batch, not by
        // comparing the `alt_active` boolean before/after — a batch that
        // contains an h→l pair nets the boolean to zero yet still performed
        // two real flips, and those must refresh the flip history / debounce
        // window or the burst lock expires early (v1.10.19 loop loophole).
        //
        // v1.10.21: same capture-before pattern for the alt-screen history
        // peek — the VT core clears the flag itself on CSI ?1049l (deep in
        // the parser, unreachable from the app layer), and the entry gate's
        // re-entry lockout must arm on that exit too. `note_exit` runs after
        // the terminal borrow ends because the gate is a sibling Pane field.
        let was_peeking = self
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
        let (response, alt_flips) = match &mut self.terminal {
            Some(terminal) => {
                let before = terminal.alt_flip_count();
                terminal.process(data);
                let flips = terminal.alt_flip_count().saturating_sub(before);
                (terminal.take_response(), flips)
            }
            None => return false,
        };
        // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING) DEBUG probe
        // (stage 3/4): first PTY output after a committed resize — measures
        // when omp starts repainting (the ioctl-to-repaint gap). Fires once
        // per resize, then disarms.
        if let Some(since_ioctl) = self.take_resize_output_probe() {
            tracing::debug!(
                since_ioctl_ms = since_ioctl.as_millis(),
                bytes = data.len(),
                "RESIZE_PROBE first_pty_output",
            );
        }
        if alt_flips > 0 {
            self.pending_alt_rescale = true;
            // v1.10.19: arm the debounce window — take_pending_alt_rescale
            // holds the recompute while toggles repeat inside it so a burst
            // coalesces into one recompute (see tab/resize.rs).
            // v1.10.26 (D-1/D-2) + v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): the
            // flip history (last two instants + source pane) is driven off the
            // counter diff — the burst-storm signature for the cols mirror
            // freeze (`burst_locked_cols`).
            self.record_alt_flip_instants(alt_flips);
        }
        if was_peeking
            && !self
                .terminal
                .as_ref()
                .is_some_and(|t| t.is_alt_screen_history_peek())
        {
            self.alt_peek_gate.note_exit();
        }
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
