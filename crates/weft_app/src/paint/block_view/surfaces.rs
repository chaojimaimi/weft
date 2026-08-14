use std::collections::HashMap;

use crate::block_component::BlockTone;
use crate::paint::block_view::layout_pass::LaidRow;
use crate::paint::primitives::push_quad;
use crate::renderer::MetalRenderer;
use weft_core::blocks::BlockId;

#[derive(Clone, Copy)]
pub(super) struct BlockSurfacePaint {
    pub(super) left: f32,
    pub(super) right: f32,
    pub(super) content_bottom: f32,
    pub(super) scroll_px: f32,
    pub(super) pitch: f32,
    pub(super) header_height: f32,
    pub(super) clip_top: f32,
    pub(super) clip_bottom: f32,
    pub(super) scale: f32,
    pub(super) background_uv: [f32; 4],
}

#[derive(Clone, Copy)]
pub(super) struct BlockSurfaceState {
    pub(super) scroll_px: f32,
    pub(super) header_height: f32,
    pub(super) hovered: Option<BlockId>,
    pub(super) selected: Option<BlockId>,
}

#[derive(Clone, Copy)]
pub(super) struct BlockSurfaceColors {
    background: [f32; 4],
    foreground: [f32; 4],
    accent: [f32; 4],
    error: [f32; 4],
    warning: [f32; 4],
    opacity: f32,
    hovered: Option<BlockId>,
    selected: Option<BlockId>,
}

fn blend(base: [f32; 4], tint: [f32; 4], amount: f32) -> [f32; 4] {
    [
        base[0] * (1.0 - amount) + tint[0] * amount,
        base[1] * (1.0 - amount) + tint[1] * amount,
        base[2] * (1.0 - amount) + tint[2] * amount,
        1.0,
    ]
}

pub(super) fn block_surface_colors(
    renderer: &MetalRenderer,
    state: BlockSurfaceState,
) -> BlockSurfaceColors {
    let ui = crate::ui_tokens::UiColors::from_theme(&renderer.theme)
        .with_increase_contrast(renderer.increase_contrast);
    BlockSurfaceColors {
        background: crate::paint::primitives::color_to_normalized(renderer.theme.background),
        foreground: crate::paint::primitives::color_to_normalized(renderer.theme.foreground),
        accent: crate::paint::primitives::color_to_normalized(renderer.theme.accent),
        error: crate::paint::primitives::color_to_normalized(ui.error),
        warning: crate::paint::primitives::color_to_normalized(ui.warning),
        opacity: renderer.opacity,
        hovered: state.hovered,
        selected: state.selected,
    }
}

pub(super) fn block_surface_color(
    colors: BlockSurfaceColors,
    block_id: BlockId,
    tone: BlockTone,
) -> [f32; 4] {
    let mut color = blend(
        colors.background,
        colors.foreground,
        if block_id.0 % 2 == 0 { 0.024 } else { 0.014 },
    );
    if tone == BlockTone::Error {
        color = blend(color, colors.error, 0.12);
    } else if tone == BlockTone::Warning {
        color = blend(color, colors.warning, 0.07);
    }
    if colors.hovered == Some(block_id) {
        color = blend(color, colors.foreground, 0.045);
    }
    if colors.selected == Some(block_id) {
        color = blend(color, colors.accent, 0.12);
    }
    color[3] = colors.opacity.clamp(0.0, 1.0);
    color
}

pub(super) fn canvas_for(
    canvases: &HashMap<BlockId, [f32; 4]>,
    block_id: Option<BlockId>,
    fallback: [f32; 4],
) -> [f32; 4] {
    block_id
        .and_then(|id| canvases.get(&id).copied())
        .unwrap_or(fallback)
}

pub(super) fn push_block_surfaces(
    renderer: &MetalRenderer,
    vertices: &mut Vec<f32>,
    rows: &[f32],
    row_data: &[LaidRow<'_>],
    layout: &crate::layout::BlockViewLayout,
    state: BlockSurfaceState,
) -> HashMap<BlockId, [f32; 4]> {
    let colors = block_surface_colors(renderer, state);
    let (su, sv, suw, svh) = renderer.space_uv();
    let paint = BlockSurfacePaint {
        left: layout.frame_left,
        right: layout.frame_right,
        content_bottom: layout.clip_bottom,
        scroll_px: state.scroll_px,
        pitch: layout.pitch,
        header_height: state.header_height,
        clip_top: layout.clip_top,
        clip_bottom: layout.clip_bottom,
        scale: renderer.scale as f32,
        background_uv: [su, sv + svh, su + suw, sv],
    };
    let mut bounds: HashMap<BlockId, (f32, f32, BlockTone)> = HashMap::new();
    for (distance, row) in rows.iter().copied().zip(row_data) {
        let (id, height, tone) = match row {
            LaidRow::Output {
                block_id: Some(id),
                chunks,
                ..
            } => (
                *id,
                chunks.len().max(1) as f32 * paint.pitch,
                BlockTone::Success,
            ),
            LaidRow::Command {
                block_id, chunks, ..
            } => (
                *block_id,
                chunks.len().max(1) as f32 * paint.pitch,
                BlockTone::Success,
            ),
            LaidRow::Header { block_id, tone, .. } => (*block_id, paint.header_height, *tone),
            _ => continue,
        };
        let top = paint.content_bottom - distance + paint.scroll_px;
        let bottom = top + height;
        bounds
            .entry(id)
            .and_modify(|range| {
                range.0 = range.0.min(top);
                range.1 = range.1.max(bottom);
                if tone != BlockTone::Success {
                    range.2 = tone;
                }
            })
            .or_insert((top, bottom, tone));
    }

    let mut canvases = HashMap::with_capacity(bounds.len());
    for (block_id, (top, bottom, tone)) in bounds {
        let color = block_surface_color(colors, block_id, tone);
        canvases.insert(block_id, color);
        let y0 = top.max(paint.clip_top);
        let y1 = bottom.min(paint.clip_bottom);
        if y1 <= y0 {
            continue;
        }
        let failed = tone == BlockTone::Error;
        let interrupted = tone == BlockTone::Warning;
        push_quad(
            vertices,
            [paint.left, y0, paint.right, y1],
            paint.background_uv,
            [0.0; 4],
            color,
        );

        let rail_color = if failed {
            Some(colors.error)
        } else if interrupted {
            Some(colors.warning)
        } else {
            None
        };
        if let Some(mut rail) = rail_color {
            rail[3] = 0.95;
            let width = (3.0 * paint.scale).max(2.0);
            push_quad(
                vertices,
                [paint.left, y0, paint.left + width, y1],
                paint.background_uv,
                [0.0; 4],
                rail,
            );
        }
    }
    canvases
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::primitives::{
        color_to_normalized, composite_color_over, ensure_minimum_text_contrast,
        text_contrast_ratio,
    };
    use weft_core::config::Theme;

    fn warm_surface_colors(selected: Option<BlockId>) -> (Theme, BlockSurfaceColors) {
        let theme = Theme::weft_warm();
        let colors = BlockSurfaceColors {
            background: color_to_normalized(theme.background),
            foreground: color_to_normalized(theme.foreground),
            accent: color_to_normalized(theme.accent),
            error: color_to_normalized(theme.output.failure),
            warning: color_to_normalized(theme.palette[3]),
            opacity: 1.0,
            hovered: None,
            selected,
        };
        (theme, colors)
    }

    fn assert_surface_contrast(tone: BlockTone, selected: bool, selection: bool) {
        let id = BlockId(2);
        let (theme, colors) = warm_surface_colors(selected.then_some(id));
        let mut canvas = block_surface_color(colors, id, tone);
        if selection {
            let accent = color_to_normalized(theme.accent);
            let background = color_to_normalized(theme.background);
            let selection = [
                accent[0] * 0.35 + background[0] * 0.65,
                accent[1] * 0.35 + background[1] * 0.65,
                accent[2] * 0.35 + background[2] * 0.65,
                0.60,
            ];
            canvas = composite_color_over(selection, canvas);
        }
        let source = color_to_normalized(theme.output.failure);
        let adjusted = ensure_minimum_text_contrast(source, canvas, 7.0);
        assert!(text_contrast_ratio(adjusted, canvas) >= 6.99);
    }

    #[test]
    fn tint_blend_stays_opaque_and_between_inputs() {
        let mixed = blend([0.0, 0.2, 0.4, 1.0], [1.0, 0.6, 0.2, 1.0], 0.25);
        for (actual, expected) in mixed.into_iter().zip([0.25, 0.3, 0.35, 1.0]) {
            assert!((actual - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn block_surface_uses_window_opacity() {
        let id = BlockId(2);
        let (_, mut colors) = warm_surface_colors(None);
        colors.opacity = 0.55;
        let surface = block_surface_color(colors, id, BlockTone::Success);
        assert!((surface[3] - 0.55).abs() < 1e-6);
    }

    #[test]
    fn normal_surface_text_reaches_target_contrast() {
        assert_surface_contrast(BlockTone::Success, false, false);
    }

    #[test]
    fn failed_surface_text_reaches_target_contrast() {
        assert_surface_contrast(BlockTone::Error, false, false);
    }

    #[test]
    fn selected_surface_text_reaches_target_contrast() {
        assert_surface_contrast(BlockTone::Success, true, false);
    }

    #[test]
    fn selection_over_failed_selected_surface_reaches_target_contrast() {
        assert_surface_contrast(BlockTone::Error, true, true);
    }
}
