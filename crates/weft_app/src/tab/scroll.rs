use super::Tab;
use weft_core::persistence::TabSnapshot;

/// Block-view scroll anchor (R2-1).
///
/// Replaces the raw `usize` offset so the snap-to-bottom caller can
/// distinguish "user is following the live tail" from "user has detached
/// to read history". The numeric offset is recovered via [`offset_value`](Self::offset_value)
/// for the external readers and SQLite persistence, which keeps the original
/// `INTEGER` column — `0 = FollowBottom`, `n > 0 = FixedDocumentRow(n)`.
///
/// `FixedWithinLiveBlock` is intentionally omitted: the current follow logic
/// (`sync_primary_history_view`) treats any non-zero offset identically
/// (flips `primary_history_view`), so the two variants would be behaviourally
/// equivalent. Adding it later only makes sense if follow logic gains a
/// live-block-aware branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockScrollAnchor {
    /// User is following the live tail. New output snaps the view to the bottom.
    FollowBottom,
    /// User has detached to a fixed document row. New output must not move the view.
    FixedDocumentRow(usize),
}

impl BlockScrollAnchor {
    pub(crate) fn offset_value(self) -> usize {
        match self {
            Self::FollowBottom => 0,
            Self::FixedDocumentRow(n) => n,
        }
    }

    fn from_offset(n: usize) -> Self {
        if n == 0 {
            Self::FollowBottom
        } else {
            Self::FixedDocumentRow(n)
        }
    }
}

/// v1.10.20: pure decision for whether the primary history snapshot view
/// stays active. Browsing is active while detached (user scrolled up); a
/// selection drag DELAYS the exit — the user may be selecting across the
/// scrolled boundary, and dropping the view mid-drag would orphan the
/// selection (改动 2). A completed block-view selection (migrated drag or a
/// plain block drag) also delays the exit — it must stay visible and
/// copyable in the snapshot view (S1); the view returns to the live grid on
/// the next sync after the selection is cleared (click clears → sync →
/// exit). A plain grid drag must NOT force the history view open, so the
/// delay only applies when it is already active.
pub(crate) fn primary_history_browsing(
    primary_app_active: bool,
    detached: bool,
    selecting: bool,
    history_view_active: bool,
    block_selection_active: bool,
) -> bool {
    primary_app_active
        && (detached || ((selecting || block_selection_active) && history_view_active))
}

/// v1.10.21: whether finishing the drained blocks should snap the view to
/// the live tail. A non-empty drain snaps UNLESS the user is browsing
/// primary history or the alt-screen history peek — both are
/// user-initiated views that must not be yanked by background output
/// (Warp: blocks and the alt screen are mutually exclusive views).
pub(crate) fn block_completion_should_snap(
    terminal: &weft_core::vt::Terminal,
    drained: &[weft_core::blocks::Block],
) -> bool {
    !drained.is_empty()
        && !terminal.primary_history_view()
        && !terminal.is_alt_screen_history_peek()
}

impl Tab {
    pub fn block_scroll(&self) -> usize {
        self.block_scroll_anchor.offset_value()
    }

    pub(crate) fn block_scroll_position(&self) -> f32 {
        self.block_scroll_anchor.offset_value() as f32 + self.block_scroll_fraction
    }

    pub(crate) fn block_scroll_anchor(&self) -> BlockScrollAnchor {
        self.block_scroll_anchor
    }

    pub fn enter_primary_history_if_active(&mut self) -> bool {
        // Does not touch block_scroll_anchor — only flips primary_history_view
        // so the renderer switches to the history snapshot. The anchor stays
        // whatever the user last set (FollowBottom until they scroll).
        let active = self
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::primary_screen_app_active);
        if active {
            let terminal = self
                .terminal
                .as_mut()
                .expect("active primary screen has a terminal");
            let entering = !terminal.primary_history_view();
            terminal.set_primary_history_view(true);
            if entering {
                self.reset_primary_history_refresh();
            }
        }
        active
    }

    pub fn set_block_scroll(&mut self, offset: usize) {
        self.block_scroll_anchor = BlockScrollAnchor::from_offset(offset);
        self.block_scroll_fraction = 0.0;
        self.sync_primary_history_view();
    }

    /// v1.10.24 (FIX_RECOVERY_DESIGN_ALIGNMENT Fix 2): Attach a persisted
    /// [`TabSnapshot`] to the active pane of a recovered tab and apply its
    /// block-view scroll offset.
    ///
    /// The recovery Restore path rebuilds tabs from the recovery YAML
    /// snapshot but leaves `restored_snapshot = None` and never applied the
    /// persisted `block_scroll_offset`. This fills both gaps with the SAME
    /// primitives the SQLite restore path uses: the snapshot is attached
    /// (as `attach_recovery_tab_snapshots` already did) and the offset is
    /// applied via [`Tab::set_block_scroll`] — the scroll half of
    /// [`Tab::restore_from_snapshot`]. No new scroll logic.
    ///
    /// v1.10.24 B1: `restored_snapshot` is guaranteed to be `None` here on
    /// the recovery path — `set_restored_cwd_fallback` no longer writes a
    /// stub snapshot (it writes `restored_cwd` instead), so the `is_some()`
    /// guard below cannot skip the real attach anymore. The v1.8.9 "attach
    /// was always a no-op" bug is fixed; see `Pane::restored_cwd`.
    ///
    /// Returns `true` when the snapshot was newly attached. A zero offset is
    /// skipped — `FollowBottom` is already the fresh-tab default, matching
    /// the SQLite path's `from_offset(0)` behavior.
    pub(crate) fn attach_recovery_snapshot(&mut self, snap: &TabSnapshot) -> bool {
        if self.restored_snapshot.is_some() {
            return false;
        }
        self.restored_snapshot = Some(snap.clone());
        if snap.block_scroll_offset != 0 {
            self.set_block_scroll(snap.block_scroll_offset);
        }
        true
    }

    pub(crate) fn scroll_block_fractional(&mut self, delta_rows: f32, max_scroll: usize) {
        let position = (self.block_scroll_position() + delta_rows).clamp(0.0, max_scroll as f32);
        let whole = position.floor() as usize;
        self.block_scroll_fraction = position - whole as f32;
        self.block_scroll_anchor = if position <= f32::EPSILON {
            BlockScrollAnchor::FollowBottom
        } else {
            BlockScrollAnchor::FixedDocumentRow(whole)
        };
        self.sync_primary_history_view();
    }

    pub fn snap_to_bottom(&mut self) {
        self.block_scroll_anchor = BlockScrollAnchor::FollowBottom;
        self.block_scroll_fraction = 0.0;
        // v1.10.21: snapshot the peek flag BEFORE the mutable terminal
        // borrow — the entry gate is a sibling Tab field and the closure
        // below can't touch it while `self.terminal` is borrowed through
        // DerefMut (disjoint-field borrows don't work through Deref).
        let peek_was_active = self
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
        let changed = self.terminal.as_mut().is_some_and(|terminal| {
            let changed = terminal.primary_history_view();
            terminal.set_primary_history_view(false);
            // v1.10.12: also leave any alt-screen history peek.
            terminal.set_alt_screen_history_peek(false);
            changed
        });
        // v1.10.21: arm the re-entry lockout only on a REAL exit (the flag
        // was set) — an unconditional note_exit would lock out peek entry
        // after every keystroke/snap, even with no peek.
        if peek_was_active {
            self.alt_peek_gate.note_exit();
        }
        if changed {
            self.reset_primary_history_refresh();
        }
    }

    pub fn scroll_up_by(&mut self, rows: usize) {
        self.set_block_scroll(self.block_scroll_anchor.offset_value().saturating_add(rows));
    }

    pub fn scroll_down_by(&mut self, rows: usize) {
        self.set_block_scroll(self.block_scroll_anchor.offset_value().saturating_sub(rows));
    }

    pub fn clamp_block_scroll(&mut self, max_scroll: usize) {
        self.set_block_scroll(self.block_scroll_anchor.offset_value().min(max_scroll));
    }

    /// v1.10.20: made `pub(crate)` — mouse release re-syncs after the
    /// deferred history-view exit (改动 2).
    pub(crate) fn sync_primary_history_view(&mut self) {
        // v1.3: snapshot the anchor offset before borrowing `terminal` so
        // the disjoint-field borrow through `DerefMut` doesn't conflict.
        // `block_scroll_anchor` and `terminal` are both on the active pane;
        // reading the offset first releases the immutable pane borrow before
        // `&mut pane.terminal` is taken.
        let detached = !matches!(self.block_scroll_anchor, BlockScrollAnchor::FollowBottom);
        // v1.10.21: snapshot the peek flag before the terminal borrow so the
        // entry gate (sibling Tab field) can be armed afterwards — see
        // snap_to_bottom for the DerefMut borrow constraint.
        let peek_was_active = self
            .active()
            .terminal
            .as_ref()
            .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
        let pane = self.active_mut();
        let changed = if let Some(terminal) = &mut pane.terminal {
            // v1.10.6: history browsing is ACTIVE ONLY while the user is
            // detached (anchor != FollowBottom). The previous
            // `(detached || primary_history_view)` kept history browsing
            // sticky forever: once the user scrolled up (entering the
            // detached BlockView snapshot), scrolling back to the tail
            // (FollowBottom) left `primary_history_view` true, so every
            // subsequent keystroke rendered the TUI in the BlockView —
            // whose IME support is a fallback (no preedit, imprecise caret).
            // The user expects scrolling back to the tail to return to the
            // live grid (native cursor/IME/color), matching Warp.
            //
            // v1.10.20 (改动 2): a selection drag delays that exit — the
            // user may be selecting across the scrolled boundary. Mouse
            // release re-syncs (`mouse_controller::handle_mouse_release`),
            // so back at FollowBottom the live grid returns then. v1.10.20
            // (S1): a completed block-view selection also delays the exit
            // (its grid half was already dropped by the migration), keeping
            // the snapshot view up until the selection is cleared — the
            // clear paths re-sync (mouse_press_controller), releasing the
            // view back to the live grid.
            let browsing = primary_history_browsing(
                terminal.primary_screen_app_active(),
                detached,
                pane.selection_handler.selecting,
                terminal.primary_history_view(),
                pane.selection_handler.block_view_selection.is_some(),
            );
            let changed = terminal.primary_history_view() != browsing;
            terminal.set_primary_history_view(browsing);
            // v1.10.12: scrolling back to the bottom also exits the alt-screen
            // history peek, returning the viewport to the live TUI grid.
            // v1.10.21: the re-entry lockout is armed below, after the
            // terminal borrow ends (gate is a sibling Tab field).
            if !detached {
                terminal.set_alt_screen_history_peek(false);
            }
            changed
        } else {
            false
        };
        // v1.10.21: arm the re-entry lockout only when the sync actually
        // cleared an active peek (`!detached && peek_was_active`) — see
        // snap_to_bottom for why the note is conditional.
        if !detached && peek_was_active {
            self.alt_peek_gate.note_exit();
        }
        if changed {
            self.reset_primary_history_refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v1.10.20 改动 2 + S1 state machine: an active selection drag delays
    /// the history-view exit, but never forces it open for a plain grid
    /// drag; a completed block-view selection keeps the snapshot view up
    /// (visible/copyable) until the selection is cleared.
    #[test]
    fn primary_history_browsing_delays_exit_only_while_selecting() {
        // Detached browsing is unaffected by selection state.
        assert!(primary_history_browsing(true, true, false, true, false));
        assert!(primary_history_browsing(true, true, true, true, true));
        // FollowBottom + no drag → exit to the live grid.
        assert!(!primary_history_browsing(true, false, false, true, false));
        // FollowBottom + drag in the history view → delayed exit.
        assert!(primary_history_browsing(true, false, true, true, false));
        // FollowBottom + completed block selection (migrated drag, S1):
        // the selection must stay visible and copyable → delayed exit.
        assert!(primary_history_browsing(true, false, false, true, true));
        // Same, mid-drag.
        assert!(primary_history_browsing(true, false, true, true, true));
        // The delay never forces the history view open when inactive.
        assert!(!primary_history_browsing(true, false, true, false, true));
        assert!(!primary_history_browsing(true, false, false, false, true));
        // Selection cleared → the next sync releases the view.
        assert!(!primary_history_browsing(true, false, false, true, false));
        // Non-primary screens never browse.
        assert!(!primary_history_browsing(false, true, false, true, false));
        assert!(!primary_history_browsing(false, false, true, false, true));
    }

    #[test]
    fn anchor_offset_round_trip() {
        assert_eq!(
            BlockScrollAnchor::from_offset(0),
            BlockScrollAnchor::FollowBottom
        );
        assert_eq!(
            BlockScrollAnchor::from_offset(7),
            BlockScrollAnchor::FixedDocumentRow(7)
        );
        assert_eq!(BlockScrollAnchor::FollowBottom.offset_value(), 0);
        assert_eq!(BlockScrollAnchor::FixedDocumentRow(7).offset_value(), 7);
    }

    /// R2-1 regression: scrolling up from FollowBottom transitions the
    /// anchor to FixedDocumentRow so the redraw_controller snap guard
    /// (`matches!(anchor, FollowBottom)`) stops snapping on new output.
    #[test]
    fn scroll_up_transitions_to_fixed_anchor() {
        let mut tab = Tab::empty();
        assert_eq!(tab.block_scroll_anchor(), BlockScrollAnchor::FollowBottom);

        tab.scroll_up_by(5);
        assert_eq!(
            tab.block_scroll_anchor(),
            BlockScrollAnchor::FixedDocumentRow(5)
        );
        assert_eq!(tab.block_scroll(), 5);
    }

    /// R2-1: snap_to_bottom restores FollowBottom so follow-on-output resumes.
    #[test]
    fn snap_to_bottom_restores_follow_anchor() {
        let mut tab = Tab::empty();
        tab.scroll_up_by(3);
        assert_eq!(
            tab.block_scroll_anchor(),
            BlockScrollAnchor::FixedDocumentRow(3)
        );

        tab.snap_to_bottom();
        assert_eq!(tab.block_scroll_anchor(), BlockScrollAnchor::FollowBottom);
        assert_eq!(tab.block_scroll(), 0);
    }

    /// R2-1: scroll_down_by back to 0 transitions through from_offset,
    /// landing on FollowBottom (not FixedDocumentRow(0)).
    #[test]
    fn scroll_down_to_zero_yields_follow_bottom() {
        let mut tab = Tab::empty();
        tab.scroll_up_by(4);
        tab.scroll_down_by(4);
        assert_eq!(tab.block_scroll_anchor(), BlockScrollAnchor::FollowBottom);
    }

    #[test]
    fn fractional_scroll_preserves_sub_row_motion_and_anchor_state() {
        let mut tab = Tab::empty();
        tab.scroll_block_fractional(0.25, 10);
        assert_eq!(tab.block_scroll(), 0);
        assert!((tab.block_scroll_position() - 0.25).abs() < f32::EPSILON);
        assert!(matches!(
            tab.block_scroll_anchor(),
            BlockScrollAnchor::FixedDocumentRow(0)
        ));

        tab.scroll_block_fractional(-0.25, 10);
        assert_eq!(tab.block_scroll_anchor(), BlockScrollAnchor::FollowBottom);
        assert_eq!(tab.block_scroll_position(), 0.0);
    }

    /// R2-1 bug scenario: even when `should_follow_running_output` would
    /// return true (CommandExecuting + not in primary history view), the
    /// anchor guard prevents snapping. This is the exact condition the
    /// redraw_controller checks before calling `snap_to_bottom()`.
    #[test]
    fn fixed_anchor_blocks_snap_guard_even_when_follow_would_fire() {
        let mut tab = Tab::empty();
        tab.scroll_up_by(2);

        // Simulate the redraw_controller guard:
        // should_follow_running_output(CommandExecuting, false) == true,
        // BUT anchor is Fixed → combined condition is false → no snap.
        let phase = weft_core::blocks::ShellPhase::CommandExecuting;
        let primary_history_view = false;
        let would_follow =
            crate::block_component::should_follow_running_output(phase, primary_history_view);
        assert!(would_follow, "follow logic alone would snap");

        let guard_allows_snap =
            matches!(tab.block_scroll_anchor(), BlockScrollAnchor::FollowBottom);
        assert!(
            !guard_allows_snap,
            "anchor guard must block snap when user has scrolled up"
        );

        // Anchor is unchanged — no snap_to_bottom was called.
        assert_eq!(
            tab.block_scroll_anchor(),
            BlockScrollAnchor::FixedDocumentRow(2)
        );
    }

    /// v1.10.21: block completion snaps unless the user is browsing primary
    /// history or the alt-screen history peek.
    #[test]
    fn block_completion_snap_respects_user_browsing_views() {
        fn drained_block() -> weft_core::blocks::Block {
            weft_core::blocks::Block {
                id: weft_core::blocks::BlockId(1),
                command: "cargo test".into(),
                cwd: None,
                output: "ok".into(),
                styled_output: None,
                exit_code: Some(0),
                started_at: std::time::SystemTime::now(),
                finished_at: None,
                collapsed: false,
            }
        }
        let t = weft_core::vt::Terminal::new(24, 80);
        assert!(
            !block_completion_should_snap(&t, &[]),
            "empty drain never snaps"
        );

        let mut t = t;
        assert!(block_completion_should_snap(&t, &[drained_block()]));
        t.set_primary_history_view(true);
        assert!(
            !block_completion_should_snap(&t, &[drained_block()]),
            "primary history browsing suppresses the snap"
        );
        t.set_primary_history_view(false);
        t.set_alt_screen_history_peek(true);
        assert!(
            !block_completion_should_snap(&t, &[drained_block()]),
            "an active alt-screen history peek suppresses the snap"
        );
        t.set_alt_screen_history_peek(false);
        assert!(block_completion_should_snap(&t, &[drained_block()]));
    }

    /// v1.10.24 B1 (FIX_RECOVERY_DESIGN_ALIGNMENT) regression: the
    /// workspace-restore cwd fallback must NOT block a later real snapshot
    /// attach. Before the fix, `set_restored_cwd_fallback` wrote a full stub
    /// `restored_snapshot`, so `attach_recovery_snapshot`'s `is_some()` guard
    /// skipped every real attach (v1.8.9 no-op): block_ids were never
    /// injected and `block_scroll_offset` never applied.
    #[test]
    fn cwd_fallback_does_not_block_snapshot_attach() {
        let mut tab = Tab::empty();
        // Real Restore order: workspace restore sets the cwd fallback first…
        tab.set_restored_cwd_fallback(Some("/saved".into()));
        assert!(
            tab.restored_snapshot.is_none(),
            "the cwd fallback must no longer write a stub snapshot"
        );
        assert_eq!(
            tab.launch_cwd(),
            Some("/saved"),
            "the fallback must stay visible to launch_cwd"
        );

        // …then attach_recovery_tab_snapshots attaches the persisted
        // snapshot. This must now succeed.
        let snap = TabSnapshot {
            position: 0,
            active: false,
            cwd: Some("/saved".into()),
            block_scroll_offset: 9,
            editor_buffer: String::new(),
            shell_phase: "AtPrompt".to_string(),
            block_ids: vec![7, 8],
        };
        assert!(
            tab.attach_recovery_snapshot(&snap),
            "attach must succeed after the cwd fallback (v1.8.9 no-op regression)"
        );
        assert_eq!(
            tab.restored_snapshot.as_ref().unwrap().block_ids,
            vec![7, 8],
            "attach must inject the persisted block_ids"
        );
        assert_eq!(
            tab.block_scroll_anchor(),
            BlockScrollAnchor::FixedDocumentRow(9),
            "attach must apply the persisted block_scroll_offset"
        );
    }
}
