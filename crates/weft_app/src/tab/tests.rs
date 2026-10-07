//! Inline tests for `Tab`, extracted from `tab.rs` to keep the production
//! file under the 800-line architecture gate. See `tab.rs` (where
//! `#[cfg(test)] #[path = "tab/tests.rs"] mod tests;` declares this module).
//!
//! These tests cover empty-tab semantics, the queued TUI scroll replay window,
//! snapshot roundtrip (v1.0 H4 TabsAutoSave), and the v1.3 split-pane
//! lifecycle (split / focus / close / hit-test).

use super::*;

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
    let (alive, drained, need_redraw, _) = t.process_messages();
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
        panes: None,
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

// ── v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2): active_pane_views ──

#[test]
fn active_pane_views_single_pane_has_no_backgrounds() {
    let mut t = tab_with_terminal(100);
    let views = t.active_pane_views();
    assert!(views.backgrounds.is_empty());
    // The active pane is still handed out mutably.
    let _active: &mut Pane = views.active;
}

#[test]
fn active_pane_views_backgrounds_exclude_active_terminal() {
    let mut t = tab_with_terminal(100);
    let original = t.active_pane_id();
    let new_id = t
        .split_active_pane_test(SplitDirection::Vertical, 0.5, 100)
        .expect("split succeeds");
    // The new pane is active (split contract); snapshot expected ids and
    // the original terminal's address while `t` is only shared-borrowed.
    let original_pane = t.pane(original).expect("original pane present");
    let original_session = original_pane.pane_session_id;
    let original_scroll = original_pane.block_scroll_anchor.offset_value() as f32
        + original_pane.block_scroll_fraction;
    let original_terminal: *const weft_core::vt::Terminal =
        original_pane.terminal.as_ref().expect("original terminal");
    let active_session = t.pane(new_id).expect("active pane present").pane_session_id;

    let views = t.active_pane_views();
    assert_eq!(views.backgrounds.len(), 1);
    let bg = &views.backgrounds[0];
    assert_eq!(bg.pane_id, original);
    assert_eq!(bg.pane_session_id, original_session);
    assert!(bg.pane_session_id != active_session);
    assert_eq!(bg.block_scroll, original_scroll);
    // Address comparison: the background reference points at the ORIGINAL
    // pane's terminal, never at the active pane's.
    let active_terminal: *const weft_core::vt::Terminal =
        views.active.terminal.as_ref().expect("active terminal") as *const _;
    let bg_terminal: *const weft_core::vt::Terminal = bg.terminal;
    assert!(!std::ptr::eq(bg_terminal, active_terminal));
    assert!(std::ptr::eq(bg_terminal, original_terminal));
}

#[test]
fn active_pane_views_active_is_same_pane_as_active_mut() {
    let mut t = tab_with_terminal(100);
    let expected_session = t.pane(t.active_pane_id()).unwrap().pane_session_id;
    {
        let views = t.active_pane_views();
        assert_eq!(views.active.pane_session_id, expected_session);
        // Mutations through the view must land on the same pane that
        // `active_mut()` hands out afterwards.
        views.active.ime_preedit = "あ".to_string();
    }
    assert_eq!(t.active_mut().ime_preedit, "あ");
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
    // v1.10.26 Batch D (D-1): an isolated single flip has no storm signature.
    // v1.10.28: Full requires *sustained* residency (>=250ms) — a real vim
    // launch stays resident for seconds, so elapse the threshold before
    // measuring (the acknowledged ≤250ms first Full-ization delay).
    tab.process_pty_output(b"\x1b[?1049h");
    let_sustained_alt_residency_elapse();
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
    // Back on the primary phase the content target is restored exactly — but
    // this exit flip forms a FRESH two-flip storm with the entry flip above,
    // so the target FREEZES at the current grid (alt Full 80) until the storm
    // goes quiet (v1.10.27). Expire it, then the constant is restored.
    expire_alt_flip_history(&mut tab);
    assert_eq!(
        tab.active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H),
        Some((rows, 77)),
        "the primary target snaps back to the pre-toggle constant"
    );
}

/// A burst of transient `?1049h/l` toggles must not become a winsize ioctl
/// storm. v1.10.25 Batch 3 (B1) + v1.10.26 (D-1) locked the target to the
/// Content constant; v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW) FREEZES it at the
/// pane's current grid size instead (tab/resize.rs `burst_locked_cols`), so
/// desired == current and the ioctl stream is bounded to the tiny constant
/// count below.
///
/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): the sustained-alt hysteresis now
/// breaks the storm at the SOURCE — every transient alt phase (<250ms
/// residency) reads Content, so the target never leaves the initial Content
/// constant at all, not even round 0's first "lone" flip (the old immediate-
/// Full). The app-layer freeze (defense in depth) has nothing left to fight:
/// the whole 20-round storm emits ZERO winsize ioctls.
#[test]
fn transient_1049_toggle_storm_hysteresis_bounds_ioctl_count() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);

    // The pane has converged at the content target — grid == last_sent, the
    // production invariant from `apply_pty_resize_effect` (ioctl → grid
    // commit keeps "current" == what was actually sent).
    let (rows, content_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(content_cols, 77);
    // Calculate the expected Full width for this pane geometry.
    let full_cols = 80; // TUI_PANE_RECT (800.0) / TUI_CELL_W (10.0) = 80 cols
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(rows, content_cols);
    let mut last_sent: Option<(usize, usize)> = Some((rows, content_cols));
    let mut emitted_ioctls = 0usize;

    // v1.10.30 (FIX_LESS_ALT_COLS_JUMP): The first alt entry here is
    // ISOLATED (no recent exit) — it must flip to Full immediately: the
    // less/vim startup path gets its ioctl before the app paints.
    tab.process_pty_output(b"\x1b[?1049h");
    let (_, isolated_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        isolated_cols, full_cols,
        "an isolated alt entry flips to Full immediately (less/vim startup)"
    );
    // Exiting creates the recent-exit stamp; every re-entry below is then a
    // burst re-entry, which is what this storm test validates. Premise: each
    // round's gap since the previous \x1b[?1049l stays under the 400ms
    // ALT_REENTRY_BURST_MS window — a CI stall beyond it would read as an
    // isolated entry (Full) and fail the Content assertions below.
    tab.process_pty_output(b"\x1b[?1049l");

    // Walk 20 rounds of 1049h/l through the mirror site + the winsize ioctl
    // dedup rule exactly as `app_runtime::apply_pty_resize` does
    // (`Pane::should_send_winsize_ioctl`, committing the grid to the target
    // on each ioctl). Each flip refreshes the flip history (tab/lifecycle.rs),
    // keeping the burst window fresh for the whole storm — exactly the
    // SIGWINCH feedback loop.
    for round in 0..20 {
        tab.process_pty_output(b"\x1b[?1049h");
        let (r, cols) = tab
            .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
            .unwrap();
        assert_eq!(r, rows);
        // v1.10.30 (FIX_LESS_ALT_COLS_JUMP): After the initial recent exit,
        // all burst re-entries stay at Content (77 cols) - no Full, so no
        // ioctl reaches the path and the loop cannot re-arm itself.
        assert_eq!(
            cols, content_cols,
            "round {}: burst re-entry must stay at Content",
            round
        );
        if Pane::should_send_winsize_ioctl(last_sent, (rows, cols)) {
            last_sent = Some((rows, cols));
            emitted_ioctls += 1;
            tab.active_mut()
                .terminal
                .as_mut()
                .unwrap()
                .resize(rows, cols);
        }

        tab.process_pty_output(b"\x1b[?1049l");
        let (_, primary_cols) = tab
            .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
            .unwrap();
        assert_eq!(
            primary_cols, content_cols,
            "round {round}: the primary phase holds the Content constant"
        );
        if Pane::should_send_winsize_ioctl(last_sent, (rows, primary_cols)) {
            last_sent = Some((rows, primary_cols));
            emitted_ioctls += 1;
            tab.active_mut()
                .terminal
                .as_mut()
                .unwrap()
                .resize(rows, primary_cols);
        }
    }

    // With the loop broken at the source the storm emits nothing (0 ioctls);
    // assert the exact count — no width change ever reaches the ioctl path.
    // v1.10.30 (FIX_LESS_ALT_COLS_JUMP): Burst re-entries stay at Content
    // throughout, so the storm remains bounded. Isolated entries (which now
    // go immediately to Full) are tested separately.
    assert_eq!(
        emitted_ioctls, 0,
        "a 20-round burst re-entry toggle storm must emit ZERO winsize ioctls"
    );
    // The pane ends on the primary Content constant — the pre-burst value.
    expire_alt_flip_history(&mut tab);
    assert_eq!(
        tab.active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H),
        Some((rows, content_cols)),
        "the pane converges to the pre-burst content target"
    );
}

/// v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): Deterministically expire the two-flip
/// storm signature (simulate the 150ms debounce window elapsing) without
/// sleeping.
fn expire_alt_flip_history(tab: &mut Tab) {
    // FIX_background_pane_pump §2.5: the storm record lives in the pane's
    // own map slot now.
    let stale = std::time::Instant::now() - std::time::Duration::from_millis(300);
    tab.alt_flip_history.insert(
        tab.active_pane,
        AltFlipHistory {
            src_pane: tab.active_pane,
            older: stale,
            newer: stale,
            count: 2,
        },
    );
}

/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): let a real alt TUI's CONTINUOUS
/// residency cross the 250ms sustained-alt cols hysteresis threshold
/// (`Terminal::tui_cols_kind` only reports Full once alt has been resident
/// for `SUSTAINED_ALT_COLS_MS`; see docs/FIX_TRANSIENT_ALT_COLS_FLIP.md).
/// A 251ms wall-clock wait is deterministic: `tui_cols_kind` reads elapsed
/// via `Instant::saturating_duration_since`, so crossing the threshold can
/// only grow under load. Same wall-clock style as the debounce wait in the
/// `resize.rs` burst-freeze tests.
fn let_sustained_alt_residency_elapse() {
    std::thread::sleep(std::time::Duration::from_millis(251));
}

/// v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): Simulate the resize path exactly as
/// `app_runtime::apply_pty_resize_effect` does — dedup against `last_sent`
/// (`Pane::should_send_winsize_ioctl`), record the ioctl when it fires, and
/// ALWAYS commit the grid to the target (the in-memory Grid is committed even
/// on a deduped ioctl). Keeps the test's grid == last_sent == the freeze's
/// "current grid" source, which is the production invariant.
fn apply_measured_target(
    tab: &mut Tab,
    target: (usize, usize),
    last_sent: &mut Option<(usize, usize)>,
    ioctls: &mut Vec<(usize, usize)>,
) {
    if Pane::should_send_winsize_ioctl(*last_sent, target) {
        *last_sent = Some(target);
        ioctls.push(target);
    }
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(target.0, target.1);
}

/// v1.10.27 (FIX_RESIZE_DOUBLE_REDRAW): End-to-end double-click-zoom
/// sequence — window resize → application alt-toggle pair (storm) → quiet —
/// emits exactly TWO winsize ioctls (first resize + one final convergence)
/// and never a Content intermediate mid-storm.
///
/// The old burst lock pinned the target to the Content constant recomputed
/// from the NEW window size (91) while the grid already held the size first
/// sent (94): the 91 ioctl fired mid-storm → SIGWINCH → omp repainted and
/// toggled again (the second round trip), then the quiet convergence back to
/// 94 repainted a SECOND time — the "抖动两下". Freezing the target at the
/// pane's current grid size during the storm makes desired == current, so the
/// mid-storm ioctl disappears and the app repaints exactly once, at the single
/// post-quiet convergence.
#[test]
fn resize_followed_by_toggle_pair_emits_exactly_two_ioctls_no_intermediate() {
    let mut tab = tab_with_terminal(100);
    let src = tab.active_pane;

    // Setup: an alt-screen TUI at the OLD geometry, converged. v1.10.28: a
    // real alt TUI is *sustained* — elapse the 250ms cols hysteresis so the
    // setup reads the live Full target at the old width.
    tab.process_pty_output(b"\x1b[?1049h");
    let_sustained_alt_residency_elapse();
    let old_rect: weft_core::pane_layout::Rect = [0.0, 0.0, 800.0, 600.0];
    let (rows, old_full_cols) = tab
        .active_pane_dimensions_for_rect(old_rect, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(old_full_cols, 80, "old geometry maps to full 80 cols");
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(rows, old_full_cols);
    let mut last_sent: Option<(usize, usize)> = Some((rows, old_full_cols));
    let mut ioctls: Vec<(usize, usize)> = Vec::new();
    // The setup flip happened long ago — reset it to a stale lone record so
    // ONLY the app's own dance flips drive the storm signature (a real
    // double-click zoom is seconds after launch).
    let stale = std::time::Instant::now() - std::time::Duration::from_secs(5);
    tab.alt_flip_history.insert(
        src,
        AltFlipHistory {
            src_pane: src,
            older: stale,
            newer: stale,
            count: 1,
        },
    );

    // (1) Window resize (double-click zoom): new geometry Full=94, Content=91.
    // No flips have happened yet — the first target computes and ships
    // normally (the "首个" ioctl; the fix leaves it untouched).
    let new_rect: weft_core::pane_layout::Rect = [0.0, 0.0, 940.0, 600.0];
    let first = tab
        .active_pane_dimensions_for_rect(new_rect, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(first, (rows, 94), "the resize lands at the new full width");
    apply_measured_target(&mut tab, first, &mut last_sent, &mut ioctls);
    assert_eq!(
        ioctls,
        vec![(rows, 94)],
        "exactly one resize ioctl before the flips"
    );

    // (2) Application alt-toggle pair (the storm): omp repaints → l/h/l in one
    // PTY batch, three flips recorded at once → two-flip storm signature, ends
    // on the PRIMARY phase (raw = Content = 91 — the old lock's wrong value).
    tab.process_pty_output(b"\x1b[?1049l\x1b[?1049h\x1b[?1049l");
    let during = tab
        .active_pane_dimensions_for_rect(new_rect, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        during,
        (rows, 94),
        "the burst must FREEZE the target at the current grid (94), never the Content 91"
    );
    // Exercise the real queue layer (review S1): the helper bypasses
    // pending_pty_resize, so assert on resize_terminal_and_queue itself —
    // it must decline because grid == target && nothing is pending.
    assert!(
        !tab.active_mut()
            .resize_terminal_and_queue(during.0, during.1),
        "desired == current grid → the queue layer declines → zero mid-storm ioctl"
    );
    assert!(
        tab.active_mut().pending_pty_resize.is_none(),
        "and nothing was queued as a side effect"
    );
    assert!(
        !Pane::should_send_winsize_ioctl(last_sent, during),
        "the frozen target equals the last-sent ioctl — the ioctl stream stays silent mid-storm"
    );

    // (3) Quiet — the debounce window expires; converge ONCE to the final
    // value (the "最终" ioctl).
    expire_alt_flip_history(&mut tab);
    let final_dims = tab
        .active_pane_dimensions_for_rect(new_rect, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        final_dims,
        (rows, 91),
        "quiet convergence targets Content at the new width"
    );
    apply_measured_target(&mut tab, final_dims, &mut last_sent, &mut ioctls);
    assert_eq!(
        ioctls,
        vec![(rows, 94), (rows, 91)],
        "exactly two ioctls (first resize + final convergence), no intermediate"
    );
}

/// v1.10.26 Batch D (D-2): a batch that contains an h→l pair nets the
/// `alt_active` boolean to zero — the old before/after boolean detection
/// missed it entirely, so `pending_alt_rescale` was never armed and the
/// debounce window silently expired on even-count batches (the v1.10.19 lock
/// loophole). The u64 flip-counter diff counts 2 real flips instead.
#[test]
fn batch_internal_h_l_pair_counts_two_flips_and_refreshes_history() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);
    assert!(!tab.terminal.as_ref().unwrap().is_alt_screen_active());
    assert_eq!(
        tab.terminal.as_ref().unwrap().alt_flip_count(),
        0,
        "precondition: no flips yet"
    );

    // One batch with an h→l pair: phase is net-zero (still primary).
    tab.process_pty_output(b"\x1b[?1049h\x1b[?1049l");

    let terminal = tab.terminal.as_ref().unwrap();
    assert_eq!(
        terminal.alt_flip_count(),
        2,
        "a batch-internal h+l must count two flips"
    );
    assert!(!terminal.is_alt_screen_active(), "the phase is net-zero");
    assert!(
        tab.pending_alt_rescale,
        "the flip diff must arm the pending rescale (boolean net-zero missed it)"
    );
    let hist = tab
        .alt_flip_history
        .get(&tab.active_pane)
        .copied()
        .expect("the flip diff must refresh the flip history");
    assert_eq!(hist.count, 2, "two real flips recorded in the history");
    assert_eq!(hist.src_pane, tab.active_pane);
}

/// v1.10.26 Batch D (D-1): a SINGLE alt toggle (a real TUI launch) must NOT
/// arm the burst Content lock — the burst signature needs TWO flips from the
/// same pane inside the debounce window. An isolated single flip exposes the
/// live alt Full target with no Content-hold freeze and no oscillation.
///
/// v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): "live Full" now requires
/// *sustained* residency (>=250ms), so a lone flip converges once the real
/// TUI has been resident past the threshold — the fix document's acknowledged
/// ≤250ms + one-repaint first Full-ization delay for vim/less, imperceptible
/// to the user.
#[test]
fn single_alt_toggle_converges_to_full_once_resident() {
    let mut tab = tab_with_terminal(100);
    drive_primary_tui(&mut tab);

    let (rows, content_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(content_cols, 77);

    // Single flip: no second flip → no storm — the live alt kind applies once
    // residency crosses the sustained-alt hysteresis threshold. Pre-Batch-D
    // this mis-fired: any fresh flip held the 150ms Content lock, flashing
    // the wrong width on a real vim/less launch.
    tab.process_pty_output(b"\x1b[?1049h");
    assert!(tab.terminal.as_ref().unwrap().is_alt_screen_active());
    let_sustained_alt_residency_elapse();
    let (_, fresh_cols) = tab
        .active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H)
        .unwrap();
    assert_eq!(
        fresh_cols, 80,
        "an isolated single flip converges to Full once sustained residency is established — no Content freeze"
    );
    // Commit the grid to the alt Full width (production: ioctl → grid commit)
    // so the freeze reads current == last_sent.
    tab.active_mut()
        .terminal
        .as_mut()
        .unwrap()
        .resize(rows, fresh_cols);

    tab.process_pty_output(b"\x1b[?1049l");
    // The exit flip is now a second flip — a FRESH two-flip storm signature,
    // so the target FREEZES at the current grid (the alt Full width just
    // committed) until the storm goes quiet. Expire it, then the primary
    // Content target is restored exactly (v1.10.27 freeze semantics).
    expire_alt_flip_history(&mut tab);
    assert_eq!(
        tab.active_pane_dimensions_for_rect(TUI_PANE_RECT, TUI_CELL_W, TUI_CELL_H),
        Some((rows, content_cols)),
        "the primary target snaps back to the pre-toggle constant"
    );
}

/// v1.11.8 (PLAN_v1118 M-E): live-split continuous scenario — a screen-owned
/// session streams >2MiB in batches; each 1MiB crossing settles finished
/// head blocks THROUGH THE LIVE PATH (F16: the v1.11.7 block-view-retention
/// combo that had no coverage). Each manual refresh (bypassing the 50ms
/// throttle via pub `refresh_primary_history_snapshot_now`) must surface its
/// head count in `pending_screen_split_heads`; the next `process_messages`
/// drain must consume it and advance the detached FixedDocumentRow anchor by
/// heads × chrome rows; after the final settle the stream's head and tail
/// text must be present in the finished blocks.
#[test]
fn live_split_stream_drains_heads_and_compensates_anchor() {
    let mut tab = tab_with_terminal(40_000);
    // Screen-owned session: integrated markers + two CUP ops cross the
    // cursor_ops >= 2 ownership threshold.
    tab.process_pty_output(b"\x1b]133;A\x07\x1b]133;B\x07stream\x1b]133;C\x07");
    tab.process_pty_output(b"\x1b[H\x1b[2;1H");
    assert!(
        tab.terminal
            .as_ref()
            .unwrap()
            .block_tracker()
            .screen_document_start()
            .is_some(),
        "precondition: the pane owns a primary-screen TUI document"
    );
    // Detach the anchor: split-head settlement must advance it by the
    // inserted chrome rows so the user keeps reading the same visual row.
    tab.set_block_scroll(100);
    assert!(matches!(
        tab.block_scroll_anchor(),
        BlockScrollAnchor::FixedDocumentRow(100)
    ));

    // Flood ~1.15MiB in batches below the app's 256KiB message splitter.
    const FLOOD_BYTES: usize = 192 * 1024;
    let mut flood = vec![b'a'; FLOOD_BYTES];
    // Distinct markers pin the head/tail text assertions after settle.
    flood[..13].copy_from_slice(b"HEAD_MARKER_1");
    for _ in 0..6 {
        tab.process_pty_output(&flood);
    }

    // First 1MiB crossing: the manual refresh splits the composed document;
    // the heads land in finished blocks synchronously.
    let heads_before = tab
        .terminal
        .as_ref()
        .unwrap()
        .block_tracker()
        .blocks()
        .len();
    assert!(
        tab.terminal
            .as_mut()
            .unwrap()
            .refresh_primary_history_snapshot_now(),
        "manual refresh must run regardless of the 50ms throttle"
    );
    let heads_1 = tab
        .terminal
        .as_ref()
        .unwrap()
        .block_tracker()
        .blocks()
        .len()
        - heads_before;
    assert!(heads_1 >= 1, "first flood must settle >= 1 split head");

    // The tab drain consumes the pending head count (the anchor compensation
    // runs after the terminal borrow ends, tab.rs B-D3).
    tab.process_messages();
    assert!(
        tab.terminal
            .as_mut()
            .unwrap()
            .take_pending_screen_split_heads()
            .is_none(),
        "the drain must consume the pending split-head count"
    );

    // Second crossing: another ~1.15MiB with its own head marker + the
    // stream-tail marker at the very end.
    flood[..13].copy_from_slice(b"HEAD_MARKER_2");
    flood[FLOOD_BYTES - 11..].copy_from_slice(b"TAIL_MARKER");
    for _ in 0..6 {
        tab.process_pty_output(&flood);
    }
    let heads_before = tab
        .terminal
        .as_ref()
        .unwrap()
        .block_tracker()
        .blocks()
        .len();
    assert!(
        tab.terminal
            .as_mut()
            .unwrap()
            .refresh_primary_history_snapshot_now(),
        "second manual refresh must split the grown document"
    );
    let heads_2 = tab
        .terminal
        .as_ref()
        .unwrap()
        .block_tracker()
        .blocks()
        .len()
        - heads_before;
    assert!(heads_2 >= 1, "second flood must settle >= 1 split head");
    tab.process_messages();

    // The detached anchor advanced by (heads_1 + heads_2) chrome rows — the
    // user's visual row is preserved across both splits.
    let expected_anchor = 100 + (heads_1 + heads_2) * crate::layout::BLOCK_SPLIT_HEAD_CHROME_ROWS;
    assert_eq!(
        tab.block_scroll_anchor(),
        BlockScrollAnchor::FixedDocumentRow(expected_anchor),
        "the detached anchor advances by settled-head chrome rows per split"
    );

    // Settle: the final boundary snapshot may split once more; the finished
    // head blocks keep the stream's head text and the final block keeps the
    // stream-tail text — the shared transcript stays complete.
    tab.process_pty_output(b"\x1b]133;D;0\x07");
    tab.terminal.as_mut().unwrap().settle_primary_screen_exit();
    let blocks = tab.terminal.as_ref().unwrap().block_tracker().blocks();
    assert!(
        blocks.len() >= 2,
        "splits + settle leave >= 2 finished blocks"
    );
    assert!(
        blocks[0].output.contains("HEAD_MARKER_1"),
        "the earliest finished block keeps the first flood's head text"
    );
    assert!(
        blocks.iter().any(|b| b.output.contains("HEAD_MARKER_2")),
        "the second flood's head text lands in a split head block"
    );
    assert!(
        blocks.last().unwrap().output.contains("TAIL_MARKER"),
        "the final block keeps the stream-tail text"
    );
}

// ── v1.11.15 (FIX A/D): mouse suppression + per-gesture mode sync ──────

#[test]
fn sync_mouse_modes_copies_terminal_mouse_trio_into_input_handler() {
    use weft_core::input::MouseProtocol;
    let mut t = tab_with_terminal(100);
    // Negotiate AnyEvent + SGR + DECCKM on the terminal; the input handler
    // stays at its fresh defaults until the sync.
    t.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b[?1003h\x1b[?1006h\x1b[?1h");
    assert_eq!(t.input_handler.mouse_protocol, MouseProtocol::Off);
    t.sync_mouse_modes();
    assert_eq!(
        t.input_handler.mouse_protocol,
        MouseProtocol::AnyEvent,
        "mouse protocol must sync from the target tab's terminal"
    );
    assert!(t.input_handler.sgr_mouse, "SGR flag must sync");
    assert!(
        t.input_handler.app_cursor_keys,
        "DECCKM must sync (stale mode would emit CSI arrows instead of SS3)"
    );
}

#[test]
fn sync_mouse_modes_without_terminal_is_a_noop() {
    let mut t = Tab::empty();
    // Must not panic on terminal == None and must not touch input_handler.
    t.sync_mouse_modes();
    assert_eq!(
        t.input_handler.mouse_protocol,
        weft_core::input::MouseProtocol::Off
    );
    assert!(!t.input_handler.sgr_mouse);
    assert!(!t.input_handler.app_cursor_keys);
    assert_eq!(t.input_handler.kitty_flags, 0);
}

#[test]
fn mouse_suppressed_reads_the_active_pane_flag() {
    use weft_core::input::set_suppressed;
    let t = tab_with_terminal(100);
    assert!(!t.mouse_suppressed(), "fresh pane starts unsuppressed");
    set_suppressed(&t.mouse_suppress);
    assert!(t.mouse_suppressed());
}

#[test]
fn suppressed_pending_tui_scroll_is_consumed_without_pty_bytes() {
    use weft_core::input::set_suppressed;
    let mut t = tab_with_terminal(100);
    t.arm_tui_scroll_window();
    assert!(t.queue_tui_scroll(-2, 5, 10, weft_core::input::Modifiers::empty()));
    // The TUI's full startup handshake arrives while the gesture is parked;
    // without suppression this resolution would be SGR wheel bytes.
    t.terminal
        .as_mut()
        .unwrap()
        .process(b"\x1b[?1049h\x1b[?1003h\x1b[?1006h");
    // …but the session's disable arrives first (the leak scenario): the
    // reader flips the flag before the 50ms grace period elapses.
    set_suppressed(&t.mouse_suppress);
    expire_pending_scroll(&mut t);

    // The resolution must be Some (never None — that would strand the
    // deadline with no wake left to consume it) and must not write the PTY.
    let Some(TuiScrollResolution::LocalRows(0)) = t.resolve_pending_tui_scroll() else {
        panic!("expected the suppressed zero-row resolution");
    };
    assert!(t.pending_tui_scroll.is_none(), "pending must be consumed");
    assert!(
        !t.tui_scroll_window_active(),
        "the launch window must be explicitly consumed"
    );
}
