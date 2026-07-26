//! Read-only BlockView row geometry used by pointer hit-testing.

use crate::paint::block_view_model::BlockViewPaintModel;
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;
use weft_core::selection::{BlockViewRow, BlockViewRowKind};

use super::actions::block_header_band_height;
use super::layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput};

/// Return the block whose output is clipped by the top edge while its command
/// row is already offscreen. That block owns the sticky CWD/command header.
pub(crate) fn sticky_block_id(
    rows: &[BlockViewRow],
    clip_top: f32,
    clip_bottom: f32,
) -> Option<BlockId> {
    let top = rows
        .iter()
        .filter(|row| row.block_id.is_some() && row.y_bottom > clip_top && row.y_top < clip_bottom)
        .min_by(|a, b| a.y_top.total_cmp(&b.y_top))?;
    let block_id = top.block_id?;
    let command = rows
        .iter()
        .find(|row| row.block_id == Some(block_id) && row.kind == BlockViewRowKind::Command)?;
    (command.y_top < clip_top).then_some(block_id)
}

pub(super) fn sticky_header_rows(has_cwd: bool) -> usize {
    if has_cwd {
        2
    } else {
        1
    }
}

impl MetalRenderer {
    /// Compute block-view rows for hit-testing WITHOUT building vertices.
    /// This is the M1 on-demand alternative to caching `block_view_rows`
    /// during `draw()`. The geometry_controller calls this when a mouse
    /// event arrives, so hit-testing uses current-frame data instead of
    /// the renderer's previous-frame cache.
    ///
    /// Shares the layout pass with `build_block_view_vertices` via
    /// `compute_block_layout_pass`, so y-bands are identical between paint
    /// and hit-testing. Only the bv_rows extraction is specific to this path.
    pub(crate) fn compute_block_view_rows(
        &self,
        model: BlockViewPaintModel<'_>,
    ) -> Vec<weft_core::selection::BlockViewRow> {
        let BlockViewPaintModel {
            blocks,
            region_bottom_y,
            cwd,
            git_branch,
            live,
            block_scroll,
            viewport_rows,
            block_hovered: _,
            spinner_phase: _,
            find_block_highlight: _,
            palette: _,
        } = model;

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return Vec::new();
        }

        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return Vec::new(),
        };
        let cwd_header_active = cwd.is_some() && live.is_none();
        let layout = crate::layout::layout_block_view(&ctx, region_bottom_y, cwd_header_active);
        let pitch = layout.pitch;
        let header_height = block_header_band_height(pitch, self.scale);
        let content_bottom_y = layout.clip_bottom;
        let cols = layout.cols;

        // Shared layout pass (single source of truth for row geometry).
        // resolve_styles=false: hit-testing doesn't need StyledLine lookups.
        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
            // R2-2 (Batch 7): build prefix sum so the layout pass can
            // binary-search the visible range. Must mirror the paint path
            // (build_block_view_vertices) so hit-testing y-bands match.
            cache.build_prefix_sum(blocks);
        }
        let layout_out = {
            let cache = self.block_layout_cache.borrow();
            compute_block_layout_pass(
                LayoutPassInput {
                    blocks,
                    live,
                    cwd,
                    git_branch,
                    block_scroll,
                    viewport_rows,
                    cols,
                    pitch,
                    header_height,
                    content_bottom_y,
                    clip_top: layout.clip_top,
                    clip_bottom: content_bottom_y,
                    resolve_styles: false,
                    styled_lookup_counter: None,
                },
                &cache,
            )
        };

        // Extract bv_rows from the layout output.
        let scroll_px = (block_scroll as f32) * pitch;
        let mut bv_rows = Vec::new();

        for (i, &dist) in layout_out.rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            match &layout_out.row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    ..
                } => {
                    if chunks.len() <= 1 {
                        bv_rows.push(BlockViewRow {
                            kind: BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: row_top_y,
                            y_bottom: row_top_y + pitch,
                        });
                    } else {
                        for (ci, chunk) in chunks.iter().enumerate() {
                            let cy = row_top_y + ci as f32 * pitch;
                            bv_rows.push(BlockViewRow {
                                kind: BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                            });
                        }
                    }
                }
                LaidRow::Command {
                    command, block_id, ..
                } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Header { text, block_id, .. } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + header_height,
                    });
                }
                LaidRow::LiveHeader { text } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::LiveCommand { command } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::LiveCommand,
                        text: command.to_string(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Blank => {}
            }
        }

        bv_rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: BlockViewRowKind, id: u64, y_top: f32, y_bottom: f32) -> BlockViewRow {
        BlockViewRow {
            kind,
            text: String::new(),
            block_id: Some(BlockId(id)),
            y_top,
            y_bottom,
        }
    }

    #[test]
    fn sticky_header_tracks_output_whose_command_scrolled_above_viewport() {
        let rows = [
            row(BlockViewRowKind::Command, 7, -20.0, 0.0),
            row(BlockViewRowKind::Output, 7, -2.0, 18.0),
            row(BlockViewRowKind::Output, 7, 18.0, 38.0),
        ];
        assert_eq!(sticky_block_id(&rows, 0.0, 100.0), Some(BlockId(7)));
    }

    #[test]
    fn sticky_header_stays_hidden_while_command_is_visible() {
        let rows = [
            row(BlockViewRowKind::Command, 9, 4.0, 24.0),
            row(BlockViewRowKind::Output, 9, 24.0, 44.0),
        ];
        assert_eq!(sticky_block_id(&rows, 0.0, 100.0), None);
    }

    #[test]
    fn sticky_header_uses_a_separate_context_row_when_cwd_is_known() {
        assert_eq!(sticky_header_rows(true), 2);
        assert_eq!(sticky_header_rows(false), 1);
    }
}
