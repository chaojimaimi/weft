use super::{
    cycle_list_selection, route_global_action, route_keyboard_entry, route_modal_mouse,
    route_modal_pointer, route_owned_pointer_move, route_session_action, route_session_input,
    GlobalActionOverlayRoute, KeyboardEntryRoute, ModalMouseCapture, ModalMouseRoute,
    MouseGestureOwner, OverlayInputContext, OverlayInputOwner, OwnedPointerMoveRoute,
    SessionInputRoute,
};

#[test]
fn modal_tab_selection_wraps_both_directions_and_clamps_stale_index() {
    assert_eq!(cycle_list_selection(2, 3, true), 0);
    assert_eq!(cycle_list_selection(0, 3, false), 2);
    assert_eq!(cycle_list_selection(usize::MAX, 3, true), 0);
    assert_eq!(cycle_list_selection(usize::MAX, 0, true), 0);
}
use weft_core::config::Action;
use winit::event::MouseButton;

#[test]
fn modal_priority_is_deterministic_when_state_is_temporarily_inconsistent() {
    let all_open = OverlayInputContext {
        note_editor_open: true,
        palette_open: true,
        settings_open: true,
        find_open: true,
        context_menu_open: true,
        panel_search_focused: true,
    };
    assert_eq!(
        OverlayInputOwner::resolve(all_open),
        Some(OverlayInputOwner::NoteEditor)
    );

    let without_note_editor = OverlayInputContext {
        note_editor_open: false,
        ..all_open
    };
    assert_eq!(
        OverlayInputOwner::resolve(without_note_editor),
        Some(OverlayInputOwner::Palette)
    );
}

#[test]
fn focused_panel_wins_only_without_modal_overlay() {
    let panel = OverlayInputContext {
        panel_search_focused: true,
        ..OverlayInputContext::default()
    };
    assert_eq!(
        OverlayInputOwner::resolve(panel),
        Some(OverlayInputOwner::PanelSearch)
    );
    assert_eq!(
        OverlayInputOwner::resolve(OverlayInputContext::default()),
        None
    );
}

/// v1.12.24 (N-1): the note editor is a focus-modal card — it owns input
/// even with every other overlay open (mirrors keyboard.rs's intercept
/// sitting before overlay routing).
#[test]
fn note_editor_owns_input_over_every_other_surface() {
    let note = OverlayInputContext {
        note_editor_open: true,
        ..OverlayInputContext::default()
    };
    assert_eq!(
        OverlayInputOwner::resolve(note),
        Some(OverlayInputOwner::NoteEditor)
    );
    let note_with_palette = OverlayInputContext {
        palette_open: true,
        ..note
    };
    assert_eq!(
        OverlayInputOwner::resolve(note_with_palette),
        Some(OverlayInputOwner::NoteEditor),
        "note editor must outrank an open palette"
    );
}

/// v1.12.26 (P1-01): exactly the four text-input overlays may wake the
/// blink timer — Settings/ContextMenu are consume-only and must stay
/// out, or a closed-over modal would keep the cursor timer alive.
#[test]
fn text_input_holders_lists_exactly_the_four_text_surfaces() {
    assert_eq!(
        OverlayInputOwner::TEXT_INPUT_HOLDERS,
        &[
            OverlayInputOwner::NoteEditor,
            OverlayInputOwner::Palette,
            OverlayInputOwner::Find,
            OverlayInputOwner::PanelSearch,
        ]
    );
    assert!(!OverlayInputOwner::TEXT_INPUT_HOLDERS.contains(&OverlayInputOwner::Settings));
    assert!(!OverlayInputOwner::TEXT_INPUT_HOLDERS.contains(&OverlayInputOwner::ContextMenu));
}

#[test]
fn all_open_closed_combinations_follow_one_priority_order() {
    for bits in 0_u16..64 {
        let context = OverlayInputContext {
            note_editor_open: bits & 0b00_0001 != 0,
            palette_open: bits & 0b00_0010 != 0,
            settings_open: bits & 0b00_0100 != 0,
            find_open: bits & 0b00_1000 != 0,
            context_menu_open: bits & 0b01_0000 != 0,
            panel_search_focused: bits & 0b10_0000 != 0,
        };
        let expected = if context.note_editor_open {
            Some(OverlayInputOwner::NoteEditor)
        } else if context.palette_open {
            Some(OverlayInputOwner::Palette)
        } else if context.settings_open {
            Some(OverlayInputOwner::Settings)
        } else if context.find_open {
            Some(OverlayInputOwner::Find)
        } else if context.context_menu_open {
            Some(OverlayInputOwner::ContextMenu)
        } else if context.panel_search_focused {
            Some(OverlayInputOwner::PanelSearch)
        } else {
            None
        };
        assert_eq!(
            OverlayInputOwner::resolve(context),
            expected,
            "bits={bits:06b}"
        );
    }
}

#[test]
fn modal_mouse_routing_blocks_terminal_and_prevents_overlay_coexistence() {
    assert_eq!(
        route_modal_mouse(true, false, false, MouseButton::Left),
        ModalMouseRoute::PaletteLeft
    );
    for button in [
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Other(8),
    ] {
        assert_eq!(
            route_modal_mouse(true, false, false, button),
            ModalMouseRoute::Consume
        );
    }
    assert_eq!(
        route_modal_mouse(false, true, true, MouseButton::Left),
        ModalMouseRoute::SettingsLeft
    );
    for button in [
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Other(8),
    ] {
        assert_eq!(
            route_modal_mouse(false, true, true, button),
            ModalMouseRoute::Consume
        );
    }
    assert_eq!(
        route_modal_mouse(false, false, true, MouseButton::Left),
        ModalMouseRoute::ContextMenuLeft
    );
    for button in [
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Other(8),
    ] {
        assert_eq!(
            route_modal_mouse(false, false, true, button),
            ModalMouseRoute::DismissContextMenu
        );
    }
    assert_eq!(
        route_modal_mouse(false, false, false, MouseButton::Right),
        ModalMouseRoute::Terminal
    );
    assert_eq!(
        route_modal_pointer(true, false, false, false, true),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_modal_pointer(false, true, false, false, true),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_modal_pointer(false, false, true, false, true),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_modal_pointer(false, false, false, false, true),
        SessionInputRoute::Dispatch
    );
}

#[test]
fn modal_mouse_capture_owns_the_complete_press_move_release_sequence() {
    let mut capture = ModalMouseCapture::default();

    assert!(capture.capture_press(true, MouseButton::Left));
    assert!(capture.is_active());

    // The press may close the modal. Motion and wheel routing must still
    // stay captured until the matching physical release arrives.
    assert_eq!(
        route_modal_pointer(false, false, false, capture.is_active(), true),
        SessionInputRoute::Consume
    );
    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::Modal)
    );
    assert!(!capture.is_active());
    assert_eq!(
        route_modal_pointer(false, false, false, capture.is_active(), true),
        SessionInputRoute::Dispatch
    );

    // Captures are per button: releasing a different button cannot end
    // ownership of the original modal gesture.
    assert!(capture.capture_press(true, MouseButton::Right));
    assert_eq!(capture.consume_release(MouseButton::Left), None);
    assert!(capture.is_active());
    assert_eq!(
        capture.consume_release(MouseButton::Right),
        Some(MouseGestureOwner::Modal)
    );
    assert!(!capture.is_active());
}

#[test]
fn modal_opened_by_the_press_captures_that_press_retroactively() {
    let mut capture = ModalMouseCapture::default();

    assert!(!capture.capture_press(false, MouseButton::Right));
    // The right-press handler opens ContextMenu without sending a PTY
    // press, so ownership is recorded after the handler returns.
    assert!(capture.capture_press(true, MouseButton::Right));
    assert!(capture.capture_press(true, MouseButton::Left));
    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::Modal)
    );
    assert!(capture.is_active());
    assert_eq!(
        capture.consume_release(MouseButton::Right),
        Some(MouseGestureOwner::Modal)
    );
    assert!(!capture.is_active());
}

#[test]
fn focus_loss_tombstones_captured_buttons_until_their_late_release() {
    let mut capture = ModalMouseCapture::default();
    assert!(capture.capture_press(true, MouseButton::Left));

    capture.suspend_active();
    assert!(!capture.is_active());
    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::Suppressed)
    );
    assert_eq!(capture.consume_release(MouseButton::Left), None);

    // If macOS omitted the old release, a later physical press proves
    // that stale tombstone is no longer relevant. Otherwise the new
    // terminal press would lose its matching release.
    assert!(capture.capture_press(true, MouseButton::Left));
    capture.suspend_active();
    assert!(!capture.capture_press(false, MouseButton::Left));
    assert_eq!(capture.consume_release(MouseButton::Left), None);
}

#[test]
fn terminal_gesture_release_keeps_the_press_session_owner() {
    let mut capture = ModalMouseCapture::default();
    capture.capture_terminal_press(MouseButton::Left, 41, true);

    assert_eq!(
        capture.terminal_move_owner(),
        Some((MouseButton::Left, MouseGestureOwner::TerminalSession(41)))
    );

    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::TerminalSession(41))
    );
    assert_eq!(capture.consume_release(MouseButton::Left), None);
}

#[test]
fn focus_loss_preserves_a_pty_press_owner_for_the_late_release() {
    let mut capture = ModalMouseCapture::default();
    capture.capture_terminal_press(MouseButton::Left, 41, true);

    capture.suspend_active();

    assert_eq!(
        capture.terminal_move_owner(),
        Some((MouseButton::Left, MouseGestureOwner::TerminalSession(41)))
    );
    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::TerminalSession(41))
    );
}

#[test]
fn move_owner_preserves_right_and_middle_buttons_and_local_press_state() {
    let mut capture = ModalMouseCapture::default();
    capture.capture_terminal_press(MouseButton::Right, 7, true);
    assert_eq!(
        capture.terminal_move_owner(),
        Some((MouseButton::Right, MouseGestureOwner::TerminalSession(7)))
    );

    capture.capture_terminal_press(MouseButton::Middle, 9, false);
    assert_eq!(
        capture.terminal_move_owner(),
        Some((MouseButton::Middle, MouseGestureOwner::LocalSession(9)))
    );
    assert_eq!(
        capture.consume_release(MouseButton::Middle),
        Some(MouseGestureOwner::LocalSession(9))
    );

    capture.capture_terminal_press(MouseButton::Left, 11, false);
    capture.capture_terminal_press(MouseButton::Left, 11, true);
    assert_eq!(
        capture.terminal_move_owner(),
        Some((MouseButton::Left, MouseGestureOwner::TerminalSession(11)))
    );
    assert!(capture.capture_press(true, MouseButton::Left));
    assert_eq!(
        capture.consume_release(MouseButton::Left),
        Some(MouseGestureOwner::Modal)
    );
}

#[test]
fn local_and_pty_move_routing_never_falls_through_to_a_different_session() {
    assert_eq!(
        route_owned_pointer_move(Some(2), Some(MouseGestureOwner::LocalSession(1))),
        OwnedPointerMoveRoute::Suppress
    );
    assert_eq!(
        route_owned_pointer_move(Some(2), Some(MouseGestureOwner::TerminalSession(1))),
        OwnedPointerMoveRoute::TerminalOwner(1)
    );
    assert_eq!(
        route_owned_pointer_move(None, Some(MouseGestureOwner::TerminalSession(1))),
        OwnedPointerMoveRoute::TerminalOwner(1)
    );
    assert_eq!(
        route_owned_pointer_move(Some(1), Some(MouseGestureOwner::LocalSession(1))),
        OwnedPointerMoveRoute::ActiveSession
    );
}

#[test]
fn global_actions_dismiss_context_menu_before_tab_or_session_mutation() {
    assert_eq!(
        route_global_action(true),
        GlobalActionOverlayRoute::DismissContextMenu
    );
    assert_eq!(
        route_global_action(false),
        GlobalActionOverlayRoute::Continue
    );
}

#[test]
fn empty_session_policy_covers_keyboard_mouse_and_native_menu_recovery() {
    assert_eq!(route_session_input(false), SessionInputRoute::Consume);
    assert_eq!(
        route_modal_pointer(false, false, false, false, false),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_session_action(false, Action::Copy),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_session_action(false, Action::ToggleSettings),
        SessionInputRoute::Consume
    );
    assert_eq!(
        route_session_action(false, Action::NewTab),
        SessionInputRoute::Dispatch
    );
    assert_eq!(route_session_input(true), SessionInputRoute::Dispatch);
}

#[test]
fn keyboard_entry_reaches_new_tab_binding_before_empty_session_guard() {
    assert_eq!(
        route_keyboard_entry(false, false, Some(Action::NewTab)),
        KeyboardEntryRoute::Action(Action::NewTab)
    );
    assert_eq!(
        route_keyboard_entry(false, false, Some(Action::Copy)),
        KeyboardEntryRoute::Action(Action::Copy)
    );
    assert_eq!(
        route_keyboard_entry(false, false, None),
        KeyboardEntryRoute::Consume
    );
    assert_eq!(
        route_keyboard_entry(true, true, Some(Action::NewTab)),
        KeyboardEntryRoute::Session
    );
}
