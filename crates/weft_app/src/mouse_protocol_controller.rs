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
            let session_id = self.sessions.active().session_id;
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
        &self,
        tab: usize,
        button: MouseButton,
        action: MouseAction,
        pos: GridPos,
    ) -> bool {
        let Some(session) = self.sessions.tab(tab) else {
            return false;
        };
        let Some(terminal) = session.terminal.as_ref() else {
            return false;
        };
        if terminal.mouse_protocol == MouseProtocol::Off {
            return false;
        }
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
            if let Some(pty) = &session.pty {
                return pty.write_sync(&bytes).is_ok();
            }
        }
        false
    }
}
