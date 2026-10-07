//! Prompt-editor stage of `handle_mouse_press` (v1.12.27b P1-02), moved
//! verbatim from `mouse_press_controller.rs` — baseline :596-641.

use super::*;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the prompt level (baseline
    /// :596-641). The in-prompt hit consumed the press; the miss path keeps
    /// its side effects (clear editor selection + `prompt_dragging = false`)
    /// inside this stage and falls through — per the plan these MUST NOT
    /// move to the caller.
    pub(crate) fn press_prompt(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v0.9: click inside the prompt input box → position the editor
        // cursor at the clicked char and start a mouse-drag selection (so
        // the user can select/copy part of the command). Clicks outside the
        // prompt box clear any active editor selection.
        if button == winit::event::MouseButton::Left {
            let in_prompt = self
                .prompt_box_rect()
                .map(|[x0, y0, x1, y1]| {
                    let xf = x as f32;
                    let yf = y as f32;
                    xf >= x0 && xf <= x1 && yf >= y0 && yf <= y1
                })
                .unwrap_or(false);
            if in_prompt {
                if let Some(pos) = self.pixel_to_editor_pos(x, y) {
                    if let Some(t) = self
                        .sessions
                        .active_mut()
                        .and_then(|tab| tab.terminal.as_mut())
                    {
                        t.editor_mut().buffer.start_selection(pos);
                    }
                    self.interaction.prompt_dragging = true;
                    // Clear any block/grid selection so Cmd+C targets the editor.
                    if let Some(tab) = self.sessions.active_mut() {
                        tab.selection_handler.clear();
                        // v1.10.20 (S1): clearing the block selection releases
                        // the delayed history-view exit — sync drops the
                        // snapshot view back to the live grid.
                        tab.sync_primary_history_view();
                    }
                    self.request_redraw();
                }
                return PressOutcome::Consumed;
            } else if let Some(t) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.terminal.as_mut())
            {
                if t.editor().buffer.has_selection() {
                    t.editor_mut().buffer.clear_selection();
                    self.request_redraw();
                }
            }
            self.interaction.prompt_dragging = false;
        }
        PressOutcome::NotHit
    }
}
