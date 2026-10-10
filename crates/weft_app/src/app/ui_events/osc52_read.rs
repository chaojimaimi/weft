//! OSC 52 read decision family (`park_and_prompt_osc52_read` /
//! `apply_osc52_read_decision` / `deliver_osc52_read_reply`), moved
//! verbatim out of `app/ui_events.rs` (v1.13.8 S3 zero-behavior
//! file-budget split; `impl App` cross-file block per the
//! app/session_pump.rs precedent). The single-slot park helpers and the
//! seq counter stay in `app/ui_events.rs` (shared with the
//! clipboard-read-request gate and its tests).

use std::sync::atomic::Ordering;

use tracing::{info, warn};
use weft_core::pane_layout::PaneId;

use super::{take_pending_if_seq_matches, try_occupy_osc52_read_slot, App, NEXT_OSC52_READ_SEQ};

impl App {
    /// v1.11.5 (PLAN_v1115 §M3): park the OSC 52 read request and defer its
    /// permission prompt to the main queue (same FIX_RECOVERY_MODAL_SPIN
    /// discipline as `park_and_prompt_paste`: `runModal` must never execute
    /// inside a winit handler).
    ///
    /// Single-slot contract (H-i): a request arriving while a prompt is
    /// already parked is DROPPED with a warn — never stacked into a modal
    /// pileup.
    pub(super) fn park_and_prompt_osc52_read(&mut self, pane_id: PaneId) {
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
            // v1.11.12 (PLAN_v11112 M-C): decision-loop send — a failure here
            // parks the OSC 52 read request forever.
            // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
            if let Err(e) = proxy.send_event(crate::AppEvent::Osc52ReadDecided { allowed, seq }) {
                warn!(error = %e, "send_event failed: OSC 52 read decision lost; parked request never resolves");
            }
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
    pub(super) fn deliver_osc52_read_reply(&mut self, seq: u64, pane_id: PaneId) {
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
                    Some(pty) => match pty.writer().write_sync(&reply) {
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
