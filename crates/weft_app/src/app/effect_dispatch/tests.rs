//! Paste-guard unit tests, moved verbatim out of `effect_dispatch.rs`'s
//! inline `mod paste_guard_tests` (v1.13.9 backlog clear file-budget
//! split). Renamed to `tests` per the repo test-module convention so the
//! file enjoys the architecture-gate `*/tests.rs` exemption (plan P3).

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
