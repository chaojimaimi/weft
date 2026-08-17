//! Single source of truth for terminal capability state.
//!
//! Historically the capability signals driving input routing, scroll policy,
//! rendering ownership, and the primary-screen command lifecycle were scattered
//! across a dozen private fields on `Terminal`. Every consumer recomputed the
//! same combination of `alt_active` / `phase` / `cursor_ops` / `mouse_protocol`
//! / interrupt / exit flags independently, which made it easy for two consumers
//! to drift (a real regression source during the v1.0 RC cycle).
//!
//! `CapabilityFlags` gathers those fields behind one struct so the existing
//! predicates on `Terminal` become thin readers over a shared state, and new
//! diagnostics (R1 task 2 — `screen_owner` / `settle_state`) derive from the
//! same place. All fields stay `pub(in crate::vt)`; external callers keep using
//! the `Terminal` accessor methods (`mouse_protocol()`, `app_cursor_keys()`,
//! `accepts_mouse_reporting_input()`, ...).
//!
//! Persistence impact is intentionally zero: every field here is ephemeral and
//! reset to its default in `Terminal::with_scrollback`. Nothing is serialized
//! into SQLite, so moving the storage location changes no on-disk schema.

use crate::blocks::{ShellPhase, StyledOutput};
use crate::input::MouseProtocol;
use std::time::Instant;

use super::screen_exit::{
    PendingPrimaryScreenExit, PrimaryScreenInterruptCapture, PrimaryScreenOwnership,
    PRIMARY_HISTORY_SNAPSHOT_INTERVAL, PRIMARY_SCREEN_EXIT_SETTLE_DELAY,
};

/// v1.10.23 (FIX_OMP_CONTENT_LOSS): text+styles of primary-screen document
/// frames superseded by DEC 2026 full-frame repaints. A repaint (CSI 2J or
/// whole-frame EL2 inside `?2026h`) clears the scrollback, which physically
/// deletes every streamed paragraph that scrolled out of the viewport; the
/// superseded frame is snapshotted into this history BEFORE the clear and
/// prepended to every subsequent screen snapshot, so history review stays
/// complete. Bounded at `MAX_OUTPUT_BYTES` (head-keeping, matching the
/// snapshot truncation semantics). Cleared at each real command start.
#[derive(Default)]
pub(in crate::vt) struct ScreenHistory {
    pub(in crate::vt) text: String,
    pub(in crate::vt) styled: Option<StyledOutput>,
}

/// Aggregated capability + primary-screen lifecycle state.
///
/// The fields are `pub(in crate::vt)` so the existing `impl Terminal` blocks in
/// `screen_exit.rs`, `ownership.rs`, `perform.rs`, and `mod.rs` can read and
/// mutate them through `self.capabilities.<field>` exactly as they used to read
/// `self.<field>`. The migration is mechanical: same fields, same semantics,
/// one storage location.
pub(in crate::vt) struct CapabilityFlags {
    pub(in crate::vt) alt_active: bool,
    pub(in crate::vt) mouse_protocol: MouseProtocol,
    /// SGR-1006 selects the SGR vs legacy mouse-report encoding.
    pub(in crate::vt) sgr_mouse: bool,
    /// Application cursor key mode (DECCKM, CSI ?1h/l).
    pub(in crate::vt) app_cursor_keys: bool,
    /// Weft's generated shell hook appends `weft-shell` to OSC 133 markers.
    /// Once observed, untagged OSC 133 belongs to the foreground application
    /// (semantic zones used by modern TUIs), not to the shell block protocol.
    pub(in crate::vt) tagged_shell_markers_seen: bool,
    /// Count of absolute cursor-addressing ops observed during the current
    /// `CommandExecuting` phase. `>= 2` distinguishes a primary-screen TUI
    /// (Claude Code, OpenCode) from a plain shell command.
    pub(in crate::vt) primary_screen_cursor_ops: u8,
    /// Whether the current primary-screen TUI has used absolute cursor
    /// addressing (CUP `H`/`f`, VPA `d` — v1.10.5: CHR `G` is horizontal-
    /// only and no longer counts) — the full-viewport repaint pattern of
    /// Claude Code / OpenCode, which need the live grid. Relative-only
    /// TUIs (openclaw: CUU/CUD/CUB + EL/ED partial redraws) render
    /// correctly in the BlockView and must stay there, so this flag
    /// keeps `show_block_view()` true for them even after `cursor_ops`
    /// crosses the TUI-detection threshold. Reset with each OSC 133 prompt
    /// marker like `primary_screen_cursor_ops`.
    pub(in crate::vt) primary_screen_absolute_addressing: bool,
    /// v1.10.12: the TUI ever used relative cursor addressing (A/B/D/…).
    /// A sparse repainter (omp/pi) repaints incrementally — shell rows above
    /// its document boundary must NOT be hidden (it didn't repaint them).
    /// Pure full-viewport CUP TUIs (claude code) never set this and DO get
    /// the boundary hiding.
    pub(in crate::vt) primary_screen_relative_addressing_seen: bool,
    /// v1.10.7: per-command render-mode lock. When a primary-screen TUI is
    /// first detected with relative-only addressing (a sparse repainter like
    /// pi/openclaw that redraws its input row per keystroke and only
    /// occasionally issues a full-viewport CUP), the BlockView stays locked
    /// for the whole command — a later CUP repaint must not flip the renderer
    /// to the live grid mid-session (that flips layout every task and loses
    /// the user's Warp-style history blocks). Set at first screen ownership
    /// (`!absolute_addressing` at that moment), reset at each real prompt
    /// boundary. Full-viewport CUP TUIs (Claude Code) detect as absolute →
    /// lock=false → live grid, unchanged.
    /// v1.10.6: the cursor's line index in the most recent primary-screen
    /// snapshot. Tracked during snapshot construction so the BlockView paint
    /// can place the caret/preedit on the exact materialized document row.
    /// `None` before a snapshot or for omitted leading/trailing/unowned rows.
    pub(in crate::vt) primary_screen_cursor_snapshot_line: Option<usize>,
    /// v1.10.26 (FIX_IME_PREEDIT): byte length of the live viewport segment
    /// text in the CURRENT in-flight screen block (the snapshot text as
    /// composed LAST by `compose_screen_history`). Stored alongside the caret
    /// line so the keystroke path (`snapshot_primary_screen_output_for_caret`)
    /// can re-derive the anchor from the published text structure itself
    /// instead of re-adding separately-tracked head counts.
    pub(in crate::vt) primary_screen_cursor_segment_len: Option<usize>,
    /// v1.10.7 (reviewer MEDIUM): last `(row, col)` that drove a caret
    /// snapshot refresh. `snapshot_primary_screen_output_for_caret` is
    /// called per keystroke (no rate limit) and rebuilds the whole document;
    /// the tracked cursor line only depends on the cursor position, so a
    /// keystroke that did not move the cursor can skip the rescan.
    pub(in crate::vt) last_caret_snapshot_cursor: Option<(usize, usize)>,
    /// Scrollback position of the first row owned by the active primary-screen
    /// application; updated by scroll/erase/reflow transforms.
    pub(in crate::vt) primary_screen_document_candidate: u64,
    /// Whether a DEC 2026 atomic full-frame repaint has been witnessed for the
    /// current primary-screen owner (gates `primary_screen_repaint_capable`).
    pub(in crate::vt) primary_screen_synchronized_frame_seen: bool,
    /// Deferred command-finalization state — set when a primary-screen TUI ends
    /// without an OSC 133;D that we can immediately honor.
    pub(in crate::vt) primary_screen_exit: Option<PendingPrimaryScreenExit>,
    /// Frozen transcript captured before the first Ctrl-C of a double-Ctrl-C
    /// exit, so the second Ctrl-C can still present the full command document.
    pub(in crate::vt) primary_screen_interrupt_capture: Option<PrimaryScreenInterruptCapture>,
    /// User-driven history-browsing toggle (scroll-up while a primary-screen
    /// TUI owns the viewport).
    pub(in crate::vt) primary_history_view: bool,
    /// v1.10.12, v1.10.21 (trigger updated): alt-screen history peek — while
    /// an alt-screen TUI (omp/less/vim without mouse reporting) owns the
    /// screen, **Shift+wheel-up** overlays the terminal's history BlockView
    /// over the TUI (plain wheels forward arrow keys to the app instead, Warp
    /// parity; the mouse_controller routes both through `alt_peek::route`).
    /// When true this makes `show_block_view()` return true even though
    /// `alt_active` is true (the BlockView paint is grid-content-independent,
    /// so this is safe). Cleared on scroll-back-to-bottom, any PTY input, a
    /// plain (non-Shift) wheel while peeking, or alt-screen exit.
    pub(in crate::vt) alt_screen_history_peek: bool,
    /// Timestamp of the most recent history-snapshot refresh (rate-limited by
    /// `PRIMARY_HISTORY_SNAPSHOT_INTERVAL`).
    pub(in crate::vt) primary_history_snapshot_at: Option<Instant>,
    /// Per-row ownership mask separating shell rows from a primary-screen TUI's
    /// live viewport, preserved across scrolls and resize/reflow.
    pub(in crate::vt) primary_screen_ownership: PrimaryScreenOwnership,
    /// v1.10.23 (FIX_OMP_CONTENT_LOSS): superseded-frame preservation history
    /// (see the `ScreenHistory` struct docs).
    pub(in crate::vt) screen_history: ScreenHistory,
}

impl Default for CapabilityFlags {
    fn default() -> Self {
        Self {
            alt_active: false,
            mouse_protocol: MouseProtocol::Off,
            sgr_mouse: false,
            app_cursor_keys: false,
            tagged_shell_markers_seen: false,
            primary_screen_absolute_addressing: false,
            primary_screen_relative_addressing_seen: false,
            primary_screen_cursor_ops: 0,
            primary_screen_cursor_snapshot_line: None,
            primary_screen_cursor_segment_len: None,
            last_caret_snapshot_cursor: None,
            primary_screen_document_candidate: 0,
            primary_screen_synchronized_frame_seen: false,
            primary_screen_exit: None,
            primary_screen_interrupt_capture: None,
            primary_history_view: false,
            alt_screen_history_peek: false,
            primary_history_snapshot_at: None,
            primary_screen_ownership: PrimaryScreenOwnership::default(),
            screen_history: ScreenHistory::default(),
        }
    }
}

/// Coarse classification of who owns the live viewport right now.
///
/// This is a diagnostic / routing hint only; precise decisions still go through
/// the existing `Terminal` predicates, which compose the underlying flags. The
/// enum exists so trace records (R1 task 2) can carry a single `screen_owner`
/// field instead of six booleans, and so a glance at a log line answers
/// "was Claude running?" without reconstructing the formula.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScreenOwner {
    /// No integrated shell yet, or sitting at the prompt.
    Shell,
    /// A primary-screen TUI (e.g. Claude Code) owns the viewport without
    /// entering the alternate screen.
    PrimaryScreenApp,
    /// An alternate-screen application (vim/less/tmux) is active.
    AltScreenApp,
    /// A primary-screen TUI has signaled exit and we are inside the 200ms
    /// settle window before finalizing its command block.
    PrimaryScreenExitPending,
}

impl ScreenOwner {
    /// Short lower-case label suitable for a `tracing` field value.
    pub fn as_str(self) -> &'static str {
        match self {
            ScreenOwner::Shell => "shell",
            ScreenOwner::PrimaryScreenApp => "primary-app",
            ScreenOwner::AltScreenApp => "alt-app",
            ScreenOwner::PrimaryScreenExitPending => "primary-exit-pending",
        }
    }
}

impl std::fmt::Display for ScreenOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle phase of a deferred primary-screen exit, for diagnostics.
///
/// Mirrors the implicit state of `Option<PendingPrimaryScreenExit>` plus the
/// settle-delay comparison, surfaced as a named enum so logs can say
/// `settle_state=settling` rather than encoding the timing by hand.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettleState {
    /// No primary-screen exit is pending.
    Idle,
    /// Exit is deferred and the 200ms idle window has not yet elapsed.
    PendingDeferred,
    /// Exit is deferred and enough idle time has elapsed — the next
    /// `settle_primary_screen_exit_if_idle` call will finalize it.
    Settling,
}

impl SettleState {
    pub fn as_str(self) -> &'static str {
        match self {
            SettleState::Idle => "idle",
            SettleState::PendingDeferred => "pending-deferred",
            SettleState::Settling => "settling",
        }
    }
}

impl std::fmt::Display for SettleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CapabilityFlags {
    /// OSC 133 is also used by foreground applications as semantic zones.
    /// After Weft's tagged hook is observed, reject untagged repaint markers
    /// so they cannot drive the outer shell's block state machine.
    pub(in crate::vt) fn accepts_shell_marker(&mut self, tagged: bool) -> bool {
        if tagged {
            self.tagged_shell_markers_seen = true;
        }
        tagged || !self.tagged_shell_markers_seen
    }

    /// Classify who owns the viewport, given the current shell phase.
    ///
    /// Phase is passed in (rather than stored here) because phase lives on
    /// `BlockTracker`, which is a peer of `CapabilityFlags` on `Terminal`, not
    /// a child. Keeping it out avoids a redundant copy and a second source of
    /// truth.
    //
    // The three readers below (`screen_owner`, `settle_state`,
    // `history_snapshot_due`) are consumed by the diagnostic tracing layer
    // (R1 task 2 + R1-5 — `session_id`/`screen_owner`/`settle_state`/
    // `history_snapshot_due` trace fields). `screen_owner` and `settle_state`
    // are wired via `Terminal` accessors; `history_snapshot_due` is wired via
    // `Terminal::history_snapshot_due()` (R1-5, v1.2.3).
    pub(in crate::vt) fn screen_owner(&self, phase: ShellPhase) -> ScreenOwner {
        if self.alt_active {
            return ScreenOwner::AltScreenApp;
        }
        if self.primary_screen_exit.is_some() {
            return ScreenOwner::PrimaryScreenExitPending;
        }
        if phase == ShellPhase::CommandExecuting && self.primary_screen_cursor_ops >= 2 {
            ScreenOwner::PrimaryScreenApp
        } else {
            ScreenOwner::Shell
        }
    }

    /// Snapshot the deferred-exit lifecycle phase at `now`.
    pub(in crate::vt) fn settle_state(&self, now: Instant) -> SettleState {
        match &self.primary_screen_exit {
            None => SettleState::Idle,
            Some(pending) => {
                if now.saturating_duration_since(pending.last_activity)
                    >= PRIMARY_SCREEN_EXIT_SETTLE_DELAY
                {
                    SettleState::Settling
                } else {
                    SettleState::PendingDeferred
                }
            }
        }
    }

    /// Whether the history-snapshot rate-limit window has elapsed since the
    /// last refresh. `false` when history browsing is inactive.
    ///
    /// R1-5: consumed by the diagnostic tracing layer via
    /// `Terminal::history_snapshot_due()` to surface whether a snapshot
    /// refresh is due at this instant. The actual refresh is driven by
    /// `refresh_primary_history_snapshot_at` (see `screen_exit.rs`), which
    /// performs the same rate-limit check inline; this helper exists purely
    /// for observability so the trace can answer "why didn't a snapshot
    /// fire at moment X?" without reconstructing the state by hand.
    pub(in crate::vt) fn history_snapshot_due(&self, now: Instant) -> bool {
        self.primary_history_view
            && now.saturating_duration_since(self.primary_history_snapshot_at.unwrap_or(now))
                >= PRIMARY_HISTORY_SNAPSHOT_INTERVAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::ShellPhase;
    use std::time::Duration;

    fn flags() -> CapabilityFlags {
        CapabilityFlags::default()
    }

    #[test]
    fn screen_owner_defaults_to_shell_at_prompt() {
        let f = flags();
        assert_eq!(f.screen_owner(ShellPhase::AtPrompt), ScreenOwner::Shell);
        assert_eq!(
            f.screen_owner(ShellPhase::NotIntegrated),
            ScreenOwner::Shell
        );
    }

    #[test]
    fn screen_owner_requires_two_cursor_ops_during_execution() {
        let mut f = flags();
        // CommandExecuting alone is not enough — cursor_ops must reach 2 so a
        // plain shell command that happens to emit a CUP isn't misclassified.
        assert_eq!(
            f.screen_owner(ShellPhase::CommandExecuting),
            ScreenOwner::Shell
        );
        f.primary_screen_cursor_ops = 1;
        assert_eq!(
            f.screen_owner(ShellPhase::CommandExecuting),
            ScreenOwner::Shell
        );
        f.primary_screen_cursor_ops = 2;
        assert_eq!(
            f.screen_owner(ShellPhase::CommandExecuting),
            ScreenOwner::PrimaryScreenApp
        );
    }

    #[test]
    fn alt_screen_takes_precedence_over_primary_exit() {
        let mut f = flags();
        f.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code: None,
            last_activity: Instant::now(),
        });
        f.alt_active = true;
        // A pending exit is only meaningful on the primary screen; if the alt
        // screen is somehow swapped in, report the alt owner.
        assert_eq!(
            f.screen_owner(ShellPhase::CommandExecuting),
            ScreenOwner::AltScreenApp
        );
    }

    #[test]
    fn exit_pending_shown_when_not_alt() {
        let mut f = flags();
        f.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code: Some(0),
            last_activity: Instant::now(),
        });
        assert_eq!(
            f.screen_owner(ShellPhase::CommandExecuting),
            ScreenOwner::PrimaryScreenExitPending
        );
    }

    #[test]
    fn settle_state_transitions_with_idle_window() {
        let mut f = flags();
        assert_eq!(f.settle_state(Instant::now()), SettleState::Idle);

        let start = Instant::now();
        f.primary_screen_exit = Some(PendingPrimaryScreenExit {
            exit_code: None,
            last_activity: start,
        });
        // Just before the window elapses — still pending.
        let within = start + Duration::from_millis(199);
        assert_eq!(f.settle_state(within), SettleState::PendingDeferred);
        // At/after the window — ready to settle.
        let ready = start + PRIMARY_SCREEN_EXIT_SETTLE_DELAY;
        assert_eq!(f.settle_state(ready), SettleState::Settling);
    }

    #[test]
    fn history_snapshot_due_respects_rate_limit() {
        let mut f = flags();
        // Inactive history view is never due.
        assert!(!f.history_snapshot_due(Instant::now()));

        let start = Instant::now();
        f.primary_history_view = true;
        f.primary_history_snapshot_at = Some(start);
        // Within the interval — not due yet.
        let within = start + Duration::from_millis(49);
        assert!(!f.history_snapshot_due(within));
        // After the interval — due.
        let ready = start + PRIMARY_HISTORY_SNAPSHOT_INTERVAL;
        assert!(f.history_snapshot_due(ready));
    }

    #[test]
    fn default_has_no_live_state() {
        let f = flags();
        assert!(!f.alt_active);
        assert_eq!(f.mouse_protocol, MouseProtocol::Off);
        assert!(!f.sgr_mouse);
        assert!(!f.app_cursor_keys);
        assert_eq!(f.primary_screen_cursor_ops, 0);
        assert!(f.primary_screen_exit.is_none());
        assert!(f.primary_screen_interrupt_capture.is_none());
        assert!(!f.primary_history_view);
    }
}
