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

/// Expand wrapped chunks into (chunk index, y_top, char offset) triples
/// ordered bottom-to-top — the invariant `bv_rows` relies on (index grows
/// upward). The last chunk (visually lowest) is pushed first; char offsets
/// are precomputed as prefix sums since iteration is reversed.
pub(super) fn wrapped_row_positions(
    y: f32,
    pitch: f32,
    chunks: &[String],
) -> Vec<(usize, f32, usize)> {
    // Prefix sums: char offset of each chunk within the source line.
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut acc = 0usize;
    for chunk in chunks {
        offsets.push(acc);
        acc += chunk.chars().count();
    }
    // Bottom-to-top: highest ci (lowest visually) first.
    (0..chunks.len())
        .rev()
        .map(|ci| (ci, y + ci as f32 * pitch, offsets[ci]))
        .collect()
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
    /// Returns `(rows, clip_top, clip_bottom)`: the laid-out rows plus the
    /// visible content band's clip boundaries from the layout pass. Callers
    /// that need the visible content edge (e.g. autoscroll) must use the clip
    /// values, not rows first/last — bv_rows carries overscan, so its topmost
    /// row can sit above `clip_top - overscan`.
    pub(crate) fn compute_block_view_rows(
        &self,
        model: BlockViewPaintModel<'_>,
    ) -> (Vec<weft_core::selection::BlockViewRow>, f32, f32) {
        let BlockViewPaintModel {
            blocks,
            region_bottom_y,
            cwd,
            git_branch,
            live,
            block_scroll,
            viewport_rows,
            block_hovered: _,
            block_selected: _,
            block_action_hovered: _,
            spinner_phase: _,
            find_block_highlight: _,
            palette: _,
            cache_namespace: _,
            block_diagnose_state,
            ai_configured: _,
            tui_cursor: _,
            tui_preedit: _,
            cursor_blink_on: _,
        } = model;

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (Vec::new(), 0.0, 0.0);
        }

        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return (Vec::new(), 0.0, 0.0),
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
            cache.sync_blocks(blocks, cols);
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
                    block_diagnose_state,
                },
                &cache,
            )
        };

        // Extract bv_rows from the layout output.
        let scroll_px = block_scroll * pitch;
        let mut bv_rows = Vec::new();

        for (i, &dist) in layout_out.rows.iter().enumerate() {
            let row_top_y = content_bottom_y - dist + scroll_px;
            match &layout_out.row_data[i] {
                LaidRow::Output {
                    text,
                    chunks,
                    block_id,
                    line,
                    ..
                } => {
                    // v1.6.1: carry the line index so click-time hyperlink
                    // resolution can look up `StyledLine::link_at`. Skip
                    // resume hints (line == usize::MAX) — they have no styled
                    // output and aren't clickable.
                    let line_idx = (*line != usize::MAX).then_some(*line);
                    if chunks.len() <= 1 {
                        bv_rows.push(BlockViewRow {
                            kind: BlockViewRowKind::Output,
                            text: text.to_string(),
                            block_id: *block_id,
                            y_top: row_top_y,
                            y_bottom: row_top_y + pitch,
                            line: line_idx,
                            chunk_char_offset: 0,
                            indent_cols: 0,
                        });
                    } else {
                        // v1.6.1: track cumulative char offset per chunk so
                        // wrapped-line link resolution can compute the
                        // full-line char index.
                        for (ci, cy, char_offset) in wrapped_row_positions(row_top_y, pitch, chunks)
                        {
                            let chunk = &chunks[ci];
                            bv_rows.push(BlockViewRow {
                                kind: BlockViewRowKind::Output,
                                text: chunk.clone(),
                                block_id: *block_id,
                                y_top: cy,
                                y_bottom: cy + pitch,
                                line: line_idx,
                                chunk_char_offset: char_offset,
                                indent_cols: 0,
                            });
                        }
                    }
                }
                LaidRow::Command {
                    chunks,
                    foldable,
                    block_id,
                    ..
                } => {
                    // R2-2 (stage 2): one BlockViewRow per wrapped chunk
                    // (mirrors Output's expansion) so hit-testing and
                    // selection see every command line.
                    for (ci, cy, char_offset) in wrapped_row_positions(row_top_y, pitch, chunks) {
                        let chunk = &chunks[ci];
                        bv_rows.push(BlockViewRow {
                            kind: BlockViewRowKind::Command,
                            text: chunk.clone(),
                            block_id: Some(*block_id),
                            y_top: cy,
                            y_bottom: cy + pitch,
                            line: None,
                            // v1.10.13: first line renders after chevron + "> "
                            // (3 cols if foldable else 2); continuations are
                            // flush-left.
                            indent_cols: if ci == 0 {
                                if *foldable {
                                    3
                                } else {
                                    2
                                }
                            } else {
                                0
                            },
                            chunk_char_offset: char_offset,
                        });
                    }
                }
                LaidRow::Header {
                    cwd,
                    duration,
                    status,
                    block_id,
                    ..
                } => {
                    // 阶段 3:分段拆开后在此拼回 " · " 全文,供身份匹配/debug。
                    let mut parts = vec![cwd.as_str()];
                    if !duration.is_empty() {
                        parts.push(duration.as_str());
                    }
                    if !status.is_empty() {
                        parts.push(status.as_str());
                    }
                    let text = parts.join(" · ");
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text,
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + header_height,
                        line: None,
                        chunk_char_offset: 0,
                        indent_cols: 0,
                    });
                }
                LaidRow::LiveHeader { text } => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Header,
                        text: text.clone(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                        indent_cols: 0,
                    });
                }
                LaidRow::Separator => {
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Separator,
                        text: String::new(),
                        block_id: None,
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                        indent_cols: 0,
                    });
                }
                LaidRow::LiveCommand { chunks, .. } => {
                    for (ci, cy, char_offset) in wrapped_row_positions(row_top_y, pitch, chunks) {
                        let chunk = &chunks[ci];
                        bv_rows.push(BlockViewRow {
                            kind: BlockViewRowKind::LiveCommand,
                            text: chunk.clone(),
                            block_id: None,
                            y_top: cy,
                            y_bottom: cy + pitch,
                            line: None,
                            // v1.10.13: first line renders after "> " (2 cols).
                            indent_cols: if ci == 0 { 2 } else { 0 },
                            chunk_char_offset: char_offset,
                        });
                    }
                }
                LaidRow::DiagnosePanel { text, block_id, .. } => {
                    // v1.8.2: include the panel's y-band in hit-testing so
                    // clicks on the panel don't fall through to whatever is
                    // below. Non-selectable.
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::DiagnosePanel,
                        text: text.clone(),
                        block_id: Some(*block_id),
                        y_top: row_top_y,
                        y_bottom: row_top_y + pitch,
                        line: None,
                        chunk_char_offset: 0,
                        indent_cols: 0,
                    });
                }
                LaidRow::Blank => {}
            }
        }

        (bv_rows, layout.clip_top, layout.clip_bottom)
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
            line: None,
            chunk_char_offset: 0,
            indent_cols: 0,
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

    #[test]
    fn wrapped_row_positions_are_bottom_to_top() {
        let chunks = vec!["ab".to_string(), "cd".to_string(), "e".to_string()];
        let pos = wrapped_row_positions(100.0, 20.0, &chunks);
        // 视觉从下到上:y 递增、ci 递减、offset 前缀和
        assert_eq!(pos, vec![(2, 140.0, 4), (1, 120.0, 2), (0, 100.0, 0)]);
    }
}
