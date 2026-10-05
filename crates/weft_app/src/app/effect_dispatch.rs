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
use crate::{clipboard_copy, clipboard_paste, warn};
use std::sync::atomic::Ordering;
use tracing::{debug, info};
use weft_core::input::{
    classify_paste, contains_dangerous_control_chars, encode_paste, format_byte_count,
    paste_preview, PasteRisk,
};

/// v1.11.1: characters of clipboard text shown in the confirmation dialog
/// (PLAN_v1111 §4.3: "前 80 字符").
const PASTE_PROMPT_PREVIEW_CHARS: usize = 80;

/// v1.11.1: how long the post-paste toast stays visible (PLAN_v1111 §4.5).
/// Checked on the existing 1 Hz autosave tick, so actual visibility can run
/// up to ~1s longer — accepted there to avoid a dedicated timer.
pub(crate) const PASTE_TOAST_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// v1.11.1: a paste parked while its confirmation dialog is on screen.
/// Consumed exactly once via the seq-matched take in
/// [`App::apply_paste_decision`]; `session_id` uses the same identity as
/// `Effect::Paste { session_id }` and `seq` pairs each park with its own
/// dialog reply (v1.11.11 M-B — a stale reply can never consume a newer
/// park, and a closed tab drops the parked paste instead of re-targeting).
pub(crate) struct PendingPaste {
    pub(crate) session_id: u64,
    pub(crate) seq: u64,
    pub(crate) text: String,
    pub(crate) risk: PasteRisk,
}

/// v1.11.11 (M-B): paste-request sequence allocator. Each park consumes one
/// seq; the deferred dialog carries it back in `AppEvent::PasteDecided` so
/// the decision pairs with the exact park that spawned it.
static NEXT_PASTE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// v1.11.1: what to do with a parked paste after the user decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasteOutcome {
    /// Cancel — drop the parked text; nothing is written anywhere.
    Discard,
    /// Apply the parked text. `grant_session` additionally flips
    /// `App::paste_allow_for_session`, exempting future pastes from BOTH
    /// risk classes until the app quits (PLAN_v1111 §7).
    Apply { grant_session: bool },
}

/// v1.11.1: free pure function mapping the dialog response to an outcome so
/// the three branches are table-testable headless (PLAN_v1111 §4.4/§5.2).
pub(crate) fn resolve_pending_paste(
    response: crate::macos_alert::PastePromptResponse,
) -> PasteOutcome {
    use crate::macos_alert::PastePromptResponse as R;
    match response {
        R::Cancel => PasteOutcome::Discard,
        R::Once => PasteOutcome::Apply {
            grant_session: false,
        },
        R::AlwaysSession => PasteOutcome::Apply {
            grant_session: true,
        },
    }
}

/// v1.11.1: exactly-once consumption of the parked paste, paired by seq
/// (v1.11.11 M-B, mirroring the OSC52 `take_pending_if_seq_matches` pattern
/// in app/ui_events.rs — a stale decision is ignored AND the slot stays
/// occupied; do NOT copy the pre-M-B unconditional take). A duplicated
/// decision event finds no matching seq and becomes a logged no-op.
fn take_pending_paste_if_seq_matches(
    pending: &mut Option<PendingPaste>,
    seq: u64,
) -> Option<PendingPaste> {
    match pending {
        Some(parked) if parked.seq == seq => pending.take(),
        Some(_) => None, // stale — leave the newer park in place
        None => None,
    }
}

/// v1.11.1: toast eligibility — only pastes that actually executed AND meet
/// the size bar show feedback ("小粘贴零打扰"). Passthrough counts bytes;
/// Editor mode counts chars per PLAN_v1111 §4.5. `>=` here (the confirm gate
/// itself is strictly-greater per §1), so an exactly-16 KiB paste shows the
/// toast without ever prompting.
fn paste_toast_eligible(
    byte_len: usize,
    char_count: usize,
    threshold_kib: u32,
    editor_mode: bool,
) -> bool {
    let size = if editor_mode { char_count } else { byte_len };
    size >= threshold_kib as usize * 1024
}

/// v1.11.1: pure expiry judgment for the paste toast (3s boundary,
/// PLAN_v1111 §5.2). Single source of truth — the renderer's
/// `clear_expired_paste_toast` delegates here so tests and production share
/// the same boundary.
pub(crate) fn paste_toast_expired(shown_at: std::time::Instant, now: std::time::Instant) -> bool {
    now.duration_since(shown_at) >= PASTE_TOAST_TTL
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
                                .terminal
                                .as_ref()
                                .map(|t| {
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
                                .terminal
                                .as_ref()
                                .map(|t| {
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

    /// Persist a batch of drained command blocks to the BlockStore. Best-effort:
    /// each failure is logged but does not abort the remaining inserts. Extracted
    /// from `process_messages` so the same logic serves the `PersistBlocks` effect.
    pub(crate) fn persist_blocks(&self, blocks: &[weft_core::blocks::Block]) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        for block in blocks {
            if let Err(e) = store.insert(block) {
                warn!(error = %e, "failed to persist block");
                continue;
            }
            if let Some(index) = &self.search_index {
                let started_ms = block
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                let document = weft_core::search::SearchDocument::from_block(
                    block.id.0,
                    &block.command,
                    block.output.as_ref(),
                    block.cwd.as_deref(),
                    started_ms,
                );
                if let Err(e) = index.upsert(&document) {
                    warn!(error = %e, block_id = block.id.0, "failed to index persisted block");
                }
            }
        }
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.3): the panel footer's「加载更早」action.
    /// Pages one batch of pre-retention history out of SQLite via keyset
    /// pagination, prepends it to the active tab's tracker (time order
    /// preserved), and reports the outcome through the toast channel.
    /// Blocks already in memory are unaffected; an exhausted DB answers with
    /// 「没有更早的历史」.
    pub(crate) fn apply_load_older_blocks(&mut self) {
        const LOAD_OLDER_PAGE: usize = 200;

        let oldest_started_ms = self
            .sessions
            .active()
            .and_then(|tab| tab.terminal.as_ref())
            .and_then(|t| t.block_tracker().blocks().first())
            .map(|b| b.started_at)
            .map(|started| {
                started
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            });
        let Some(oldest_started_ms) = oldest_started_ms else {
            // No blocks at all: the footer button is not even rendered, so
            // reaching here means a stale click — answer with the empty hint.
            self.show_block_history_toast("没有更早的历史");
            return;
        };
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        match store.older_than(oldest_started_ms, LOAD_OLDER_PAGE) {
            Ok(blocks) if blocks.is_empty() => {
                self.show_block_history_toast("没有更早的历史");
            }
            Ok(blocks) => {
                let loaded = blocks.len();
                if let Some(t) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.terminal.as_mut())
                {
                    t.block_tracker_mut().load_older_to_front(blocks);
                }
                info!(loaded, "loaded older block history from store");
                self.show_block_history_toast(&format!("已加载 {loaded} 条"));
                self.request_redraw();
            }
            Err(e) => {
                warn!(error = %e, "failed to load older block history");
            }
        }
    }

    /// v1.11.2 X4: reuse the paste-toast surface for generic feedback text.
    /// Same 3s TTL and 1 Hz expiry tick apply.
    /// v1.12.25 (audit 3-B, P1-05): `pub(crate)` so the palette workflow
    /// arms can surface DB read failures on the same toast channel.
    pub(crate) fn show_block_history_toast(&mut self, message: &str) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_paste_toast(Some((message.to_string(), std::time::Instant::now())));
        }
        self.request_redraw();
    }

    /// Read the system clipboard (synchronous — NSPasteboard has AppKit main
    /// thread affinity) and apply the text to `tab`. Editor mode inserts into
    /// the prompt buffer; Passthrough forwards to the PTY with optional
    /// bracketed-paste wrapping. `Effect::Paste` is the system-clipboard
    /// entry point; find-bar Cmd+V can pass already-read text directly.
    ///
    /// v1.11.1 (PLAN_v1111 §2/§3): single large-paste guard point. Small,
    /// benign pastes keep the old synchronous path; large/dangerous ones are
    /// parked and confirmed via a deferred NSAlert before anything is written.
    pub(crate) fn apply_paste(&mut self, tab: usize) {
        let Some(text) = clipboard_paste() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        // PLAN_v1111 §4.2: clamp a hand-edited threshold to the legal tiers
        // before classifying.
        let paste_cfg = {
            let cfg =
                crate::settings_validation::runtime_paste_config(&self.config_state.config.paste);
            weft_core::input::PasteGuardCfg {
                confirm_large: cfg.confirm_large,
                confirm_control_chars: cfg.confirm_control_chars,
                size_threshold_kib: cfg.size_threshold_kib,
            }
        };
        let Some(risk) = classify_paste(
            text.len(),
            contains_dangerous_control_chars(&text),
            &paste_cfg,
        ) else {
            // Benign paste — pre-v1.11.1 behavior, unchanged.
            self.apply_paste_text(tab, &text);
            return;
        };
        if self.paste_allow_for_session {
            // Session-wide allowance exempts BOTH risk classes
            // (PLAN_v1111 §7 会话标志语义); resets on app restart.
            self.apply_paste_text_with_toast(tab, &text);
            return;
        }
        self.park_and_prompt_paste(tab, text, risk);
    }

    /// v1.11.1: park the flagged paste and defer its confirmation dialog to
    /// the main queue. The decision returns asynchronously as
    /// [`crate::AppEvent::PasteDecided`].
    ///
    /// FIX_RECOVERY_MODAL_SPIN discipline (highest-risk item of this task):
    /// `runModal` must NEVER execute inside a winit handler — winit's
    /// EventLoopWaker timer stays armed while `event_handler.in_use()` and
    /// the modal's nested run loop spins at ~84% CPU. The only legal pattern
    /// is `dispatch2` + `Queue::main().exec_async` + proxy round-trip
    /// (recovery_controller.rs template).
    fn park_and_prompt_paste(&mut self, tab: usize, text: String, risk: PasteRisk) {
        // v1.11.11 (M-B): the parked paste carries the tab's stable session
        // id — a tab closed while the dialog is up drops the decision instead
        // of re-targeting a shifted index. The drain already verified the tab
        // exists (index came from the session-id reverse lookup), so this
        // read is defensive-only.
        let Some(session_id) = self.sessions.tab(tab).map(|t| t.session_id) else {
            warn!(tab, "paste tab vanished before parking; dropping the paste");
            return;
        };
        // Park BEFORE deferring; consumed exactly once by
        // `apply_paste_decision`. A second Cmd+V while a dialog is up parks
        // over it — decisions pair with whatever is parked at that moment,
        // same contract as the recovery prompt state machine (each park gets
        // a fresh seq, so the older dialog's reply is dropped as stale).
        if self.pending_paste_confirm.is_some() {
            warn!("large paste confirmed while another confirmation was in flight; superseding the parked paste");
        }
        let headline = format!(
            "即将粘贴 {}（风险：{}）到终端",
            format_byte_count(text.len()),
            risk.label()
        );
        let preview = paste_preview(&text, PASTE_PROMPT_PREVIEW_CHARS);
        let bytes = text.len();
        let seq = NEXT_PASTE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pending_paste_confirm = Some(PendingPaste {
            session_id,
            seq,
            text,
            risk,
        });
        info!(
            session_id,
            seq,
            risk = %risk.label(),
            bytes,
            "large paste parked; prompting off the winit handler"
        );
        let proxy = self.proxy.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let response = match objc2_foundation::MainThreadMarker::new() {
                Some(mtm) => {
                    match crate::macos_alert::show_large_paste_prompt(mtm, headline, preview) {
                        Ok(response) => response,
                        Err(error) => {
                            // Fail-closed (PLAN_v1111 §4.4): an alert that
                            // cannot be shown must never turn into a write.
                            warn!(%error, "paste prompt failed; discarding the paste");
                            crate::macos_alert::PastePromptResponse::Cancel
                        }
                    }
                }
                None => {
                    // AlertError::NotMainThread equivalent → fail-closed:
                    // dropping the paste is always recoverable by manually
                    // re-pasting (same stance as close_confirmation.rs).
                    warn!("paste prompt skipped: not on main thread; discarding the paste");
                    crate::macos_alert::PastePromptResponse::Cancel
                }
            };
            // v1.11.12 (PLAN_v11112 M-C): decision-loop send — a failure here
            // parks the paste forever (the dialog decided, nobody hears it).
            // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
            if let Err(e) = proxy.send_event(crate::AppEvent::PasteDecided { response, seq }) {
                tracing::warn!(error = %e, "send_event failed: paste decision lost; parked paste never resolves");
            }
        });
    }

    /// v1.11.1: handle the user's paste decision delivered from the deferred
    /// dialog block. Consumes the seq-matched parked paste exactly once; a
    /// stray, duplicated or stale (seq-mismatched) event without a matching
    /// park is a logged no-op.
    pub(crate) fn apply_paste_decision(
        &mut self,
        response: crate::macos_alert::PastePromptResponse,
        seq: u64,
    ) {
        let Some(pending) = take_pending_paste_if_seq_matches(&mut self.pending_paste_confirm, seq)
        else {
            debug!(
                ?response,
                seq, "paste decision did not match the parked paste's seq; dropped"
            );
            return;
        };
        // v1.11.11 (M-B): the target tab may have closed while the dialog was
        // up — drop the decision instead of writing into a shifted index.
        let Some(tab) = self.sessions.tab_index_by_session_id(pending.session_id) else {
            warn!(
                session_id = pending.session_id,
                seq,
                ?response,
                "paste target tab closed while the confirmation was pending; discarding"
            );
            return;
        };
        match resolve_pending_paste(response) {
            PasteOutcome::Discard => {
                info!(
                    session_id = pending.session_id,
                    seq,
                    bytes = pending.text.len(),
                    ?pending.risk,
                    "large paste cancelled; discarded without writing"
                );
            }
            PasteOutcome::Apply { grant_session } => {
                if grant_session {
                    self.paste_allow_for_session = true;
                }
                info!(
                    session_id = pending.session_id,
                    seq,
                    bytes = pending.text.len(),
                    ?pending.risk,
                    grant_session,
                    "large paste approved"
                );
                self.apply_paste_text_with_toast(tab, &pending.text);
            }
        }
    }

    /// v1.11.1 (PLAN_v1111 §4.5): expire the paste toast on the 1 Hz
    /// autosave tick — deliberately rides the existing timer (granularity
    /// may extend visibility ~1s past the 3s TTL) instead of a new one.
    pub(crate) fn expire_paste_toast_tick(&mut self) {
        let expired = self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.clear_expired_paste_toast(std::time::Instant::now()));
        if expired {
            self.request_redraw();
        }
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
                        .filter_map(|(_, pane)| pane.terminal.as_ref())
                        .map(|t| t.block_tracker().blocks().len())
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
        // T14 (PLAN_v11217 §3.9): the block-prune 24h gate rides this 1 Hz
        // tick too — O(1) checks (two Instant compares + one AtomicBool swap)
        // and the actual prune runs on its own background thread/connection.
        self.maybe_run_block_prune_tick();
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

    /// v1.11.1: apply an already-approved paste and surface the post-paste
    /// toast when it meets the size bar (PLAN_v1111 §4.5). Used for both the
    /// confirmed path and the session-allowance fast path.
    fn apply_paste_text_with_toast(&mut self, tab: usize, text: &str) {
        // Snapshot eligibility inputs before the mutable apply.
        let threshold_kib =
            crate::settings_validation::runtime_paste_config(&self.config_state.config.paste)
                .size_threshold_kib;
        let editor_mode = self
            .sessions
            .tab(tab)
            .and_then(|t| t.terminal.as_ref())
            .is_some_and(|t| t.effective_input_mode() == weft_core::input::InputMode::Editor);
        self.apply_paste_text(tab, text);
        if paste_toast_eligible(text.len(), text.chars().count(), threshold_kib, editor_mode) {
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_paste_toast(Some((
                    format!("已粘贴 {}", format_byte_count(text.len())),
                    std::time::Instant::now(),
                )));
            }
            self.request_redraw();
        }
    }

    /// Apply already-read `text` to `tab` according to its input mode. Split
    /// out so callers that already hold the clipboard text (e.g. the find bar
    /// Cmd+V path) can skip the NSPasteboard round-trip.
    pub(crate) fn apply_paste_text(&mut self, tab: usize, text: &str) {
        let mode = self
            .sessions
            .tab(tab)
            .and_then(|t| t.terminal.as_ref())
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Preserve pasted newlines explicitly because insert_char rejects
            // controls; drop CR to normalize external CRLF text.
            if let Some(t) = self
                .sessions
                .tab_mut(tab)
                .and_then(|tab| tab.terminal.as_mut())
            {
                let buf = &mut t.editor_mut().buffer;
                for c in text.chars() {
                    if c == '\n' {
                        buf.split_newline();
                    } else if c != '\r' {
                        buf.insert_char(c);
                    }
                }
            }
            self.request_redraw();
        } else {
            // Passthrough: forward to the PTY.
            let bracketed = self
                .sessions
                .tab(tab)
                .and_then(|t| t.terminal.as_ref())
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(text, bracketed);
            if let Some(session) = self.sessions.tab_mut(tab) {
                // v1.11.7 (PLAN_v1117 §三 M2.1, D-c): paste is real user input
                // — forwarding to the PTY counts as interactive stdin, so the
                // noninteractive render tier falls back to the classic
                // takeover for the pasted-into TUI. Only the passthrough
                // branch marks: editor-insert pastes are never forwarded.
                if let Some(t) = session.terminal.as_mut() {
                    t.note_interactive_stdin();
                }
                // v1.11.15 (FIX E): a partial write surfaces a truncation
                // toast — defensive only on production macOS (n_tty silently
                // discards input overflow; the master always reports full
                // success — pty.rs's saturated-child anchor). The branch
                // exists for ssh/remote ptys and future platform behavior.
                match session.write_user_input(&bytes) {
                    Ok(n) if n < bytes.len() => {
                        warn!(written = n, total = bytes.len(), tab, "paste truncated");
                        if let Some(renderer) = self.renderer.as_mut() {
                            renderer.set_paste_toast(Some((
                                "粘贴已截断".to_string(),
                                std::time::Instant::now(),
                            )));
                        }
                        self.request_redraw();
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, tab, "failed to paste to PTY"),
                }
            }
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
            let Some(terminal) = tab.terminal.as_ref() else {
                return;
            };
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

#[cfg(test)]
mod paste_guard_tests {
    use super::{
        paste_toast_eligible, paste_toast_expired, resolve_pending_paste,
        take_pending_paste_if_seq_matches, PasteOutcome, PendingPaste, PASTE_TOAST_TTL,
    };
    use crate::macos_alert::PastePromptResponse;

    fn pending(session_id: u64, seq: u64) -> Option<PendingPaste> {
        Some(PendingPaste {
            session_id,
            seq,
            text: "payload".to_string(),
            risk: weft_core::input::PasteRisk::Large,
        })
    }

    // ── resolve_pending_paste three-branch table (PLAN_v1111 §5.2) ─────

    #[test]
    fn paste_decision_table_maps_each_response() {
        assert_eq!(
            resolve_pending_paste(PastePromptResponse::Cancel),
            PasteOutcome::Discard
        );
        assert_eq!(
            resolve_pending_paste(PastePromptResponse::Once),
            PasteOutcome::Apply {
                grant_session: false
            }
        );
        assert_eq!(
            resolve_pending_paste(PastePromptResponse::AlwaysSession),
            PasteOutcome::Apply {
                grant_session: true
            }
        );
    }

    // ── park/take exactly-once state machine (v1.11.11: seq-paired) ────

    #[test]
    fn parked_paste_is_consumed_exactly_once_and_stray_decisions_are_noops() {
        // None → Some: park while the deferred prompt is on screen (seq 1).
        let mut slot = pending(3, 1);
        assert!(slot.is_some());

        // Some → None: the first decision with the matching seq consumes it.
        let consumed = take_pending_paste_if_seq_matches(&mut slot, 1);
        assert_eq!(consumed.as_ref().map(|p| p.session_id), Some(3));
        assert!(slot.is_none(), "take must empty the slot");

        // A duplicated/stray decision event finds nothing — no double apply.
        assert!(take_pending_paste_if_seq_matches(&mut slot, 1).is_none());
        assert!(take_pending_paste_if_seq_matches(&mut slot, 1).is_none());
    }

    #[test]
    fn superseding_a_parked_paste_replaces_the_text_in_place() {
        let mut slot = pending(0, 1);
        assert_eq!(
            slot.as_ref().map(|p| p.session_id),
            Some(0),
            "first paste parked"
        );
        // A second Cmd+V while the dialog is up parks over the first with a
        // fresh seq (2).
        slot = Some(PendingPaste {
            session_id: 2,
            seq: 2,
            text: "newer paste".to_string(),
            risk: weft_core::input::PasteRisk::ControlChars,
        });
        // v1.11.11 (M-B): the FIRST dialog's decision (seq 1) is stale — it
        // must NOT consume the newer park.
        assert!(
            take_pending_paste_if_seq_matches(&mut slot, 1).is_none(),
            "stale decision must leave the newer park in place"
        );
        assert!(slot.is_some(), "newer park survives the stale decision");
        let consumed = take_pending_paste_if_seq_matches(&mut slot, 2).unwrap();
        assert_eq!(consumed.session_id, 2);
        assert_eq!(consumed.text, "newer paste");
        assert!(slot.is_none());
    }

    #[test]
    fn stale_seq_drop_never_consumes_a_newer_park() {
        // One park, one stale decision: taken is None, the park stays for its
        // own matching decision.
        let mut slot = Some(PendingPaste {
            session_id: 7,
            seq: 4,
            text: "keep".to_string(),
            risk: weft_core::input::PasteRisk::Large,
        });
        for stale_seq in [1, 2, 3, 5, 99] {
            assert!(
                take_pending_paste_if_seq_matches(&mut slot, stale_seq).is_none(),
                "seq {stale_seq} must be dropped as stale"
            );
            assert!(slot.is_some(), "park must survive stale seq {stale_seq}");
        }
        // The matching decision applies — carries the parked identity.
        let applied = take_pending_paste_if_seq_matches(&mut slot, 4).expect("matching seq");
        assert_eq!(applied.session_id, 7);
        assert_eq!(applied.text, "keep");
        assert!(slot.is_none());
    }

    #[test]
    fn cancel_discards_without_writing_and_once_keeps_session_flag_untouched() {
        // The caller-side contract expressed as data: Discard carries no
        // grant; Once must not flip the session flag.
        match resolve_pending_paste(PastePromptResponse::Cancel) {
            PasteOutcome::Discard => {}
            other => panic!("cancel must discard, got {other:?}"),
        }
        match resolve_pending_paste(PastePromptResponse::Once) {
            PasteOutcome::Apply { grant_session } => assert!(!grant_session),
            other => panic!("once must apply without grant, got {other:?}"),
        }
    }

    // ── toast eligibility + expiry (PLAN_v1111 §4.5/§5.2) ──────────────

    #[test]
    fn toast_shows_only_at_or_above_threshold_and_counts_chars_for_editor() {
        const KIB16: u32 = 16;
        let ascii_bytes = 16 * 1024; // ASCII: byte count == char count
                                     // Passthrough counts bytes: exact threshold shows (>=), below hides.
        assert!(paste_toast_eligible(ascii_bytes, ascii_bytes, KIB16, false));
        assert!(!paste_toast_eligible(
            ascii_bytes - 1,
            ascii_bytes - 1,
            KIB16,
            false
        ));
        // Editor mode counts chars (PLAN_v1111 §4.5): the same physical
        // paste can qualify differently depending on mode.
        let cjk_bytes = 4 * 1024; // ≈1365 three-byte CJK chars = ~12 KiB
        assert!(
            !paste_toast_eligible(cjk_bytes, 1000, KIB16, true),
            "editor with few chars stays quiet"
        );
        assert!(paste_toast_eligible(cjk_bytes, 20 * 1024, KIB16, true));
        assert!(
            !paste_toast_eligible(cjk_bytes, 20 * 1024, KIB16, false),
            "passthrough would judge the same payload by bytes"
        );
    }

    #[test]
    fn toast_expires_at_the_three_second_boundary() {
        let start = std::time::Instant::now();
        let later = |millis: u64| start + std::time::Duration::from_millis(millis);
        // Just before TTL: still visible.
        assert!(!paste_toast_expired(
            start,
            later(PASTE_TOAST_TTL.as_millis() as u64 - 1)
        ));
        // At and past TTL: expired.
        assert!(paste_toast_expired(
            start,
            later(PASTE_TOAST_TTL.as_millis() as u64)
        ));
        assert!(paste_toast_expired(
            start,
            later(PASTE_TOAST_TTL.as_millis() as u64 + 999)
        ));
    }
}
