use super::*;
use weft_core::input::{KeyCode, Modifiers};

// ── Shell ──────────────────────────────────────────────────────────

#[test]
fn canonical_shell_lifts_background_and_uses_unified_border() {
    let shell = CommandSurfaceShell::canonical(
        [0.0, 0.0, 100.0, 50.0],
        4.0,
        true,
        [0.1, 0.1, 0.1, 1.0],
        [0.0, 0.0, 1.0, 1.0],
    );
    // bg = bg + (1 - bg) * 0.08 = 0.1 + 0.9*0.08 = 0.172
    assert!((shell.bg_color[0] - 0.172).abs() < 1e-4);
    assert!((shell.bg_color[3] - 1.0).abs() < 1e-4);
    assert_eq!(shell.border_color, [0.5, 0.5, 0.5, 0.20]);
    assert!(shell.with_resize_handles);
    assert_eq!(shell.shadow_pad, 4.0);
}

#[test]
fn build_shell_emits_shadow_bg_border_and_handles() {
    let mut verts = Vec::new();
    let shell = CommandSurfaceShell::canonical(
        [10.0, 10.0, 110.0, 60.0],
        4.0,
        true,
        [0.2, 0.2, 0.2, 1.0],
        [0.0, 0.0, 1.0, 1.0],
    );
    build_command_surface_shell(&mut verts, shell);

    // Each push_quad emits 6 verts × 12 floats = 72 floats.
    // Expected quads: 1 shadow + 1 bg + 4 border edges = 6 quads.
    // Plus resize handles: 4 triangles (3 verts × 12 floats = 36) +
    // 2 connecting lines (quads, 72 each). Total handle floats:
    // 4*36 + 2*72 = 144 + 144 = 288.
    // Shell quads: 6 * 72 = 432. Total = 432 + 288 = 720.
    assert_eq!(verts.len(), 720);

    // Shadow pad expands the rect by 4 on each side.
    // Verify the first quad's first vertex is the shadow top-left.
    assert_eq!(verts[0], 10.0 - 4.0); // x0 - shadow_pad
    assert_eq!(verts[1], 10.0 - 4.0); // y0 - shadow_pad
}

#[test]
fn build_shell_skips_shadow_when_pad_zero() {
    let mut verts_with = Vec::new();
    let mut verts_without = Vec::new();
    build_command_surface_shell(
        &mut verts_with,
        CommandSurfaceShell::canonical(
            [0.0, 0.0, 100.0, 50.0],
            4.0,
            false,
            [0.2, 0.2, 0.2, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ),
    );
    build_command_surface_shell(
        &mut verts_without,
        CommandSurfaceShell::canonical(
            [0.0, 0.0, 100.0, 50.0],
            0.0,
            false,
            [0.2, 0.2, 0.2, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ),
    );
    // The version with shadow has one extra quad (the shadow).
    assert_eq!(verts_with.len() - verts_without.len(), 72);
}

#[test]
fn resize_handles_emit_expected_vertex_count() {
    let mut verts = Vec::new();
    build_command_surface_resize_handles(&mut verts, [0.0, 0.0, 100.0, 50.0], [0.0; 4]);
    // 4 triangles (3 verts × 12 floats = 36) + 2 quads (6 verts × 12 = 72)
    // = 4*36 + 2*72 = 144 + 144 = 288.
    assert_eq!(verts.len(), 288);
}

// ── Row state ──────────────────────────────────────────────────────

#[test]
fn row_state_plain_has_no_background() {
    let state = CommandSurfaceRowState::default();
    assert_eq!(state.background([0.2; 4], [0.8; 4]), None);
}

#[test]
fn row_state_selected_blends_accent_with_bg() {
    let state = CommandSurfaceRowState {
        selected: true,
        ..Default::default()
    };
    let bg = [0.2, 0.2, 0.2, 1.0];
    let accent = [0.8, 0.8, 0.8, 1.0];
    let color = state.background(bg, accent).unwrap();
    // accent*0.35 + bg*0.65 = 0.8*0.35 + 0.2*0.65 = 0.28 + 0.13 = 0.41
    assert!((color[0] - 0.41).abs() < 1e-4);
    assert_eq!(color[3], 1.0);
}

#[test]
fn row_state_hovered_lifts_bg_slightly() {
    let state = CommandSurfaceRowState {
        hovered: true,
        ..Default::default()
    };
    let bg = [0.2, 0.2, 0.2, 1.0];
    let color = state.background(bg, [0.8; 4]).unwrap();
    // bg + (1-bg)*0.04 = 0.2 + 0.8*0.04 = 0.232
    assert!((color[0] - 0.232).abs() < 1e-4);
}

#[test]
fn row_state_disabled_wins_over_selected_and_hovered() {
    let state = CommandSurfaceRowState {
        selected: true,
        hovered: true,
        disabled: true,
    };
    let bg = [0.2, 0.2, 0.2, 1.0];
    let color = state.background(bg, [0.8; 4]).unwrap();
    // disabled formula: bg*0.6 + 0.2 = 0.12 + 0.2 = 0.32
    assert!((color[0] - 0.32).abs() < 1e-4);
    // Must NOT match the selected or hover formula.
    let selected_color = CommandSurfaceRowState {
        selected: true,
        ..Default::default()
    }
    .background(bg, [0.8; 4])
    .unwrap();
    assert_ne!(color, selected_color);
}

#[test]
fn build_row_bg_emits_quad_only_when_state_has_background() {
    let bg_uv = [0.0, 0.0, 1.0, 1.0];
    let theme_bg = [0.2, 0.2, 0.2, 1.0];
    let accent = [0.8, 0.8, 0.8, 1.0];

    // Plain row → no quad.
    let mut verts = Vec::new();
    build_command_surface_row_bg(
        &mut verts,
        [0.0, 0.0, 100.0, 20.0],
        CommandSurfaceRowState::default(),
        theme_bg,
        accent,
        bg_uv,
    );
    assert!(verts.is_empty());

    // Selected row → one quad (72 floats).
    let mut verts = Vec::new();
    build_command_surface_row_bg(
        &mut verts,
        [0.0, 0.0, 100.0, 20.0],
        CommandSurfaceRowState {
            selected: true,
            ..Default::default()
        },
        theme_bg,
        accent,
        bg_uv,
    );
    assert_eq!(verts.len(), 72);
    // Inset by 1px on left/right.
    assert_eq!(verts[0], 1.0); // x0 + 1
}

// ── State ──────────────────────────────────────────────────────────

#[test]
fn state_status_text_matches_each_variant() {
    assert_eq!(CommandSurfaceState::Ready.status_text(), "");
    assert_eq!(CommandSurfaceState::Loading.status_text(), "Loading…");
    assert_eq!(CommandSurfaceState::Empty.status_text(), "No results");
    assert_eq!(
        CommandSurfaceState::Error("bad regex".into()).status_text(),
        "Error: bad regex"
    );
    assert_eq!(
        CommandSurfaceState::Error(String::new()).status_text(),
        "Error"
    );
    assert_eq!(CommandSurfaceState::Disabled.status_text(), "Unavailable");
}

#[test]
fn state_shows_results_only_when_ready() {
    assert!(CommandSurfaceState::Ready.shows_results());
    assert!(!CommandSurfaceState::Loading.shows_results());
    assert!(!CommandSurfaceState::Empty.shows_results());
    assert!(!CommandSurfaceState::Error("x".into()).shows_results());
    assert!(!CommandSurfaceState::Disabled.shows_results());
}

#[test]
fn state_is_error_only_for_error_variant() {
    assert!(!CommandSurfaceState::Ready.is_error());
    assert!(!CommandSurfaceState::Loading.is_error());
    assert!(!CommandSurfaceState::Empty.is_error());
    assert!(CommandSurfaceState::Error("x".into()).is_error());
    assert!(!CommandSurfaceState::Disabled.is_error());
}

#[test]
fn find_surface_state_regex_error_takes_priority() {
    let state = find_surface_state("foo", false, 5, Some("unbalanced paren"));
    assert_eq!(state, CommandSurfaceState::Error("unbalanced paren".into()));
}

#[test]
fn find_surface_state_empty_query_is_ready() {
    let state = find_surface_state("", false, 0, None);
    assert_eq!(state, CommandSurfaceState::Ready);
}

#[test]
fn find_surface_state_busy_is_loading() {
    let state = find_surface_state("foo", true, 0, None);
    assert_eq!(state, CommandSurfaceState::Loading);
}

#[test]
fn find_surface_state_no_matches_is_empty() {
    let state = find_surface_state("foo", false, 0, None);
    assert_eq!(state, CommandSurfaceState::Empty);
}

#[test]
fn find_surface_state_matches_is_ready() {
    let state = find_surface_state("foo", false, 3, None);
    assert_eq!(state, CommandSurfaceState::Ready);
}

#[test]
fn palette_surface_state_empty_query_with_results_is_ready() {
    assert_eq!(
        palette_surface_state("", 4, true),
        CommandSurfaceState::Ready
    );
}

#[test]
fn palette_surface_state_query_no_results_is_empty() {
    assert_eq!(
        palette_surface_state("zzz", 0, true),
        CommandSurfaceState::Empty
    );
}

#[test]
fn palette_surface_state_no_store_still_ready_for_builtins() {
    assert_eq!(
        palette_surface_state("", 0, false),
        CommandSurfaceState::Ready
    );
}

#[test]
fn completion_surface_state_empty_when_no_matches() {
    assert_eq!(completion_surface_state(0), CommandSurfaceState::Empty);
    assert_eq!(completion_surface_state(5), CommandSurfaceState::Ready);
}

// ── Keyboard protocol ──────────────────────────────────────────────

#[test]
fn resolve_key_returns_unhandled_for_modifier_chords() {
    assert_eq!(
        resolve_command_surface_key(KeyCode::Enter, Modifiers::SUPER),
        CommandSurfaceKeyAction::Unhandled
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Up, Modifiers::CONTROL),
        CommandSurfaceKeyAction::Unhandled
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Down, Modifiers::ALT),
        CommandSurfaceKeyAction::Unhandled
    );
}

#[test]
fn resolve_key_maps_navigation_keys() {
    assert_eq!(
        resolve_command_surface_key(KeyCode::Escape, Modifiers::empty()),
        CommandSurfaceKeyAction::Cancel
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Up, Modifiers::empty()),
        CommandSurfaceKeyAction::MoveUp
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Down, Modifiers::empty()),
        CommandSurfaceKeyAction::MoveDown
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::PageUp, Modifiers::empty()),
        CommandSurfaceKeyAction::PageUp
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::PageDown, Modifiers::empty()),
        CommandSurfaceKeyAction::PageDown
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Enter, Modifiers::empty()),
        CommandSurfaceKeyAction::Accept
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Tab, Modifiers::empty()),
        CommandSurfaceKeyAction::CycleFocus
    );
}

#[test]
fn resolve_key_shift_does_not_block_navigation() {
    // Shift is allowed through — surfaces decide (Shift+Enter = prev).
    assert_eq!(
        resolve_command_surface_key(KeyCode::Enter, Modifiers::SHIFT),
        CommandSurfaceKeyAction::Accept
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Tab, Modifiers::SHIFT),
        CommandSurfaceKeyAction::CycleFocus
    );
}

#[test]
fn resolve_key_unhandled_for_printable_and_misc() {
    assert_eq!(
        resolve_command_surface_key(KeyCode::Char('a'), Modifiers::empty()),
        CommandSurfaceKeyAction::Unhandled
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Backspace, Modifiers::empty()),
        CommandSurfaceKeyAction::Unhandled
    );
    assert_eq!(
        resolve_command_surface_key(KeyCode::Left, Modifiers::empty()),
        CommandSurfaceKeyAction::Unhandled
    );
}

#[test]
fn apply_page_selection_forward_advances_by_page_size() {
    // selection=5, len=20, page_size=8 → 5+8=13
    assert_eq!(apply_page_selection(5, 20, 8, true), 13);
}

#[test]
fn apply_page_selection_backward_decreases_by_page_size() {
    // selection=13, len=20, page_size=8 → 13-8=5
    assert_eq!(apply_page_selection(13, 20, 8, false), 5);
}

#[test]
fn apply_page_selection_clamps_to_last() {
    // selection=18, len=20, page_size=8 → 18+8=26 → clamp to 19
    assert_eq!(apply_page_selection(18, 20, 8, true), 19);
}

#[test]
fn apply_page_selection_clamps_to_first() {
    // selection=3, len=20, page_size=8 → 3-8 → 0
    assert_eq!(apply_page_selection(3, 20, 8, false), 0);
}

#[test]
fn apply_page_selection_zero_len_returns_zero() {
    assert_eq!(apply_page_selection(5, 0, 8, true), 0);
    assert_eq!(apply_page_selection(5, 0, 8, false), 0);
}

#[test]
fn apply_page_selection_step_is_at_least_one() {
    // page_size=0 → step clamps to 1.
    assert_eq!(apply_page_selection(5, 20, 0, true), 6);
    assert_eq!(apply_page_selection(5, 20, 0, false), 4);
}

// ── Focus restore ──────────────────────────────────────────────────

#[test]
fn compute_focus_returns_none_when_nothing_active() {
    assert_eq!(
        compute_current_focus(false, false, false, false, false, false, false, 0),
        None
    );
}

#[test]
fn compute_focus_palette_wins_over_settings_and_find() {
    let focus = compute_current_focus(true, true, true, false, false, true, false, 2);
    assert_eq!(focus, Some(FocusId::PaletteQuery));
}

#[test]
fn compute_focus_settings_when_palette_closed() {
    let focus = compute_current_focus(false, true, true, false, false, false, false, 1);
    assert_eq!(focus, Some(FocusId::Settings));
}

#[test]
fn compute_focus_find_when_no_palette_or_settings() {
    let focus = compute_current_focus(false, false, true, false, false, false, false, 0);
    assert_eq!(focus, Some(FocusId::FindQuery));
}

#[test]
fn compute_focus_context_menu_beats_panel_and_editor() {
    let focus = compute_current_focus(false, false, false, true, true, true, false, 3);
    assert_eq!(focus, Some(FocusId::ContextMenu));
}

#[test]
fn compute_focus_panel_search_when_no_modal() {
    let focus = compute_current_focus(false, false, false, false, true, true, false, 1);
    assert_eq!(focus, Some(FocusId::SidebarSearch));
}

#[test]
fn compute_focus_completion_when_editor_and_popup_active() {
    let focus = compute_current_focus(false, false, false, false, false, true, true, 0);
    assert_eq!(focus, Some(FocusId::Completion));
}

#[test]
fn compute_focus_tab_index_when_only_editor_active() {
    let focus = compute_current_focus(false, false, false, false, false, true, false, 4);
    assert_eq!(focus, Some(FocusId::Tab(4)));
}

#[test]
fn save_focus_keeps_existing_prev_when_already_saved() {
    // Re-entry into a second modal must not overwrite the original focus.
    let prev = Some(FocusId::Tab(0));
    let result = save_focus_for_modal(Some(FocusId::FindQuery), prev, FocusId::PaletteQuery);
    assert_eq!(result, Some(FocusId::Tab(0)));
}

#[test]
fn save_focus_returns_prev_when_current_differs_from_opening() {
    let result = save_focus_for_modal(Some(FocusId::Tab(2)), None, FocusId::PaletteQuery);
    assert_eq!(result, Some(FocusId::Tab(2)));
}

#[test]
fn save_focus_returns_none_when_current_equals_opening() {
    // Opening the same surface again (e.g. re-focusing palette while
    // already open) shouldn't save palette as its own prev.
    let result = save_focus_for_modal(Some(FocusId::PaletteQuery), None, FocusId::PaletteQuery);
    assert_eq!(result, None);
}

#[test]
fn save_focus_returns_none_when_no_current_focus() {
    let result = save_focus_for_modal(None, None, FocusId::Settings);
    assert_eq!(result, None);
}

// ── F6: Focus scope stack ─────────────────────────────────────────

#[test]
fn current_focus_scope_defaults_to_terminal_when_empty() {
    assert_eq!(current_focus_scope(&[]), FocusScope::Terminal);
}

#[test]
fn current_focus_scope_returns_last_pushed() {
    let stack = vec![FocusScope::Terminal, FocusScope::Modal];
    assert_eq!(current_focus_scope(&stack), FocusScope::Modal);
}

#[test]
fn push_focus_scope_appends_scope() {
    let stack = push_focus_scope(vec![], FocusScope::Modal);
    assert_eq!(stack, vec![FocusScope::Modal]);
    let stack = push_focus_scope(stack, FocusScope::Sidebar);
    assert_eq!(stack, vec![FocusScope::Modal, FocusScope::Sidebar]);
}

#[test]
fn pop_focus_scope_returns_last_and_remaining() {
    let stack = vec![FocusScope::Terminal, FocusScope::Modal];
    let (popped, remaining) = pop_focus_scope(stack);
    assert_eq!(popped, Some(FocusScope::Modal));
    assert_eq!(remaining, vec![FocusScope::Terminal]);
}

#[test]
fn pop_focus_scope_on_empty_returns_none() {
    let (popped, remaining) = pop_focus_scope(vec![]);
    assert_eq!(popped, None);
    assert!(remaining.is_empty());
}

// ── F6: cycle_focus ───────────────────────────────────────────────

#[test]
fn cycle_focus_forward_wraps_around() {
    let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::FindQuery), true),
        Some(FocusId::PaletteQuery)
    );
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::PaletteQuery), true),
        Some(FocusId::FindQuery) // wraps
    );
}

#[test]
fn cycle_focus_backward_wraps_around() {
    let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::FindQuery), false),
        Some(FocusId::PaletteQuery) // wraps backward
    );
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::PaletteQuery), false),
        Some(FocusId::FindQuery)
    );
}

#[test]
fn cycle_focus_single_candidate_returns_it() {
    let cands = [FocusId::Settings];
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Settings), true),
        Some(FocusId::Settings)
    );
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Settings), false),
        Some(FocusId::Settings)
    );
}

#[test]
fn cycle_focus_empty_returns_none() {
    let cands: [FocusId; 0] = [];
    assert_eq!(cycle_focus(&cands, Some(FocusId::FindQuery), true), None);
    assert_eq!(cycle_focus(&cands, None, false), None);
}

#[test]
fn cycle_focus_current_not_in_list_starts_at_first_or_last() {
    let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
    // Current not in list → forward starts at first (index 0).
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Settings), true),
        Some(FocusId::FindQuery)
    );
    // Current not in list → backward starts at last.
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Settings), false),
        Some(FocusId::PaletteQuery)
    );
}

#[test]
fn cycle_focus_none_current_forward_starts_at_first() {
    let cands = [FocusId::FindQuery, FocusId::PaletteQuery];
    assert_eq!(cycle_focus(&cands, None, true), Some(FocusId::FindQuery));
}

#[test]
fn cycle_focus_three_candidates_cycles_in_order() {
    let cands = [FocusId::Tab(0), FocusId::Completion, FocusId::SidebarSearch];
    // Forward: 0 → 1 → 2 → 0
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Tab(0)), true),
        Some(FocusId::Completion)
    );
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::Completion), true),
        Some(FocusId::SidebarSearch)
    );
    assert_eq!(
        cycle_focus(&cands, Some(FocusId::SidebarSearch), true),
        Some(FocusId::Tab(0))
    );
}
