//! Host-owned context band for primary-screen TUIs.
//!
//! Programs such as Claude Code do not enter DEC 1049. On every SIGWINCH
//! they home the cursor, clear the primary viewport and repaint it, so shell
//! context stored in that same Grid is necessarily erased. These two rows are
//! outside the dimensions advertised to the PTY and therefore remain stable.

use crate::block_component::live_context_label;
use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::renderer::MetalRenderer;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrimaryTuiContext {
    pub(crate) cwd: String,
    pub(crate) command: String,
}

pub(crate) fn primary_tui_context(terminal: &weft_core::vt::Terminal) -> Option<PrimaryTuiContext> {
    if !terminal.primary_screen_app_active() {
        return None;
    }
    let live = terminal.block_tracker().in_flight()?;
    Some(PrimaryTuiContext {
        cwd: live_context_label(live.cwd.or(terminal.cwd()), terminal.git_branch())
            .unwrap_or_else(|| "~".to_string()),
        command: live.command.to_string(),
    })
}

impl MetalRenderer {
    pub(crate) fn build_primary_tui_context_vertices(
        &self,
        terminal: &weft_core::vt::Terminal,
    ) -> Vec<f32> {
        let Some(context) = primary_tui_context(terminal) else {
            return Vec::new();
        };
        let Some(ctx) = self.layout_ctx else {
            return Vec::new();
        };
        let ch = ctx.cell_h;
        if ch <= 0.0 {
            return Vec::new();
        }
        let top = ctx.top() - crate::terminal_geometry::PRIMARY_TUI_CONTEXT_ROWS as f32 * ch;
        let bottom = ctx.top();
        let colors = crate::ui_tokens::UiColors::from_theme(&self.theme);
        let mut vertices = Vec::new();
        push_quad(
            &mut vertices,
            [ctx.left(), top, ctx.right(), bottom],
            [0.0; 4],
            [0.0; 4],
            color_to_normalized(colors.raised),
        );
        push_quad(
            &mut vertices,
            [ctx.left(), bottom - 1.0, ctx.right(), bottom],
            [0.0; 4],
            [0.0; 4],
            color_to_normalized(colors.border_subtle),
        );
        let cols = terminal.grid().num_cols;
        self.push_text(
            &mut vertices,
            ctx.left(),
            top,
            &context.cwd,
            color_to_normalized(colors.text_secondary),
            cols,
        );
        self.push_text(
            &mut vertices,
            ctx.left(),
            top + ch,
            &format!("❯ {}", context.command),
            color_to_normalized(colors.text_primary),
            cols,
        );
        vertices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::vt::Terminal;

    #[test]
    fn context_survives_a_tui_clear_and_comes_from_shell_state() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]7;file://localhost/tmp/claude\x07");
        terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
        terminal.process(b"\x1b[H\x1b[2K\x1b[2;1HClaude Code");
        assert!(terminal.primary_screen_app_active());
        assert_eq!(
            primary_tui_context(&terminal),
            Some(PrimaryTuiContext {
                cwd: "/tmp/claude".into(),
                command: "claude".into(),
            })
        );
    }
}
