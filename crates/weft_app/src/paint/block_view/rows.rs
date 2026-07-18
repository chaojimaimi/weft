//! Read-only BlockView row geometry used by pointer hit-testing.

use std::rc::Rc;

use crate::block_component::{block_presentation, command_resume_hints, live_context_label};
use crate::paint::block_view_model::BlockViewPaintModel;
use crate::paint::grid_cache::wrap_line_chunks;
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;
use weft_core::selection::{BlockViewRow, BlockViewRowKind};

/// Return the block whose output is clipped by the top edge while its command
/// row is already offscreen. That block owns the sticky CWD/command header.
pub(super) fn sticky_block_id(
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
    /// The layout logic mirrors `build_block_view_vertices` (layout pass +
    /// bv_rows extraction). Both paths use the same `layout_block_view` +
    /// `wrap_line_chunks` + `block_layout_cache` so the y-bands are identical.
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
            block_hovered: _,
            spinner_phase: _,
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
        let content_bottom_y = layout.clip_bottom;
        let cols = layout.cols;

        // Layout pass: compute cumulative y-distances + row data.
        // Mirrors build_block_view_vertices lines 96-209.
        enum LaidRow<'a> {
            Output {
                text: &'a str,
                chunks: Rc<[String]>,
                block_id: Option<BlockId>,
            },
            Command {
                command: &'a str,
                block_id: BlockId,
            },
            Header {
                text: String,
                block_id: BlockId,
            },
            Separator,
            LiveCommand {
                command: &'a str,
            },
            LiveHeader {
                text: String,
            },
            Blank,
        }

        let mut rows: Vec<f32> = Vec::new();
        let mut row_data: Vec<LaidRow> = Vec::new();
        let mut cursor_dist = 0.0;

        if let Some(live) = live {
            const MAX_LAYOUT_LINES_LIVE: usize = 2000;
            let all_lines: Vec<&str> = live.output.lines().collect();
            let skip = all_lines.len().saturating_sub(MAX_LAYOUT_LINES_LIVE);
            let live_lines: Vec<&str> = all_lines[skip..].to_vec();
            for line in live_lines.iter().rev() {
                let chunks: Rc<[String]> =
                    Rc::from(wrap_line_chunks(line, cols).collect::<Vec<_>>());
                let vis_rows = chunks.len();
                cursor_dist += vis_rows as f32 * pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Output {
                    text: line,
                    chunks,
                    block_id: None,
                });
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::LiveCommand {
                command: live.command,
            });
            if let Some(text) = live_context_label(live.cwd.or(cwd), git_branch) {
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::LiveHeader { text });
            }
            cursor_dist += pitch;
            rows.push(cursor_dist);
            row_data.push(LaidRow::Separator);
        }

        {
            let mut cache = self.block_layout_cache.borrow_mut();
            for b in blocks.iter() {
                cache.ensure_cached(b, cols);
            }
        }
        {
            let cache = self.block_layout_cache.borrow();
            for b in blocks.iter().rev() {
                let cached = cache.get(b.id.0);
                if !b.collapsed {
                    for hint in command_resume_hints(b).iter().rev() {
                        let chunks: Rc<[String]> =
                            Rc::from(wrap_line_chunks(hint, cols).collect::<Vec<_>>());
                        cursor_dist += chunks.len() as f32 * pitch;
                        rows.push(cursor_dist);
                        row_data.push(LaidRow::Output {
                            text: hint,
                            chunks,
                            block_id: Some(b.id),
                        });
                    }
                    for line in cached.lines.iter().rev() {
                        let text = &b.output[line.byte_start..line.byte_end];
                        let vis_rows = line.chunks.len();
                        cursor_dist += vis_rows as f32 * pitch;
                        rows.push(cursor_dist);
                        row_data.push(LaidRow::Output {
                            text,
                            chunks: Rc::clone(&line.chunks),
                            block_id: Some(b.id),
                        });
                    }
                }
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Command {
                    command: &b.command,
                    block_id: b.id,
                });
                cursor_dist += pitch;
                rows.push(cursor_dist);
                let presentation = block_presentation(b, cached.lines.len());
                row_data.push(LaidRow::Header {
                    text: presentation.label,
                    block_id: b.id,
                });
                cursor_dist += pitch;
                rows.push(cursor_dist);
                row_data.push(LaidRow::Separator);
                if b.command.split_whitespace().next() == Some("clear") {
                    cursor_dist += vp_h;
                    rows.push(cursor_dist);
                    row_data.push(LaidRow::Blank);
                }
            }
        }

        // Extract bv_rows from the layout data.
        // Mirrors build_block_view_vertices lines 229-302 (bv_rows portion only).
        let scroll_px = (block_scroll as f32) * pitch;
        let mut bv_rows = Vec::new();

        for (i, &dist) in rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            match &row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
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
                LaidRow::Command { command, block_id } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Command,
                        text: command.to_string(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                    });
                }
                LaidRow::Header { text, block_id } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
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
