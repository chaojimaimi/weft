//! Read-only BlockView row geometry used by pointer hit-testing.

use crate::paint::block_view_model::BlockViewPaintModel;
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;
use weft_core::selection::{BlockViewRow, BlockViewRowKind};

use super::actions::block_header_band_height;
use super::layout_pass::{compute_block_layout_pass, LaidRow, LayoutPassInput};

/// Hit-test tuple: laid rows + clip band + overlay header bands (v1.20.0 D8).
#[allow(clippy::type_complexity)]
pub(crate) type BlockViewHitRows = (
    Vec<BlockViewRow>,
    f32,
    f32,
    Vec<crate::block_component::BlockChromeBand>,
);

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
    /// M6-b B-5: idle-pump gate for the paint path's convergence pump. The
    /// pump only runs when no live capture owns the view — a screen-owned
    /// in-flight document is NOT capturing (its history is the screen), and
    /// the 200ms deferred-exit settle window stays inert — and the previous
    /// frame's build segment was cheap. `build_us` comes from frame_trace's
    /// always-on segment timer (recorded even with the probe disabled), read
    /// before this frame's own build_end overwrites it.
    pub(super) fn block_view_pump_idle(
        &self,
        live: Option<&weft_core::blocks::InFlightBlock>,
    ) -> bool {
        live.map_or(true, |l| l.screen_origin) && self.frame_trace.borrow().build_us() < 8_000
    }

    /// Compute block-view rows for hit-testing WITHOUT building vertices.
    /// This is the M1 on-demand alternative to caching `block_view_rows`
    /// during `draw()`. The geometry_controller calls this when a mouse
    /// event arrives, so hit-testing uses current-frame data instead of
    /// the renderer's previous-frame cache.
    ///
    /// Shares the layout pass with `build_block_view_vertices` via
    /// `compute_block_layout_pass`, so y-bands are identical between paint
    /// and hit-testing. Only the bv_rows extraction is specific to this path.
    /// Returns `BlockViewHitRows` = `(rows, clip_top, clip_bottom,
    /// chrome_bands)`: the laid-out rows, the visible content band's clip
    /// boundaries from the layout pass, and the overlay header bands for
    /// hover (v1.20.0 D8; empty when `complete_block_overlay` is off).
    /// Callers
    /// that need the visible content edge (e.g. autoscroll) must use the clip
    /// values, not rows first/last — bv_rows carries overscan, so its topmost
    /// row can sit above `clip_top - overscan`.
    pub(crate) fn compute_block_view_rows(
        &self,
        model: BlockViewPaintModel<'_>,
    ) -> BlockViewHitRows {
        let BlockViewPaintModel {
            blocks,
            live_head_lines: _,
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
            cache_namespace,
            block_diagnose_state,
            ai_configured: _,
            tui_cursor: _,
            tui_preedit: _,
            cursor_blink_on: _,
            is_alt: _,
            now,
            region,
        } = model;

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return (Vec::new(), 0.0, 0.0, Vec::new());
        }

        let ctx = match self.layout_ctx {
            Some(c) => c,
            None => return (Vec::new(), 0.0, 0.0, Vec::new()),
        };
        let cwd_header_active = cwd.is_some() && live.is_none();
        let layout = crate::layout::layout_block_view(&ctx, region_bottom_y, cwd_header_active);
        let pitch = layout.pitch;
        let header_height = block_header_band_height(pitch, self.scale);
        let content_bottom_y = layout.clip_bottom;
        let cols = layout.cols;

        // Shared layout pass (single source of truth for row geometry).
        // resolve_styles=false: hit-testing doesn't need StyledLine lookups.
        // M5-b: laid rows borrow from the cache (zero-copy row text) — the
        // borrow lives until bv_rows (owning Strings) is built.
        {
            let mut cache = self.block_layout_cache.borrow_mut();
            // v1.20.0 (PLAN_v1200 §4 D2e): same flag-fold as the paint path —
            // hit-test and paint agree on the sync domain.
            cache.set_complete_block_overlay(self.complete_block_overlay);
            // M6-b B-1: same band derivation as the paint path (block_view.rs)
            // so hit-testing geometry matches what sync classified.
            // F7-2 (PLAN_v1184 §2-F7-2): the freeze band twin of the paint
            // site — a mouse hit-test during the animation must not rebuild
            // in-band blocks at the in-flight cols either (same defer-all
            // shape; uncached blocks still Both-build for the `get()`
            // precondition).
            let band = if crate::paint::zoom_render::rewrap_freeze_active() {
                crate::paint::grid_cache::BandSync {
                    low_rows: 0,
                    high_rows: 0,
                }
            } else {
                crate::paint::grid_cache::BandSync::for_viewport(
                    block_scroll as usize,
                    viewport_rows,
                )
            };
            cache.sync_blocks(blocks, cols, band);
        }
        let cache = self.block_layout_cache.borrow();
        // v1.19.0 (PLAN_v1190 §4 P1-1): hit-test geometry mirrors the paint
        // path — same region sync + snapshot borrow before the layout pass.
        if let Some(region) = &region {
            self.region_live_cache.sync(cache_namespace, region, cols);
        }
        let region_state = self.region_live_cache.state(cache_namespace);
        let layout_out = compute_block_layout_pass(
            LayoutPassInput {
                blocks,
                live,
                pane_session_id: cache_namespace,
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
                // v1.20.0 (PLAN_v1200 §4 D10): hit-test mirrors the paint
                // frame's flag (renderer state) so both passes agree on the
                // stream shape.
                complete_block_overlay: self.complete_block_overlay,
                block_diagnose_state,
                now,
                region,
                region_state: region_state.as_deref(),
            },
            &cache,
            &mut self.live_layout_cache.borrow_mut(),
        );

        // Extract bv_rows from the layout output. Hit-testing keeps the
        // historical full-row set (cull=false) so pointer mapping is
        // unchanged; the paint path culls, see `build_bv_rows`.
        let scroll_px = block_scroll * pitch;
        // v1.20.0 (PLAN_v1200 D8): screen-space header bands for hover — the
        // overlay track paints chrome where no laid row exists, so the
        // laid-row scan in `block_at` cannot see them. Same formula as the
        // paint emission (`build_block_view_vertices`) so they can't drift.
        let chrome_bands: Vec<crate::block_component::BlockChromeBand> = layout_out
            .overlay_rows
            .iter()
            .filter_map(|chrome| {
                let block_id = chrome.block_id?;
                chrome.header.as_ref()?;
                let y_top = content_bottom_y - chrome.dist_px + scroll_px;
                let y_bottom = y_top + chrome.height_px;
                // Strict on purpose: zero-height overlap has no hoverable
                // interior, so hit-testing is stricter than the paint clip.
                (y_bottom > layout.clip_top && y_top < layout.clip_bottom).then_some(
                    crate::block_component::BlockChromeBand {
                        y_top,
                        y_bottom,
                        block_id,
                    },
                )
            })
            .collect();
        let bv_rows = build_bv_rows(
            &layout_out.rows,
            &layout_out.row_data,
            BvRowsGeometry {
                pitch,
                header_height,
                content_bottom_y,
                scroll_px,
                clip_top: layout.clip_top,
                clip_bottom: layout.clip_bottom,
                cull: false,
            },
        );

        (bv_rows, layout.clip_top, layout.clip_bottom, chrome_bands)
    }
}

/// Build selection/hit-test rows from a shared layout output (bottom-to-top;
/// wrapped chunks expand into per-chunk rows). Single source for both the
/// paint path (`build_block_view_vertices`) and hit-testing
/// (`compute_block_view_rows`) so their y-bands can't drift.
/// v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): `cull=true` materializes only rows
/// intersecting the clip window; Command rows are always kept (sticky-block
/// detection needs them, see `sticky_block_id`).
/// v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): the selection is content-anchored,
/// so the cull no longer retains selection rows — `bv_rows` is the pure
/// visible window again (the selection never reads these rows).
/// Geometry inputs for [`build_bv_rows`] (paint + hit-testing parity).
pub(super) struct BvRowsGeometry {
    pub(super) pitch: f32,
    pub(super) header_height: f32,
    pub(super) content_bottom_y: f32,
    pub(super) scroll_px: f32,
    pub(super) clip_top: f32,
    pub(super) clip_bottom: f32,
    /// Materialize only clip-visible rows (+ Command retention).
    pub(super) cull: bool,
}

pub(super) fn build_bv_rows(
    rows: &[f32],
    row_data: &[LaidRow<'_>],
    geometry: BvRowsGeometry,
) -> Vec<BlockViewRow> {
    let BvRowsGeometry {
        pitch,
        header_height,
        content_bottom_y,
        scroll_px,
        clip_top,
        clip_bottom,
        cull,
    } = geometry;
    let mut bv_rows = Vec::new();
    for (i, &dist) in rows.iter().enumerate() {
        // M5-b: Output entries are per-visual-row and carry their chunk
        // offset within the line — the row's own band sits `chunk_idx`
        // pitches below the pushed (line-top) dist. Byte-equal to the
        // legacy wrapped expansion (`y + ci * pitch`).
        let top = content_bottom_y - dist + scroll_px;
        let (row_top_y, row_height) = match &row_data[i] {
            LaidRow::Output { chunk_idx, .. } => (top + *chunk_idx as f32 * pitch, pitch),
            LaidRow::Header { .. } => (top, header_height),
            _ => (top, pitch),
        };
        let visible = row_top_y + row_height >= clip_top && row_top_y <= clip_bottom;
        // v1.10.26: no selection retention — the keep set is the pure
        // visible window plus Command rows (sticky detection).
        let keep = !cull || visible || matches!(&row_data[i], LaidRow::Command { .. });
        let y = row_top_y;
        match &row_data[i] {
            LaidRow::Output {
                text,
                line_text: _,
                block_id,
                line,
                chunk_idx: _,
                char_offset,
                ..
            } => {
                // v1.6.1: carry the line index so click-time hyperlink
                // resolution can look up `StyledLine::link_at`. Skip
                // resume hints (line == usize::MAX) — they have no styled
                // output and aren't clickable.
                let line_idx = (*line != usize::MAX).then_some(*line);
                if !keep {
                    continue;
                }
                bv_rows.push(BlockViewRow {
                    kind: BlockViewRowKind::Output,
                    text: text.to_string(),
                    block_id: *block_id,
                    y_top: y,
                    y_bottom: y + pitch,
                    line: line_idx,
                    chunk_char_offset: *char_offset,
                    indent_cols: 0,
                });
            }
            LaidRow::Command {
                chunks,
                foldable,
                block_id,
                ..
            } => {
                for (ci, cy, char_offset) in wrapped_row_positions(y, pitch, chunks) {
                    let chunk = &chunks[ci];
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::Command,
                        text: chunk.clone(),
                        block_id: Some(*block_id),
                        y_top: cy,
                        y_bottom: cy + pitch,
                        line: None,
                        // v1.10.13: first line renders after chevron + "> "
                        // (3 cols if foldable else 2); continuations flush-left.
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
                if !keep {
                    continue;
                }
                bv_rows.push(BlockViewRow {
                    kind: BlockViewRowKind::Header,
                    text,
                    block_id: Some(*block_id),
                    y_top: y,
                    y_bottom: y + header_height,
                    line: None,
                    chunk_char_offset: 0,
                    indent_cols: 0,
                });
            }
            LaidRow::LiveHeader { text } => {
                if !keep {
                    continue;
                }
                bv_rows.push(BlockViewRow {
                    kind: BlockViewRowKind::Header,
                    text: text.clone(),
                    block_id: None,
                    y_top: y,
                    y_bottom: y + pitch,
                    line: None,
                    chunk_char_offset: 0,
                    indent_cols: 0,
                });
            }
            LaidRow::Separator => {
                if !keep {
                    continue;
                }
                bv_rows.push(BlockViewRow {
                    kind: BlockViewRowKind::Separator,
                    text: String::new(),
                    block_id: None,
                    y_top: y,
                    y_bottom: y + pitch,
                    line: None,
                    chunk_char_offset: 0,
                    indent_cols: 0,
                });
            }
            LaidRow::LiveCommand { chunks, .. } => {
                if !chunks.iter().any(|_chunk| keep) {
                    continue;
                }
                for (ci, cy, char_offset) in wrapped_row_positions(y, pitch, chunks) {
                    let chunk = &chunks[ci];
                    bv_rows.push(BlockViewRow {
                        kind: BlockViewRowKind::LiveCommand,
                        text: chunk.clone(),
                        block_id: None,
                        y_top: cy,
                        y_bottom: cy + pitch,
                        line: None,
                        indent_cols: if ci == 0 { 2 } else { 0 },
                        chunk_char_offset: char_offset,
                    });
                }
            }
            LaidRow::DiagnosePanel { text, block_id, .. } => {
                // v1.8.2: include the panel's y-band in hit-testing so
                // clicks on the panel don't fall through to whatever is
                // below. Non-selectable.
                if !keep {
                    continue;
                }
                bv_rows.push(BlockViewRow {
                    kind: BlockViewRowKind::DiagnosePanel,
                    text: text.clone(),
                    block_id: Some(*block_id),
                    y_top: y,
                    y_bottom: y + pitch,
                    line: None,
                    chunk_char_offset: 0,
                    indent_cols: 0,
                });
            }
            LaidRow::Blank => {}
        }
    }
    bv_rows
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
