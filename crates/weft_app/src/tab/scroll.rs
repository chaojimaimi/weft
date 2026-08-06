use super::Tab;

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
        let changed = self.terminal.as_mut().is_some_and(|terminal| {
            let changed = terminal.primary_history_view();
            terminal.set_primary_history_view(false);
            changed
        });
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

    fn sync_primary_history_view(&mut self) {
        // v1.3: snapshot the anchor offset before borrowing `terminal` so
        // the disjoint-field borrow through `DerefMut` doesn't conflict.
        // `block_scroll_anchor` and `terminal` are both on the active pane;
        // reading the offset first releases the immutable pane borrow before
        // `&mut pane.terminal` is taken.
        let detached = !matches!(self.block_scroll_anchor, BlockScrollAnchor::FollowBottom);
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
            let browsing = terminal.primary_screen_app_active() && detached;
            let changed = terminal.primary_history_view() != browsing;
            terminal.set_primary_history_view(browsing);
            changed
        } else {
            false
        };
        if changed {
            self.reset_primary_history_refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
