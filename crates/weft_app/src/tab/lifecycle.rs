use super::Tab;
#[cfg(test)]
use crate::app::parse_worker::parse_batch_locked;
#[cfg(test)]
use crate::AppMsg;
#[cfg(test)]
use std::time::Instant;

impl Tab {
    /// v1.13.6 T10 P2 (D5): the main thread no longer parses PTY output —
    /// `process_pty_output` is retired from production and survives as the
    /// TEST seam for the pre-worker tests (tab/tests.rs resize/alt-flip
    /// families): it runs the worker's D2 parse and applies the tab-level
    /// alt-flip side effects INLINE (synchronously), which is what those
    /// tests assert against (the production path defers them to the
    /// `AltFlipped` control event — see `feed_pty_output_for_test`).
    #[cfg(test)]
    pub(crate) fn process_pty_output(&mut self, data: &[u8]) {
        let pane_id = self.active_pane;
        let (response, flips, peek_exited) = {
            let Some(pane) = self.panes.get(&pane_id) else {
                return;
            };
            let Some(mut terminal) = pane.lock_terminal() else {
                return;
            };
            let parsed = parse_batch_locked(&mut terminal, data);
            (parsed.response, parsed.alt_flips, parsed.peek_exited)
        };
        // No PTY in the test panes — the worker's reply leg has no target.
        let _ = response;
        if flips > 0 {
            self.pending_alt_rescale = true;
            self.record_alt_flip_instants(pane_id, flips, Instant::now());
        }
        if peek_exited {
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                pane.alt_peek_gate.note_exit();
            }
        }
    }

    /// v1.13.6 T10 P2 test seam: run the worker's D2 parse for one pane and
    /// enqueue the SAME control events the worker would (`AltFlipped`),
    /// plus raise the pane's `had_output` flag — so the
    /// `Tab::process_messages` control path is exercised end to end without
    /// a real thread. Replaces the retired `AppMsg::PtyOutput` injection.
    #[cfg(test)]
    pub(crate) fn feed_pty_output_for_test(
        &mut self,
        pane_id: weft_core::pane_layout::PaneId,
        data: &[u8],
    ) {
        let (response, flips, peek_exited) = {
            let Some(pane) = self.panes.get(&pane_id) else {
                return;
            };
            let Some(mut terminal) = pane.lock_terminal() else {
                return;
            };
            let parsed = parse_batch_locked(&mut terminal, data);
            (parsed.response, parsed.alt_flips, parsed.peek_exited)
        };
        let Some(pane) = self.panes.get(&pane_id) else {
            return;
        };
        let _ = response; // no writer in test panes — the reply leg has no target
        pane.had_output
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if flips > 0 || peek_exited {
            let _ = pane.msg_tx.send(AppMsg::AltFlipped {
                at: Instant::now(),
                flips,
                peek_exited,
            });
        }
    }

    /// v1.13.6 T10 P2 (D2): BEGIN the async close contract — drop every
    /// PTY-backed pane's `Pty` (SIGHUP + master fd close → reader EOF → the
    /// pane's parse worker finishes the channel backlog into the terminal →
    /// `PtyExited` control event). The panes STAY in the tree until their
    /// `PtyExited` arrives; the exit arm then runs the force-settle + drain
    /// that `finish_pending_blocks` used to do inline, and the last pane's
    /// exit walks the existing remove_dead path (which persists the final
    /// snapshot — N-2 — and converts the final blocks into PersistBlocks
    /// effects). Output produced before the close is therefore NOT lost:
    /// the worker parses it before reporting the exit (bounded by the
    /// channel's 8 MiB ceiling — the retired `MAX_CLOSE_TAIL` semantics).
    ///
    /// Returns `true` when at least one pane is still parsing (async close
    /// in flight); `false` when every pane was already PTY-less (nothing
    /// will ever report `PtyExited` — the caller must close synchronously).
    pub(crate) fn begin_close(&mut self) -> bool {
        let mut async_pending = false;
        for pane in self.panes.values_mut() {
            if pane.pty.is_some() {
                // P1-1: stamp the close watchdog deadline so a SIGHUP-immune
                // child cannot leave the pane hanging without a PtyExited.
                pane.begin_close_teardown();
                async_pending = true;
            }
        }
        async_pending
    }

    /// Finish any pending block work for this tab. Called on the close
    /// paths (tab close / drag close / app shutdown).
    ///
    /// v1.13.6 T10 P2 (D2): the bounded close-tail drain
    /// (`drain_bounded_close_tail`) is GONE — the close contract is async:
    /// the close path drops the pane's PTY, the parse worker parses the
    /// backlog into the terminal, and the `PtyExited` arm (pane_pump) runs
    /// this same settle+drain when it arrives. What remains here is the
    /// synchronous finish for panes that can still hold unpersisted block
    /// state without a live worker (PTY-less panes, restored sessions, the
    /// shutdown path).
    pub fn finish_pending_blocks(&mut self) -> Vec<weft_core::blocks::Block> {
        let split_heads;
        let blocks = {
            let Some(mut terminal) = self.lock_terminal() else {
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
}
