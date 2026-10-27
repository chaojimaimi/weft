//! Pure focus routing for keyboard input.
//!
//! The handlers still live on `App` during the incremental migration, but
//! ownership priority is defined and tested here instead of being inferred
//! from the order of unrelated `if` statements.

/// Move one item with wraparound. Used by Tab/Shift+Tab on command surfaces
/// so modal keyboard ownership never escapes to the terminal editor.
pub(crate) fn cycle_list_selection(selection: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    let current = selection.min(len - 1);
    if forward {
        (current + 1) % len
    } else {
        current.checked_sub(1).unwrap_or(len - 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayInputOwner {
    // v1.12.24 (N-1): the note editor is a focus-modal card — highest
    // priority, mirroring keyboard.rs's "note_editor.open captures all keys"
    // intercept (which sits before overlay routing and keybindings).
    NoteEditor,
    Palette,
    Settings,
    Find,
    ContextMenu,
    PanelSearch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayInputContext {
    pub(crate) note_editor_open: bool,
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
    /// v1.12.26 (P1-01): the overlay owners that host a text input with a
    /// blinking caret (palette query / find bar / panel search / note
    /// editor). While one of these is open, the cursor-blink timer thread
    /// must keep waking the loop so the overlay's own caret keeps blinking
    /// regardless of the terminal cursor's state — the old terminal-only
    /// wake terms froze the shared blink phase whenever a command was
    /// executing or the cursor style wasn't blinking. Settings and
    /// ContextMenu are consume-only surfaces (no text input) and must NOT
    /// wake the timer.
    pub(crate) const TEXT_INPUT_HOLDERS: &'static [OverlayInputOwner] = &[
        OverlayInputOwner::NoteEditor,
        OverlayInputOwner::Palette,
        OverlayInputOwner::Find,
        OverlayInputOwner::PanelSearch,
    ];

    pub(crate) fn resolve(context: OverlayInputContext) -> Option<Self> {
        // v1.12.24 (N-1): NoteEditor first — the focus-modal card presses
        // every other surface (palette/find/settings/menu/panel).
        if context.note_editor_open {
            Some(Self::NoteEditor)
        } else if context.palette_open {
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
#[path = "input_router/tests.rs"]
mod tests;
