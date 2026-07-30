//! macOS native IME context maintenance.
//!
//! winit stores marked text on the window's `NSView`, while Weft renders a
//! separate preedit string per tab. Whenever input ownership changes (command
//! submission, tab switch, modal switch), both layers must be reset together;
//! otherwise an old composition can be committed into the new PTY/tab.

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use weft_core::input::InputMode;
use weft_core::input::{InputHandler, KeyCode, Modifiers};
use weft_core::vt::Terminal;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::layout::LayoutCtx;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImeCursorArea {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn grid_cursor_area(ctx: LayoutCtx, row: usize, col: usize) -> Option<ImeCursorArea> {
    (ctx.cell_w.is_finite() && ctx.cell_h.is_finite() && ctx.cell_w > 0.0 && ctx.cell_h > 0.0)
        .then_some(ImeCursorArea {
            x: ctx.left() + col as f32 * ctx.cell_w,
            y: ctx.top() + row as f32 * ctx.cell_h,
            width: ctx.cell_w,
            height: ctx.cell_h,
        })
}

fn editor_cursor_area_from_buffer(
    ctx: LayoutCtx,
    lines: &[String],
    cursor: (usize, usize),
    _scroll_offset: usize,
) -> Option<ImeCursorArea> {
    lines.get(cursor.0)?;
    let (_, prompt, _) = crate::paint::prompt::prompt_layout_for_buffer(&ctx, lines, cursor);
    Some(ImeCursorArea {
        x: prompt.cursor_x,
        y: prompt.cursor_y,
        width: ctx.cell_w,
        height: prompt.row_height,
    })
}

fn editor_cursor_area(ctx: LayoutCtx, terminal: &Terminal) -> Option<ImeCursorArea> {
    let buffer = &terminal.editor().buffer;
    editor_cursor_area_from_buffer(ctx, &buffer.lines, buffer.cursor, buffer.scroll_offset)
}

/// Keep the native macOS candidate window attached to Weft's GPU caret.
/// winit converts this physical top-left rectangle into AppKit's text-input
/// coordinate system and invalidates the current character coordinates.
pub fn update_cursor_area(window: &Window, ctx: LayoutCtx, terminal: &Terminal) {
    let area = match terminal.effective_input_mode() {
        InputMode::Editor => editor_cursor_area(ctx, terminal),
        InputMode::Passthrough => {
            let cursor = &terminal.grid().cursor;
            grid_cursor_area(ctx, cursor.row, cursor.col)
        }
    };
    let Some(area) = area else { return };
    window.set_ime_cursor_area(
        PhysicalPosition::new(area.x.round() as i32, area.y.round() as i32),
        PhysicalSize::new(
            area.width.max(1.0).round() as u32,
            area.height.max(1.0).round() as u32,
        ),
    );
}

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
    use super::{
        direct_keyboard_text, editor_cursor_area_from_buffer, encode_passthrough_key,
        grid_cursor_area, ImeCursorArea,
    };
    use crate::layout::LayoutCtx;
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

    #[test]
    fn tui_ime_anchor_uses_content_origin_and_grid_cursor() {
        let ctx = LayoutCtx {
            viewport: (1200.0, 800.0),
            cell_w: 10.0,
            cell_h: 20.0,
            padding_x: 12.0,
            padding_y: 8.0,
            chrome_top: 32.0,
            chrome_left: 200.0,
            pane_origin: (0.0, 0.0),
            clip: None,
        };
        assert_eq!(
            grid_cursor_area(ctx, 4, 7),
            Some(ImeCursorArea {
                x: 282.0,
                y: 120.0,
                width: 10.0,
                height: 20.0,
            })
        );
    }

    #[test]
    fn invalid_cell_metrics_do_not_publish_native_ime_anchor() {
        let mut ctx = LayoutCtx::new((100.0, 100.0), 0.0, 20.0, 0.0, 0.0);
        assert_eq!(grid_cursor_area(ctx, 0, 0), None);
        ctx.cell_w = 10.0;
        ctx.cell_h = f32::NAN;
        assert_eq!(grid_cursor_area(ctx, 0, 0), None);
    }

    #[test]
    fn editor_ime_anchor_counts_cjk_as_two_display_columns() {
        let ctx = LayoutCtx::new((800.0, 600.0), 10.0, 20.0, 12.0, 8.0);
        let lines = vec!["A中B".to_string()];
        let area = editor_cursor_area_from_buffer(ctx, &lines, (0, 2), 0).unwrap();
        // Text has a 1.5-cell content gutter, then "> " (two cells);
        // A + 中 occupy three more cells.
        assert_eq!(area.x, 12.0 + 6.5 * 10.0);
        assert_eq!(area.y, 552.0);
        assert_eq!(area.width, 10.0);
        assert_eq!(area.height, 20.0);
    }
}
