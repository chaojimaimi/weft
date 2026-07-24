//! Block header copy/fold hit regions and vector icons.

use crate::overlay::HitRegion;
use crate::paint::primitives::{push_line, push_quad};
use crate::renderer::MetalRenderer;
use weft_core::blocks::{Block, BlockId};

#[derive(Clone, Copy, Debug, PartialEq)]
struct BlockHeaderActionGeometry {
    copy: [f32; 4],
    fold: [f32; 4],
    copy_center: [f32; 2],
    fold_center: [f32; 2],
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
    let g = block_header_action_geometry(right, y, pitch, cell_width, scale);
    (g.copy, g.fold)
}

fn block_header_action_geometry(
    right: f32,
    y: f32,
    pitch: f32,
    cell_width: f32,
    scale: f64,
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
    BlockHeaderActionGeometry {
        copy: [copy_x0, y, copy_x1, y + band_height],
        fold: [fold_x0, y, fold_x1, y + band_height],
        copy_center: [(copy_x0 + copy_x1) * 0.5, center_y],
        fold_center: [(fold_x0 + fold_x1) * 0.5, center_y],
    }
}

pub(super) fn block_header_band_height(pitch: f32, scale: f64) -> f32 {
    crate::ui_tokens::compact_control_row_span(pitch, scale) as f32 * pitch
}

pub(super) fn block_header_text_cols(left: f32, right: f32, cell_width: f32, scale: f64) -> usize {
    if !cell_width.is_finite() || cell_width <= 0.0 {
        return 0;
    }
    let geometry = block_header_action_geometry(right, 0.0, cell_width, cell_width, scale);
    let text_right = geometry.copy[0] - cell_width * 0.5;
    ((text_right - left).max(0.0) / cell_width).floor() as usize
}

pub(super) struct BlockHeaderActionPaint {
    pub(super) block_id: BlockId,
    pub(super) block_hovered: Option<BlockId>,
    pub(super) y: f32,
    pub(super) pitch: f32,
    pub(super) right: f32,
    pub(super) cell_width: f32,
    pub(super) cell_height: f32,
    pub(super) foreground: [f32; 4],
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
        y,
        pitch,
        right,
        cell_width: cw,
        cell_height: ch,
        foreground: fg,
    } = paint;
    // F3-1: Hover action buttons (copy + fold) on the header row's
    // right side. Hit regions are always registered; fold remains faintly
    // visible as a discoverability cue and copy appears with the hover surface.
    let is_hovered = block_hovered == Some(block_id);
    let geometry = block_header_action_geometry(right, y, pitch, cw, renderer.scale);
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
        let surface = [fg[0], fg[1], fg[2], 0.11];
        for rect in [geometry.copy, geometry.fold] {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_targets_are_large_and_leave_a_scrollbar_gutter() {
        let geometry = block_header_action_geometry(1000.0, 100.0, 40.0, 20.0, 2.0);
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
        let cols = block_header_text_cols(40.0, 1000.0, 20.0, 2.0);
        let geometry = block_header_action_geometry(1000.0, 0.0, 40.0, 20.0, 2.0);
        assert!(40.0 + cols as f32 * 20.0 <= geometry.copy[0]);
        assert!(cols < ((1000.0 - 40.0) / 20.0) as usize);
    }
}
