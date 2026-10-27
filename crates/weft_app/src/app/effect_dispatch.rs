//! Effect dispatch + clipboard/paste helpers extracted from `main.rs`.
//!
//! `drain_effects` is the single fan-out point for `Effect` values produced
//! across the app (PTY writes, interrupts, resizes, clipboard, persistence,
//! tab lifecycle). `copy_selection` and `apply_paste*` live here because they
//! are the two main producers/consumers of clipboard effects.
//!
//! v1.11.1 (PLAN_v1111 §2/§3): `apply_paste` is THE single large-paste guard
//! point — Editor and Passthrough branches share this entry, so one gate
//! covers both. The confirmation dialog never runs inside a winit handler
//! (FIX_RECOVERY_MODAL_SPIN discipline): the text parks in
//! [`crate::App::pending_paste_confirm`], the alert is dispatched to the main
//! queue via `exec_async`, and the decision comes back as
//! `AppEvent::PasteDecided`.

use crate::effect::Effect;
use crate::{clipboard_copy, warn};
use std::sync::atomic::Ordering;

pub(crate) use paste::{paste_toast_expired, PendingPaste};

#[cfg(test)]
use paste::{
    paste_toast_eligible, resolve_pending_paste, take_pending_paste_if_seq_matches, PasteOutcome,
    PASTE_TOAST_TTL,
};

/// v1.15.0 (T3): the footer feedback's TTL — same 3s feel as the paste toast
/// (own constant so the two policies can diverge without touching call
/// sites).
pub(crate) const FOOTER_FEEDBACK_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// v1.15.0 (T3): pure expiry judgment for the footer feedback
/// (`paste_toast_expired` precedent — one shared boundary for tests and the
/// 1 Hz tick).
pub(crate) fn footer_feedback_expired(
    shown_at: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    now.duration_since(shown_at) >= FOOTER_FEEDBACK_TTL
}

impl crate::App {
    /// Fan out `effects` to their side effects. Each effect is dispatched
    /// exactly once; a failed dispatch logs but does not abort the remaining
    /// effects in the batch.
    pub(crate) fn drain_effects(&mut self, effects: impl IntoIterator<Item = Effect>) {
        for effect in effects {
            match effect {
                Effect::WritePty { session_id, bytes } => {
                    // v1.11.11 (M-B): effects carry the stable session id; the
                    // drain reverse-looks-up the current tab index. A closed
                    // tab means the effect is stale — dropping it fixes the
                    // previous silent mis-delivery (index shifts re-targeted
                    // old effects into unrelated sessions).
                    if let Some(tab) = self.sessions.tab_index_by_session_id(session_id) {
                        if let Some(session) = self.sessions.tab_mut(tab) {
                            let input_seq = session.input_seq();
                            let (screen_owner, settle_state, history_snapshot_due) = session
                                .with_terminal(|t| {
                                    (t.screen_owner(), t.settle_state(), t.history_snapshot_due())
                                })
                                .unwrap_or((
                                    weft_core::vt::ScreenOwner::Shell,
                                    weft_core::vt::SettleState::Idle,
                                    false,
                                ));
                            tracing::debug!(
                                session_id,
                                input_seq,
                                tab,
                                bytes_len = bytes.len(),
                                %screen_owner,
                                %settle_state,
                                history_snapshot_due,
                                delivery = "pty-write",
                                "effect dispatched",
                            );
                            if let Err(error) = session.write_user_input(&bytes) {
                                warn!(%error, tab, "failed to apply PTY write effect");
                            }
                        }
                    } else {
                        warn!(
                            session_id,
                            bytes_len = bytes.len(),
                            "dropping stale WritePty effect: no tab owns the session"
                        );
                    }
                }
                Effect::InterruptPty { session_id } => {
                    if let Some(tab) = self.sessions.tab_index_by_session_id(session_id) {
                        if let Some(session) = self.sessions.tab_mut(tab) {
                            let input_seq = session.input_seq();
                            let (screen_owner, settle_state, history_snapshot_due) = session
                                .with_terminal(|t| {
                                    (t.screen_owner(), t.settle_state(), t.history_snapshot_due())
                                })
                                .unwrap_or((
                                    weft_core::vt::ScreenOwner::Shell,
                                    weft_core::vt::SettleState::Idle,
                                    false,
                                ));
                            tracing::debug!(
                                session_id,
                                input_seq,
                                tab,
                                %screen_owner,
                                %settle_state,
                                history_snapshot_due,
                                delivery = "pty-etx",
                                "interrupt effect dispatched",
                            );
                            let delivered = session.interrupt_pty();
                            if !delivered {
                                warn!(
                                    tab,
                                    "interrupt delivery failed; preserving PTY output and phase"
                                );
                            }
                        }
                    } else {
                        warn!(
                            session_id,
                            "dropping stale InterruptPty effect: no tab owns the session"
                        );
                    }
                }
                Effect::ResizePty {
                    session_id,
                    pane_id,
                    rows,
                    cols,
                } => {
                    if let Some(tab) = self.sessions.tab_index_by_session_id(session_id) {
                        self.apply_pty_resize_effect(tab, pane_id, rows, cols);
                    } else {
                        warn!(
                            session_id,
                            "dropping stale ResizePty effect: no tab owns the session"
                        );
                    }
                }
                Effect::CopyClipboard { text } => clipboard_copy(&text),
                Effect::PersistTabs => self.save_all_tabs(),
                Effect::PersistBlocks { blocks } => self.persist_blocks(&blocks),
                Effect::LoadOlderBlocks => self.apply_load_older_blocks(),
                Effect::Paste { session_id } => {
                    if let Some(tab) = self.sessions.tab_index_by_session_id(session_id) {
                        self.apply_paste(tab);
                    } else {
                        warn!(
                            session_id,
                            "dropping stale Paste effect: no tab owns the session"
                        );
                    }
                }
                Effect::Exit => self.should_exit = true,
                Effect::TabClosed {
                    removed_idx,
                    new_active,
                    is_last,
                } => {
                    // close_tab already applied the synchronous mutations;
                    // retain this effect as the post-close extension point.
                    tracing::info!(removed_idx, new_active, is_last, "tab closed effect");
                }
                Effect::TabSwitched { new_idx, prev_idx } => {
                    // Synchronous mutation (sessions.next/prev, IME reset,
                    // find refresh, tab-bar scroll) already ran in
                    // `next_tab`/`prev_tab`. Extension point for future
                    // post-switch consumers.
                    tracing::info!(new_idx, prev_idx, "tab switched effect");
                }
                Effect::RequestRedraw => self.request_redraw(),
            }
        }
    }

    /// v1.15.0 (T3): drop the ACTIVE tab's footer feedback once its TTL
    /// elapsed (paste-toast expiry precedent). Returns whether an entry was
    /// removed so the caller requests a redraw.
    fn expire_active_tab_footer_feedback(&mut self) -> bool {
        let Some(tab) = self.sessions.active_mut() else {
            return false;
        };
        let Some((_, shown_at)) = &tab.footer_feedback else {
            return false;
        };
        let due = footer_feedback_expired(*shown_at, std::time::Instant::now());
        if due {
            tab.footer_feedback = None;
        }
        due
    }

    /// The 1 Hz `AppEvent::TabsAutoSave` body, extracted from app_runtime.rs
    /// (architecture-gate ceiling) so the whole tick — save, recovery
    /// snapshot, timing metric, toast expiry — lives in one effect-domain fn.
    pub(crate) fn run_tabs_autosave_tick(&mut self) {
        // v1.10.23: suppressed while the recovery prompt is pending — an
        // autosave would DELETE-and-replace the tabs table and overwrite the
        // on-disk crash snapshot before the user chooses.
        let tick_started = std::time::Instant::now();
        if crate::recovery_controller::autosave_suppressed(&self.pending_recovery) {
            tracing::debug!("skipping autosave while recovery prompt is pending");
        } else {
            self.save_changed_tabs();
            // v1.6.3: debounced recovery snapshot write alongside the tabs
            // autosave (skipped internally when nothing changed).
            if let Some(ws) = self.capture_workspace("recovery".into()) {
                if let Err(e) = self.recovery.write_snapshot_if_changed(&ws) {
                    warn!(error = %e, "recovery snapshot write failed");
                }
            }
        }
        // v1.11.2 X6 (PLAN_v1112 §6): observe every tick for the probe's p95
        // line; warn when a tick visibly eats the frame budget (>5 ms),
        // including scale so slow ticks are attributable.
        let tick_elapsed = tick_started.elapsed();
        self.performance_probe.record_autosave_tick(tick_elapsed);
        if tick_elapsed > std::time::Duration::from_millis(5) {
            let tabs = self.sessions.tabs().len();
            let total_blocks: usize = self
                .sessions
                .tabs()
                .iter()
                .map(|tab| {
                    tab.panes()
                        .filter_map(|(_, pane)| {
                            pane.with_terminal(|t| t.block_tracker().blocks().len())
                        })
                        .sum::<usize>()
                })
                .sum();
            tracing::warn!(
                elapsed_ms = tick_elapsed.as_secs_f64() * 1000.0,
                tabs,
                total_blocks,
                "autosave tick exceeded 5ms"
            );
        }
        // v1.11.1: paste-toast expiry rides this tick.
        self.expire_paste_toast_tick();
        // T14 (PLAN_v11217 §3.9): the block-prune 24h gate rides this 1 Hz tick too (O(1) checks; the prune runs on its own background thread).
        self.maybe_run_block_prune_tick();
        // T16d (PLAN_v11217 §3.11): the memory-pressure check rides this tick too — one O(1) FFI read + compares; a 60s debounce anchors any response.
        self.maybe_run_memory_pressure_tick();
    }

    /// T14 (PLAN_v11217 §3.9 3): block-library auto-cleanup scheduling.
    /// First pass 10s after startup (the arm anchor — keeps the prune and
    /// its WAL/pragma work out of the cold-start measurement window; this
    /// 1 Hz tick then drives the check), later passes at a 24h cadence via
    /// `last_block_prune`. Both gates 0=Off → the prune never runs
    /// (bit-identical to pre-T14). Re-entrancy via an AtomicBool swap on
    /// the main thread; the spawned thread owns a DEDICATED BlockStore
    /// connection (rusqlite Connection is Send, not Sync) and clears the
    /// flag when done.
    fn maybe_run_block_prune_tick(&mut self) {
        const PRUNE_STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
        const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
        if self.block_prune_arm.elapsed() < PRUNE_STARTUP_DELAY {
            return;
        }
        if self
            .last_block_prune
            .is_some_and(|last| last.elapsed() < PRUNE_INTERVAL)
        {
            return;
        }
        let gates = &self.config_state.config.blocks;
        if gates.history_max_age_days == 0 && gates.history_max_db_mb == 0 {
            return; // both gates Off — never spawn
        }
        if self.block_prune_in_flight.swap(true, Ordering::SeqCst) {
            return; // a pass is already running
        }
        self.last_block_prune = Some(std::time::Instant::now());
        let Some(path) = crate::app::helpers::weft_cache_dir().map(|c| c.join("blocks.db")) else {
            self.block_prune_in_flight.store(false, Ordering::SeqCst);
            return;
        };
        let age_days = gates.history_max_age_days;
        let max_db_mb = gates.history_max_db_mb;
        let in_flight = self.block_prune_in_flight.clone();
        let spawned = std::thread::Builder::new()
            .name(String::from("weft-prune"))
            .spawn(move || {
                // Reviewer MEDIUM-3: a panic inside the prune pass must not
                // wedge the re-entry guard for the rest of the session — the
                // guard releases on scope exit however the closure ends.
                let _guard = crate::app::helpers::PruneGuard(&in_flight);
                let started = std::time::Instant::now();
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                match weft_core::persistence::run_block_prune(&path, age_days, max_db_mb, now_ms) {
                    Ok(Some(report)) => {
                        tracing::info!(
                            age_deleted = report.age_deleted,
                            size_deleted = report.size_deleted,
                            bytes_before = report.bytes_before,
                            bytes_after = report.bytes_after,
                            terminal = ?report.terminal,
                            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                            "block prune finished"
                        );
                    }
                    Ok(None) => {} // both gates Off (config raced to Off)
                    Err(e) => {
                        // Batch-busy aborts land here as partial passes —
                        // the 24h window retries naturally, no retry loop.
                        tracing::warn!(error = %e, "block prune failed; will retry next window");
                    }
                }
            });
        if spawned.is_err() {
            // Thread spawn failed: release the guard; the next window
            // retries (no busy loop — last_block_prune stays armed).
            tracing::warn!("failed to spawn prune thread; will retry next window");
            self.block_prune_in_flight.store(false, Ordering::SeqCst);
        }
    }

    /// Copy selection to system clipboard.
    ///
    /// Dispatches on the active view: block view copies from the
    /// content-anchored `BlockViewSelection` read through the CURRENT
    /// document source (what the user sees — no stale snapshot), grid view
    /// copies from the terminal Grid. This split fixes the "复制错位" bug
    /// where a grid-coordinate copy landed on the wrong line because the
    /// block view's pitch/scroll/layout don't map 1:1 to grid rows.
    pub(crate) fn copy_selection(&mut self) {
        let text = {
            // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — nothing
            // selected, nothing to copy.
            let Some(tab) = self.sessions.active() else {
                return;
            };
            let Some(terminal) = tab.lock_terminal() else {
                return;
            };
            let terminal = &*terminal;
            // Editor drag-selection takes priority over block/grid selection.
            terminal
                .editor()
                .buffer
                .selected_text()
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    if terminal.show_block_view() {
                        let source = crate::selection::SelectionDocSource::new(
                            terminal.block_tracker().session_blocks(),
                            terminal.block_tracker().in_flight().map(|live| live.output),
                        );
                        tab.selection_handler.block_view_text(&source)
                    } else {
                        tab.selection_handler.selected_text(terminal.grid())
                    }
                })
        };
        self.drain_effects(crate::effect::copy_clipboard_effects(text));
    }
}

mod paste;

// Block-persist effect bodies live in the child module (v1.13.9 file-budget
// split, pure code motion). `drain_effects` and its for-loop dispatch
// sequence stay here verbatim — the plan P2 invariant: the effects iteration
// order and the shared-state mutation sequence inside the loop are unchanged.
mod block_persist;

// paste_guard_tests moved to the gate-exempt sibling module (repo test-module
// convention); renamed to `tests` so the file enjoys the `*/tests.rs` budget
// exemption (plan P3 — a non-`tests` module name would still count against
// the parent's budget).
#[cfg(test)]
#[path = "effect_dispatch/tests.rs"]
mod tests;
