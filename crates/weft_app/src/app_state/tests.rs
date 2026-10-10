// ── Tests ───────────────────────────────────────────────────────────────

use super::{
    adjust_index_after_move, ContextMenu, InteractionState, PanelState, SessionManager,
    SettingsState, TabBarState, WindowRuntimeState,
};
use crate::tab::Tab;
use std::time::Instant;
use weft_core::config::Config;

// v1.5.0: `config_state_preserves_preferred_dark_theme` moved to
// `config_state.rs` alongside the ConfigState struct.

#[test]
fn tab_bar_clear_hover_preserves_scroll_and_click_history() {
    let now = Instant::now();
    let mut state = TabBarState {
        hovered_tab: Some(2),
        scroll_offset: 42.0,
        plus_hovered: true,
        arrow_left_hovered: true,
        arrow_right_hovered: true,
        last_titlebar_click: Some(now),
    };
    state.clear_hover();
    assert_eq!(state.hovered_tab, None);
    assert!(!state.plus_hovered);
    assert!(!state.arrow_left_hovered);
    assert!(!state.arrow_right_hovered);
    assert_eq!(state.scroll_offset, 42.0);
    assert_eq!(state.last_titlebar_click, Some(now));
}

#[test]
fn closing_panel_clears_only_transient_ownership() {
    let mut state = PanelState {
        open: true,
        query: "git".into(),
        selection: 3,
        search_focused: true,
        last_click: Some((Instant::now(), 3)),
        ..PanelState::default()
    };
    state.close();
    assert!(!state.open);
    assert!(!state.search_focused);
    assert!(state.last_click.is_none());
    assert_eq!(state.query, "git");
    assert_eq!(state.selection, 3);
}

#[test]
fn settings_open_uses_fresh_config_and_close_discards_transient_flags() {
    let mut config = Config::default();
    config.font.size = 19.0;
    let mut state = SettingsState::new();
    state.open_from(&config);
    assert!(state.open);
    assert_eq!(state.draft.font.size, 19.0);
    state.dirty = true;
    state.error = Some("save failed".into());
    state.close();
    assert!(!state.open);
    assert!(!state.dirty);
    assert!(state.error.is_none());
}

#[test]
fn interaction_defaults_keep_popup_within_supported_bounds() {
    let state = InteractionState::new();
    assert!((0.3..=0.95).contains(&state.popup_width_scale));
    assert!((3..=20).contains(&state.popup_max_rows));
    assert!(state.drag_state.is_none());
    assert!(state.context_menu.is_none());
    assert!(state.block_hovered.is_none());
}

#[test]
fn context_menu_ownership_uses_stable_session_identity() {
    let menu = ContextMenu {
        session_id: 42,
        block_id: None,
        x: 0.0,
        y: 0.0,
        selection: 0,
    };
    assert!(menu.belongs_to_session(42));
    assert!(!menu.belongs_to_session(7));
}

#[test]
fn window_runtime_starts_with_visible_cursor_animation() {
    let state = WindowRuntimeState::new();
    assert!(state.cursor_blink_on);
    assert_eq!(state.cursor_blink_phase, 0.0);
    assert!(state
        .cursor_anim_active
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn session_manager_starts_without_an_invalid_active_tab() {
    let state = SessionManager::new();
    assert!(state.is_empty());
    assert_eq!(state.active_idx(), 0);
    assert_eq!(state.prev_drawn_tab(), 0);
}

#[test]
fn open_tab_increments_active() {
    let mut sm = SessionManager::new();
    sm.push_tab(Tab::empty());
    sm.set_active(0);
    assert_eq!(sm.len(), 1);
    assert_eq!(sm.active_idx(), 0);
    sm.push_tab(Tab::empty());
    sm.set_active(1);
    assert_eq!(sm.active_idx(), 1);
}

#[test]
fn close_last_tab_signals_exit() {
    let mut sm = SessionManager::new();
    sm.push_tab(Tab::empty());
    // Closing the only tab → is_last = true
    assert!(sm.close_active());
    assert!(sm.is_empty());
}

#[test]
fn close_non_last_tab_keeps_session() {
    let mut sm = SessionManager::new();
    sm.push_tab(Tab::empty());
    sm.push_tab(Tab::empty());
    sm.set_active(1);
    // Closing active (tab 1, not the last remaining) → is_last = false
    assert!(!sm.close_active());
    assert_eq!(sm.len(), 1);
}

#[test]
fn close_background_reindexes_active() {
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(2);
    // Close tab 0 (before active) → active should shift to 1
    assert!(!sm.close_background(0));
    assert_eq!(sm.active_idx(), 1);
    assert_eq!(sm.len(), 2);
}

#[test]
fn stable_session_id_survives_reindex_and_removed_owner_disappears() {
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    let removed = sm.tab(0).unwrap().session_id;
    let survivor = sm.tab(2).unwrap().session_id;

    assert!(!sm.close_background(0));
    assert_eq!(sm.tab_index_by_session_id(removed), None);
    assert_eq!(sm.tab_index_by_session_id(survivor), Some(1));
}

#[test]
fn switch_wraps_around() {
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(2);
    let (new, prev) = sm.next();
    assert_eq!(prev, 2);
    assert_eq!(new, 0); // wraps
    let (new, prev) = sm.prev();
    assert_eq!(prev, 0);
    assert_eq!(new, 2); // wraps back
}

#[test]
fn switch_to_clamps_index() {
    let mut sm = SessionManager::new();
    sm.push_tab(Tab::empty());
    let (new, prev) = sm.switch_to(99);
    assert_eq!(new, 0);
    assert_eq!(prev, 0);
}

#[test]
fn move_tab_rightward_adjusts_active() {
    let mut sm = SessionManager::new();
    for _ in 0..5 {
        sm.push_tab(Tab::empty());
    }
    // [A, B, C, D, E], active = B (index 1)
    sm.set_active(1);
    sm.move_tab(1, 3); // → [A, C, D, B, E]
    assert_eq!(sm.active_idx(), 3, "active should follow the moved tab");
    assert_eq!(sm.len(), 5);
}

#[test]
fn move_tab_leftward_adjusts_active() {
    let mut sm = SessionManager::new();
    for _ in 0..5 {
        sm.push_tab(Tab::empty());
    }
    // [A, B, C, D, E], active = D (index 3)
    sm.set_active(3);
    sm.move_tab(3, 1); // → [A, D, B, C, E]
    assert_eq!(sm.active_idx(), 1, "active should follow the moved tab");
}

#[test]
fn move_tab_preserves_non_active_indices() {
    let mut sm = SessionManager::new();
    for _ in 0..5 {
        sm.push_tab(Tab::empty());
    }
    // [A, B, C, D, E], active = A (index 0)
    sm.set_active(0);
    sm.move_tab(1, 3); // → [A, C, D, B, E]
    assert_eq!(
        sm.active_idx(),
        0,
        "active was not the moved tab — should stay put"
    );
}

#[test]
fn move_tab_noop_on_equal_or_oob() {
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(1);
    sm.move_tab(1, 1);
    assert_eq!(sm.active_idx(), 1);
    sm.move_tab(0, 99);
    assert_eq!(sm.len(), 3, "out-of-bounds move should be a no-op");
    sm.move_tab(99, 0);
    assert_eq!(sm.len(), 3);
}

#[test]
fn move_tab_session_id_survives_reorder() {
    let mut sm = SessionManager::new();
    for _ in 0..4 {
        sm.push_tab(Tab::empty());
    }
    let moved_id = sm.tab(1).unwrap().session_id;
    let other_id = sm.tab(3).unwrap().session_id;
    sm.move_tab(1, 3);
    assert_eq!(
        sm.tab_index_by_session_id(moved_id),
        Some(3),
        "moved tab should now be at index 3"
    );
    assert_eq!(
        sm.tab_index_by_session_id(other_id),
        Some(2),
        "displaced tab should shift left"
    );
}

#[test]
fn adjust_index_after_move_rightward() {
    // Move from 1 to 3: [A,B,C,D,E] → [A,C,D,B,E]
    assert_eq!(adjust_index_after_move(0, 1, 3), 0); // A stays
    assert_eq!(adjust_index_after_move(1, 1, 3), 3); // B moved
    assert_eq!(adjust_index_after_move(2, 1, 3), 1); // C shifts left
    assert_eq!(adjust_index_after_move(3, 1, 3), 2); // D shifts left
    assert_eq!(adjust_index_after_move(4, 1, 3), 4); // E stays
}

#[test]
fn adjust_index_after_move_leftward() {
    // Move from 3 to 1: [A,B,C,D,E] → [A,D,B,C,E]
    assert_eq!(adjust_index_after_move(0, 3, 1), 0); // A stays
    assert_eq!(adjust_index_after_move(1, 3, 1), 2); // B shifts right
    assert_eq!(adjust_index_after_move(2, 3, 1), 3); // C shifts right
    assert_eq!(adjust_index_after_move(3, 3, 1), 1); // D moved
    assert_eq!(adjust_index_after_move(4, 3, 1), 4); // E stays
}

#[test]
fn remove_dead_active_tab_focuses_previous() {
    // v1.11.16 (Fix B3): removing the active tab must focus the
    // previous tab, matching close_active semantics.
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(1); // [A,B,C] active = B
    assert!(!sm.remove_dead(1)); // remove active B
    assert_eq!(sm.len(), 2);
    assert_eq!(sm.active_idx(), 0); // focus falls to A (index 0)
}

#[test]
fn remove_dead_active_last_tab_focuses_new_last() {
    // v1.11.16 (Fix B3): removing the last active tab focuses the
    // new last tab (index len-1), matching close_active semantics.
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(2); // [A,B,C] active = C
    assert!(!sm.remove_dead(2)); // remove active C
    assert_eq!(sm.len(), 2);
    assert_eq!(sm.active_idx(), 1); // focus falls to B (the new last)
}

#[test]
fn remove_dead_background_tab_keeps_index_behavior() {
    // v1.11.16 (Fix B3): removing a BACKGROUND (non-active) tab must
    // keep close_background's reindex behavior, not close_active's.
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(1); // [A,B,C] active = B
                      // Remove tab 0 (before active): active shifts down to 0.
    assert!(!sm.remove_dead(0));
    assert_eq!(sm.len(), 2);
    assert_eq!(sm.active_idx(), 0);

    // Reset and remove the background tab AFTER active.
    let mut sm = SessionManager::new();
    for _ in 0..3 {
        sm.push_tab(Tab::empty());
    }
    sm.set_active(1); // [A,B,C] active = B
                      // Remove tab 2 (after active): active index stays at 1.
    assert!(!sm.remove_dead(2));
    assert_eq!(sm.len(), 2);
    assert_eq!(sm.active_idx(), 1);
}

#[test]
fn remove_dead_last_remaining_tab_reports_is_last() {
    // v1.11.16 (Fix B3): removing the only remaining tab reports
    // is_last = true (session should exit).
    let mut sm = SessionManager::new();
    sm.push_tab(Tab::empty());
    assert!(sm.remove_dead(0));
    assert!(sm.is_empty());
}

// v1.12.25 (audit 3-B, P1-01): the Option-ization contract — the
// empty-tabs transient must read as `None`, never panic.
#[test]
fn active_on_empty_returns_none() {
    let sm = SessionManager::new();
    assert!(sm.active().is_none());
}

#[test]
fn active_mut_on_empty_returns_none() {
    let mut sm = SessionManager::new();
    assert!(sm.active_mut().is_none());
}
