//! Block header copy/fold hit regions and vector icons.

use crate::overlay::HitRegion;
use crate::paint::primitives::push_line;
use crate::renderer::MetalRenderer;
use weft_core::blocks::{Block, BlockId};

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
    // F3-1: Hover action buttons (copy + fold) on the header
    // row's right side. Hit regions are always registered (so
    // clicks work even when not visible), but the icons are
    // only drawn when this block is hovered.
    let is_hovered = block_hovered == Some(block_id);
    let btn_w = cw * 2.0;
    let btn_gap = cw * 0.5;
    let btn_cy = y + pitch * 0.5;
    let line_w = crate::ui_tokens::UiMetrics::for_scale(renderer.scale).stroke;
    let btn_color = if is_hovered { fg } else { [0.0; 4] };

    // Fold button (rightmost).
    let fold_cx = right - btn_w * 0.5;
    let fold_x0 = fold_cx - btn_w * 0.5;
    hit_regions.push(crate::overlay::HitRegion {
        x0: fold_x0,
        y0: y,
        x1: fold_x0 + btn_w,
        y1: y + pitch,
        target: crate::overlay::HitTarget::BlockActionFold(block_id),
    });
    if is_hovered {
        // Draw a chevron: ▾ (expanded) or ▸ (collapsed).
        let chev_r = ch * 0.14;
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
    let copy_cx = fold_cx - btn_w - btn_gap;
    let copy_x0 = copy_cx - btn_w * 0.5;
    hit_regions.push(crate::overlay::HitRegion {
        x0: copy_x0,
        y0: y,
        x1: copy_x0 + btn_w,
        y1: y + pitch,
        target: crate::overlay::HitTarget::BlockActionCopy(block_id),
    });
    if is_hovered {
        // Draw a simple copy icon: two overlapping squares.
        let sq_r = ch * 0.14;
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
