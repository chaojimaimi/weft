//! v1.11.5 (PLAN_v1115 §M2): dispatch table for app-facing `UiEvent`s
//! drained from the terminal (OSC 52 / OSC 9 / OSC 777).
//!
//! Invoked by `App::process_messages` once per batch, AFTER the per-tab
//! borrows are released. Each arm gates against config + app state; the
//! actual sinks land in later modules (M3 OSC52 read prompt, M4
//! NotificationSink, M7 Dock badge) — until they land, approved events
//! end in an explicit tracing trail so the OSC path stays exercised.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use weft_core::config::Osc52Mode;
use weft_core::pane_layout::PaneId;
use weft_core::vt::UiEvent;

use crate::App;
use winit::raw_window_handle::HasWindowHandle as _;

/// Monotonic OSC 52 read-request sequence. Every parked read gets a fresh
/// seq; the modal's decision must echo it back so a stale answer can never
/// consume a newer slot (peek-compare-take, PLAN_v1115 §M3).
static NEXT_OSC52_READ_SEQ: AtomicU64 = AtomicU64::new(1);

/// A parked OSC 52 read request (single slot, PLAN_v1115 H-i).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Osc52ReadPark {
    pub seq: u64,
    pub pane_id: PaneId,
}

/// Gate verdict for an OSC 52 read request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadGate {
    /// Unrestricted mode: read + reply immediately, no prompt, no park.
    Allow,
    /// Solicited: park + prompt the user.
    Prompt,
    /// Silently treat as denied (off mode, or inside the deny cooldown).
    SilentDeny,
}

/// Pure gate: `[clipboard].osc52` off → silent; deny cooldown active →
/// silent (above ALL modes — unrestricted never arms the cooldown, but the
/// ordering keeps the anti-modal-bombing invariant single-sourced); else
/// `unrestricted` reads through (config/UI promise: silent pass-through)
/// and `default` prompts.
pub(crate) fn osc52_read_gate(
    mode: Osc52Mode,
    cooldown: crate::notify_policy::DenyCooldown,
    now: Instant,
) -> ReadGate {
    if mode == Osc52Mode::Off {
        return ReadGate::SilentDeny;
    }
    if cooldown.in_cooldown(now) {
        return ReadGate::SilentDeny;
    }
    if mode == Osc52Mode::Unrestricted {
        return ReadGate::Allow;
    }
    ReadGate::Prompt
}

/// Pure peek-compare-take: consume the parked request ONLY when its seq
/// matches the decision seq. A stale decision is ignored AND the slot stays
/// occupied (do NOT copy the large-paste template's unconditional take —
/// PLAN_v1115 §M3 explicitly forbids it).
pub(crate) fn take_pending_if_seq_matches(
    slot: &mut Option<Osc52ReadPark>,
    seq: u64,
) -> Option<Osc52ReadPark> {
    match slot {
        Some(parked) if parked.seq == seq => slot.take(),
        Some(_) => None, // stale — leave the newer park in place
        None => None,
    }
}

/// Pure single-slot occupation (PLAN_v1115 H-i): `false` when the slot is
/// already occupied — the caller must then DROP the new request with a
/// warn (never stack modal prompts).
pub(crate) fn try_occupy_osc52_read_slot(
    slot: &mut Option<Osc52ReadPark>,
    seq: u64,
    pane_id: PaneId,
) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(Osc52ReadPark { seq, pane_id });
    true
}

/// Dock-badge debounce: 200 ms latest-wins (PLAN_v1115 D-e). `Clear`
/// bypasses the window and applies immediately; everything else is staged
/// and applied at most once per `PROGRESS_DEBOUNCE`, keeping the newest
/// value (a faster stream overwrites the pending slot — latest wins).
pub(crate) const PROGRESS_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, Default)]
pub(crate) struct DockBadgeDebounce {
    pending: Option<String>,
    last_applied: Option<Instant>,
}

/// What the debounce wants the caller to do right now.
#[derive(Debug)]
pub(crate) enum DebounceAction {
    /// Apply this badge text now (or clear when `None`).
    Apply(Option<String>),
    /// Stage it; nothing to do this tick.
    Deferred,
}

impl DockBadgeDebounce {
    /// Feed one OSC 9;4-derived badge value (already mapped by
    /// `badge_text`; `None` = clear). Clear applies immediately (plan D-e
    /// exception); other values apply if the last application was ≥ 200 ms
    /// ago, else they replace the pending slot (latest wins).
    pub(crate) fn accept(&mut self, now: Instant, value: Option<String>) -> DebounceAction {
        if value.is_none() {
            self.pending = None;
            self.last_applied = Some(now);
            return DebounceAction::Apply(None);
        }
        match self.last_applied {
            None => {
                self.last_applied = Some(now);
                DebounceAction::Apply(value)
            }
            Some(last) if now.duration_since(last) >= PROGRESS_DEBOUNCE => {
                self.pending = None;
                self.last_applied = Some(now);
                DebounceAction::Apply(value)
            }
            Some(_) => {
                self.pending = value;
                DebounceAction::Deferred
            }
        }
    }

    /// Flush the staged value once its 200 ms window has elapsed. Called
    /// from the frame drain and the 1 Hz autosave tick so a stalled
    /// progress stream still lands its final badge (~1 s worst case).
    pub(crate) fn flush_if_due(&mut self, now: Instant) -> Option<Option<String>> {
        let pending = self.pending.take()?;
        let due = self
            .last_applied
            .map_or(true, |last| now.duration_since(last) >= PROGRESS_DEBOUNCE);
        if due {
            self.last_applied = Some(now);
            Some(Some(pending))
        } else {
            self.pending = Some(pending);
            None
        }
    }
}

impl App {
    /// v1.11.5 (PLAN_v1115 §M2): dispatch one batch of drained ui events.
    pub(crate) fn dispatch_ui_events(&mut self, events: Vec<UiEvent>) {
        for event in events {
            match event {
                UiEvent::RemoteNotify { title, body } => self.on_remote_notify(title, body),
                UiEvent::ClipboardWrite { data, truncated } => {
                    self.on_clipboard_write(data, truncated);
                }
                UiEvent::ClipboardReadRequest => self.on_clipboard_read_request(),
                UiEvent::DockProgress(progress) => {
                    self.on_dock_progress(crate::notify_policy::badge_text(progress));
                }
            }
        }
    }

    /// OSC 9;message / OSC 777;notify — a program asks for a notification.
    /// Gate: master switch → focus (out of focus only) → rate limiter.
    /// A gated notification must leave a debug trail (plan D-f) or "my
    /// notification does not pop" reports become impossible to triage.
    fn on_remote_notify(&mut self, title: String, body: String) {
        let notifications = self.config_state.config.notifications;
        if !notifications.enabled {
            tracing::debug!(%title, %body, "remote notify gated: notifications disabled");
            return;
        }
        if self.window_focused {
            tracing::debug!(%title, %body, "remote notify gated: window focused");
            return;
        }
        if !self.notify_limiter.try_acquire(std::time::Instant::now()) {
            tracing::debug!(%title, %body, "remote notify gated: rate limiter");
            return;
        }
        // Approved: post through the sink (M4). Remote notifications carry
        // no block target — activation only fronts the window (M6).
        tracing::debug!(%title, %body, "remote notify approved; posting");
        self.notification_sink
            .post(crate::macos_notifications::NotificationPayload {
                title,
                body,
                sound: notifications.sound,
                block_id: None,
            });
    }

    /// OSC 52;c;<b64> — a program writes the system clipboard.
    fn on_clipboard_write(&mut self, data: Vec<u8>, truncated: bool) {
        // Gate: `[clipboard].osc52 != off` (M8-config; `unrestricted` and
        // `default` both permit writes — they differ on the read side).
        let mode = self.config_state.config.clipboard.osc52;
        if mode == weft_core::config::Osc52Mode::Off {
            tracing::debug!(bytes = data.len(), "OSC 52 write gated: clipboard off");
            return;
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        // NUL bytes in the decoded payload would silently clear the
        // pasteboard (macos_system.rs strips them defensively too) and
        // would confuse user-facing toasts — strip here, before reuse.
        let clean = text.replace('\0', "");
        crate::macos_system::clipboard_copy(&clean);
        if truncated {
            let message = format!(
                "OSC 52 内容超大：已截断为前 {} 字节写入剪贴板",
                weft_core::vt::OSC52_MAX_BYTES
            );
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_paste_toast(Some((message, std::time::Instant::now())));
                self.request_redraw();
            }
            warn!(
                bytes = data.len(),
                "OSC 52 write truncated to the business cap"
            );
        }
        tracing::debug!(bytes = clean.len(), truncated, "OSC 52 clipboard write");
    }

    /// OSC 52;c;? — a program asks to read the clipboard.
    ///
    /// Gate (M8-config + M5 cooldown): `off` mode or an in-flight deny
    /// cooldown → silent (no prompt, no answer); otherwise park the request
    /// (single slot — an occupied slot DROPS the new request with a warn,
    /// H-i) and prompt off the winit handler.
    fn on_clipboard_read_request(&mut self) {
        let mode = self.config_state.config.clipboard.osc52;
        let now = std::time::Instant::now();
        let gate = osc52_read_gate(mode, self.osc52_deny_cooldown, now);
        match gate {
            ReadGate::SilentDeny => {
                tracing::debug!("OSC 52 read request silently denied (mode or cooldown)");
            }
            ReadGate::Allow => {
                // Unrestricted: read + reply now, no prompt, no park
                // (reviewer P1-1 — the mode's config/UI promise).
                let pane_id = self.sessions.active().active_pane_id();
                self.deliver_osc52_read_reply(
                    NEXT_OSC52_READ_SEQ.fetch_add(1, Ordering::Relaxed),
                    pane_id,
                );
            }
            ReadGate::Prompt => {
                // F18: the request came from the active pane's terminal.
                let pane_id = self.sessions.active().active_pane_id();
                self.park_and_prompt_osc52_read(pane_id);
            }
        }
    }

    /// OSC 9;4 — Dock badge text through the 200 ms latest-wins debounce
    /// (M7: real setter; the text mapping is M5 badge_text).
    fn on_dock_progress(&mut self, badge: Option<String>) {
        match self
            .dock_badge_debounce
            .accept(std::time::Instant::now(), badge)
        {
            DebounceAction::Apply(value) => {
                // SAFETY: dispatch drain runs on the main event-loop thread.
                unsafe { crate::macos_notifications::set_dock_badge(value.as_deref()) }
            }
            DebounceAction::Deferred => {}
        }
    }

    /// Flush a staged dock badge once its debounce window elapsed. Hooked
    /// into the frame drain and the 1 Hz autosave tick.
    pub(crate) fn flush_dock_badge(&mut self) {
        if let Some(value) = self
            .dock_badge_debounce
            .flush_if_due(std::time::Instant::now())
        {
            // SAFETY: frame drain / autosave tick run on the main thread.
            unsafe { crate::macos_notifications::set_dock_badge(value.as_deref()) }
        }
    }

    /// v1.11.5 (PLAN_v1115 §M6): user clicked a posted notification — activate
    /// the app, key the window, then jump to the command block. Typed
    /// objc2-app-kit APIs ONLY (no raw msg_send): `activateIgnoringOtherApps`,
    /// `makeKeyAndOrderFront`, `ns_window_of` (macos_window.rs). A stale or
    /// already-cleaned block id still fronts the window and shows a toast.
    pub(crate) fn handle_notification_activated(&mut self, block_id: i64) {
        // 1) Front + activate (typed APIs). `activateIgnoringOtherApps`
        //    is soft-deprecated by AppKit in favor of NSApp.activate, but
        //    the plan (PLAN_v1115 §M6) pins this exact API; the deprecated
        //    path remains fully supported.
        #[allow(deprecated)]
        if let Some(mtm) = objc2_foundation::MainThreadMarker::new() {
            objc2_app_kit::NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
        if let Some(window) = self.window.as_ref() {
            if let Ok(handle) = window.window_handle() {
                if let winit::raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                    unsafe {
                        if let Some(view) = objc2::rc::Retained::retain(
                            appkit.ns_view.as_ptr().cast::<objc2_app_kit::NSView>(),
                        ) {
                            if let Some(ns_window) = crate::macos_window::ns_window_of(&view) {
                                ns_window.makeKeyAndOrderFront(None);
                            }
                        }
                    }
                }
            }
        }

        // 2) Jump to the block (PanelController::navigate_to_block, F12).
        let ok = self.navigate_to_block(weft_core::blocks::BlockId(block_id as u64));
        if ok {
            tracing::info!(block_id, "notification activation: jumped to block");
        } else {
            tracing::warn!(
                block_id,
                "notification activation: block gone (cleaned); fronting only"
            );
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_paste_toast(Some((
                    "该命令块已被清理".to_string(),
                    std::time::Instant::now(),
                )));
                self.request_redraw();
            }
        }
    }

    /// v1.11.5 (PLAN_v1115 §M5 wiring): a `process_messages` batch drained
    /// finished command blocks — decide per block whether it earns a
    /// notification (threshold ≥ `[notifications] threshold_secs` + window
    /// OUT of focus + rate limiter; exact-equality hits). `finish_pending_blocks`
    /// (pane close path) deliberately does NOT notify — accepted limitation
    /// (plan F11) with this comment as the marker.
    pub(crate) fn dispatch_block_completion_notifications(
        &mut self,
        blocks: &[weft_core::blocks::Block],
    ) {
        let notifications = self.config_state.config.notifications;
        if !notifications.enabled {
            return;
        }
        let policy = crate::notify_policy::NotifyPolicy {
            enabled: true,
            threshold: std::time::Duration::from_secs(notifications.threshold_secs),
            sound: notifications.sound,
        };
        for block in blocks {
            let Some(finished_at) = block.finished_at else {
                continue;
            };
            let ok = crate::notify_policy::block_complete_should_notify(
                block.started_at,
                finished_at,
                std::time::SystemTime::now(),
                self.window_focused,
                &policy,
                &mut self.notify_limiter,
            );
            if !ok {
                continue;
            }
            let elapsed = finished_at
                .duration_since(block.started_at)
                .unwrap_or_default();
            let (title, body) = crate::notify_policy::notification_text(
                &block.command,
                elapsed,
                block.exit_code.is_some_and(|code| code != 0),
            );
            tracing::debug!(
                block_id = block.id.0,
                elapsed_ms = elapsed.as_millis(),
                "block completion notification"
            );
            self.notification_sink
                .post(crate::macos_notifications::NotificationPayload {
                    title,
                    body,
                    sound: policy.sound,
                    block_id: Some(block.id.0 as i64),
                });
        }
    }

    /// v1.11.5 (PLAN_v1115 §M3): park the OSC 52 read request and defer its
    /// permission prompt to the main queue (same FIX_RECOVERY_MODAL_SPIN
    /// discipline as `park_and_prompt_paste`: `runModal` must never execute
    /// inside a winit handler).
    ///
    /// Single-slot contract (H-i): a request arriving while a prompt is
    /// already parked is DROPPED with a warn — never stacked into a modal
    /// pileup.
    fn park_and_prompt_osc52_read(&mut self, pane_id: PaneId) {
        let seq = NEXT_OSC52_READ_SEQ.fetch_add(1, Ordering::Relaxed);
        if !try_occupy_osc52_read_slot(&mut self.pending_osc52_read, seq, pane_id) {
            warn!(
                ?pane_id,
                "OSC 52 read request dropped: another prompt is already parked (single slot)"
            );
            return;
        }
        info!(
            seq,
            ?pane_id,
            "OSC 52 read request parked; prompting off the winit handler"
        );
        let proxy = self.proxy.clone();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let allowed = match objc2_foundation::MainThreadMarker::new() {
                Some(mtm) => match crate::macos_alert::show_osc52_read_prompt(mtm) {
                    Ok(allowed) => allowed,
                    Err(error) => {
                        // Fail-closed (like the paste prompt): an alert that
                        // cannot be shown must never turn into a read.
                        warn!(%error, "OSC 52 read prompt failed; answering as deny");
                        false
                    }
                },
                None => {
                    warn!("OSC 52 read prompt skipped: not on main thread; answering as deny");
                    false
                }
            };
            let _ = proxy.send_event(crate::AppEvent::Osc52ReadDecided { allowed, seq });
        });
    }

    /// v1.11.5 (PLAN_v1115 §M3): apply the user's OSC 52 read decision.
    ///
    /// peek-compare-take: the decision pairs with the parked slot ONLY when
    /// `seq` matches. A stale decision is ignored WITHOUT consuming the slot
    /// (a newer park stays active — deliberately unlike the large-paste
    /// template's unconditional take).
    ///
    /// - deny → NO answer at all (D-c, ghostty-style) + starts the 30 s
    ///   cooldown so the same server can't re-prompt every cycle.
    /// - allow → read the system clipboard; an over-cap clipboard
    ///   (> OSC52_READ_REPLY_MAX) or a closed pane answers as deny (the
    ///   reply is never half-sent); otherwise the answer is written
    ///   STRAIGHT to the pane's PTY (`write_sync`, never through
    ///   `pending_output` — see PLAN_v1115 F3: async decisions have no
    ///   guaranteed later drain).
    pub(crate) fn apply_osc52_read_decision(&mut self, allowed: bool, seq: u64) {
        let Some(park) = take_pending_if_seq_matches(&mut self.pending_osc52_read, seq) else {
            warn!(
                seq,
                parked_seq = self.pending_osc52_read.map(|p| p.seq),
                "OSC 52 read decision without a matching parked request; ignoring"
            );
            return;
        };
        if !allowed {
            self.osc52_deny_cooldown
                .mark_deny(std::time::Instant::now());
            info!(
                seq,
                "OSC 52 read denied by user; silent no-answer + 30s cooldown"
            );
            return;
        }
        self.deliver_osc52_read_reply(seq, park.pane_id);
    }

    /// Shared allow-path tail (used by the user-approved decision AND the
    /// unrestricted direct grant): read the system clipboard, size-gate the
    /// reply, then write it STRAIGHT to the pane's PTY (`write_sync`, never
    /// through `pending_output` — see PLAN_v1115 F3: async decisions have
    /// no guaranteed later drain).
    fn deliver_osc52_read_reply(&mut self, seq: u64, pane_id: PaneId) {
        // Read the clipboard and size-gate the reply.
        let Some(text) = crate::macos_system::clipboard_paste() else {
            warn!(
                seq,
                "OSC 52 read allowed but clipboard read failed; no answer"
            );
            return;
        };
        let Some(reply) = weft_core::vt::osc52_read_reply(text.as_bytes()) else {
            warn!(
                seq,
                len = text.len(),
                "OSC 52 read: clipboard exceeds reply cap; answering as deny (no partial base64)"
            );
            return;
        };
        // Locate the pane by id (tab indices shift on close/split — D-g);
        // a closed pane drops the reply with a warn.
        let mut delivered = false;
        for tab in self.sessions.tabs_mut() {
            if let Some(pane) = tab.pane_mut(pane_id) {
                match &pane.pty {
                    Some(pty) => match pty.write_sync(&reply) {
                        Ok(()) => {
                            info!(
                                seq,
                                pane = ?pane_id,
                                bytes = reply.len(),
                                "OSC 52 read reply delivered to pane"
                            );
                        }
                        Err(error) => {
                            warn!(seq, %error, "OSC 52 read reply write failed");
                        }
                    },
                    None => {
                        warn!(seq, pane = ?pane_id, "OSC 52 read: pane has no PTY; dropping reply");
                    }
                }
                delivered = true;
                break;
            }
        }
        if !delivered {
            warn!(seq, pane = ?pane_id, "OSC 52 read: pane closed; dropping reply");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_clear_applies_immediately() {
        let mut d = DockBadgeDebounce::default();
        let t0 = Instant::now();
        match d.accept(t0, Some("42%".into())) {
            DebounceAction::Apply(Some(v)) => assert_eq!(v, "42%"),
            other => panic!("first percent must apply, got {other:?}"),
        }
        // Clear bypasses the window even 1 ms later
        match d.accept(t0 + Duration::from_millis(1), None) {
            DebounceAction::Apply(None) => {}
            other => panic!("clear must apply immediately, got {other:?}"),
        }
    }

    #[test]
    fn debounce_latest_wins_within_window() {
        let mut d = DockBadgeDebounce::default();
        let t0 = Instant::now();
        let _ = d.accept(t0, Some("10%".into()));
        // 100 ms later: staged, not applied
        match d.accept(t0 + Duration::from_millis(100), Some("20%".into())) {
            DebounceAction::Deferred => {}
            other => panic!("expected deferred, got {other:?}"),
        }
        // newest value replaces the pending slot (latest wins)
        match d.accept(t0 + Duration::from_millis(150), Some("30%".into())) {
            DebounceAction::Deferred => {}
            other => panic!("expected deferred, got {other:?}"),
        }
        // window elapsed → the NEWEST staged value applies
        match d.accept(t0 + Duration::from_millis(201), Some("40%".into())) {
            DebounceAction::Apply(Some(v)) => assert_eq!(v, "40%"),
            other => panic!("expected apply of newest, got {other:?}"),
        }
    }

    #[test]
    fn debounce_flush_lands_staged_value() {
        let mut d = DockBadgeDebounce::default();
        let t0 = Instant::now();
        let _ = d.accept(t0, Some("10%".into()));
        let _ = d.accept(t0 + Duration::from_millis(50), Some("70%".into()));
        // not due yet
        assert!(d.flush_if_due(t0 + Duration::from_millis(100)).is_none());
        // due → lands the latest value; second flush is empty
        let flushed = d
            .flush_if_due(t0 + Duration::from_millis(250))
            .expect("due");
        assert_eq!(flushed.as_deref(), Some("70%"));
        assert!(d.flush_if_due(t0 + Duration::from_millis(300)).is_none());
    }

    // ── M3: OSC 52 read park / gate (headless, injected) ──────────────

    #[test]
    fn read_gate_mode_and_cooldown() {
        use crate::notify_policy::DenyCooldown;
        use weft_core::config::Osc52Mode;

        let t0 = Instant::now();
        let fresh = DenyCooldown::new(Duration::from_secs(30));
        // off mode → silent regardless of cooldown
        assert_eq!(
            osc52_read_gate(Osc52Mode::Off, fresh, t0),
            ReadGate::SilentDeny
        );
        // default mode, no cooldown → prompt
        assert_eq!(
            osc52_read_gate(Osc52Mode::Default, fresh, t0),
            ReadGate::Prompt
        );
        // unrestricted mode → read through, no prompt (reviewer P1-1)
        assert_eq!(
            osc52_read_gate(Osc52Mode::Unrestricted, fresh, t0),
            ReadGate::Allow
        );
        // deny cooldown armed → silent even in unrestricted mode
        let mut denied = DenyCooldown::new(Duration::from_secs(30));
        denied.mark_deny(t0);
        assert_eq!(
            osc52_read_gate(Osc52Mode::Unrestricted, denied, t0),
            ReadGate::SilentDeny,
            "cooldown gate applies above mode (anti modal-bombing)"
        );
        // cooldown expired → prompt again
        assert_eq!(
            osc52_read_gate(Osc52Mode::Default, denied, t0 + Duration::from_secs(31)),
            ReadGate::Prompt
        );
    }

    #[test]
    fn stale_decision_does_not_consume_newer_slot() {
        // park seq=1, decision seq=2 (stale) → slot UNTOUCHED
        let mut slot = Some(Osc52ReadPark {
            seq: 2,
            pane_id: PaneId(7),
        });
        let taken = take_pending_if_seq_matches(&mut slot, 1);
        assert!(taken.is_none(), "stale decision yields nothing");
        assert!(
            slot.is_some(),
            "stale decision must NOT consume the newer park (peek-compare-take)"
        );
        // matching decision consumes exactly once
        let taken = take_pending_if_seq_matches(&mut slot, 2);
        assert_eq!(
            taken,
            Some(Osc52ReadPark {
                seq: 2,
                pane_id: PaneId(7)
            })
        );
        assert!(slot.is_none(), "consumed");
        // decision with no park at all → nothing
        let mut empty = None;
        assert!(take_pending_if_seq_matches(&mut empty, 99).is_none());
    }

    #[test]
    fn occupied_slot_rejects_new_request() {
        let mut slot: Option<Osc52ReadPark> = None;
        assert!(
            try_occupy_osc52_read_slot(&mut slot, 1, PaneId(3)),
            "empty slot accepts"
        );
        assert!(
            !try_occupy_osc52_read_slot(&mut slot, 2, PaneId(4)),
            "occupied slot rejects (H-i single slot)"
        );
        assert_eq!(
            slot,
            Some(Osc52ReadPark {
                seq: 1,
                pane_id: PaneId(3)
            })
        );
    }

    #[test]
    fn denyless_gate_plus_reply_builder_answer_shapes() {
        // deny zero-output contract at the byte level: the reply builder is
        // reached only on the allow path (apply_osc52_read_decision); the
        // deny path performs no PTY write. Here we pin the allow-path
        // shapes that headless tests rely on:
        //   1. empty clipboard → `52;c;` + BEL (always an answer)
        //   2. over-cap data → None → treated as deny (datapoint in core)
        assert!(weft_core::vt::osc52_read_reply(b"").is_some());
        let over = vec![b'x'; weft_core::vt::OSC52_READ_REPLY_MAX + 1];
        assert!(weft_core::vt::osc52_read_reply(&over).is_none());
    }
}
