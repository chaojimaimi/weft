//! Block header copy/fold hit regions and vector icons.

use crate::overlay::HitRegion;
use crate::paint::primitives::{push_line, push_quad};
use crate::renderer::MetalRenderer;
use weft_core::blocks::{Block, BlockId};

#[derive(Clone, Copy, Debug, PartialEq)]
struct BlockHeaderActionGeometry {
    copy: [f32; 4],
    fold: [f32; 4],
    /// v1.8.2: diagnose button slot. Only populated (non-zero width) when
    /// `ai_configured` is true. Positioned to the left of `copy`.
    diagnose: [f32; 4],
    copy_center: [f32; 2],
    fold_center: [f32; 2],
    diagnose_center: [f32; 2],
}

/// Batch 5 Step 3: expose copy/fold button bounds for accessibility.
/// Returns `(copy_rect, fold_rect)` so screen readers can register
/// independent Button semantics on the sticky header actions.
pub(crate) fn block_header_action_rects(
    right: f32,
    y: f32,
    pitch: f32,
    cell_width: f32,
    scale: f64,
) -> ([f32; 4], [f32; 4]) {
    let g = block_header_action_geometry(right, y, pitch, cell_width, scale, false);
    (g.copy, g.fold)
}

fn block_header_action_geometry(
    right: f32,
    y: f32,
    pitch: f32,
    cell_width: f32,
    scale: f64,
    ai_configured: bool,
) -> BlockHeaderActionGeometry {
    let metrics = crate::ui_tokens::UiMetrics::for_scale(scale);
    let band_height = block_header_band_height(pitch, scale);
    // Use at least 28 logical points on both axes. The layout reserves a whole
    // multi-row header band for this target, so its half-open bounds never
    // steal a click from an adjacent output row. Keep an additional right
    // gutter so the fold action never competes with scrollbar dragging.
    let button_width = metrics.control_compact.max(cell_width * 2.6);
    let gap = (cell_width * 0.45).max(metrics.stroke * 2.0);
    let scrollbar_gutter = (metrics.control_compact * 0.72).max(cell_width * 1.8);
    let fold_x1 = right - scrollbar_gutter;
    let fold_x0 = fold_x1 - button_width;
    let copy_x1 = fold_x0 - gap;
    let copy_x0 = copy_x1 - button_width;
    let center_y = y + band_height * 0.5;
    // v1.8.2: When AI is configured, reserve a diagnose slot to the left of
    // copy. The slot always exists in geometry (so text budget is stable)
    // but the button is only drawn/hit-tested for failed blocks.
    let (diagnose_x0, diagnose_x1) = if ai_configured {
        let dx1 = copy_x0 - gap;
        let dx0 = dx1 - button_width;
        (dx0, dx1)
    } else {
        // No slot: collapse to zero-width at copy_x0 so it doesn't affect layout.
        (copy_x0, copy_x0)
    };
    BlockHeaderActionGeometry {
        copy: [copy_x0, y, copy_x1, y + band_height],
        fold: [fold_x0, y, fold_x1, y + band_height],
        diagnose: [diagnose_x0, y, diagnose_x1, y + band_height],
        copy_center: [(copy_x0 + copy_x1) * 0.5, center_y],
        fold_center: [(fold_x0 + fold_x1) * 0.5, center_y],
        diagnose_center: [(diagnose_x0 + diagnose_x1) * 0.5, center_y],
    }
}

pub(super) fn block_header_band_height(pitch: f32, scale: f64) -> f32 {
    crate::ui_tokens::compact_control_row_span(pitch, scale) as f32 * pitch
}

pub(super) fn block_header_text_cols(
    left: f32,
    right: f32,
    cell_width: f32,
    scale: f64,
    ai_configured: bool,
) -> usize {
    if !cell_width.is_finite() || cell_width <= 0.0 {
        return 0;
    }
    let geometry = block_header_action_geometry(
        right,
        0.0,
        cell_width,
        cell_width,
        scale,
        ai_configured,
    );
    // v1.8.2: when ai_configured, text must stop before the diagnose slot.
    let text_right = if ai_configured {
        geometry.diagnose[0] - cell_width * 0.5
    } else {
        geometry.copy[0] - cell_width * 0.5
    };
    ((text_right - left).max(0.0) / cell_width).floor() as usize
}

pub(super) struct BlockHeaderActionPaint {
    pub(super) block_id: BlockId,
    pub(super) block_hovered: Option<BlockId>,
    pub(super) action_hovered: Option<crate::block_component::BlockHeaderAction>,
    pub(super) y: f32,
    pub(super) pitch: f32,
    pub(super) right: f32,
    pub(super) cell_width: f32,
    pub(super) cell_height: f32,
    pub(super) foreground: [f32; 4],
    /// v1.8.2: Whether the local Ollama backend is configured. When true,
    /// the diagnose button slot is reserved in geometry and the button is
    /// drawn + hit-tested for blocks with `exit_code != 0`.
    pub(super) ai_configured: bool,
}

pub(super) fn push_block_header_actions(
    renderer: &MetalRenderer,
    verts: &mut Vec<f32>,
    hit_regions: &mut Vec<HitRegion>,
    blocks: &[Block],
    paint: BlockHeaderActionPaint,
) {
    let BlockHeaderActionPaint {
        block_id,
        block_hovered,
        action_hovered,
        y,
        pitch,
        right,
        cell_width: cw,
        cell_height: ch,
        foreground: fg,
        ai_configured,
    } = paint;
    // F3-1: Hover action buttons (copy + fold) on the header row's
    // right side. Hit regions are always registered; fold remains faintly
    // visible as a discoverability cue and copy appears with the hover surface.
    let is_hovered = block_hovered == Some(block_id);
    let copy_hovered =
        action_hovered == Some(crate::block_component::BlockHeaderAction::Copy(block_id));
    let fold_hovered = action_hovered
        == Some(crate::block_component::BlockHeaderAction::ToggleFold(
            block_id,
        ));
    let diagnose_hovered = action_hovered
        == Some(crate::block_component::BlockHeaderAction::Diagnose(block_id));
    let geometry = block_header_action_geometry(
        right,
        y,
        pitch,
        cw,
        renderer.scale,
        ai_configured,
    );
    let metrics = crate::ui_tokens::UiMetrics::for_scale(renderer.scale);
    let line_w = metrics.stroke * 1.5;
    let btn_color = if is_hovered {
        fg
    } else {
        [fg[0], fg[1], fg[2], fg[3] * 0.38]
    };
    let (su, sv, suw, svh) = renderer.space_uv();
    let bg_uv = [su, sv + svh, su + suw, sv];

    if is_hovered {
        let inset_x = metrics.stroke * 2.0;
        let inset_y = (pitch * 0.10).max(metrics.stroke);
        // v1.8.2: include the diagnose surface in the hover loop only when
        // the slot is active (ai_configured) and the block has failed.
        let block_failed = blocks
            .iter()
            .find(|b| b.id == block_id)
            .and_then(|b| b.exit_code)
            .map(|c| c != 0)
            .unwrap_or(false);
        let diagnose_active = ai_configured && block_failed;
        let mut hover_targets: Vec<([f32; 4], bool)> =
            vec![(geometry.copy, copy_hovered), (geometry.fold, fold_hovered)];
        if diagnose_active {
            hover_targets.push((geometry.diagnose, diagnose_hovered));
        }
        for (rect, exact_hover) in hover_targets {
            let surface = [fg[0], fg[1], fg[2], if exact_hover { 0.22 } else { 0.055 }];
            push_quad(
                verts,
                [
                    rect[0] + inset_x,
                    rect[1] + inset_y,
                    rect[2] - inset_x,
                    rect[3] - inset_y,
                ],
                bg_uv,
                [0.0; 4],
                surface,
            );
            if exact_hover {
                let border = [fg[0], fg[1], fg[2], 0.34];
                let [x0, y0, x1, y1] = [
                    rect[0] + inset_x,
                    rect[1] + inset_y,
                    rect[2] - inset_x,
                    rect[3] - inset_y,
                ];
                let stroke = metrics.stroke.max(1.0);
                for edge in [
                    [x0, y0, x1, y0 + stroke],
                    [x0, y1 - stroke, x1, y1],
                    [x0, y0, x0 + stroke, y1],
                    [x1 - stroke, y0, x1, y1],
                ] {
                    push_quad(verts, edge, bg_uv, [0.0; 4], border);
                }
            }
        }
    }

    // Fold button (rightmost).
    hit_regions.push(crate::overlay::HitRegion {
        x0: geometry.fold[0],
        y0: geometry.fold[1],
        x1: geometry.fold[2],
        y1: geometry.fold[3],
        target: crate::overlay::HitTarget::BlockActionFold(block_id),
    });
    {
        // Draw a chevron: ▾ (expanded) or ▸ (collapsed).
        let fold_cx = geometry.fold_center[0];
        let btn_cy = geometry.fold_center[1];
        let chev_r = ch * 0.21;
        // Look up collapsed state from the block list.
        let collapsed = blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.collapsed)
            .unwrap_or(false);
        if collapsed {
            // ▸ (right-pointing triangle).
            push_line(
                verts,
                fold_cx - chev_r * 0.5,
                btn_cy - chev_r,
                fold_cx + chev_r * 0.5,
                btn_cy,
                line_w,
                btn_color,
            );
            push_line(
                verts,
                fold_cx + chev_r * 0.5,
                btn_cy,
                fold_cx - chev_r * 0.5,
                btn_cy + chev_r,
                line_w,
                btn_color,
            );
        } else {
            // ▾ (down-pointing chevron).
            push_line(
                verts,
                fold_cx - chev_r,
                btn_cy - chev_r * 0.5,
                fold_cx,
                btn_cy + chev_r * 0.5,
                line_w,
                btn_color,
            );
            push_line(
                verts,
                fold_cx,
                btn_cy + chev_r * 0.5,
                fold_cx + chev_r,
                btn_cy - chev_r * 0.5,
                line_w,
                btn_color,
            );
        }
    }

    // Copy button (to the left of fold).
    hit_regions.push(crate::overlay::HitRegion {
        x0: geometry.copy[0],
        y0: geometry.copy[1],
        x1: geometry.copy[2],
        y1: geometry.copy[3],
        target: crate::overlay::HitTarget::BlockActionCopy(block_id),
    });
    if is_hovered {
        // Draw a simple copy icon: two overlapping squares.
        let copy_cx = geometry.copy_center[0];
        let btn_cy = geometry.copy_center[1];
        let sq_r = ch * 0.20;
        // Back square (top-right).
        push_line(
            verts,
            copy_cx - sq_r * 0.3,
            btn_cy - sq_r,
            copy_cx + sq_r * 0.7,
            btn_cy - sq_r,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx + sq_r * 0.7,
            btn_cy - sq_r,
            copy_cx + sq_r * 0.7,
            btn_cy + sq_r * 0.4,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx + sq_r * 0.7,
            btn_cy + sq_r * 0.4,
            copy_cx - sq_r * 0.3,
            btn_cy + sq_r * 0.4,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx - sq_r * 0.3,
            btn_cy + sq_r * 0.4,
            copy_cx - sq_r * 0.3,
            btn_cy - sq_r,
            line_w,
            btn_color,
        );
        // Front square (bottom-left).
        push_line(
            verts,
            copy_cx - sq_r,
            btn_cy - sq_r * 0.4,
            copy_cx,
            btn_cy - sq_r * 0.4,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx,
            btn_cy - sq_r * 0.4,
            copy_cx,
            btn_cy + sq_r,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx,
            btn_cy + sq_r,
            copy_cx - sq_r,
            btn_cy + sq_r,
            line_w,
            btn_color,
        );
        push_line(
            verts,
            copy_cx - sq_r,
            btn_cy + sq_r,
            copy_cx - sq_r,
            btn_cy - sq_r * 0.4,
            line_w,
            btn_color,
        );
    }

    // v1.8.2: Diagnose button (leftmost, only when ai_configured && failed).
    let block_failed = blocks
        .iter()
        .find(|b| b.id == block_id)
        .and_then(|b| b.exit_code)
        .map(|c| c != 0)
        .unwrap_or(false);
    if ai_configured && block_failed {
        hit_regions.push(crate::overlay::HitRegion {
            x0: geometry.diagnose[0],
            y0: geometry.diagnose[1],
            x1: geometry.diagnose[2],
            y1: geometry.diagnose[3],
            target: crate::overlay::HitTarget::BlockActionDiagnose(block_id),
        });
        // Draw a simple "!" exclamation mark icon to signal "diagnose failure".
        let diag_cx = geometry.diagnose_center[0];
        let btn_cy = geometry.diagnose_center[1];
        let r = ch * 0.22;
        let stem_w = line_w.max(1.5);
        // Vertical stem (top 70% of the icon).
        push_line(
            verts,
            diag_cx,
            btn_cy - r,
            diag_cx,
            btn_cy + r * 0.3,
            stem_w,
            btn_color,
        );
        // Dot (bottom 30%).
        let dot_s = stem_w * 1.4;
        push_quad(
            verts,
            [diag_cx - dot_s * 0.5, btn_cy + r * 0.55, diag_cx + dot_s * 0.5, btn_cy + r * 0.55 + dot_s],
            bg_uv,
            [0.0; 4],
            btn_color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_targets_are_large_and_leave_a_scrollbar_gutter() {
        let geometry = block_header_action_geometry(1000.0, 100.0, 40.0, 20.0, 2.0, false);
        assert!(geometry.copy[2] - geometry.copy[0] >= 56.0);
        assert!(geometry.fold[2] - geometry.fold[0] >= 56.0);
        assert!(geometry.copy[3] - geometry.copy[1] >= 56.0);
        assert!(geometry.fold[3] - geometry.fold[1] >= 56.0);
        assert!(geometry.copy[2] < geometry.fold[0]);
        assert!(1000.0 - geometry.fold[2] >= 36.0);
        assert_eq!(geometry.copy[1], 100.0);
        assert_eq!(geometry.fold[3], 180.0);
    }

    #[test]
    fn header_text_budget_stops_before_action_surfaces() {
        let cols = block_header_text_cols(40.0, 1000.0, 20.0, 2.0, false);
        let geometry = block_header_action_geometry(1000.0, 0.0, 40.0, 20.0, 2.0, false);
        assert!(40.0 + cols as f32 * 20.0 <= geometry.copy[0]);
        assert!(cols < ((1000.0 - 40.0) / 20.0) as usize);
    }

    #[test]
    fn diagnose_slot_is_reserved_when_ai_configured() {
        let g_on = block_header_action_geometry(1000.0, 100.0, 40.0, 20.0, 2.0, true);
        let g_off = block_header_action_geometry(1000.0, 100.0, 40.0, 20.0, 2.0, false);
        // When ai_configured, diagnose has a real slot to the left of copy.
        assert!(g_on.diagnose[2] - g_on.diagnose[0] >= 56.0);
        assert!(g_on.diagnose[2] <= g_on.copy[0]);
        // When not configured, diagnose collapses to zero width.
        assert_eq!(g_off.diagnose[2] - g_off.diagnose[0], 0.0);
        // Text budget shrinks when ai_configured (room for diagnose button).
        let cols_on = block_header_text_cols(40.0, 1000.0, 20.0, 2.0, true);
        let cols_off = block_header_text_cols(40.0, 1000.0, 20.0, 2.0, false);
        assert!(cols_on < cols_off);
    }
}
