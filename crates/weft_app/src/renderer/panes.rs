//! Per-pane base-content rendering for split tabs.

use crate::glyph::GlyphAtlas;
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::overlays::FindDrawState;
use crate::paint::prompt::PromptDrawParams;
use crate::paint::tab_bar::TabBarDrawState;
use crate::renderer::MetalRenderer;
use weft_core::input::InputMode;
use weft_core::selection::SelectionHandler;
use weft_core::vt::Terminal;

/// Immutable draw inputs for one non-active pane.
#[derive(Clone, Copy)]
pub struct PaneRenderInfo<'a> {
    pub rect: crate::layout::Rect,
    pub terminal: &'a Terminal,
    pub block_scroll: f32,
    pub submit_on_ctrl_enter: bool,
    /// v1.4.1: pane-scoped namespace for the styled-line vertex cache.
    pub pane_session_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneBaseView {
    Block,
    Grid,
}

fn pane_base_view(terminal: &Terminal) -> PaneBaseView {
    if terminal.show_block_view() {
        PaneBaseView::Block
    } else {
        PaneBaseView::Grid
    }
}

fn background_prompt<'a>(pane: &'a PaneRenderInfo<'a>) -> Option<PromptDrawParams<'a>> {
    (pane.terminal.effective_input_mode() == InputMode::Editor).then(|| PromptDrawParams {
        focused: false,
        cwd: pane.terminal.cwd(),
        lines: &pane.terminal.editor().buffer.lines,
        cursor: pane.terminal.editor().buffer.cursor,
        preedit: None,
        preedit_cursor: None,
        search: pane.terminal.editor().search_view(),
        selection: None,
        scroll_offset: pane.terminal.editor().buffer.scroll_offset,
        submit_on_ctrl_enter: pane.submit_on_ctrl_enter,
    })
}

impl MetalRenderer {
    pub(super) fn warm_background_pane_atlases(
        atlas: &mut GlyphAtlas,
        viewport_h: f32,
        panes: &[PaneRenderInfo<'_>],
        tab_bar: &TabBarDrawState,
    ) {
        let no_find: Option<FindDrawState> = None;
        let no_note: Option<crate::paint::overlays::NoteEditorDrawState> = None;
        for pane in panes {
            let prompt = background_prompt(pane);
            Self::warm_atlas(
                atlas,
                viewport_h,
                &no_find,
                &no_note,
                pane.terminal,
                pane.terminal.grid(),
                None,
                prompt.as_ref(),
                None,
                None,
                None,
                None,
                tab_bar,
                // Background panes don't render diagnose panels.
                &[],
            );
        }
    }

    /// Build one background pane's base content. Block-view panes emit legacy
    /// vertices bounded by LayoutCtx.clip; grid panes emit a dual-stream
    /// `GridInstanceBatch` whose bg/glyph float offsets are appended to the
    /// caller's flat buffers and recorded as a `PaneInstanceRanges` entry
    /// (paired with the pane's scissor rect). Interactive state stays
    /// active-only.
    pub(super) fn build_background_pane_content(
        &self,
        pane: &PaneRenderInfo<'_>,
        bg_stream: &mut Vec<f32>,
        glyph_stream: &mut Vec<f32>,
    ) -> Vec<f32> {
        match pane_base_view(pane.terminal) {
            PaneBaseView::Grid => {
                let bg_start = bg_stream.len();
                let glyph_start = glyph_stream.len();
                let pane_batch = self.build_grid_instances_for_background_pane(pane.terminal);
                bg_stream.extend(pane_batch.bg_stream);
                glyph_stream.extend(pane_batch.glyph_stream);
                let bg_end = bg_stream.len();
                let glyph_end = glyph_stream.len();
                self.pane_instance_ranges.borrow_mut().push((
                    pane.rect,
                    crate::paint::grid_instances::PaneInstanceRanges {
                        bg_range: (bg_start, bg_end),
                        glyph_range: (glyph_start, glyph_end),
                    },
                ));
                tracing::debug!(
                    bg_rect = ?pane.rect,
                    bg_run_count = (bg_end - bg_start) / 8,
                    glyph_instance_count = (glyph_end - glyph_start) / 16,
                    grid_rows = pane.terminal.grid().num_rows,
                    grid_cols = pane.terminal.grid().num_cols,
                    "built background grid pane"
                );
                Vec::new()
            }
            PaneBaseView::Block => {
                let editor_mode = pane.terminal.effective_input_mode() == InputMode::Editor;
                let prompt = background_prompt(pane);
                let ctx = self.layout_ctx.expect("background pane LayoutCtx");
                let region_bottom_y = prompt.as_ref().map_or_else(
                    || ctx.bottom(),
                    |p| {
                        crate::paint::prompt::prompt_layout_for_buffer(&ctx, p.lines, p.cursor)
                            .1
                            .box_rect[1]
                    },
                );
                let mut selection = SelectionHandler::new();
                let (mut vertices, _, _) = self.build_block_view_vertices(
                    BlockViewPaintModel {
                        blocks: pane.terminal.block_tracker().session_blocks(),
                        live_head_lines: pane.terminal.screen_head_lines(),
                        region_bottom_y,
                        cwd: pane.terminal.cwd(),
                        git_branch: pane.terminal.git_branch(),
                        live: (!editor_mode)
                            .then(|| pane.terminal.block_tracker().in_flight())
                            .flatten(),
                        block_scroll: pane.block_scroll,
                        viewport_rows: pane.terminal.grid().num_rows,
                        block_hovered: None,
                        block_selected: None,
                        block_action_hovered: None,
                        spinner_phase: -1.0,
                        find_block_highlight: None,
                        palette: pane.terminal.palette(),
                        cache_namespace: pane.pane_session_id,
                        block_diagnose_state: &std::collections::HashMap::new(),
                        ai_configured: false,
                        tui_cursor: None,
                        tui_preedit: None,
                        cursor_blink_on: false,
                        is_alt: pane.terminal.is_alt_screen_active(),
                    },
                    &mut selection,
                );
                if let Some(prompt) = prompt {
                    vertices.extend(self.build_prompt_vertices(&prompt, 0.0, false));
                }
                tracing::debug!(bg_rect = ?pane.rect, "built background block pane");
                vertices
            }
        }
    }

    pub(crate) fn draw_legacy_vertex_ranges(
        &self,
        encoder: &metal::RenderCommandEncoderRef,
        vertices_len: usize,
    ) {
        let ranges = self.pane_vertex_ranges.borrow();
        let mut pane_end = 0usize;
        for (rect, range) in ranges.iter() {
            debug_assert_eq!(range.start, pane_end, "pane vertex ranges form a prefix");
            let [x0, y0, x1, y1] = *rect;
            let sx = x0.max(0.0).min(self.viewport.0) as u64;
            let sy = y0.max(0.0).min(self.viewport.1) as u64;
            let sw = (x1 - x0).max(0.0).min(self.viewport.0 - sx as f32) as u64;
            let sh = (y1 - y0).max(0.0).min(self.viewport.1 - sy as f32) as u64;
            pane_end = range.end;
            if sw == 0 || sh == 0 || range.end <= range.start {
                continue;
            }
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: sx,
                y: sy,
                width: sw,
                height: sh,
            });
            encoder.draw_primitives(
                metal::MTLPrimitiveType::Triangle,
                (range.start / 12) as u64,
                ((range.end - range.start) / 12) as u64,
            );
        }

        let vertex_count = vertices_len / 12;
        let tail_start = pane_end / 12;
        if tail_start < vertex_count {
            encoder.set_scissor_rect(metal::MTLScissorRect {
                x: 0,
                y: 0,
                width: self.viewport.0 as u64,
                height: self.viewport.1 as u64,
            });
            encoder.draw_primitives(
                metal::MTLPrimitiveType::Triangle,
                tail_start as u64,
                (vertex_count - tail_start) as u64,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_pane_uses_its_own_terminal_view_mode() {
        let mut terminal = Terminal::new(24, 80);
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Grid);

        terminal.process(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        assert!(terminal.show_block_view());
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Block);

        terminal.process(b"\x1b[?1049h");
        assert!(!terminal.show_block_view());
        assert_eq!(pane_base_view(&terminal), PaneBaseView::Grid);
    }

    #[test]
    fn background_prompt_keeps_editor_content_without_focus() {
        let mut terminal = Terminal::new(24, 80);
        terminal.process(b"\x1b]133;A\x07");
        terminal.editor_mut().buffer.set_text("draft command");
        let pane = PaneRenderInfo {
            rect: [0.0, 0.0, 400.0, 300.0],
            terminal: &terminal,
            block_scroll: 0.0,
            submit_on_ctrl_enter: true,
            pane_session_id: 1,
        };

        let prompt = background_prompt(&pane).expect("integrated prompt");
        assert!(!prompt.focused);
        assert_eq!(prompt.lines, &["draft command"]);
        assert!(prompt.submit_on_ctrl_enter);
    }
}
