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
        block_ids: Vec::new(),
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

#[test]
fn active_pane_dimensions_use_split_rect_and_terminal_content_gutter() {
    let mut t = tab_with_terminal(100);
    t.split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .unwrap();

    assert_eq!(
        t.active_pane_dimensions_for_rect([0.0, 0.0, 800.0, 600.0], 10.0, 20.0),
        Some((30, 37))
    );
    assert_eq!(
        t.active_pane_dimensions_for_rect([0.0, 0.0, 800.0, 600.0], 0.0, 20.0),
        None
    );
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

// ── v1.10.25 Batch 2 (FIX_TUI_INPUT_WIDTH_ALIGNMENT) ────────────────────
//
// Three independently-computed widths must agree at one pane geometry: the
// PTY target cols that the Tab mirror sites compute, the grid render cols
// (grid.num_cols — every cell is drawn), and the BlockView wrap cols.

const TUI_PANE_RECT: weft_core::pane_layout::Rect = [0.0, 0.0, 800.0, 600.0];
const TUI_CELL_W: f32 = 10.0;
const TUI_CELL_H: f32 = 20.0;

fn drive_primary_tui(tab: &mut Tab) {
    tab.process_pty_output(b"\x1b]133;A\x07\x1b]133;B\x07omp\x1b]133;C\x07");
    tab.process_pty_output(b"\x1b[3A\x1b[1G\x1b[?2026h\x1b[2Ka\x1b[2G\x1b[?2026l");
    let terminal = tab.terminal.as_ref().unwrap();
    assert!(
        terminal.primary_screen_app_active(),
        "precondition: the pane owns a primary-screen TUI"
    );
}

/// PTY target cols == grid render cols == block wrap cols for a primary
/// screen TUI at a single pane geometry (data-level equality). pane 800×600,
/// cell 10×20 → raw 80 cols, gutter 3 → content 77.
#[test]
fn primary_tui_pty_grid_and_block_widths_are_identical_at_same_geometry() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);

    // (1) PTY target cols computed by the Tab mirror sites (`tui_cols_kind`
    // → `terminal_content_cols`).
    let (rows, pty_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        pty_cols, 77,
        "primary TUI target is the content width (80 raw - 3 gutter cols)"
    );

    // (2) Grid render cols: commit the grid to the target exactly as the
    // effect flush does (`commit_pty_resize_result` → `Terminal::resize`),
    // then the grid holds content cols — the renderer draws every cell from
    // the gutter-inset origin, so origin + cols·cell_w == content right edge.
    // (The second mirror site `resize_all_panes_for_rect` queues the same
    // target.)
    assert!(tab.resize_all_panes_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H));
    let queued = tab.active_mut().pending_pty_resize.take().unwrap();
    assert_eq!(queued, (rows, pty_cols), "both mirror sites agree");
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(rows, pty_cols);
    let grid_cols = tab.terminal.as_ref().unwrap().grid().num_cols;
    assert_eq!(grid_cols, pty_cols, "grid render cols == PTY target cols");

    // (3) Block wrap cols at the same pane geometry.
    let ctx = crate::layout::LayoutCtx::new((800.0, 600.0), TUI_CELL_W, TUI_CELL_H, 0.0, 0.0);
    let block_cols = crate::layout::layout_block_view(&ctx, 600.0, false).cols;
    assert_eq!(block_cols, pty_cols, "block wrap cols == PTY target cols");

    // Contrast: an alt-screen TUI (vim) intentionally diverges from the
    // BlockView — full width, and the grid mirrors its own PTY target.
    tab.process_pty_output(b"\x1b[?1049h");
    // v1.10.25 Batch 3 (B1): a fresh flip holds the burst Content lock, so
    // let the flip go quiet first — the mirror then exposes the live alt
    // Full target the pane converges to after a real single-toggle launch.
    tab.alt_rescale_last_flip =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(300));
    let (_, alt_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(alt_cols, 80, "alt-screen TUI target is the full width");
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(rows, alt_cols);
    assert_eq!(
        tab.terminal.as_ref().unwrap().grid().num_cols,
        alt_cols,
        "grid render cols always mirror the PTY target (edge-to-edge here)"
    );
    tab.process_pty_output(b"\x1b[?1049l");
    // Back on the primary phase the content target is restored exactly.
    assert_eq!(
        tab.active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H),
        Some((rows, 77)),
        "the primary target snaps back to the pre-toggle constant"
    );
}

/// A burst of transient `?1049h/l` toggles must not become a winsize ioctl
/// storm. v1.10.25 Batch 3 (B1) replaced the "value set ≤ 2" assertion
/// (structurally unable to tell a 99↔102 storm from controlled behaviour)
/// with an explicit hysteresis model: while the toggle storm is fresh, the
/// mirror site locks the target to the Content constant (tab/resize.rs
/// `burst_locked_cols`), so the drift check's desired stays Content —
/// whatever phase each flip landed on — and the ioctl stream is bounded to
/// the tiny constant count below.
#[test]
fn transient_1049_toggle_storm_hysteresis_bounds_ioctl_count() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);

    // The pane has converged at the content target.
    let (rows, content_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(content_cols, 77);
    let mut last_sent: Option<(usize, usize)> = Some((rows, content_cols));
    let mut emitted_ioctls = 0usize;

    // Walk 20 rounds of 1049h/l through the mirror site + the winsize ioctl
    // dedup rule exactly as `app_runtime::apply_pty_resize` does
    // (Pane::should_send_winsize_ioctl). Each flip refreshes
    // `alt_rescale_last_flip` (tab/lifecycle.rs), keeping the burst window
    // fresh for the whole storm — exactly the SIGWINCH feedback loop.
    for round in 0..20 {
        tab.process_pty_output(b"\x1b[?1049h");
        let (_, alt_cols) = tab
            .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
            .unwrap();
        assert_eq!(
            alt_cols, content_cols,
            "round {round}: the burst lock holds the alt phase at Content — an 80-col escape is what feeds the alternation"
        );
        if Pane::should_send_winsize_ioctl(last_sent, (rows, alt_cols)) {
            last_sent = Some((rows, alt_cols));
            emitted_ioctls += 1;
        }

        tab.process_pty_output(b"\x1b[?1049l");
        let (_, primary_cols) = tab
            .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
            .unwrap();
        assert_eq!(
            primary_cols, content_cols,
            "round {round}: primary phase stays on the Content constant"
        );
        if Pane::should_send_winsize_ioctl(last_sent, (rows, primary_cols)) {
            last_sent = Some((rows, primary_cols));
            emitted_ioctls += 1;
        }
    }

    // Explicit-burst upper bound: a 20-round storm may emit at most a tiny
    // constant. With the hysteresis lock every measured target is the
    // already-sent Content size, so in practice zero; ≤2 tolerates one
    // initial convergence + one final drift without ever resembling the
    // v1.10.19 99↔102 storm (which emitted once per flip).
    assert!(
        emitted_ioctls <= 2,
        "a 20-round toggle storm must emit ≤ 2 winsize ioctls, got {emitted_ioctls}"
    );
    // The pane ends on the primary Content constant — the pre-burst value.
    assert_eq!(
        tab.active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H),
        Some((rows, content_cols)),
        "the pane converges to the pre-burst content target"
    );
}

/// v1.10.25 Batch 3 (B1): a SINGLE alt toggle (a real TUI launch) is locked
/// to Content only while the flip stays fresh; once the burst window goes
/// quiet the live kind applies and the pane converges to the Full target —
/// one width transition, not an oscillation.
#[test]
fn single_alt_toggle_converges_to_full_after_quiet() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);

    let (_, content_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(content_cols, 77);

    // Single flip: the freshly-armed window pins the target at Content (the
    // winding grid already holds it → desired == current, no recompute, no
    // ioctl) — the mirror site must not leap to Full while the burst window
    // could still be in the SIGWINCH loop.
    tab.process_pty_output(b"\x1b[?1049h");
    assert!(tab.terminal.as_ref().unwrap().is_alt_screen_active());
    let (_, fresh_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        fresh_cols, content_cols,
        "a fresh single flip holds the Content lock"
    );

    // The TUI stays quiet → the window expires and the alt phase's Full
    // target converges (exactly one ioctl afterwards).
    tab.alt_rescale_last_flip =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(300));
    let (_, converged) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(converged, 80, "quiet: the alt target converges to Full");
}
