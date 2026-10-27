use crate::{clipboard_paste, warn};
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
pub(super) fn take_pending_paste_if_seq_matches(
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
pub(super) fn paste_toast_eligible(
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
    /// v1.15.0 (T3): the active tab's footer feedback rides the same tick —
    /// a background tab's stale feedback clears within one tick (≤1s) after
    /// reactivation, since only the active tab is swept.
    pub(crate) fn expire_paste_toast_tick(&mut self) {
        let expired = self
            .renderer
            .as_mut()
            .is_some_and(|renderer| renderer.clear_expired_paste_toast(std::time::Instant::now()));
        let footer_expired = self.expire_active_tab_footer_feedback();
        if expired || footer_expired {
            self.request_redraw();
        }
    }
}

impl crate::App {
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
            .and_then(|t| {
                t.with_terminal(|t| t.effective_input_mode() == weft_core::input::InputMode::Editor)
            })
            .unwrap_or(false);
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
            .and_then(|t| t.with_terminal(|t| t.effective_input_mode()))
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Preserve pasted newlines explicitly because insert_char rejects
            // controls; drop CR to normalize external CRLF text.
            if let Some(mut t) = self
                .sessions
                .tab_mut(tab)
                .and_then(|tab| tab.lock_terminal())
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
                .and_then(|t| t.with_terminal(|t| t.bracketed_paste))
                .unwrap_or(false);
            let bytes = encode_paste(text, bracketed);
            if let Some(session) = self.sessions.tab_mut(tab) {
                // v1.11.7 (PLAN_v1117 §三 M2.1, D-c): paste is real user input
                // — forwarding to the PTY counts as interactive stdin, so the
                // noninteractive render tier falls back to the classic
                // takeover for the pasted-into TUI. Only the passthrough
                // branch marks: editor-insert pastes are never forwarded.
                session.with_terminal(|t| t.note_interactive_stdin());
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
}
