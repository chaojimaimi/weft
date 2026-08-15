//! Alt-screen wheel routing (v1.10.21) — the mouse_controller alt branch,
//! extracted so the controller stays wiring-only.
//!
//! Scope: `is_alt_screen_active() && !mouse_reporting` only. Every gesture
//! goes through the pure state machine in `crate::alt_peek` (docs/
//! FIX_ALT_PEEK_WARP_ALIGNMENT.md, Warp parity): a plain wheel forwards
//! arrow keys to the TUI, Shift+wheel-up enters the history peek, a plain
//! wheel inside the peek returns to the live TUI instantly, and the entry
//! gate (re-entry lockout + ≥2-row net-travel) filters the trackpad's
//! inertial tail.

use super::*;

impl App {
    /// Route one quantized alt-screen wheel gesture. `rows` is the signed
    /// cell-row delta (up = positive), `lines` its unsigned magnitude for
    /// batch-encoding arrow keys; `up` is the direction.
    pub(super) fn handle_alt_screen_wheel(&mut self, rows: i32, lines: usize, up: bool) {
        let shift = self.interaction.mods.state().shift_key();
        let peeking = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
        // Feed the gate only Shift gestures' signed rows (up = positive):
        // entry requires Shift anyway, so plain-wheel travel must not
        // accumulate toward the threshold across separate gestures (a
        // later 1-line Shift flick would otherwise enter peek). The gate
        // also enforces the 400ms post-exit lockout (see `PeekEntryGate`).
        let entry_allowed = if shift {
            self.sessions
                .active_mut()
                .alt_peek_gate
                .allow_entry(std::time::Instant::now(), rows as f32)
        } else {
            self.sessions
                .active_mut()
                .alt_peek_gate
                .allow_entry(std::time::Instant::now(), 0.0)
        };
        match crate::alt_peek::route(peeking, shift, up, entry_allowed) {
            crate::alt_peek::AltWheelAction::ForwardArrows { up } => {
                // Plain wheel, no peek: Up/Down arrow keys let the TUI
                // scroll its own content natively (less/vim/omp).
                let key = if up { KeyCode::Up } else { KeyCode::Down };
                let single = self
                    .sessions
                    .active_mut()
                    .input_handler
                    .encode_key(key, Modifiers::empty());
                if !single.is_empty() {
                    let mut batch = Vec::with_capacity(single.len() * lines);
                    for _ in 0..lines {
                        batch.extend_from_slice(&single);
                    }
                    let _ = self.sessions.active_mut().write_user_input(&batch);
                }
            }
            crate::alt_peek::AltWheelAction::EnterPeek => {
                // Shift+wheel-up: overlay the history BlockView over the
                // TUI. Only when the block tracker has bootstrapped
                // (history exists).
                let bootstrap = self
                    .sessions
                    .active()
                    .terminal
                    .as_ref()
                    .is_some_and(|t| t.block_tracker().bootstrap_ready());
                if bootstrap {
                    if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                        t.set_alt_screen_history_peek(true);
                    }
                    self.scroll_local_view(rows);
                }
            }
            crate::alt_peek::AltWheelAction::ScrollBlocks { .. } => {
                // Peek active, Shift+wheel: navigate the browsed history.
                // Scrolling back to the bottom clears the peek flag
                // (set_block_scroll → sync_primary_history_view) and arms
                // the re-entry lockout.
                self.scroll_local_view(rows);
            }
            crate::alt_peek::AltWheelAction::ExitPeek => {
                // Peek active, plain wheel: "普通手势=与应用交互" — snap
                // back to the live TUI and consume the gesture.
                // snap_to_bottom clears the flag and arms the gate lockout.
                self.sessions.active_mut().snap_to_bottom();
                self.request_redraw();
            }
            crate::alt_peek::AltWheelAction::Noop => {
                // Denied entry (lockout / sub-threshold flick) or
                // Shift+wheel-down without a peek: consume, do nothing.
            }
        }
    }
}
