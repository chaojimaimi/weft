//! Block-view stages of `handle_mouse_press` (v1.12.27b P1-02), moved
//! verbatim from `mouse_press_controller.rs` — baseline :387-506. Order
//! within the file is the original cascade order.

use super::*;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the block-scrollbar level
    /// (baseline :387-424).
    pub(crate) fn press_block_scrollbar(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // The block-view scrollbar owns its expanded hit strip before text
        // selection and PTY mouse reporting. Clicking the track jumps the
        // thumb under the pointer and immediately begins a drag.
        if button == winit::event::MouseButton::Left {
            if let Some(layout) = self.active_scrollbar_layout() {
                let xf = x as f32;
                let yf = y as f32;
                if crate::scrollbar_component::contains(layout.hit, xf, yf) {
                    let thumb_h = layout.thumb[3] - layout.thumb[1];
                    let grab_offset =
                        crate::scrollbar_component::thumb_grab_offset(&layout, xf, yf, true)
                            .unwrap_or(thumb_h / 2.0);
                    let offset = crate::scrollbar_component::scroll_offset_for_pointer(
                        &layout,
                        yf,
                        grab_offset,
                    );
                    // Clear before set_block_scroll's internal sync so the
                    // history view can exit in the same press (a surviving
                    // block selection would hold the snapshot view open).
                    if let Some(tab) = self.sessions.active_mut() {
                        tab.selection_handler.clear();
                        tab.set_block_scroll(offset);
                    }
                    self.interaction.scrollbar_drag =
                        Some(crate::scrollbar_component::ScrollbarDragState {
                            layout,
                            grab_offset,
                        });
                    self.interaction.scrollbar_hovered = true;
                    if let Some(window) = &self.window {
                        window.set_cursor(winit::window::CursorIcon::NsResize);
                    }
                    self.request_redraw();
                    return PressOutcome::Consumed;
                }
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the block-header action level
    /// (baseline :426-461).
    pub(crate) fn press_block_header_actions(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // F3-1: Block header hover-action buttons (copy / fold). These are
        // registered as HitRegions during draw() on the header row's right
        // side. Check the renderer's cached hit_regions BEFORE the chevron
        // handler so clicks on the small action buttons don't fall through to
        // text selection. Buttons only render when the block is hovered, but
        // the hit regions are always registered (so clicks work even if the
        // hover state lagged behind by a frame).
        if button == winit::event::MouseButton::Left && self.block_view_active() {
            if let Some(renderer) = &self.renderer {
                let xf = x as f32;
                let yf = y as f32;
                let hit =
                    crate::block_component::block_header_action_at(&renderer.hit_regions, xf, yf);
                if let Some(hit) = hit {
                    match hit {
                        crate::block_component::BlockHeaderAction::Copy(id) => {
                            self.run_context_action(Some(id), "copy_command");
                            self.request_redraw();
                        }
                        crate::block_component::BlockHeaderAction::ToggleFold(id) => {
                            self.run_context_action(Some(id), "toggle_fold");
                            self.request_redraw();
                        }
                        crate::block_component::BlockHeaderAction::Diagnose(id) => {
                            // v1.8.2: Trigger AI diagnose for this failed block.
                            self.spawn_block_diagnose(id);
                        }
                        crate::block_component::BlockHeaderAction::CloseDiagnose(id) => {
                            // v1.8.2: Close the diagnose panel for this block.
                            self.close_block_diagnose(id);
                        }
                    }
                    return PressOutcome::Consumed;
                }
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the collapse-chevron level
    /// (baseline :463-506). The `compute_block_view_rows` failure path
    /// (baseline :482) also consumed the press (plain `return`), preserved
    /// as `Consumed`.
    pub(crate) fn press_collapse_chevron(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v0.9 W3 (revised): block collapse/expand — only clicking the chevron
        // (▸/▾ in the first cell of a Command row) toggles fold. Clicking the
        // rest of the command line starts a normal text selection instead, so
        // the user can select/copy command text. This reverts the earlier
        // "click anywhere on the command line folds" behavior.
        if button == winit::event::MouseButton::Left && self.block_view_active() {
            if let Some(renderer) = &self.renderer {
                let content_left = self
                    .renderer
                    .as_ref()
                    .and_then(|renderer| renderer.layout_ctx)
                    .map(|ctx| ctx.left())
                    .unwrap_or_else(|| renderer.padding_x());
                let cw = renderer.cell_width() as f32;
                let xf = x as f32;
                let yf = y as f32;
                // Chevron occupies the first cell [content_left, content_left + cw).
                if xf >= content_left && xf < content_left + cw {
                    let Some((rows, _, _)) = self.compute_block_view_rows() else {
                        return PressOutcome::Consumed;
                    };
                    for row in &rows {
                        if row.kind == weft_core::selection::BlockViewRowKind::Command
                            && yf >= row.y_top
                            && yf < row.y_bottom
                        {
                            if let Some(bid) = row.block_id {
                                if let Some(term) = self
                                    .sessions
                                    .active_mut()
                                    .and_then(|tab| tab.terminal.as_mut())
                                {
                                    term.block_tracker_mut().toggle_collapse(bid);
                                    // M5-b P2-2: no invalidate — collapsed mismatch
                                    // takes the WidthOnly rebuild path (L1 kept).
                                    self.request_redraw();
                                }
                            }
                            return PressOutcome::Consumed;
                        }
                    }
                }
            }
        }
        PressOutcome::NotHit
    }
}
