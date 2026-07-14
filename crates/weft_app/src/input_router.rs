//! Pure focus routing for keyboard input.
//!
//! The handlers still live on `App` during the incremental migration, but
//! ownership priority is defined and tested here instead of being inferred
//! from the order of unrelated `if` statements.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayInputOwner {
    Palette,
    Settings,
    Find,
    ContextMenu,
    PanelSearch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayInputContext {
    pub(crate) palette_open: bool,
    pub(crate) settings_open: bool,
    pub(crate) find_open: bool,
    pub(crate) context_menu_open: bool,
    pub(crate) panel_search_focused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModalMouseRoute {
    PaletteLeft,
    SettingsLeft,
    ContextMenuLeft,
    DismissContextMenu,
    Consume,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlobalActionOverlayRoute {
    DismissContextMenu,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionInputRoute {
    Dispatch,
    Consume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyboardEntryRoute {
    Action(weft_core::config::Action),
    Session,
    Consume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnedPointerMoveRoute {
    ActiveSession,
    TerminalOwner(u64),
    Suppress,
}

/// Tracks mouse buttons whose press was owned by a modal surface. A modal can
/// close while handling that press, but ownership must remain with it until
/// the matching physical release so a partial gesture never reaches the PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseGestureOwner {
    Modal,
    TerminalSession(u64),
    LocalSession(u64),
    Suppressed,
}

#[derive(Debug, Default)]
pub(crate) struct ModalMouseCapture {
    active_gestures: Vec<(winit::event::MouseButton, MouseGestureOwner)>,
    suppressed_releases: Vec<winit::event::MouseButton>,
}

impl ModalMouseCapture {
    pub(crate) fn capture_press(
        &mut self,
        modal_owned: bool,
        button: winit::event::MouseButton,
    ) -> bool {
        // A new physical press proves any release tombstone for this button
        // is stale (macOS omitted the old release while unfocused).
        self.suppressed_releases
            .retain(|suppressed| *suppressed != button);
        if modal_owned {
            if let Some((_, owner)) = self
                .active_gestures
                .iter_mut()
                .find(|(captured, _)| *captured == button)
            {
                *owner = MouseGestureOwner::Modal;
            } else {
                self.active_gestures
                    .push((button, MouseGestureOwner::Modal));
            }
        }
        modal_owned
    }

    pub(crate) fn capture_terminal_press(
        &mut self,
        button: winit::event::MouseButton,
        session_id: u64,
        pty_press_sent: bool,
    ) {
        self.suppressed_releases
            .retain(|suppressed| *suppressed != button);
        let owner = if pty_press_sent {
            MouseGestureOwner::TerminalSession(session_id)
        } else {
            MouseGestureOwner::LocalSession(session_id)
        };
        if let Some((_, current)) = self
            .active_gestures
            .iter_mut()
            .find(|(captured, _)| *captured == button)
        {
            if *current != MouseGestureOwner::Modal {
                *current = owner;
            }
        } else {
            self.active_gestures.push((button, owner));
        }
    }

    pub(crate) fn consume_release(
        &mut self,
        button: winit::event::MouseButton,
    ) -> Option<MouseGestureOwner> {
        if let Some(index) = self
            .active_gestures
            .iter()
            .position(|(captured, _)| *captured == button)
        {
            let (_, owner) = self.active_gestures.swap_remove(index);
            return Some(owner);
        }
        if let Some(index) = self
            .suppressed_releases
            .iter()
            .position(|captured| *captured == button)
        {
            self.suppressed_releases.swap_remove(index);
            return Some(MouseGestureOwner::Suppressed);
        }
        None
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active_gestures
            .iter()
            .any(|(_, owner)| *owner == MouseGestureOwner::Modal)
    }

    pub(crate) fn terminal_move_owner(
        &self,
    ) -> Option<(winit::event::MouseButton, MouseGestureOwner)> {
        self.active_gestures
            .iter()
            .rev()
            .find_map(|(button, owner)| match owner {
                MouseGestureOwner::TerminalSession(_) | MouseGestureOwner::LocalSession(_) => {
                    Some((*button, *owner))
                }
                MouseGestureOwner::Modal | MouseGestureOwner::Suppressed => None,
            })
    }

    /// Stop local/modal pointer motion after focus loss while retaining their
    /// release tombstones. A press already written to a PTY keeps its stable
    /// Session owner so a late release can still be delivered to the same TUI.
    pub(crate) fn suspend_active(&mut self) {
        let mut retained = Vec::new();
        for (button, owner) in self.active_gestures.drain(..) {
            if matches!(owner, MouseGestureOwner::TerminalSession(_)) {
                retained.push((button, owner));
            } else if !self.suppressed_releases.contains(&button) {
                self.suppressed_releases.push(button);
            }
        }
        self.active_gestures = retained;
    }
}

pub(crate) fn route_keyboard_entry(
    has_sessions: bool,
    has_terminal: bool,
    bound_action: Option<weft_core::config::Action>,
) -> KeyboardEntryRoute {
    if has_sessions && has_terminal {
        KeyboardEntryRoute::Session
    } else if let Some(action) = bound_action {
        KeyboardEntryRoute::Action(action)
    } else {
        KeyboardEntryRoute::Consume
    }
}

pub(crate) fn route_owned_pointer_move(
    active_session: Option<u64>,
    owner: Option<MouseGestureOwner>,
) -> OwnedPointerMoveRoute {
    match owner {
        Some(MouseGestureOwner::TerminalSession(owner)) if active_session != Some(owner) => {
            OwnedPointerMoveRoute::TerminalOwner(owner)
        }
        Some(MouseGestureOwner::LocalSession(owner)) if active_session != Some(owner) => {
            OwnedPointerMoveRoute::Suppress
        }
        Some(MouseGestureOwner::Modal | MouseGestureOwner::Suppressed) => {
            OwnedPointerMoveRoute::Suppress
        }
        _ => OwnedPointerMoveRoute::ActiveSession,
    }
}

pub(crate) fn route_session_input(has_sessions: bool) -> SessionInputRoute {
    if has_sessions {
        SessionInputRoute::Dispatch
    } else {
        SessionInputRoute::Consume
    }
}

pub(crate) fn route_session_action(
    has_sessions: bool,
    action: weft_core::config::Action,
) -> SessionInputRoute {
    if has_sessions || action == weft_core::config::Action::NewTab {
        SessionInputRoute::Dispatch
    } else {
        SessionInputRoute::Consume
    }
}

pub(crate) fn route_modal_pointer(
    palette_open: bool,
    settings_open: bool,
    context_menu_open: bool,
    modal_capture_active: bool,
    has_sessions: bool,
) -> SessionInputRoute {
    if palette_open || settings_open || context_menu_open || modal_capture_active || !has_sessions {
        SessionInputRoute::Consume
    } else {
        SessionInputRoute::Dispatch
    }
}

pub(crate) fn route_global_action(context_menu_open: bool) -> GlobalActionOverlayRoute {
    if context_menu_open {
        GlobalActionOverlayRoute::DismissContextMenu
    } else {
        GlobalActionOverlayRoute::Continue
    }
}

pub(crate) fn route_modal_mouse(
    palette_open: bool,
    settings_open: bool,
    context_menu_open: bool,
    button: winit::event::MouseButton,
) -> ModalMouseRoute {
    if palette_open {
        if button == winit::event::MouseButton::Left {
            ModalMouseRoute::PaletteLeft
        } else {
            ModalMouseRoute::Consume
        }
    } else if settings_open {
        if button == winit::event::MouseButton::Left {
            ModalMouseRoute::SettingsLeft
        } else {
            ModalMouseRoute::Consume
        }
    } else if context_menu_open {
        if button == winit::event::MouseButton::Left {
            ModalMouseRoute::ContextMenuLeft
        } else {
            ModalMouseRoute::DismissContextMenu
        }
    } else {
        ModalMouseRoute::Terminal
    }
}

impl OverlayInputOwner {
    pub(crate) fn resolve(context: OverlayInputContext) -> Option<Self> {
        if context.palette_open {
            Some(Self::Palette)
        } else if context.settings_open {
            Some(Self::Settings)
        } else if context.find_open {
            Some(Self::Find)
        } else if context.context_menu_open {
            Some(Self::ContextMenu)
        } else if context.panel_search_focused {
            Some(Self::PanelSearch)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        route_global_action, route_keyboard_entry, route_modal_mouse, route_modal_pointer,
        route_owned_pointer_move, route_session_action, route_session_input,
        GlobalActionOverlayRoute, KeyboardEntryRoute, ModalMouseCapture, ModalMouseRoute,
        MouseGestureOwner, OverlayInputContext, OverlayInputOwner, OwnedPointerMoveRoute,
        SessionInputRoute,
    };
    use weft_core::config::Action;
    use winit::event::MouseButton;

    #[test]
    fn modal_priority_is_deterministic_when_state_is_temporarily_inconsistent() {
        let all_open = OverlayInputContext {
            palette_open: true,
            settings_open: true,
            find_open: true,
            context_menu_open: true,
            panel_search_focused: true,
        };
        assert_eq!(
            OverlayInputOwner::resolve(all_open),
            Some(OverlayInputOwner::Palette)
        );

        let without_palette = OverlayInputContext {
            palette_open: false,
            ..all_open
        };
        assert_eq!(
            OverlayInputOwner::resolve(without_palette),
            Some(OverlayInputOwner::Settings)
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

    #[test]
    fn all_open_closed_combinations_follow_one_priority_order() {
        for bits in 0_u8..32 {
            let context = OverlayInputContext {
                palette_open: bits & 0b0001 != 0,
                settings_open: bits & 0b0010 != 0,
                find_open: bits & 0b0100 != 0,
                context_menu_open: bits & 0b1000 != 0,
                panel_search_focused: bits & 0b1_0000 != 0,
            };
            let expected = if context.palette_open {
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
                "bits={bits:05b}"
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
}
