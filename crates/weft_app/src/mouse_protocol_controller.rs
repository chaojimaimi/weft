//! PTY mouse-protocol ownership and encoding.

use super::*;

impl App {
    /// Send a mouse event to the PTY that owns the physical gesture. Presses
    /// establish a stable Session ID; moves keep that owner across Tab index
    /// changes until the matching release is routed by the window controller.
    pub(super) fn send_mouse_event(
        &mut self,
        mut button: MouseButton,
        action: MouseAction,
        pos: GridPos,
    ) {
        let mut physical_button = match button {
            MouseButton::Left => winit::event::MouseButton::Left,
            MouseButton::Middle => winit::event::MouseButton::Middle,
            MouseButton::Right => winit::event::MouseButton::Right,
        };
        let tab = if action == MouseAction::Move {
            match self.interaction.modal_mouse_capture.terminal_move_owner() {
                Some((
                    owner_button,
                    crate::input_router::MouseGestureOwner::TerminalSession(session_id),
                )) => {
                    let Some(tab) = self.sessions.tab_index_by_session_id(session_id) else {
                        return;
                    };
                    physical_button = owner_button;
                    button = match owner_button {
                        winit::event::MouseButton::Left => MouseButton::Left,
                        winit::event::MouseButton::Middle => MouseButton::Middle,
                        winit::event::MouseButton::Right => MouseButton::Right,
                        winit::event::MouseButton::Back
                        | winit::event::MouseButton::Forward
                        | winit::event::MouseButton::Other(_) => return,
                    };
                    tab
                }
                Some((_, crate::input_router::MouseGestureOwner::LocalSession(_))) => return,
                None => self.sessions.active_idx(),
                Some((_, _)) => return,
            }
        } else {
            self.sessions.active_idx()
        };
        if action == MouseAction::Press {
            // v1.12.25 (audit 3-B, P1-01): no active session on the empty-tabs
            // transient — nothing to capture, ignore the press.
            let Some(session_id) = self.sessions.active().map(|tab| tab.session_id) else {
                return;
            };
            let pty_press_sent = self.send_mouse_event_to_session(tab, button, action, pos);
            self.interaction.modal_mouse_capture.capture_terminal_press(
                physical_button,
                session_id,
                pty_press_sent,
            );
            return;
        }
        self.send_mouse_event_to_session(tab, button, action, pos);
    }

    pub(super) fn send_mouse_event_to_session(
        &mut self,
        tab: usize,
        button: MouseButton,
        action: MouseAction,
        pos: GridPos,
    ) -> bool {
        {
            let Some(session) = self.sessions.tab(tab) else {
                return false;
            };
            let Some(terminal) = session.terminal.as_ref() else {
                return false;
            };
            // v1.11.15 (FIX A): the PTY reader saw this session's
            // mouse-disable sequence (or its exit) — stop feeding hover/report
            // bytes into a shell that may already be back in cooked mode.
            if session.mouse_suppressed() {
                return false;
            }
            if !terminal.accepts_mouse_reporting_input() {
                return false;
            }
        }
        // v1.11.15 (FIX D): per-gesture mode sync against the TARGET tab.
        // One call here closes the three existing gaps (release-only
        // gestures, the TerminalOwner move shortcut, non-active owner tabs)
        // because it reads this tab's terminal instead of relying on the
        // active-tab sync points elsewhere.
        if let Some(session) = self.sessions.tab_mut(tab) {
            session.sync_mouse_modes();
        }
        let Some(session) = self.sessions.tab(tab) else {
            return false;
        };
        let mut m = Modifiers::empty();
        if self.interaction.mods.state().shift_key() {
            m |= Modifiers::SHIFT;
        }
        if self.interaction.mods.state().alt_key() {
            m |= Modifiers::ALT;
        }
        if self.interaction.mods.state().control_key() {
            m |= Modifiers::CONTROL;
        }
        let bytes = session
            .input_handler
            .encode_mouse(button, action, pos.col, pos.row, m);
        if let Some(bytes) = bytes {
            return self
                .sessions
                .tab_mut(tab)
                .is_some_and(|session| session.write_user_input(&bytes).is_ok());
        }
        false
    }
}
