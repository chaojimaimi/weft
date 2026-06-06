//! Keyboard input encoding for terminal emulators.
//!
//! Translates abstract key events into VT100/VT220 escape sequences
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

/// Encodes keyboard input into VT escape sequences for the PTY.
pub struct InputHandler {
    /// Whether to use application cursor key mode (DECCKM).
    /// When true, arrow keys send SS3 sequences (ESC O A/B/C/D).
    /// When false, arrow keys send CSI sequences (ESC [ A/B/C/D).
    pub app_cursor_keys: bool,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            app_cursor_keys: false,
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
        // Ctrl+Backspace often sends DEL (0x7F), plain Backspace sends BS (0x08)
        // or DEL depending on terminal config. We follow modern convention:
        // plain = DEL, Ctrl = DEL.
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
    /// Normal mode: CSI A/B/C/D
    /// Application mode: SS3 A/B/C/D (ESC O A/B/C/D)
    /// With modifiers: CSI 1;modifier A/B/C/D
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
        // Page Up: CSI 5~  or  CSI 5;modifier ~
        // Page Down: CSI 6~  or  CSI 6;modifier ~
        // For simplicity, use unmodified form for now.
        // modifier support can be added later.
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
                    b"\x1bOP".to_vec() // SS3 P
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
                // F5–F10: CSI 15~ through CSI 21~ (with gaps)
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
                // F11–F12: CSI 23~ through CSI 24~
                let code = 23 + (n - 11);
                if has_mods {
                    let m = self.modifier_code(mods);
                    format!("\x1b[{code};{m}~").into_bytes()
                } else {
                    format!("\x1b[{code}~").into_bytes()
                }
            }
            _ => Vec::new(), // Unsupported function key
        }
    }

    /// CSI param-based keys (Home/End/Insert/Delete) with optional modifiers.
    /// Without modifiers: CSI <n> ~
    /// With modifiers: CSI <n> ; <mod> ~
    fn encode_csi_tilde_or_mod(&self, code: u8, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);
        if has_mods {
            let m = self.modifier_code(mods);
            format!("\x1b[{code};{m}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    }

    /// Map modifier flags to the VT220 modifier code used in CSI sequences.
    /// 1=Shift, 2=Alt, 3=Shift+Alt, 4=Ctrl, 5=Shift+Ctrl,
    /// 6=Alt+Ctrl, 7=Shift+Alt+Ctrl, 8=Meta (same as Alt).
    fn modifier_code(&self, mods: Modifiers) -> u8 {
        let shift = mods.contains(Modifiers::SHIFT) as u8;
        let alt = mods.contains(Modifiers::ALT) as u8;
        let ctrl = mods.contains(Modifiers::CONTROL) as u8;
        match (shift, alt, ctrl) {
            (0, 0, 0) => 1, // no mods (shouldn't be called, but safe default)
            (1, 0, 0) => 2, // Shift
            (0, 1, 0) => 3, // Alt
            (1, 1, 0) => 4, // Shift+Alt
            (0, 0, 1) => 5, // Ctrl
            (1, 0, 1) => 6, // Shift+Ctrl
            (0, 1, 1) => 7, // Alt+Ctrl
            (1, 1, 1) => 8, // Shift+Alt+Ctrl
            _ => 1,
        }
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
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
        // Ctrl+Shift+A should also produce ctrl code
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
}
