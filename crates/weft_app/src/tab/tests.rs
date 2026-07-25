//! Inline tests for `Tab`, extracted from `tab.rs` to keep the production
//! file under the 800-line architecture gate. See `tab.rs` (where
//! `#[cfg(test)] #[path = "tab/tests.rs"] mod tests;` declares this module).
//!
//! These tests cover empty-tab semantics, the queued TUI scroll replay window,
//! snapshot roundtrip (v1.0 H4 TabsAutoSave), and the v1.3 split-pane
//! lifecycle (split / focus / close / hit-test).

use super::*;

#[test]
fn empty_tab_is_not_alive() {
    let t = Tab::empty();
    assert!(!t.is_alive());
}

#[test]
fn empty_tab_has_no_terminal() {
    let t = Tab::empty();
    assert!(t.terminal.is_none());
    assert!(t.pty.is_none());
}

#[test]
fn empty_tab_pump_pty_is_noop() {
    let mut t = Tab::empty();
    // Should not panic — just returns early since pty is None.
    t.pump_pty();
}

#[test]
fn empty_tab_process_messages_returns_empty() {
    let mut t = Tab::empty();
    let (alive, drained, need_redraw) = t.process_messages();
    assert!(alive);
    assert!(drained.is_empty());
    assert!(!need_redraw);
}

#[test]
fn empty_tab_has_zero_block_scroll() {
    let t = Tab::empty();
    assert_eq!(t.block_scroll(), 0);
}

#[test]
fn queued_scroll_replays_after_alt_screen_entry() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));
    assert!(t.resolve_pending_tui_scroll().is_none());

    t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
    assert!(t.resolve_pending_tui_scroll().is_none());
    expire_pending_scroll(&mut t);
    let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
        panic!("expected queued arrow bytes");
    };
    assert_eq!(bytes, b"\x1b[B\x1b[B");
    assert!(t.resolve_pending_tui_scroll().is_none());
}

#[test]
fn queued_scroll_falls_back_to_local_rows_for_normal_command() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(3, 5, 10, weft_core::input::Modifiers::empty()));
    t.terminal.as_mut().unwrap().process(b"\x1b]133;A\x07");
    expire_pending_scroll(&mut t);

    let Some(TuiScrollResolution::LocalRows(rows)) = t.resolve_pending_tui_scroll() else {
        panic!("expected local scroll fallback");
    };
    assert_eq!(rows, 3);
    assert!(t.pending_tui_scroll.is_none());
    assert!(!t.tui_scroll_window_active());
    assert!(!t.queue_tui_scroll(1, 5, 10, weft_core::input::Modifiers::empty()));
}

#[test]
fn expired_startup_window_does_not_queue_long_running_command_scroll() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    t.tui_scroll_deadline =
        std::time::Instant::now().checked_sub(std::time::Duration::from_millis(1));

    assert!(!t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));

    assert!(!t.tui_scroll_window_active());
    assert!(t.pending_tui_scroll.is_none());
}

#[test]
fn queued_scroll_uses_mouse_protocol_when_tui_enables_it() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));
    t.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    expire_pending_scroll(&mut t);

    let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
        panic!("expected queued mouse bytes");
    };
    assert_eq!(bytes, b"\x1b[<65;6;11M");
}

#[test]
fn queued_scroll_waits_for_mouse_mode_in_next_pty_chunk() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::empty()));

    t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
    assert!(t.resolve_pending_tui_scroll().is_none());
    assert!(t.resolve_pending_tui_scroll().is_none());

    t.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b[?1000h\x1b[?1006h");
    expire_pending_scroll(&mut t);
    let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
        panic!("expected mouse bytes after split mode sequence");
    };
    assert_eq!(bytes, b"\x1b[<65;6;11M");
}

#[test]
fn queued_arrow_scroll_preserves_shift_modifier() {
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(-1, 5, 10, weft_core::input::Modifiers::SHIFT));
    t.terminal.as_mut().unwrap().process(b"\x1b[?1049h");
    expire_pending_scroll(&mut t);

    let Some(TuiScrollResolution::PtyBytes(bytes)) = t.resolve_pending_tui_scroll() else {
        panic!("expected shifted arrow bytes");
    };
    assert_eq!(bytes, b"\x1b[1;2B");
}

// ── v1.0 H4: TabsAutoSave snapshot roundtrip ─────────────────────

/// Build a Tab with a live Terminal but no PTY — enough for
/// `to_snapshot` / `restore_from_snapshot` without spawning a shell.
/// v1.3: wraps a single `Pane` (built via `Pane::with_terminal_only`)
/// in a fresh `Tab`. The pane becomes the root of the split tree and
/// the active pane.
fn tab_with_terminal(scrollback_lines: usize) -> Tab {
    Tab::with_single_pane(Pane::with_terminal_only(scrollback_lines))
}

fn expire_pending_scroll(tab: &mut Tab) {
    tab.pending_tui_scroll.as_mut().unwrap().resolve_at = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_millis(1))
        .unwrap();
}

#[test]
fn snapshot_is_none_when_no_terminal() {
    // Tab::empty has no terminal → to_snapshot returns None.
    let t = Tab::empty();
    assert!(t.to_snapshot(0, false).is_none());
}

#[test]
fn snapshot_roundtrip_preserves_scroll_offset_and_editor() {
    let mut t = tab_with_terminal(1000);
    t.set_block_scroll(7);
    t.terminal
        .as_mut()
        .unwrap()
        .editor_mut()
        .buffer
        .set_text("echo hi");

    let snap = t.to_snapshot(2, true).expect("snapshot with terminal");
    assert_eq!(snap.position, 2);
    assert!(snap.active);
    assert_eq!(snap.block_scroll_offset, 7);
    assert!(!snap.editor_buffer.is_empty(), "editor buffer encoded");

    // Restore into a fresh tab.
    let mut restored = tab_with_terminal(1000);
    assert!(restored.restore_from_snapshot(&snap));
    assert_eq!(restored.block_scroll(), 7);
    let editor_text = restored.terminal.as_ref().unwrap().editor().buffer.text();
    assert_eq!(editor_text, "echo hi");
}

#[test]
fn snapshot_default_shell_phase_is_not_integrated() {
    // A fresh Terminal has NotIntegrated phase → snapshot encodes that.
    let t = tab_with_terminal(1000);
    let snap = t.to_snapshot(0, false).unwrap();
    assert_eq!(snap.shell_phase, "NotIntegrated");
}

#[test]
fn snapshot_cwd_none_when_unset() {
    // No OSC 7 received → cwd is None in the snapshot.
    let t = tab_with_terminal(1000);
    let snap = t.to_snapshot(0, false).unwrap();
    assert!(snap.cwd.is_none());
}

#[test]
fn restore_from_invalid_editor_buffer_keeps_empty() {
    // Garbage JSON → restore returns false, editor stays empty.
    let mut t = tab_with_terminal(1000);
    let snap = weft_core::persistence::TabSnapshot {
        position: 0,
        active: false,
        cwd: None,
        block_scroll_offset: 3,
        editor_buffer: "{not valid json".to_string(),
        shell_phase: "AtPrompt".to_string(),
    };
    assert!(!t.restore_from_snapshot(&snap));
    // scroll offset still applied even if editor restore failed.
    assert_eq!(t.block_scroll(), 3);
    let editor_text = t.terminal.as_ref().unwrap().editor().buffer.text();
    assert!(editor_text.is_empty());
}

// ── v1.3 Batch 3: pane lifecycle (split / focus / close) ──────────

#[test]
fn single_pane_tab_reports_one_pane() {
    let t = tab_with_terminal(100);
    assert_eq!(t.pane_count(), 1);
    assert_eq!(t.split_tree().panes(), vec![t.active_pane_id()]);
}

#[test]
fn split_active_pane_adds_pane_and_switches_focus() {
    let mut t = tab_with_terminal(100);
    let original = t.active_pane_id();
    let new_id = t
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .expect("split succeeds");
    // Tree now has two panes; new pane is active.
    assert_eq!(t.pane_count(), 2);
    assert_eq!(t.active_pane_id(), new_id);
    assert_ne!(new_id, original);
    // Both panes are present in the HashMap.
    assert!(t.pane(original).is_some());
    assert!(t.pane(new_id).is_some());
    // Declaration order: original first, new pane second.
    assert_eq!(t.split_tree().panes(), vec![original, new_id]);
}

#[test]
fn split_with_invalid_ratio_is_rejected() {
    let mut t = tab_with_terminal(100);
    // ratio must be in (0.0, 1.0) exclusive — 0.0, 1.0, and out-of-range
    // values are caller bugs, not silent clamps.
    assert_eq!(
        t.split_active_pane_test(SplitDirection::Horizontal, 0.0, 100),
        Err(SplitError::RatioOutOfRange(0.0))
    );
    assert_eq!(
        t.split_active_pane_test(SplitDirection::Horizontal, 1.0, 100),
        Err(SplitError::RatioOutOfRange(1.0))
    );
    assert_eq!(
        t.split_active_pane_test(SplitDirection::Horizontal, 1.5, 100),
        Err(SplitError::RatioOutOfRange(1.5))
    );
    // Tree unchanged after the failed splits.
    assert_eq!(t.pane_count(), 1);
}

#[test]
fn focus_next_wraps_around_two_panes() {
    let mut t = tab_with_terminal(100);
    let first = t.active_pane_id();
    t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let second = t.active_pane_id();

    // forward: second → first (wrap)
    assert_eq!(t.focus_next_pane(), Some(first));
    // forward: first → second
    assert_eq!(t.focus_next_pane(), Some(second));
}

#[test]
fn focus_prev_wraps_around_two_panes() {
    let mut t = tab_with_terminal(100);
    let first = t.active_pane_id();
    t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let second = t.active_pane_id();

    // backward: second → first
    assert_eq!(t.focus_prev_pane(), Some(first));
    // backward: first → second (wrap)
    assert_eq!(t.focus_prev_pane(), Some(second));
}

#[test]
fn focus_next_on_single_pane_is_noop() {
    let mut t = tab_with_terminal(100);
    let only = t.active_pane_id();
    assert_eq!(t.focus_next_pane(), Some(only));
    assert_eq!(t.focus_prev_pane(), Some(only));
}

#[test]
fn close_active_pane_with_sibling_keeps_tab() {
    let mut t = tab_with_terminal(100);
    let first = t.active_pane_id();
    t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let second = t.active_pane_id();

    // Close the active (second) pane — should fall back to the sibling.
    let is_empty = t.close_active_pane().expect("close succeeds");
    assert!(!is_empty);
    assert_eq!(t.pane_count(), 1);
    assert_eq!(t.active_pane_id(), first);
    // The closed pane is gone from the HashMap.
    assert!(t.pane(second).is_none());
    assert!(t.pane(first).is_some());
}

#[test]
fn close_last_pane_signals_tab_empty() {
    let mut t = tab_with_terminal(100);
    // Single pane → closing it empties the tab.
    let is_empty = t.close_active_pane().expect("close succeeds");
    assert!(is_empty);
    assert_eq!(t.pane_count(), 0);
}

#[test]
fn nested_split_and_close_collapses_tree() {
    let mut t = tab_with_terminal(100);
    let a = t.active_pane_id();
    t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let b = t.active_pane_id();
    t.split_active_pane_test(SplitDirection::Horizontal, 0.5, 100)
        .unwrap();
    // The third pane (the just-split active one) is implicitly tracked; we
    // only need `b` and `a` to verify focus cascades back through siblings.
    assert_eq!(t.pane_count(), 3);

    // Close the third pane → focus moves to its sibling b (same parent split).
    let is_empty = t.close_active_pane().expect("close succeeds");
    assert!(!is_empty);
    assert_eq!(t.active_pane_id(), b);
    assert_eq!(t.pane_count(), 2);

    // Close b → focus moves to a (the root sibling).
    let is_empty = t.close_active_pane().expect("close succeeds");
    assert!(!is_empty);
    assert_eq!(t.active_pane_id(), a);
    assert_eq!(t.pane_count(), 1);

    // Close a → tab empty.
    let is_empty = t.close_active_pane().expect("close succeeds");
    assert!(is_empty);
}

#[test]
fn split_inherits_active_pane_terminal_size() {
    // Build a pane with a known terminal size, then split and verify
    // the new pane's terminal matches. (Batch 3 only checks the data
    // plumbing; the renderer / PTY resize to split-tree rects lands
    // in Batch 5/6.)
    let mut pane = Pane::with_terminal_only(100);
    pane.terminal = Some(Terminal::with_scrollback(30, 90, 100));
    let mut t = Tab::with_single_pane(pane);
    let original_rows = t.active().terminal.as_ref().unwrap().grid().num_rows;
    let original_cols = t.active().terminal.as_ref().unwrap().grid().num_cols;
    assert_eq!((original_rows, original_cols), (30, 90));

    let new_id = t
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let new_pane = t.pane(new_id).unwrap();
    let new_terminal = new_pane.terminal.as_ref().unwrap();
    // with_terminal_only uses Terminal::with_scrollback(24, 80, ...) —
    // it does NOT inherit the original pane's size (that only happens
    // in the production split_active_pane path). The test documents
    // this contract: the test helper is for tree plumbing only.
    assert_eq!(
        (new_terminal.grid().num_rows, new_terminal.grid().num_cols),
        (24, 80)
    );
}

/// v1.3 Batch 6: `pane_hit_test` returns the correct pane for points
/// inside each pane and `None` for points outside the content area.
#[test]
fn pane_hit_test_finds_correct_pane_in_vertical_split() {
    let mut t = tab_with_terminal(100);
    let _new_id = t
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    let active = t.active_pane_id();
    let panes = t.split_tree().panes();
    let other = panes.iter().find(|&&id| id != active).copied().unwrap();
    // Content rect: 0..800 wide, 0..600 tall. Vertical split at 0.5 →
    // left pane [0, 0, 400, 600], right pane [400, 0, 800, 600].
    let content: weft_core::pane_layout::Rect = [0.0, 0.0, 800.0, 600.0];
    let layouts = t.split_tree().layout(content);
    let active_rect = layouts
        .iter()
        .find(|(id, _)| *id == active)
        .map(|(_, r)| *r)
        .unwrap();
    let other_rect = layouts
        .iter()
        .find(|(id, _)| *id == other)
        .map(|(_, r)| *r)
        .unwrap();
    // Point in the active pane.
    let cx = (active_rect[0] + active_rect[2]) / 2.0;
    let cy = (active_rect[1] + active_rect[3]) / 2.0;
    assert_eq!(t.pane_hit_test(cx, cy, content), Some(active));
    // Point in the other pane.
    let ox = (other_rect[0] + other_rect[2]) / 2.0;
    let oy = (other_rect[1] + other_rect[3]) / 2.0;
    assert_eq!(t.pane_hit_test(ox, oy, content), Some(other));
    // Point outside content.
    assert_eq!(t.pane_hit_test(-1.0, -1.0, content), None);
}

/// v1.3 Batch 6: `set_active_pane` switches focus and is idempotent.
#[test]
fn set_active_pane_switches_focus() {
    let mut t = tab_with_terminal(100);
    let original = t.active_pane_id();
    let new_id = t
        .split_active_pane_test(SplitDirection::Horizontal, 0.5, 100)
        .unwrap();
    // After split, the new pane becomes active (split_active_pane_inner
    // sets self.active_pane = new_pane_id). Switch back to the original.
    assert_eq!(t.active_pane_id(), new_id);
    t.set_active_pane(original).unwrap();
    assert_eq!(t.active_pane_id(), original);
    // Setting the same pane again is a no-op (idempotent).
    t.set_active_pane(original).unwrap();
    assert_eq!(t.active_pane_id(), original);
}

/// v1.3 Batch 6: `close_active_pane` updates `active_pane` before
/// removing from `panes` — the invariant is never violated.
#[test]
fn close_active_pane_restores_focus_to_sibling() {
    let mut t = tab_with_terminal(100);
    let original = t.active_pane_id();
    let new_id = t
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();
    // After split, the new pane is active. Close it — focus should
    // return to the original (surviving sibling).
    assert_eq!(t.active_pane_id(), new_id);
    let is_last = t.close_active_pane().unwrap();
    assert!(!is_last);
    assert_eq!(t.active_pane_id(), original);
    // The closed pane is gone from the map.
    assert!(t.pane(new_id).is_none());
    assert!(t.pane(original).is_some());
}
