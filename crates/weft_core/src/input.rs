//! Keyboard and mouse input encoding for terminal emulators.
//!
//! Translates abstract key/mouse events into VT100/VT220 escape sequences
//! that can be sent to the PTY for the shell to interpret.

use bitflags::bitflags;

bitflags! {
    /// Keyboard modifier flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Modifiers: u8 {
        const SHIFT   = 0x01;
        const ALT     = 0x02;
        const CONTROL = 0x04;
        const SUPER   = 0x08;
    }
}

/// Physical key code, independent of platform key event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// Printable character key.
    Char(char),
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Escape.
    Escape,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Home key.
    Home,
    /// End key.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Delete (forward).
    Delete,
    /// Insert.
    Insert,
    /// Function key F1–F12.
    F(u8),
    /// Numpad key (0–9).
    Numpad(char),
}

/// Mouse button for terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Mouse action type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Move,
}

/// SGR mouse protocol mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No mouse protocol — mouse events not forwarded to PTY.
    Off,
    /// X10 mode: report on button press only (CSI M).
    X10,
    /// Normal tracking: report button press/release (CSI M).
    Normal,
    /// Button-event tracking: report press/release + motion with button held.
    ButtonEvent,
    /// Any-event tracking: report all mouse events.
    AnyEvent,
}

/// Encodes keyboard input into VT escape sequences for the PTY.
pub struct InputHandler {
    /// Whether to use application cursor key mode (DECCKM).
    /// When true, arrow keys send SS3 sequences (ESC O A/B/C/D).
    /// When false, arrow keys send CSI sequences (ESC [ A/B/C/D).
    pub app_cursor_keys: bool,
    /// Current mouse protocol mode.
    pub mouse_protocol: MouseProtocol,
    /// Mouse coordinate origin (0 or 1 based, SGR uses 1-based).
    mouse_coord_base: u8,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            app_cursor_keys: false,
            mouse_protocol: MouseProtocol::Off,
            mouse_coord_base: 1, // SGR uses 1-based
        }
    }

    /// Encode a key event into bytes to send to the PTY.
    pub fn encode_key(&self, key: KeyCode, mods: Modifiers) -> Vec<u8> {
        match key {
            KeyCode::Char(c) => self.encode_char(c, mods),
            KeyCode::Enter => self.encode_enter(mods),
            KeyCode::Backspace => self.encode_backspace(mods),
            KeyCode::Tab => self.encode_tab(mods),
            KeyCode::Escape => vec![0x1B],
            KeyCode::Up => self.encode_arrow('A', mods),
            KeyCode::Down => self.encode_arrow('B', mods),
            KeyCode::Right => self.encode_arrow('C', mods),
            KeyCode::Left => self.encode_arrow('D', mods),
            KeyCode::Home => self.encode_home(mods),
            KeyCode::End => self.encode_end(mods),
            KeyCode::PageUp => self.encode_page('H', mods),
            KeyCode::PageDown => self.encode_page('I', mods),
            KeyCode::Delete => self.encode_delete(mods),
            KeyCode::Insert => self.encode_insert(mods),
            KeyCode::F(n) => self.encode_function_key(n, mods),
            KeyCode::Numpad(c) => vec![c as u8],
        }
    }

    /// Encode a mouse event using SGR mouse protocol.
    /// Returns bytes to send to PTY, or None if mouse protocol is off.
    pub fn encode_mouse(
        &self,
        button: MouseButton,
        action: MouseAction,
        col: usize,
        row: usize,
        mods: Modifiers,
    ) -> Option<Vec<u8>> {
        if self.mouse_protocol == MouseProtocol::Off {
            return None;
        }

        // SGR mouse mode: CSI < Pb ; Px ; Py M (press) / m (release)
        let btn_code = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };

        // Add modifier bits
        let mut pb = btn_code;
        if mods.contains(Modifiers::SHIFT) {
            pb |= 4;
        }
        if mods.contains(Modifiers::ALT) {
            pb |= 8;
        }
        if mods.contains(Modifiers::CONTROL) {
            pb |= 16;
        }

        // Motion flag
        if action == MouseAction::Move {
            pb |= 32;
        }

        let suffix = match action {
            MouseAction::Press => 'M',
            MouseAction::Release => 'm',
            MouseAction::Move => {
                // Only send move events in ButtonEvent or AnyEvent mode
                match self.mouse_protocol {
                    MouseProtocol::ButtonEvent | MouseProtocol::AnyEvent => 'M',
                    _ => return None,
                }
            }
        };

        let px = col + self.mouse_coord_base as usize;
        let py = row + self.mouse_coord_base as usize;

        Some(format!("\x1b[<{pb};{px};{py}{suffix}").into_bytes())
    }

    /// Set the mouse protocol mode (from DEC private mode sequences).
    pub fn set_mouse_protocol(&mut self, mode: MouseProtocol) {
        self.mouse_protocol = mode;
    }

    /// Handle DEC private mode set/reset for mouse protocols.
    /// Returns true if the mode was handled.
    pub fn handle_mouse_mode(&mut self, mode: u16, set: bool) -> bool {
        let new_mode = if set {
            match mode {
                9 => Some(MouseProtocol::X10),
                1000 => Some(MouseProtocol::Normal),
                1002 => Some(MouseProtocol::ButtonEvent),
                1003 => Some(MouseProtocol::AnyEvent),
                _ => None,
            }
        } else {
            // Resetting any mouse mode turns it off
            match mode {
                9 | 1000 | 1002 | 1003 => Some(MouseProtocol::Off),
                _ => None,
            }
        };

        if let Some(m) = new_mode {
            self.mouse_protocol = m;
            true
        } else {
            false
        }
    }

    /// Encode scroll wheel events.
    pub fn encode_scroll(&self, up: bool, col: usize, row: usize, mods: Modifiers) -> Option<Vec<u8>> {
        if self.mouse_protocol == MouseProtocol::Off {
            return None;
        }

        // Scroll wheel: button 4 (up) or 5 (down) in SGR mode
        let btn_code = if up { 64 } else { 65 }; // bit 6 set for scroll

        let mut pb = btn_code;
        if mods.contains(Modifiers::SHIFT) {
            pb |= 4;
        }
        if mods.contains(Modifiers::ALT) {
            pb |= 8;
        }
        if mods.contains(Modifiers::CONTROL) {
            pb |= 16;
        }

        let px = col + self.mouse_coord_base as usize;
        let py = row + self.mouse_coord_base as usize;

        Some(format!("\x1b[<{pb};{px};{py}M").into_bytes())
    }

    fn encode_char(&self, c: char, mods: Modifiers) -> Vec<u8> {
        // Ctrl+A through Ctrl+Z → 0x01 through 0x1A
        if mods.contains(Modifiers::CONTROL) {
            if let Some(byte) = self.ctrl_char(c) {
                return vec![byte];
            }
        }

        // Alt+key → ESC followed by the key
        if mods.contains(Modifiers::ALT) {
            let mut buf = vec![0x1B];
            let mut char_buf = [0u8; 4];
            let s = c.encode_utf8(&mut char_buf);
            buf.extend_from_slice(s.as_bytes());
            return buf;
        }

        // Normal printable character
        let mut char_buf = [0u8; 4];
        let s = if mods.contains(Modifiers::SHIFT) {
            // Shift only affects case for letters
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase().encode_utf8(&mut char_buf)
            } else {
                c.encode_utf8(&mut char_buf)
            }
        } else {
            c.encode_utf8(&mut char_buf)
        };
        s.as_bytes().to_vec()
    }

    /// Ctrl+letter → control byte (Ctrl+A = 0x01, Ctrl+Z = 0x1A).
    fn ctrl_char(&self, c: char) -> Option<u8> {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            Some((c as u8) - b'a' + 1)
        } else {
            // Ctrl+[ → 0x1B, Ctrl+\ → 0x1C, Ctrl+] → 0x1D, Ctrl+^ → 0x1E
            match c {
                '[' => Some(0x1B),
                '\\' => Some(0x1C),
                ']' => Some(0x1D),
                '^' => Some(0x1E),
                '_' => Some(0x1F),
                '@' => Some(0x00),
                _ => None,
            }
        }
    }

    fn encode_enter(&self, mods: Modifiers) -> Vec<u8> {
        if mods.contains(Modifiers::ALT) {
            vec![0x1B, b'\r']
        } else {
            vec![b'\r']
        }
    }

    fn encode_backspace(&self, mods: Modifiers) -> Vec<u8> {
        if mods.contains(Modifiers::ALT) {
            vec![0x1B, 0x7F]
        } else {
            vec![0x7F]
        }
    }

    fn encode_tab(&self, mods: Modifiers) -> Vec<u8> {
        if mods.contains(Modifiers::ALT) {
            vec![0x1B, b'\t']
        } else {
            vec![b'\t']
        }
    }

    /// Encode arrow keys.
    fn encode_arrow(&self, dir: char, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);

        if has_mods {
            let mod_code = self.modifier_code(mods);
            format!("\x1b[1;{mod_code}{dir}").into_bytes()
        } else if self.app_cursor_keys {
            format!("\x1bO{dir}").into_bytes()
        } else {
            format!("\x1b[{dir}").into_bytes()
        }
    }

    fn encode_home(&self, mods: Modifiers) -> Vec<u8> {
        self.encode_csi_tilde_or_mod(1, mods)
    }

    fn encode_end(&self, mods: Modifiers) -> Vec<u8> {
        self.encode_csi_tilde_or_mod(4, mods)
    }

    fn encode_insert(&self, mods: Modifiers) -> Vec<u8> {
        self.encode_csi_tilde_or_mod(2, mods)
    }

    fn encode_delete(&self, mods: Modifiers) -> Vec<u8> {
        self.encode_csi_tilde_or_mod(3, mods)
    }

    fn encode_page(&self, suffix: char, _mods: Modifiers) -> Vec<u8> {
        format!("\x1b[{suffix}").into_bytes()
    }

    fn encode_function_key(&self, n: u8, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);

        match n {
            1 => {
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[1;{m}P").into_bytes()
                } else {
                    b"\x1bOP".to_vec()
                }
            }
            2 => {
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[1;{m}Q").into_bytes()
                } else {
                    b"\x1bOQ".to_vec()
                }
            }
            3 => {
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[1;{m}R").into_bytes()
                } else {
                    b"\x1bOR".to_vec()
                }
            }
            4 => {
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[1;{m}S").into_bytes()
                } else {
                    b"\x1bOS".to_vec()
                }
            }
            5..=10 => {
                let codes = [15u8, 17, 18, 19, 20, 21];
                let code = codes[n as usize - 5];
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[{code};{m}~").into_bytes()
                } else {
                    format!("\x1b[{code}~").into_bytes()
                }
            }
            11..=12 => {
                let code = 23 + (n - 11);
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[{code};{m}~").into_bytes()
                } else {
                    format!("\x1b[{code}~").into_bytes()
                }
            }
            _ => Vec::new(),
        }
    }

    fn encode_csi_tilde_or_mod(&self, code: u8, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);
        if has_mods {
            let m = self.modifier_code(mods);
            format!("\x1b[{code};{m}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    }

    fn modifier_code(&self, mods: Modifiers) -> u8 {
        let shift = mods.contains(Modifiers::SHIFT) as u8;
        let alt = mods.contains(Modifiers::ALT) as u8;
        let ctrl = mods.contains(Modifiers::CONTROL) as u8;
        match (shift, alt, ctrl) {
            (0, 0, 0) => 1,
            (1, 0, 0) => 2,
            (0, 1, 0) => 3,
            (1, 1, 0) => 4,
            (0, 0, 1) => 5,
            (1, 0, 1) => 6,
            (0, 1, 1) => 7,
            (1, 1, 1) => 8,
            _ => 1,
        }
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode bracketed paste start/end sequences.
pub fn bracketed_paste_start() -> Vec<u8> {
    b"\x1b[200~".to_vec()
}

pub fn bracketed_paste_end() -> Vec<u8> {
    b"\x1b[201~".to_vec()
}

/// Wrap text for bracketed paste mode.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = bracketed_paste_start();
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend(bracketed_paste_end());
        bytes
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> InputHandler {
        InputHandler::new()
    }

    // ── Basic keys ────────────────────────────────────────────────

    #[test]
    fn plain_char() {
        assert_eq!(handler().encode_key(KeyCode::Char('a'), Modifiers::empty()), b"a");
    }

    #[test]
    fn uppercase_char() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('A'), Modifiers::empty()),
            b"A"
        );
    }

    #[test]
    fn shift_char() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('a'), Modifiers::SHIFT),
            b"A"
        );
    }

    #[test]
    fn enter_key() {
        assert_eq!(handler().encode_key(KeyCode::Enter, Modifiers::empty()), b"\r");
    }

    #[test]
    fn tab_key() {
        assert_eq!(handler().encode_key(KeyCode::Tab, Modifiers::empty()), b"\t");
    }

    #[test]
    fn escape_key() {
        assert_eq!(handler().encode_key(KeyCode::Escape, Modifiers::empty()), b"\x1b");
    }

    #[test]
    fn backspace_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Backspace, Modifiers::empty()),
            b"\x7f"
        );
    }

    // ── Ctrl combinations ─────────────────────────────────────────

    #[test]
    fn ctrl_a() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('a'), Modifiers::CONTROL),
            b"\x01"
        );
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('z'), Modifiers::CONTROL),
            b"\x1a"
        );
    }

    #[test]
    fn ctrl_uppercase() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('A'), Modifiers::CONTROL),
            b"\x01"
        );
    }

    #[test]
    fn ctrl_bracket() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('['), Modifiers::CONTROL),
            b"\x1b"
        );
    }

    // ── Alt combinations ──────────────────────────────────────────

    #[test]
    fn alt_char() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('a'), Modifiers::ALT),
            b"\x1ba"
        );
    }

    #[test]
    fn alt_enter() {
        assert_eq!(
            handler().encode_key(KeyCode::Enter, Modifiers::ALT),
            b"\x1b\r"
        );
    }

    // ── Arrow keys ────────────────────────────────────────────────

    #[test]
    fn arrow_normal_mode() {
        let h = handler();
        assert_eq!(h.encode_key(KeyCode::Up, Modifiers::empty()), b"\x1b[A");
        assert_eq!(h.encode_key(KeyCode::Down, Modifiers::empty()), b"\x1b[B");
        assert_eq!(h.encode_key(KeyCode::Right, Modifiers::empty()), b"\x1b[C");
        assert_eq!(h.encode_key(KeyCode::Left, Modifiers::empty()), b"\x1b[D");
    }

    #[test]
    fn arrow_app_cursor_mode() {
        let mut h = handler();
        h.app_cursor_keys = true;
        assert_eq!(h.encode_key(KeyCode::Up, Modifiers::empty()), b"\x1bOA");
        assert_eq!(h.encode_key(KeyCode::Down, Modifiers::empty()), b"\x1bOB");
        assert_eq!(h.encode_key(KeyCode::Right, Modifiers::empty()), b"\x1bOC");
        assert_eq!(h.encode_key(KeyCode::Left, Modifiers::empty()), b"\x1bOD");
    }

    #[test]
    fn arrow_shift() {
        let h = handler();
        assert_eq!(h.encode_key(KeyCode::Up, Modifiers::SHIFT), b"\x1b[1;2A");
        assert_eq!(h.encode_key(KeyCode::Left, Modifiers::ALT), b"\x1b[1;3D");
    }

    #[test]
    fn arrow_ctrl() {
        assert_eq!(
            handler().encode_key(KeyCode::Right, Modifiers::CONTROL),
            b"\x1b[1;5C"
        );
    }

    // ── Home / End / Insert / Delete ──────────────────────────────

    #[test]
    fn home_key() {
        assert_eq!(handler().encode_key(KeyCode::Home, Modifiers::empty()), b"\x1b[1~");
    }

    #[test]
    fn end_key() {
        assert_eq!(handler().encode_key(KeyCode::End, Modifiers::empty()), b"\x1b[4~");
    }

    #[test]
    fn insert_key() {
        assert_eq!(handler().encode_key(KeyCode::Insert, Modifiers::empty()), b"\x1b[2~");
    }

    #[test]
    fn delete_key() {
        assert_eq!(handler().encode_key(KeyCode::Delete, Modifiers::empty()), b"\x1b[3~");
    }

    #[test]
    fn home_with_shift() {
        assert_eq!(
            handler().encode_key(KeyCode::Home, Modifiers::SHIFT),
            b"\x1b[1;2~"
        );
    }

    // ── Page Up / Page Down ───────────────────────────────────────

    #[test]
    fn page_up() {
        assert_eq!(handler().encode_key(KeyCode::PageUp, Modifiers::empty()), b"\x1b[H");
    }

    #[test]
    fn page_down() {
        assert_eq!(handler().encode_key(KeyCode::PageDown, Modifiers::empty()), b"\x1b[I");
    }

    // ── Function keys ─────────────────────────────────────────────

    #[test]
    fn f1_through_f4() {
        let h = handler();
        assert_eq!(h.encode_key(KeyCode::F(1), Modifiers::empty()), b"\x1bOP");
        assert_eq!(h.encode_key(KeyCode::F(2), Modifiers::empty()), b"\x1bOQ");
        assert_eq!(h.encode_key(KeyCode::F(3), Modifiers::empty()), b"\x1bOR");
        assert_eq!(h.encode_key(KeyCode::F(4), Modifiers::empty()), b"\x1bOS");
    }

    #[test]
    fn f5_through_f10() {
        let h = handler();
        assert_eq!(h.encode_key(KeyCode::F(5), Modifiers::empty()), b"\x1b[15~");
        assert_eq!(h.encode_key(KeyCode::F(6), Modifiers::empty()), b"\x1b[17~");
        assert_eq!(h.encode_key(KeyCode::F(7), Modifiers::empty()), b"\x1b[18~");
        assert_eq!(h.encode_key(KeyCode::F(8), Modifiers::empty()), b"\x1b[19~");
        assert_eq!(h.encode_key(KeyCode::F(9), Modifiers::empty()), b"\x1b[20~");
        assert_eq!(h.encode_key(KeyCode::F(10), Modifiers::empty()), b"\x1b[21~");
    }

    #[test]
    fn f11_f12() {
        let h = handler();
        assert_eq!(h.encode_key(KeyCode::F(11), Modifiers::empty()), b"\x1b[23~");
        assert_eq!(h.encode_key(KeyCode::F(12), Modifiers::empty()), b"\x1b[24~");
    }

    #[test]
    fn f1_with_shift() {
        assert_eq!(
            handler().encode_key(KeyCode::F(1), Modifiers::SHIFT),
            b"\x1b[1;2P"
        );
    }

    // ── Modifier code mapping ─────────────────────────────────────

    #[test]
    fn modifier_code_all_combinations() {
        let h = handler();
        assert_eq!(h.modifier_code(Modifiers::SHIFT), 2);
        assert_eq!(h.modifier_code(Modifiers::ALT), 3);
        assert_eq!(h.modifier_code(Modifiers::SHIFT | Modifiers::ALT), 4);
        assert_eq!(h.modifier_code(Modifiers::CONTROL), 5);
        assert_eq!(h.modifier_code(Modifiers::SHIFT | Modifiers::CONTROL), 6);
        assert_eq!(h.modifier_code(Modifiers::ALT | Modifiers::CONTROL), 7);
        assert_eq!(
            h.modifier_code(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL),
            8
        );
    }

    // ── Numpad ────────────────────────────────────────────────────

    #[test]
    fn numpad_digit() {
        assert_eq!(handler().encode_key(KeyCode::Numpad('5'), Modifiers::empty()), b"5");
    }

    // ── Mouse protocol ────────────────────────────────────────────

    #[test]
    fn mouse_off_returns_none() {
        let h = handler();
        assert!(h.encode_mouse(MouseButton::Left, MouseAction::Press, 5, 10, Modifiers::empty()).is_none());
    }

    #[test]
    fn mouse_sgr_left_press() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        let bytes = h.encode_mouse(MouseButton::Left, MouseAction::Press, 5, 10, Modifiers::empty());
        assert!(bytes.is_some());
        let bytes = bytes.unwrap();
        // SGR: ESC[<0;6;11M (1-based coords)
        assert!(bytes.starts_with(b"\x1b[<0;6;11M"));
    }

    #[test]
    fn mouse_sgr_release() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        let bytes = h.encode_mouse(MouseButton::Left, MouseAction::Release, 5, 10, Modifiers::empty());
        assert!(bytes.is_some());
        let bytes = bytes.unwrap();
        // Release ends with 'm' (lowercase)
        assert!(bytes.ends_with(b"m"));
    }

    #[test]
    fn mouse_scroll() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        let bytes = h.encode_scroll(true, 5, 10, Modifiers::empty());
        assert!(bytes.is_some());
        let bytes = bytes.unwrap();
        // Scroll up: button 64
        assert!(bytes.starts_with(b"\x1b[<64;"));
    }

    #[test]
    fn mouse_mode_handling() {
        let mut h = handler();
        assert!(h.handle_mouse_mode(1000, true));
        assert_eq!(h.mouse_protocol, MouseProtocol::Normal);
        assert!(h.handle_mouse_mode(1002, true));
        assert_eq!(h.mouse_protocol, MouseProtocol::ButtonEvent);
        assert!(h.handle_mouse_mode(1000, false));
        assert_eq!(h.mouse_protocol, MouseProtocol::Off);
    }

    // ── Bracketed paste ───────────────────────────────────────────

    #[test]
    fn bracketed_paste_wrapping() {
        let bytes = encode_paste("hello", true);
        assert!(bytes.starts_with(b"\x1b[200~"));
        assert!(bytes.ends_with(b"\x1b[201~"));
        assert!(bytes.windows(5).any(|w| w == b"hello"));
    }

    #[test]
    fn unbracketed_paste() {
        let bytes = encode_paste("hello", false);
        assert_eq!(bytes, b"hello");
    }
}
