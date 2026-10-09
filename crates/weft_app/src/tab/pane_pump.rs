//! Main-thread message processing (v1.13.6 T10 P2, PLAN_v1136 §1 D6).
//!
//! Since the parse moved to the per-pane worker (`app/parse_worker.rs`),
//! this module no longer consumes PTY BYTES — the frame budget, the
//! oversize-split tail, the round-robin rotation, and the flood signal all
//! died with the main-thread drain (T1's `pane_pump_budget_tests` went with
//! them; the deletion list is in the P2 commit message). What remains is
//! the control half of the pump, per pane:
//!
//! 1. drain the worker→main control channel (`AppMsg::AltFlipped` /
//!    `AppMsg::PtyExited` — the old `PtyOutput` is gone);
//! 2. take the pane's `had_output` flag (the worker's lock-free "I parsed
//!    bytes this frame" signal — the D6 had_output semantic);
//! 3. run the `PtyExited` arm (kitty reset + the natural-exit close
//!    machinery, which now also serves the async close contract);
//! 4. the short-lock post-processing pass (unchanged list: keypress-bypass
//!    refresh → settle → block drain → ui events → split heads →
//!    completion snap), 50 ms rate limit intact.
//!
//! FIX_background_pane_pump history (v2.1) still applies structurally: every
//! pane's channel is drained and every pane's post pass runs — a background
//! pane's parse output reaches ITS terminal (in the worker now) and its
//! control events are consumed HERE.

use std::collections::HashMap;

use super::Tab;
use crate::AppMsg;
use weft_core::pane_layout::PaneId;

impl Tab {
    /// Process worker control events into the panes and drain finished
    /// blocks. Returns `(alive, drained_blocks, need_redraw, ui_events)`
    /// where `ui_events` carries `(source_pane, event)` pairs.
    ///
    /// PtyExited per pane (spec §2.2, semantics preserved from the
    /// `AppMsg::PtyExit` arm): a non-last pane's shell exit walks the
    /// existing pane-close infrastructure (`SplitTree::close_pane` via
    /// `close_exited_pane`) and does NOT touch `alive`; only the LAST
    /// pane's exit keeps the old `alive = false` whole-tab semantics. This
    /// arm now also completes the async CLOSE contract (D2): a user-closed
    /// pane's `Pty` was dropped by `begin_close`/`close_active_pane`, and
    /// its `PtyExited` arrives here after the worker parsed the backlog —
    /// the force-settle + drain below is the `finish_pending_blocks`
    /// parity, so output produced before the close still lands in its
    /// block. An exit never interrupts the same pass's consumption of the
    /// other panes.
    pub fn process_messages(
        &mut self,
    ) -> (
        bool,
        Vec<weft_core::blocks::Block>,
        bool,
        Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) {
        let mut need_redraw = false;
        let mut alive = true;
        let mut exited_panes: Vec<PaneId> = Vec::new();
        let mut drained: Vec<weft_core::blocks::Block> = Vec::new();
        let mut ui_events: Vec<(PaneId, weft_core::vt::UiEvent)> = Vec::new();
        // D6 had_output: per-pane, taken once per frame and reused as the
        // post-pass's "output arrived" gate (the old `processed_panes`).
        let mut pane_had_output: HashMap<PaneId, bool> = HashMap::new();

        // Deterministic order (HashMap keys() is process-randomly shuffled).
        // No rotation any more: without a frame budget there is no deferral
        // to rotate.
        let mut pane_ids: Vec<PaneId> = self.panes.keys().copied().collect();
        pane_ids.sort_unstable();
        for pane_id in pane_ids {
            // 1. Drain THIS pane's control channel (try_recv only — D9 rule
            // 4 exemption; the worker's sends are crossbeam bounded, the
            // main thread never blocks). PtyExited is the LAST event the
            // worker ever enqueues, so stop draining at it.
            let mut exit_status: Option<std::result::Result<i32, String>> = None;
            while let Some(msg) = self
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.msg_rx.try_recv().ok())
            {
                match msg {
                    AppMsg::AltFlipped {
                        at,
                        flips,
                        peek_exited,
                    } => {
                        // Alt-flip bookkeeping, timestamps from the WORKER's
                        // diff point (P2 review: no one-frame drift of the
                        // storm/debounce windows).
                        if flips > 0 {
                            self.pending_alt_rescale = true;
                            self.record_alt_flip_instants(pane_id, flips, at);
                        }
                        if peek_exited {
                            if let Some(pane) = self.panes.get_mut(&pane_id) {
                                pane.alt_peek_gate.note_exit();
                            }
                        }
                    }
                    AppMsg::PtyExited(code) => {
                        // P1-1: a pane the close watchdog already finalized
                        // consumes a late real exit as a no-op (the worker's
                        // `let _ = send` is quiet by construction; the flag
                        // keeps double-settlement structurally impossible
                        // even while the pane is still in the map).
                        if self
                            .panes
                            .get(&pane_id)
                            .is_some_and(|pane| pane.close_settled)
                        {
                            tracing::debug!(
                                pane = ?pane_id,
                                "PtyExited after watchdog finalization; no-op"
                            );
                            break;
                        }
                        exit_status = Some(code);
                        break;
                    }
                }
            }

            // 2. had_output (D6): the worker parsed bytes for this pane
            // since the last frame. Also counts as "processed" for the
            // post-pass gates, and a natural exit counts too (its
            // force-settle must run).
            let had_output = self
                .panes
                .get(&pane_id)
                .map(|pane| pane.take_had_output())
                .unwrap_or(false);
            let pane_processed = had_output || exit_status.is_some();
            pane_had_output.insert(pane_id, pane_processed);
            need_redraw |= pane_processed;
            // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING) DEBUG
            // probe (stage 3/4): first post-resize output — measures the
            // ioctl-to-repaint gap. T10 P2: the parse moved to the worker,
            // so "first output" is observed at pump granularity (the first
            // frame a pane reports had_output); the one-shot arm/disarm
            // contract is unchanged (debug instrumentation only).
            if pane_processed {
                if let Some(since_ioctl) = self.take_resize_output_probe() {
                    tracing::debug!(
                        since_ioctl_ms = since_ioctl.as_millis(),
                        pane = ?pane_id,
                        "RESIZE_PROBE first_pty_output"
                    );
                }
            }

            // 3. PtyExited arm — semantics preserved (shared tail with the
            // close watchdog below).
            if let Some(code) = exit_status {
                if !self.finalize_dead_pane(
                    pane_id,
                    &format!("Shell exited: {code:?}"),
                    &mut exited_panes,
                    &mut drained,
                    &mut ui_events,
                ) {
                    alive = false;
                }
            } else {
                // P1-1 close watchdog: a SIGHUP-immune child (nohup/setsid)
                // holding the slave fd keeps the reader alive forever — no
                // EOF, no Exit, no PtyExited — which would leave a pane
                // dropped by `begin_close`/`close_active_pane` hanging
                // FOREVER (the retired bounded close-drain had no such
                // failure mode). Past the deadline without finalization,
                // run the exact exit-arm tail here. The check is
                // unconditional (it must fire in a zero-output silence) and
                // the `weft-close-watchdog` one-shot wake thread guarantees
                // a pump runs at/after the deadline.
                let watchdog_due = self.panes.get(&pane_id).is_some_and(|pane| {
                    !pane.close_settled
                        && pane
                            .close_deadline
                            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                });
                if watchdog_due {
                    if !self.finalize_dead_pane(
                        pane_id,
                        "close watchdog: PtyExited timed out after PTY teardown; finalizing pane",
                        &mut exited_panes,
                        &mut drained,
                        &mut ui_events,
                    ) {
                        alive = false;
                    }
                    need_redraw = true;
                }
            }
        }

        // Post-processing pass — per pane; the sequence matches the
        // pre-worker pass exactly: keypress-bypass refresh → settle → block
        // drain → ui events → split-head anchor compensation →
        // block-completion snap.
        let now = std::time::Instant::now();
        // Sorted to match the consume pass above — drained blocks / ui
        // events append in a stable pane order (determinism, not correctness).
        let mut survivor_ids: Vec<PaneId> = self.panes.keys().copied().collect();
        survivor_ids.sort_unstable();
        for pane_id in survivor_ids {
            let pane_processed = pane_had_output.get(&pane_id).copied().unwrap_or(false);
            let pane_exited = exited_panes.contains(&pane_id);
            let mut pane_drained: Vec<weft_core::blocks::Block> = Vec::new();
            let mut split_heads = 0usize;
            let mut reset_scroll = false;
            let mut terminal_gone = false;
            if let Some(pane) = self.panes.get_mut(&pane_id) {
                // v1.10.4: keypress bypass window — publish now (gated).
                need_redraw |= if pane_processed && pane.primary_history_refresh.take_force() {
                    pane.with_terminal(|t| t.refresh_primary_history_snapshot_now())
                        .unwrap_or(false)
                } else {
                    pane.refresh_primary_history_snapshot(pane_processed)
                };
                if let Some(mut terminal) = pane.lock_terminal() {
                    let settled = if !pane_exited {
                        terminal.settle_primary_screen_exit_if_idle(now)
                    } else {
                        terminal.settle_primary_screen_exit()
                    };
                    if settled {
                        need_redraw = true;
                    }
                    pane_drained = terminal.block_tracker_mut().drain_unpersisted();
                    // v1.11.5 (PLAN_v1115 §M2): drain app-facing ui events at
                    // the response drain point, source-tagged.
                    ui_events.extend(terminal.take_ui_events().into_iter().map(|e| (pane_id, e)));
                    // v1.10.26 Batch D (D-3): 1MiB history-split heads settled
                    // this frame; the anchor compensation below runs after the
                    // terminal borrow ends (disjoint-pane field).
                    split_heads = terminal.take_pending_screen_split_heads().unwrap_or(0);
                    // v1.10.21: don't yank THIS pane's active history peek.
                    reset_scroll =
                        crate::tab::scroll::block_completion_should_snap(&terminal, &pane_drained);
                } else {
                    terminal_gone = true;
                }
            }
            drained.extend(pane_drained);
            if terminal_gone {
                continue;
            }
            if split_heads > 0 {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.compensate_anchor_for_split(split_heads);
                }
            }
            if reset_scroll {
                if let Some(pane) = self.panes.get_mut(&pane_id) {
                    pane.snap_to_bottom();
                }
            }
        }
        // v1.11.10: DEC 2026 synchronized output suppresses presents. Every
        // pane's terminal feeds the same frame now, so any synchronized pane
        // suppresses.
        if self.any_synchronized_output() {
            need_redraw = false;
        }

        (alive, drained, need_redraw, ui_events)
    }

    /// Shared finalization tail for a dead pane — the PtyExited arm AND the
    /// P1-1 close watchdog both land here, so the watchdog path gets the
    /// exact exit semantics (kitty reset, force-settle, block drain, tree
    /// shrink / last-pane tab death). Marks the pane settled first (the
    /// P1-1 double-settle guard). Returns `false` when this was the LAST
    /// pane (the caller must propagate `alive = false`).
    ///
    /// Re-entrancy: locks (D9 rule 5 roster) — never call inside a guard.
    fn finalize_dead_pane(
        &mut self,
        pane_id: PaneId,
        reason: &str,
        exited_panes: &mut Vec<PaneId>,
        drained: &mut Vec<weft_core::blocks::Block>,
        ui_events: &mut Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) -> bool {
        tracing::info!(pane = ?pane_id, "{reason}");
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.close_settled = true;
        }
        // v1.11.4: kitty negotiated flags die with the shell.
        if let Some(mut t) = self
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.lock_terminal())
        {
            t.kitty_reset();
        }
        if self.panes.len() == 1 {
            // LAST pane: keep the current whole-tab semantics — the app
            // layer (`remove_dead` in session_pump) drops the tab; the
            // post-drain pass below force-settles this pane.
            exited_panes.push(pane_id);
            false
        } else {
            self.close_exited_pane(pane_id, drained, ui_events);
            true
        }
    }

    /// §2.2: a NON-last pane's shell exit. Mirrors the user pane-close path:
    /// force-settle the dying shell's primary-screen state and drain its
    /// finished blocks + ui events first (they must persist like any other
    /// pane's — `finish_pending_blocks` parity), then shrink the tree via
    /// [`weft_core::pane_layout::SplitTree::close_pane`] and drop the pane.
    /// NEVER touches `alive` — surviving panes keep the tab open.
    /// (`kitty_reset` already ran at exit detection.)
    fn close_exited_pane(
        &mut self,
        pane_id: PaneId,
        drained: &mut Vec<weft_core::blocks::Block>,
        ui_events: &mut Vec<(PaneId, weft_core::vt::UiEvent)>,
    ) {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            if let Some(mut terminal) = pane.lock_terminal() {
                // A shell that died mid-command still finalizes its in-flight
                // block before the pane drops.
                terminal.settle_primary_screen_exit();
                drained.extend(terminal.block_tracker_mut().drain_unpersisted());
                ui_events.extend(terminal.take_ui_events().into_iter().map(|e| (pane_id, e)));
            }
        }
        let new_active = match self.split_tree.close_pane(pane_id) {
            Ok(new_active) => new_active,
            Err(error) => {
                // Only reachable if the tree and the pane map desynced (caller
                // bug). Keep the tab usable: drop just the pane's own state.
                tracing::error!(?pane_id, %error, "PtyExited for a pane missing from the split tree; dropping the pane entry only");
                self.panes.remove(&pane_id);
                self.alt_flip_history.remove(&pane_id);
                return;
            }
        };
        let was_active = self.active_pane == pane_id;
        // Focus: an exiting ACTIVE pane hands focus to the absorbing sibling
        // (user-close parity); an exiting BACKGROUND pane must not move the
        // user's focus — re-sync the tree's focus to the kept active pane
        // (`close_pane` otherwise parks tree focus on the survivor).
        let desired = if was_active {
            new_active
        } else {
            Some(self.active_pane)
        };
        if let Some(id) = desired {
            self.active_pane = id;
            // `set_active` is a deliberate no-op while zoomed (the zoomed
            // pane stays focused) and only errors on stale ids — `id` is a
            // survivor leaf.
            let _ = self.split_tree.set_active(id);
        }
        self.panes.remove(&pane_id);
        // §2.5: the per-pane storm record dies with the pane.
        self.alt_flip_history.remove(&pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tab;
    use crate::pane::Pane;
    use crate::AppMsg;
    use weft_core::pane_layout::{PaneId, SplitDirection};

    /// Two-pane test tab built from `with_terminal_only` panes (no real
    /// PTY — output is injected through the worker-shaped test seam).
    /// Returns `(tab, background_id, active_id)`: `split_active_pane_test`
    /// focuses the NEW pane, so `first` ends up in the background.
    fn two_pane_tab() -> (Tab, PaneId, PaneId) {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let first = tab.active_pane_id();
        let second = tab
            .split_active_pane_test(SplitDirection::Vertical, 0.5, 1000)
            .expect("test split is infallible for a single-leaf tree");
        (tab, first, second)
    }

    fn inject_exit(pane_id: PaneId, code: Result<i32, String>) -> impl FnOnce(&mut Tab) {
        move |tab: &mut Tab| {
            tab.pane(pane_id)
                .unwrap_or_else(|| panic!("pane {pane_id:?} vanished"))
                .msg_tx
                .send(AppMsg::PtyExited(code))
                .expect("test channel is empty and bounded(1024)");
        }
    }

    fn row_text(tab: &Tab, pane_id: PaneId, row: usize) -> String {
        tab.pane(pane_id)
            .and_then(|pane| pane.with_terminal(|t| t.grid().row_text(row)))
            .unwrap_or_default()
    }

    /// §三 background-pane end to end: output parsed for a BACKGROUND pane
    /// (worker side — here via the worker-shaped test seam) must advance
    /// THAT pane's terminal and raise the pane's had_output flag, which the
    /// pump converts into a redraw request.
    #[test]
    fn background_pane_output_advances_its_terminal() {
        let (mut tab, bg, active) = two_pane_tab();
        tab.feed_pty_output_for_test(bg, b"hello");
        tab.feed_pty_output_for_test(active, b"WORLD");

        let (_, _, need_redraw, _) = tab.process_messages();

        assert_eq!(
            row_text(&tab, bg, 0),
            "hello",
            "the background pane's terminal must advance (the freeze under repair)"
        );
        assert_eq!(
            row_text(&tab, active, 0),
            "WORLD",
            "the active pane's consumption must keep working unchanged"
        );
        assert!(need_redraw, "parsed output flags a redraw via had_output");
        // The flags are take-semantics: the next quiet frame does not
        // request a redraw again.
        let (_, _, need_redraw_again, _) = tab.process_messages();
        assert!(!need_redraw_again, "had_output must clear after the pump");
    }

    /// §二.2 PtyExited semantics (1/3): a NON-LAST pane's shell exit shrinks
    /// the split tree via the user-close-pane infrastructure and must NOT
    /// set `alive = false` — surviving panes keep the tab open.
    #[test]
    fn background_pane_exit_shrinks_tree_and_keeps_tab_alive() {
        let (mut tab, bg, active) = two_pane_tab();
        inject_exit(bg, Ok(0))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(alive, "a background pane's exit must not close the tab");
        assert_eq!(tab.pane_count(), 1, "the exited pane's leaf is removed");
        assert!(tab.pane(bg).is_none(), "the exited pane's state is dropped");
        assert!(tab.pane(active).is_some(), "the survivor stays");
        assert_eq!(
            tab.active_pane_id(),
            active,
            "focus must stay on the pane the user was using"
        );
    }

    /// §二.2 PtyExited semantics (2/3): the LAST pane's exit keeps the
    /// current `alive = false` semantics — the app layer removes the whole
    /// tab (session_pump `remove_dead`). Pin of the existing behavior.
    #[test]
    fn last_pane_exit_still_closes_the_tab() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let only = tab.active_pane_id();
        inject_exit(only, Ok(0))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(!alive, "the last pane's exit closes the tab as before");
        assert_eq!(
            tab.pane_count(),
            1,
            "the pane itself is left for remove_dead"
        );
    }

    /// §二.2 PtyExited semantics (3/3): a PtyExited in one pane's channel
    /// must not interrupt the SAME frame's consumption of the other panes.
    #[test]
    fn pane_exit_does_not_interrupt_other_panes_consumption() {
        let (mut tab, bg, active) = two_pane_tab();
        tab.feed_pty_output_for_test(active, b"LIVE");
        inject_exit(bg, Ok(7))(&mut tab);

        let (alive, _, _, _) = tab.process_messages();

        assert!(alive);
        assert_eq!(
            row_text(&tab, active, 0),
            "LIVE",
            "the surviving pane is consumed in the same pass as the exit"
        );
        assert_eq!(tab.pane_count(), 1);
    }

    /// P1-1 close watchdog: a deadline that is already due (injected as
    /// "now" — the injection point standing in for a SIGHUP-immune child
    /// that would never Exit) finalizes the LAST pane on the next pump with
    /// the exit arm's exact tail (force-settle + block drain, `alive=false`),
    /// and the LATE real PtyExited is then consumed as a no-op — no double
    /// settle, no panic, the tab is simply left for remove_dead. The
    /// deadline check runs unconditionally (this test has zero worker
    /// output wakes; only the pump runs).
    #[test]
    fn close_watchdog_finalizes_a_hung_pane_and_late_exit_is_a_noop() {
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(100));
        let only = tab.active_pane_id();
        {
            let pane = tab.pane_mut(only).unwrap();
            // In-flight block so the finalization's drain is observable.
            pane.lock_terminal()
                .unwrap()
                .process(b"\x1b]133;A\x07\x1b]133;B\x07hung-app\x1b]133;C\x07");
            // The tail a real dying shell emits (its exit hook runs the final
            // 133;D before the PTY goes quiet — the marker the settle path
            // needs). The watchdog exists for the pathological no-marker
            // hang; its tail is IDENTICAL to the exit arm either way.
            pane.lock_terminal()
                .unwrap()
                .process(b"silent tail bytes\x1b]133;D;0\x07");
            // Close initiation: teardown done, watchdog deadline due NOW
            // (the injected-hung-child scenario).
            pane.begin_close_teardown();
            pane.close_deadline = Some(std::time::Instant::now());
        }

        // Frame 1: the watchdog fires (no PtyExited ever arrives).
        let (alive, drained, need_redraw, _) = tab.process_messages();
        assert!(
            !alive,
            "the last pane's watchdog finalization kills the tab"
        );
        assert!(need_redraw);
        assert_eq!(
            drained.len(),
            1,
            "the force-settle must finalize the in-flight block"
        );
        assert!(
            drained[0].output.contains("silent tail bytes"),
            "watchdog finalization preserves the parsed tail: {:?}",
            drained[0].output
        );
        assert!(
            tab.pane(only).is_some_and(|pane| pane.close_settled),
            "the pane is marked settled (double-settle guard armed)"
        );

        // Frame 2: the late REAL PtyExited arrives — consumed as a no-op.
        tab.msg_tx.send(AppMsg::PtyExited(Ok(0))).unwrap();
        let (alive, drained, _, _) = tab.process_messages();
        assert!(
            alive,
            "the late exit must not re-run the finalization (alive stays for remove_dead)"
        );
        assert!(
            drained.is_empty(),
            "no double settlement: nothing left to drain"
        );
    }

    /// §二.4 (2): a BACKGROUND pane's alt-screen flip arms the tab-level
    /// `pending_alt_rescale` — the single-value merge is intentional
    /// (single main thread, resize commit is per-pane). The flip arrives as
    /// the worker's `AltFlipped` control event, stamp included.
    #[test]
    fn background_pane_alt_flip_arms_pending_alt_rescale() {
        let (mut tab, bg, _active) = two_pane_tab();
        assert!(!tab.pending_alt_rescale);
        // The worker-side parse diff → control event, end to end: run the
        // worker-shaped parse (which enqueues AltFlipped) for an alt-enter.
        tab.feed_pty_output_for_test(bg, b"\x1b[?1049h");

        assert!(
            tab.pane(bg)
                .map(|pane| !pane.msg_rx.is_empty())
                .unwrap_or(false),
            "the worker-shaped parse must enqueue the AltFlipped control event"
        );
        let _ = tab.process_messages();

        assert!(
            tab.pending_alt_rescale,
            "any pane's flip must arm the pending rescale, not just the active pane's"
        );
        assert!(
            tab.alt_flip_history.contains_key(&bg),
            "the flip history is recorded in the SOURCE pane's slot"
        );
    }
}
