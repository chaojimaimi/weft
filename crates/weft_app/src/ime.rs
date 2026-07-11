//! macOS native IME context maintenance.
//!
//! winit stores marked text on the window's `NSView`, while Weft renders a
//! separate preedit string per tab. Whenever input ownership changes (command
//! submission, tab switch, modal switch), both layers must be reset together;
//! otherwise an old composition can be committed into the new PTY/tab.

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use weft_core::input::{InputHandler, KeyCode, Modifiers};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Prefer winit's layout-aware text for ordinary printable keyboard events.
/// Control/Alt/Super chords must keep using the terminal key encoder because
/// they carry VT/control semantics rather than literal text. Shift is allowed:
/// `KeyEvent.text` already contains `?`, `:`, non-US layout characters, etc.
fn direct_keyboard_text(text: Option<&str>, mods: Modifiers) -> Option<Vec<u8>> {
    if mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER) {
        return None;
    }
    let text = text.filter(|text| !text.is_empty())?;
    Some(text.as_bytes().to_vec())
}

/// Resolve one winit KeyboardInput event to exactly one PTY byte vector.
/// Printable text follows the active keyboard layout; terminal chords and
/// non-printable keys retain InputHandler's VT encoding.
pub fn encode_passthrough_key(
    input: &InputHandler,
    key: KeyCode,
    mods: Modifiers,
    text: Option<&str>,
) -> Vec<u8> {
    if matches!(key, KeyCode::Char(_)) {
        if let Some(bytes) = direct_keyboard_text(text, mods) {
            return bytes;
        }
    }
    input.encode_key(key, mods)
}

/// Discard the native marked-text composition without toggling winit's IME
/// permission state. Toggling false/true leaves winit's macOS `ImeState` in
/// `Disabled` until a later `setMarkedText:` callback and makes direct text
/// insertion from third-party IMEs race with the application's EventGate.
pub fn discard_marked_text(window: &Window) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        use objc2::msg_send;

        let raw = match window.window_handle() {
            Ok(handle) => handle.as_raw(),
            Err(_) => return,
        };
        let RawWindowHandle::AppKit(appkit) = raw else {
            return;
        };
        let Some(view): Option<Retained<NSView>> = Retained::retain(appkit.ns_view.as_ptr().cast())
        else {
            return;
        };
        if let Some(context) = view.inputContext() {
            context.discardMarkedText();
        }
        // WinitView implements NSTextInputClient at runtime. Calling
        // `unmarkText` clears winit's own marked_text/ImeState as well as the
        // native context, matching WarpHostView's two-layer reset. Keep the
        // raw message inside catch_unwind per the project's objc2 discipline.
        let _: () = msg_send![&*view, unmarkText];
    }));
}

#[cfg(test)]
mod tests {
    use super::{direct_keyboard_text, encode_passthrough_key};
    use weft_core::input::{InputHandler, KeyCode, Modifiers};

    #[test]
    fn printable_keyboard_text_preserves_layout_and_shifted_punctuation() {
        assert_eq!(
            direct_keyboard_text(Some("/"), Modifiers::empty()),
            Some(b"/".to_vec())
        );
        assert_eq!(
            direct_keyboard_text(Some("?"), Modifiers::SHIFT),
            Some(b"?".to_vec())
        );
        assert_eq!(
            direct_keyboard_text(Some(":"), Modifiers::SHIFT),
            Some(b":".to_vec())
        );
        assert_eq!(
            direct_keyboard_text(Some("中"), Modifiers::empty()),
            Some("中".as_bytes().to_vec())
        );
    }

    #[test]
    fn terminal_chords_do_not_use_literal_keyboard_text() {
        for mods in [Modifiers::CONTROL, Modifiers::ALT, Modifiers::SUPER] {
            assert_eq!(direct_keyboard_text(Some("x"), mods), None);
        }
        assert_eq!(direct_keyboard_text(None, Modifiers::empty()), None);
        assert_eq!(direct_keyboard_text(Some(""), Modifiers::empty()), None);
    }

    #[test]
    fn less_and_vim_punctuation_encode_exactly_once_from_layout_text() {
        let input = InputHandler::new();
        let cases = [
            (KeyCode::Char('/'), Modifiers::empty(), "/"),
            (KeyCode::Char('/'), Modifiers::SHIFT, "?"),
            (KeyCode::Char(';'), Modifiers::SHIFT, ":"),
        ];
        for (key, mods, text) in cases {
            assert_eq!(
                encode_passthrough_key(&input, key, mods, Some(text)),
                text.as_bytes(),
                "one keyboard event must produce exactly its layout text"
            );
        }
        assert_eq!(
            encode_passthrough_key(&input, KeyCode::Char('c'), Modifiers::CONTROL, Some("c")),
            b"\x03"
        );
    }
}
