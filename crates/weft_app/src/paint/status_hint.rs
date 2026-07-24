//! F2 P0-2: Lightweight status hint badge rendered when the terminal is in
//! a non-editor, non-integrated passthrough state.
//!
//! Drawn as a small, semi-transparent text label at the bottom-left of the
//! content area so the user knows why the editor input box is unavailable.
//! "Non-disturbing" design: 50% alpha accent color, single text row.

use crate::paint::primitives::color_to_normalized;
use crate::renderer::MetalRenderer;
use weft_core::blocks::ShellPhase;

/// The status hint text shown for each non-editor state, in priority order
/// Returns `None` in Editor mode and alt-screen mode (full-screen TUIs own
/// every terminal cell).
fn status_hint_text(terminal: &weft_core::vt::Terminal) -> Option<&'static str> {
    if terminal.is_alt_screen_active() {
        // Full-screen TUIs own every cell; never cover their status line.
        return None;
    }
    match terminal.block_tracker().phase() {
        ShellPhase::NotIntegrated => Some("\u{25be} passthrough"),
        ShellPhase::AtPrompt | ShellPhase::CommandExecuting => None,
    }
}

impl MetalRenderer {
    /// Build vertices for the bottom-left status hint badge. Returns an empty
    /// `Vec` when no hint is applicable (Editor / AtPrompt mode).
    pub(crate) fn build_status_hint_vertices(
        &self,
        terminal: &weft_core::vt::Terminal,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let Some(hint) = status_hint_text(terminal) else {
            return verts;
        };
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_h <= 0.0 {
            return verts;
        }
        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return verts,
        };
        // Position: bottom-left of the content area, one row above the
        // bottom padding. Keep it subtle — 50% alpha accent color.
        let x = ctx.left();
        let y = (ctx.bottom() - ch).max(ctx.top());
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let accent = color_to_normalized(ui.focus);
        let fg = [accent[0], accent[1], accent[2], accent[3] * 0.50];
        let max_cols = hint.chars().count();
        self.push_text(&mut verts, x, y, hint, fg, max_cols);
        verts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::ShellPhase;
    use weft_core::vt::Terminal;

    #[test]
    fn no_hint_at_prompt() {
        let mut t = Terminal::new(24, 80);
        // Simulate bootstrap + AtPrompt.
        t.process(b"\x1b]133;A\x07");
        assert_eq!(t.block_tracker().phase(), ShellPhase::AtPrompt);
        assert!(status_hint_text(&t).is_none());
    }

    #[test]
    fn hint_passthrough_when_not_integrated() {
        let t = Terminal::new(24, 80);
        assert_eq!(t.block_tracker().phase(), ShellPhase::NotIntegrated);
        assert_eq!(status_hint_text(&t), Some("\u{25be} passthrough"));
    }

    #[test]
    fn hint_alt_screen_when_active() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b[?1049h");
        assert!(t.is_alt_screen_active());
        assert_eq!(status_hint_text(&t), None);
    }

    #[test]
    fn running_command_uses_block_indicator_instead_of_bottom_overlay() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07"); // AtPrompt
        t.process(b"\x1b]133;B\x07"); // CommandExecuting
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        assert_eq!(status_hint_text(&t), None);
    }

    #[test]
    fn alt_screen_takes_priority_over_running() {
        let mut t = Terminal::new(24, 80);
        t.process(b"\x1b]133;A\x07"); // AtPrompt
        t.process(b"\x1b]133;B\x07"); // CommandExecuting
        t.process(b"\x1b[?1049h"); // alt screen
        assert!(t.is_alt_screen_active());
        assert_eq!(t.block_tracker().phase(), ShellPhase::CommandExecuting);
        assert_eq!(status_hint_text(&t), None);
    }
}
