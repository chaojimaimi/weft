use std::io::Write;

use super::keys::{KeyCode, Modifiers};
use super::mouse::{MouseAction, MouseButton, MouseProtocol};

/// Encodes keyboard input into VT escape sequences for the PTY.
pub struct InputHandler {
    /// Whether to use application cursor key mode (DECCKM).
    /// When true, arrow keys send SS3 sequences (ESC O A/B/C/D).
    /// When false, arrow keys send CSI sequences (ESC [ A/B/C/D).
    pub app_cursor_keys: bool,
    /// Current mouse protocol mode.
    pub mouse_protocol: MouseProtocol,
    /// v1.0 fix: SGR-1006 encoding flag (CSI ?1006h). When true, mouse/scroll
    /// events use the SGR format; when false, the legacy `\x1b[M...` format.
    /// Synced from `Terminal.sgr_mouse` alongside `mouse_protocol`.
    pub sgr_mouse: bool,
    /// Mouse coordinate origin (0 or 1 based, SGR uses 1-based).
    mouse_coord_base: u8,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            app_cursor_keys: false,
            mouse_protocol: MouseProtocol::Off,
            sgr_mouse: false,
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

    /// Encode a mouse event.
    /// Returns bytes to send to PTY, or None if mouse protocol is off or the
    /// event isn't reportable in the current mode.
    ///
    /// v1.0 fix: emit the format the app actually requested. `sgr_mouse`
    /// (CSI ?1006h) selects `\x1b[<Pb;Px;Py M/m`; otherwise the legacy
    /// `\x1b[M` + 3 encoded chars (button, x, y, each +32) is used. Sending
    /// SGR format to an app that enabled only 1000 left the bytes
    /// un-parseable → leftover leaked as visible text (vim `~@k`) and put
    /// the app into a broken state where subsequent keys stopped working.
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

        let btn_code = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        let pb = self.encode_mouse_button(btn_code, action, mods);

        let px = col + self.mouse_coord_base as usize;
        let py = row + self.mouse_coord_base as usize;

        if self.sgr_mouse {
            // SGR-1006: press/move → 'M', release → 'm'. Motion is only
            // sent in ButtonEvent/AnyEvent modes.
            let suffix = match action {
                MouseAction::Press => 'M',
                MouseAction::Release => 'm',
                MouseAction::Move => match self.mouse_protocol {
                    MouseProtocol::ButtonEvent | MouseProtocol::AnyEvent => 'M',
                    _ => return None,
                },
            };
            let mut buf = Vec::with_capacity(16);
            let _ = write!(buf, "\x1b[<{pb};{px};{py}{suffix}");
            Some(buf)
        } else {
            // Legacy X10/normal: CSI M then 3 chars (button, x, y) each +32.
            // No release event in legacy mode (release arrives as button 3).
            if action == MouseAction::Release {
                return None;
            }
            // Motion only in ButtonEvent/AnyEvent.
            if action == MouseAction::Move
                && !matches!(
                    self.mouse_protocol,
                    MouseProtocol::ButtonEvent | MouseProtocol::AnyEvent
                )
            {
                return None;
            }
            // Coordinates > 223 (255-32) can't be encoded in legacy mode —
            // drop rather than send a corrupt report.
            if px > 223 || py > 223 {
                return None;
            }
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(b"\x1b[M");
            buf.push((pb as u8).wrapping_add(32));
            buf.push((px as u8).wrapping_add(32));
            buf.push((py as u8).wrapping_add(32));
            Some(buf)
        }
    }

    /// Build the SGR/legacy mouse button code from the logical button +
    /// modifiers + motion flag.
    fn encode_mouse_button(&self, btn_code: u32, action: MouseAction, mods: Modifiers) -> u32 {
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
        if action == MouseAction::Move {
            pb |= 32;
        }
        pb
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
    /// Encode a scroll-wheel event. Wheel is button 4 (up) / 5 (down), encoded
    /// as button codes 64/65 (bit 6 = wheel) in the mouse report.
    ///
    /// v1.0 fix: format-aware (SGR when `sgr_mouse`, legacy otherwise) — see
    /// `encode_mouse`. A wheel event is always a "press" (no release), so the
    /// legacy path emits one `CSI M` report with button 64/65.
    pub fn encode_scroll(
        &self,
        up: bool,
        col: usize,
        row: usize,
        mods: Modifiers,
    ) -> Option<Vec<u8>> {
        if self.mouse_protocol == MouseProtocol::Off {
            return None;
        }

        // Scroll wheel: bit 6 set, +1 for down.
        let btn_code = if up { 64 } else { 65 };
        let pb = self.encode_mouse_button(btn_code, MouseAction::Press, mods);

        let px = col + self.mouse_coord_base as usize;
        let py = row + self.mouse_coord_base as usize;

        if self.sgr_mouse {
            let mut buf = Vec::with_capacity(16);
            let _ = write!(buf, "\x1b[<{pb};{px};{py}M");
            Some(buf)
        } else {
            // Legacy: coords > 223 can't be encoded — drop.
            if px > 223 || py > 223 {
                return None;
            }
            let mut buf = Vec::with_capacity(6);
            buf.extend_from_slice(b"\x1b[M");
            buf.push((pb as u8).wrapping_add(32));
            buf.push((px as u8).wrapping_add(32));
            buf.push((py as u8).wrapping_add(32));
            Some(buf)
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
            let shifted = match c {
                c if c.is_ascii_lowercase() => Some(c.to_ascii_uppercase()),
                // US keyboard shifted symbols (Shift only changes case for letters
                // otherwise — without this map, Shift+`/4/- etc. emit the base char).
                '`' => Some('~'),
                '1' => Some('!'),
                '2' => Some('@'),
                '3' => Some('#'),
                '4' => Some('$'),
                '5' => Some('%'),
                '6' => Some('^'),
                '7' => Some('&'),
                '8' => Some('*'),
                '9' => Some('('),
                '0' => Some(')'),
                '-' => Some('_'),
                '=' => Some('+'),
                '[' => Some('{'),
                ']' => Some('}'),
                '\\' => Some('|'),
                ';' => Some(':'),
                '\'' => Some('"'),
                ',' => Some('<'),
                '.' => Some('>'),
                '/' => Some('?'),
                _ => None,
            };
            match shifted {
                Some(sc) => sc.encode_utf8(&mut char_buf),
                None => c.encode_utf8(&mut char_buf),
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
        if mods.contains(Modifiers::SHIFT) {
            b"\x1b[Z".to_vec()
        } else if mods.contains(Modifiers::ALT) {
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
            self.csi_with_mod(1, dir as u8, mod_code)
        } else if self.app_cursor_keys {
            vec![0x1b, b'O', dir as u8]
        } else {
            vec![0x1b, b'[', dir as u8]
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
        vec![0x1b, b'[', suffix as u8]
    }

    fn encode_function_key(&self, n: u8, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);

        match n {
            // F1–F4: application mode `ESC O P/Q/R/S`, or CSI form with a modifier.
            1..=4 => {
                let final_byte = b'P' + (n - 1);
                if has_mods {
                    let m = self.modifier_code(mods);
                    self.csi_with_mod(1, final_byte, m)
                } else {
                    vec![0x1b, b'O', final_byte]
                }
            }
            // F5–F10 use CSI <code> ~ (codes 15/17/18/19/20/21).
            5..=10 => {
                let codes = [15u8, 17, 18, 19, 20, 21];
                let code = codes[(n - 5) as usize];
                self.csi_tilde(code, mods)
            }
            // F11–F12 use CSI 23/24 ~.
            11..=12 => {
                let code = 23 + (n - 11);
                self.csi_tilde(code, mods)
            }
            _ => Vec::new(),
        }
    }

    fn encode_csi_tilde_or_mod(&self, code: u8, mods: Modifiers) -> Vec<u8> {
        self.csi_tilde(code, mods)
    }

    /// Build `\x1b[<param>;<mod><final>` — used by arrow keys and F1–F4 when a
    /// modifier is held. Writes bytes directly instead of going through `format!`
    /// (no `String` allocation, no formatter machinery).
    fn csi_with_mod(&self, param: u8, final_byte: u8, mod_code: u8) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        let _ = write!(buf, "\x1b[{param};{mod_code}");
        buf.push(final_byte);
        buf
    }

    /// Build `\x1b[<code>;<m>~` (modifier held) or `\x1b[<code>~`.
    fn csi_tilde(&self, code: u8, mods: Modifiers) -> Vec<u8> {
        let has_mods = mods.intersects(Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL);
        let mut buf = Vec::with_capacity(8);
        if has_mods {
            let m = self.modifier_code(mods);
            let _ = write!(buf, "\x1b[{code};{m}~");
        } else {
            let _ = write!(buf, "\x1b[{code}~");
        }
        buf
    }

    pub(super) fn modifier_code(&self, mods: Modifiers) -> u8 {
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
