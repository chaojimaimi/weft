//! v1.10.21: alt-screen wheel routing + history-peek entry gate (pure logic).
//!
//! Scope: `is_alt_screen_active() && !mouse_reporting` only — apps with SGR
//! mouse reporting (vim `set mouse=a`, htop) and the primary screen keep
//! their own, untouched paths. Design + full state table: docs/
//! FIX_ALT_PEEK_WARP_ALIGNMENT.md (Warp parity: a plain wheel always means
//! "interact with the app" — arrow keys to the PTY, or exit the peek).

use std::time::Instant;

/// Decision for one alt-screen wheel gesture (before the PTY write).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AltWheelAction {
    /// Not peeking, plain wheel: forward Up/Down arrow keys to the PTY
    /// (`\x1bOA/B`, Warp parity). `up` = wheel-up.
    ForwardArrows { up: bool },
    /// Not peeking, Shift+wheel-up, gate admits: overlay the history
    /// BlockView over the TUI.
    EnterPeek,
    /// Peeking, Shift+wheel: navigate the browsed block history.
    ScrollBlocks { up: bool },
    /// Peeking, plain wheel (either direction): instantly return to the
    /// live TUI — "普通手势=与应用交互", one flick exits.
    ExitPeek,
    /// Consume the gesture with no effect (Shift+wheel-down while not
    /// peeking, or a Shift+wheel-up denied by the entry gate).
    Noop,
}

/// Full routing table. `entry_allowed` is the gate verdict (re-entry
/// lockout + minimum-travel threshold) and only matters for the
/// not-peeking Shift+wheel-up branch.
pub(crate) fn route(peeking: bool, shift: bool, up: bool, entry_allowed: bool) -> AltWheelAction {
    match (peeking, shift) {
        // Peek active: Shift keeps browsing history; a plain wheel hands
        // the gesture back to the app.
        (true, true) => AltWheelAction::ScrollBlocks { up },
        (true, false) => AltWheelAction::ExitPeek,
        // Not peeking: Shift+wheel-up enters the peek — unless the gate
        // denies (inertial tail right after an exit, or a sub-threshold
        // flick). Shift+wheel-down is a no-op (nothing to leave).
        (false, true) if up && entry_allowed => AltWheelAction::EnterPeek,
        (false, true) => AltWheelAction::Noop,
        // Not peeking, plain wheel: forward arrows to the TUI.
        (false, false) => AltWheelAction::ForwardArrows { up },
    }
}

/// Re-entry lockout + minimum-travel gate for entering the alt-screen
/// history peek.
///
/// Two guards, both aimed at the same failure mode (the trackpad's
/// inertial tail re-opening the peek right after the user left it):
///
/// 1. `last_exit` — after a peek exits, entry requests are ignored for
///    [`EXIT_LOCKOUT`](Self::EXIT_LOCKOUT) (400ms, `Instant` timestamp).
/// 2. `accumulated` — entering needs ≥ [`MIN_ENTRY_ROWS`](Self::MIN_ENTRY_ROWS)
///    rows of NET upward travel so a single-row flick never yanks the TUI
///    into history. Every alt-screen wheel gesture feeds its signed rows
///    (up = positive) into the accumulator, mirroring `scroll_input`'s
///    quantization semantics; a satisfied entry (or an exit) resets it.
#[derive(Debug, Default)]
pub(crate) struct PeekEntryGate {
    /// When the peek last exited; `None` before the first exit (no lock).
    last_exit: Option<Instant>,
    /// Net upward rows since the last entry/exit (downward travel
    /// subtracts, clamped at 0).
    accumulated: f32,
}

impl PeekEntryGate {
    /// Lockout window after a peek exit (inertial-tail suppression).
    pub(crate) const EXIT_LOCKOUT: std::time::Duration = std::time::Duration::from_millis(400);
    /// Minimum net upward rows before an entry request is honored.
    pub(crate) const MIN_ENTRY_ROWS: f32 = 2.0;

    /// Record a peek exit: arm the re-entry lockout and reset the travel
    /// accumulator so the next gesture starts fresh. Idempotent — callers
    /// invoke it on every peek-clearing path (snap to bottom, scroll back
    /// to tail, alt-screen exit, plain wheel in peek).
    pub(crate) fn note_exit(&mut self) {
        self.last_exit = Some(Instant::now());
        self.accumulated = 0.0;
    }

    /// Feed one alt-screen wheel gesture's signed rows (`rows > 0` = up)
    /// and return whether entering the peek is allowed right now. An
    /// allowed entry consumes the accumulator; a denied one (sub-threshold)
    /// keeps it so the gesture can still cross the threshold on a later
    /// event. Requests inside the lockout window are ignored ENTIRELY —
    /// they neither enter nor accumulate, so the inertial tail can't bank
    /// travel toward a post-lockout entry.
    pub(crate) fn allow_entry(&mut self, now: Instant, rows: f32) -> bool {
        if self
            .last_exit
            .is_some_and(|exited| now.duration_since(exited) < Self::EXIT_LOCKOUT)
        {
            return false;
        }
        self.accumulated = (self.accumulated + rows).max(0.0);
        if self.accumulated < Self::MIN_ENTRY_ROWS {
            return false;
        }
        self.accumulated = 0.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── route(): full state table ─────────────────────────────────────

    #[test]
    fn plain_wheel_forwards_arrows_in_both_directions() {
        assert_eq!(
            route(false, false, true, false),
            AltWheelAction::ForwardArrows { up: true }
        );
        assert_eq!(
            route(false, false, true, true),
            AltWheelAction::ForwardArrows { up: true }
        );
        assert_eq!(
            route(false, false, false, false),
            AltWheelAction::ForwardArrows { up: false }
        );
        assert_eq!(
            route(false, false, false, true),
            AltWheelAction::ForwardArrows { up: false }
        );
    }

    #[test]
    fn shift_wheel_up_enters_peek_only_when_gate_admits() {
        assert_eq!(route(false, true, true, true), AltWheelAction::EnterPeek);
        // Locked out / sub-threshold flick → swallowed, no peek, no arrows.
        assert_eq!(route(false, true, true, false), AltWheelAction::Noop);
    }

    #[test]
    fn shift_wheel_down_without_peek_is_noop() {
        assert_eq!(route(false, true, false, true), AltWheelAction::Noop);
        assert_eq!(route(false, true, false, false), AltWheelAction::Noop);
    }

    #[test]
    fn peeking_shift_wheel_scrolls_blocks() {
        assert_eq!(
            route(true, true, true, false),
            AltWheelAction::ScrollBlocks { up: true }
        );
        assert_eq!(
            route(true, true, false, false),
            AltWheelAction::ScrollBlocks { up: false }
        );
    }

    #[test]
    fn peeking_plain_wheel_exits_instantly() {
        assert_eq!(route(true, false, true, false), AltWheelAction::ExitPeek);
        assert_eq!(route(true, false, false, false), AltWheelAction::ExitPeek);
        assert_eq!(route(true, false, false, true), AltWheelAction::ExitPeek);
    }

    // ── PeekEntryGate: lockout + travel threshold ─────────────────────

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn entry_requires_two_net_upward_rows() {
        let mut gate = PeekEntryGate::default();
        assert!(
            !gate.allow_entry(t0(), 1.0),
            "single-row flick must not enter"
        );
        assert!(!gate.allow_entry(t0(), 0.5), "still under 2 rows");
        assert!(
            gate.allow_entry(t0(), 0.5),
            "accumulated net 2 rows → admitted"
        );
    }

    #[test]
    fn down_scroll_subtracts_from_accumulator() {
        let mut gate = PeekEntryGate::default();
        assert!(
            !gate.allow_entry(t0(), 1.5),
            "net 1.5 rows is below the threshold"
        );
        // Downward travel cancels the net up-scroll (clamped at 0).
        assert!(!gate.allow_entry(t0(), -2.0), "net 0 rows");
        assert!(
            !gate.allow_entry(t0(), 1.0),
            "net 1 row still below threshold"
        );
        assert!(gate.allow_entry(t0(), 1.0), "net 2 rows → admitted");
    }

    #[test]
    fn satisfied_entry_resets_accumulator() {
        let mut gate = PeekEntryGate::default();
        assert!(gate.allow_entry(t0(), 2.0));
        assert!(
            !gate.allow_entry(t0(), 1.0),
            "accumulator reset after entry"
        );
        assert!(gate.allow_entry(t0(), 1.0), "fresh 2 rows → admitted again");
    }

    #[test]
    fn no_prior_exit_means_no_lockout() {
        let mut gate = PeekEntryGate::default();
        // Fresh gate: 2 rows admit immediately, no waiting window.
        assert!(gate.allow_entry(t0(), 2.0));
    }

    #[test]
    fn exit_arms_lockout_window() {
        let mut gate = PeekEntryGate::default();
        gate.note_exit();
        // Capture the clock AFTER the stamp so the offsets are provably
        // inside/outside the lockout regardless of the sub-ms gap between
        // the two calls (an exact `+400ms` boundary would race that gap).
        let after_exit = Instant::now();
        assert!(!gate.allow_entry(after_exit + Duration::from_millis(100), 2.0));
        assert!(!gate.allow_entry(after_exit + Duration::from_millis(399), 2.0));
        assert!(gate.allow_entry(after_exit + Duration::from_millis(500), 2.0));
    }

    #[test]
    fn exit_resets_accumulator_and_lock_expires() {
        let mut gate = PeekEntryGate::default();
        assert!(gate.allow_entry(Instant::now(), 2.0)); // entered
        gate.note_exit(); // peek exits: lock armed, accumulator dropped
        let after_exit = Instant::now();
        assert!(
            !gate.allow_entry(after_exit + Duration::from_millis(50), 1.0),
            "locked"
        );
        // Locked events are ignored without accumulating, so after the lock
        // expires the post-exit travel starts fresh: 1 row is still below
        // the threshold, the second row admits.
        assert!(!gate.allow_entry(after_exit + Duration::from_millis(450), 1.0));
        assert!(gate.allow_entry(after_exit + Duration::from_millis(460), 1.0));
    }

    #[test]
    fn note_exit_is_idempotent() {
        let mut gate = PeekEntryGate::default();
        gate.allow_entry(t0(), 5.0);
        gate.note_exit();
        gate.note_exit(); // double-clear from overlapping paths is harmless
        assert!(!gate.allow_entry(t0(), 3.0), "lock armed by either call");
    }
}
