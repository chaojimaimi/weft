//! Keyboard and mouse input encoding for terminal emulators.
//!
//! Translates abstract key/mouse events into VT100/VT220 escape sequences
//! that can be sent to the PTY for the shell to interpret.

pub mod handler;
pub mod keys;
pub mod kitty;
pub mod mode;
pub mod mouse;
pub mod paste;

pub use handler::InputHandler;
pub use keys::{KeyCode, Modifiers};
pub use kitty::{encode_kitty_key, KittyEventKind};
pub use mode::{effective_mode, InputMode};
pub use mouse::{MouseAction, MouseButton, MouseProtocol};
pub use paste::{
    bracketed_paste_end, bracketed_paste_start, build_submit_bytes, classify_paste,
    contains_dangerous_control_chars, encode_paste, format_byte_count, paste_preview,
    PasteGuardCfg, PasteRisk, DEFAULT_PASTE_SIZE_THRESHOLD_KIB,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> InputHandler {
        InputHandler::new()
    }

    // ── Basic keys ────────────────────────────────────────────────

    #[test]
    fn plain_char() {
        assert_eq!(
            handler().encode_key(KeyCode::Char('a'), Modifiers::empty()),
            b"a"
        );
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
    fn shift_symbols() {
        // Regression: Shift+symbol must yield the shifted char, not the base char.
        assert_eq!(
            handler().encode_key(KeyCode::Char('`'), Modifiers::SHIFT),
            b"~"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Char('4'), Modifiers::SHIFT),
            b"$"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Char('-'), Modifiers::SHIFT),
            b"_"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Char('['), Modifiers::SHIFT),
            b"{"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Char('/'), Modifiers::SHIFT),
            b"?"
        );
    }

    #[test]
    fn enter_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Enter, Modifiers::empty()),
            b"\r"
        );
    }

    #[test]
    fn tab_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Tab, Modifiers::empty()),
            b"\t"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Tab, Modifiers::SHIFT),
            b"\x1b[Z"
        );
        assert_eq!(
            handler().encode_key(KeyCode::Tab, Modifiers::SHIFT | Modifiers::ALT),
            b"\x1b[Z"
        );
    }

    #[test]
    fn escape_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Escape, Modifiers::empty()),
            b"\x1b"
        );
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
        assert_eq!(
            handler().encode_key(KeyCode::Home, Modifiers::empty()),
            b"\x1b[1~"
        );
    }

    #[test]
    fn end_key() {
        assert_eq!(
            handler().encode_key(KeyCode::End, Modifiers::empty()),
            b"\x1b[4~"
        );
    }

    #[test]
    fn insert_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Insert, Modifiers::empty()),
            b"\x1b[2~"
        );
    }

    #[test]
    fn delete_key() {
        assert_eq!(
            handler().encode_key(KeyCode::Delete, Modifiers::empty()),
            b"\x1b[3~"
        );
    }

    #[test]
    fn home_with_shift() {
        assert_eq!(
            handler().encode_key(KeyCode::Home, Modifiers::SHIFT),
            b"\x1b[1;2~"
        );
    }

    // ── Page Up / Page Down ───────────────────────────────────────
    // v1.11.13 (PLAN_v11113 §M3): behavior change — PageUp/PageDown now
    // emit the standard xterm `CSI 5~` / `CSI 6~` (the old bare `CSI H` /
    // `CSI I` collided with terminfo khome=\E[H, so vim/less read PageUp
    // as Home).

    #[test]
    fn page_up() {
        assert_eq!(
            handler().encode_key(KeyCode::PageUp, Modifiers::empty()),
            b"\x1b[5~"
        );
    }

    #[test]
    fn page_down() {
        assert_eq!(
            handler().encode_key(KeyCode::PageDown, Modifiers::empty()),
            b"\x1b[6~"
        );
    }

    #[test]
    fn page_up_down_with_mods() {
        assert_eq!(
            handler().encode_key(KeyCode::PageUp, Modifiers::SHIFT),
            b"\x1b[5;2~"
        );
        assert_eq!(
            handler().encode_key(KeyCode::PageDown, Modifiers::CONTROL),
            b"\x1b[6;5~"
        );
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
        assert_eq!(
            h.encode_key(KeyCode::F(10), Modifiers::empty()),
            b"\x1b[21~"
        );
    }

    #[test]
    fn f11_f12() {
        let h = handler();
        assert_eq!(
            h.encode_key(KeyCode::F(11), Modifiers::empty()),
            b"\x1b[23~"
        );
        assert_eq!(
            h.encode_key(KeyCode::F(12), Modifiers::empty()),
            b"\x1b[24~"
        );
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
        assert_eq!(
            handler().encode_key(KeyCode::Numpad('5'), Modifiers::empty()),
            b"5"
        );
    }

    // ── Mouse protocol ────────────────────────────────────────────

    #[test]
    fn mouse_off_returns_none() {
        let h = handler();
        assert!(h
            .encode_mouse(
                MouseButton::Left,
                MouseAction::Press,
                5,
                10,
                Modifiers::empty()
            )
            .is_none());
    }

    #[test]
    fn mouse_sgr_left_press() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        h.sgr_mouse = true; // SGR-1006 encoding (CSI ?1006h)
        let bytes = h.encode_mouse(
            MouseButton::Left,
            MouseAction::Press,
            5,
            10,
            Modifiers::empty(),
        );
        assert!(bytes.is_some());
        let bytes = bytes.unwrap();
        // SGR: ESC[<0;6;11M (1-based coords)
        assert!(bytes.starts_with(b"\x1b[<0;6;11M"));
    }

    #[test]
    fn mouse_legacy_press() {
        // v1.0: when sgr_mouse is OFF (app did not enable 1006), use the
        // legacy `\x1b[M` + 3 encoded chars (button, x, y, each +32).
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        h.sgr_mouse = false;
        let bytes = h.encode_mouse(
            MouseButton::Left,
            MouseAction::Press,
            5,
            10,
            Modifiers::empty(),
        );
        // button 0→32 (' '), px=col(5)+base(1)=6→38 ('&'), py=row(10)+1=11→43 ('+')
        assert_eq!(bytes.as_deref(), Some(b"\x1b[M &+".as_slice()));
    }

    #[test]
    fn mouse_sgr_release() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        h.sgr_mouse = true;
        let bytes = h.encode_mouse(
            MouseButton::Left,
            MouseAction::Release,
            5,
            10,
            Modifiers::empty(),
        );
        assert!(bytes.is_some());
        let bytes = bytes.unwrap();
        // Release ends with 'm' (lowercase)
        assert!(bytes.ends_with(b"m"));
    }

    #[test]
    fn mouse_scroll() {
        let mut h = handler();
        h.mouse_protocol = MouseProtocol::Normal;
        h.sgr_mouse = true;
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

    // ── v0.5 editor takeover: submit bytes + effective mode ─────────

    use crate::blocks::ShellPhase;

    #[test]
    fn build_submit_bytes_bracketed_wraps_multiline() {
        let bytes = build_submit_bytes("echo a\necho b", true);
        // Ctrl-U, bracketed-paste open, command, close, \n
        assert_eq!(bytes[0], 0x15);
        assert!(bytes.starts_with(&[0x15, 0x1b, b'[', b'2', b'0', b'0', b'~']));
        assert!(bytes.windows(6).any(|w| w == b"\x1b[201~"));
        assert_eq!(*bytes.last().unwrap(), b'\n');
    }

    #[test]
    fn build_submit_bytes_no_bracket_rewrites_newline_to_cr() {
        let bytes = build_submit_bytes("echo a\necho b", false);
        assert_eq!(bytes[0], 0x15);
        // Internal \n became \r; no bracket wrappers.
        assert!(!bytes.windows(6).any(|w| w == b"\x1b[200~"));
        let body = &bytes[1..bytes.len() - 1]; // strip Ctrl-U and trailing \n
        assert_eq!(body, b"echo a\recho b");
    }

    #[test]
    fn build_submit_bytes_empty_command() {
        let bytes = build_submit_bytes("", true);
        assert_eq!(bytes, b"\x15\n"); // Ctrl-U + \n, no paste wrappers on empty
    }

    #[test]
    fn build_submit_bytes_strips_escape_when_not_bracketed() {
        // ESC is stripped, neutralizing the control sequence (the trailing
        // `[2J` is harmless literal text and passes through). Without this a
        // pasted command could inject terminal sequences when bracketed paste
        // is off.
        let bytes = build_submit_bytes("echo\x1b[2J", false);
        assert_eq!(bytes, b"\x15echo[2J\n");
        // A bare ESC mid-command is also stripped.
        assert_eq!(build_submit_bytes("a\x1bb", false), b"\x15ab\n");
    }

    #[test]
    fn build_submit_bytes_keeps_tab_when_not_bracketed() {
        let bytes = build_submit_bytes("a\tb", false);
        assert_eq!(bytes, b"\x15a\tb\n");
    }

    #[test]
    fn effective_mode_takes_editor_only_at_prompt_bootstrapped() {
        assert_eq!(
            effective_mode(ShellPhase::AtPrompt, false, true, false),
            InputMode::Editor
        );
    }

    #[test]
    fn effective_mode_alt_screen_forces_passthrough() {
        assert_eq!(
            effective_mode(ShellPhase::AtPrompt, true, true, false),
            InputMode::Passthrough
        );
    }

    #[test]
    fn effective_mode_not_bootstrapped_forces_passthrough() {
        assert_eq!(
            effective_mode(ShellPhase::AtPrompt, false, false, false),
            InputMode::Passthrough
        );
    }

    #[test]
    fn effective_mode_command_executing_passthrough() {
        assert_eq!(
            effective_mode(ShellPhase::CommandExecuting, false, true, false),
            InputMode::Passthrough
        );
    }

    #[test]
    fn effective_mode_not_integrated_passthrough() {
        assert_eq!(
            effective_mode(ShellPhase::NotIntegrated, false, true, false),
            InputMode::Passthrough
        );
    }

    #[test]
    fn effective_mode_just_submitted_passthrough() {
        // Enter pressed, 133;B not yet arrived → command_from_editor=true.
        assert_eq!(
            effective_mode(ShellPhase::AtPrompt, false, true, true),
            InputMode::Passthrough
        );
    }
}
