//! Block-view scrollbar layout and interaction mapping.

use crate::layout::{LayoutCtx, Rect, Spacing};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarLayout {
    pub(crate) track: Rect,
    pub(crate) thumb: Rect,
    pub(crate) hit: Rect,
    pub(crate) travel: f32,
    pub(crate) max_scroll: usize,
    idle_width: f32,
    hover_width: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarDragState {
    pub(crate) layout: ScrollbarLayout,
    pub(crate) grab_offset: f32,
}

pub(crate) fn scrollbar_layout(
    ctx: &LayoutCtx,
    total: usize,
    visible: usize,
    max_scroll: usize,
    scroll: usize,
) -> Option<ScrollbarLayout> {
    if visible >= total || max_scroll == 0 || ctx.height() <= 0.0 {
        return None;
    }

    let track = [ctx.left(), ctx.top(), ctx.right(), ctx.bottom()];
    let idle_width = (ctx.cell_w * 0.5).max(5.0);
    let hover_width = (ctx.cell_w * 0.9).max(8.0);
    let bar_x = ctx.right() - idle_width;
    let min_thumb = Spacing::row_md(ctx) * 3.0;
    let ratio = visible as f32 / total as f32;
    let thumb_h = (ctx.height() * ratio).max(min_thumb).min(ctx.height());
    let travel = (ctx.height() - thumb_h).max(0.0);
    let scroll_ratio = scroll.min(max_scroll) as f32 / max_scroll as f32;
    let thumb_y = ctx.top() + travel * (1.0 - scroll_ratio);
    let thumb = [bar_x, thumb_y, ctx.right(), thumb_y + thumb_h];
    // Keep the indicator visually subtle while making it comfortably grabbable.
    let hit_w = (ctx.cell_w * 1.5).max(12.0);
    let hit = [ctx.right() - hit_w, ctx.top(), ctx.right(), ctx.bottom()];

    Some(ScrollbarLayout {
        track,
        thumb,
        hit,
        travel,
        max_scroll,
        idle_width,
        hover_width,
    })
}

/// Visible thumb bounds. Hover/drag expands left into content while keeping
/// the right edge and vertical scroll position stable.
pub(crate) fn visual_thumb(layout: &ScrollbarLayout, emphasized: bool) -> Rect {
    let width = if emphasized {
        layout.hover_width
    } else {
        layout.idle_width
    };
    [
        layout.thumb[2] - width,
        layout.thumb[1],
        layout.thumb[2],
        layout.thumb[3],
    ]
}

pub(crate) fn thumb_grab_offset(
    layout: &ScrollbarLayout,
    x: f32,
    y: f32,
    emphasized: bool,
) -> Option<f32> {
    contains(visual_thumb(layout, emphasized), x, y).then_some(y - layout.thumb[1])
}

pub(crate) fn contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect[0] && x <= rect[2] && y >= rect[1] && y <= rect[3]
}

/// Convert a pointer position into Weft's bottom-origin block scroll offset.
pub(crate) fn scroll_offset_for_pointer(
    layout: &ScrollbarLayout,
    pointer_y: f32,
    grab_offset: f32,
) -> usize {
    if layout.travel <= f32::EPSILON {
        return 0;
    }
    let thumb_top =
        (pointer_y - grab_offset).clamp(layout.track[1], layout.track[1] + layout.travel);
    let from_top = (thumb_top - layout.track[1]) / layout.travel;
    ((1.0 - from_top) * layout.max_scroll as f32)
        .round()
        .clamp(0.0, layout.max_scroll as f32) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::LayoutCtx;

    fn ctx() -> LayoutCtx {
        LayoutCtx::new((1000.0, 700.0), 10.0, 20.0, 8.0, 8.0)
    }

    #[test]
    fn layout_places_newest_at_bottom_and_oldest_at_top() {
        let newest = scrollbar_layout(&ctx(), 100, 25, 75, 0).unwrap();
        let oldest = scrollbar_layout(&ctx(), 100, 25, 75, 75).unwrap();
        assert_eq!(newest.thumb[3], newest.track[3]);
        assert_eq!(oldest.thumb[1], oldest.track[1]);
    }

    #[test]
    fn layout_is_absent_when_content_fits() {
        assert!(scrollbar_layout(&ctx(), 25, 25, 0, 0).is_none());
    }

    #[test]
    fn hit_area_is_wider_than_visible_thumb() {
        let layout = scrollbar_layout(&ctx(), 100, 25, 75, 0).unwrap();
        assert!(layout.hit[2] - layout.hit[0] > layout.thumb[2] - layout.thumb[0]);
    }

    #[test]
    fn hovered_thumb_expands_toward_content_without_moving_right_edge() {
        let layout = scrollbar_layout(&ctx(), 100, 25, 75, 0).unwrap();
        let idle = visual_thumb(&layout, false);
        let hovered = visual_thumb(&layout, true);
        assert!(idle[2] - idle[0] >= 5.0);
        assert!(hovered[2] - hovered[0] > idle[2] - idle[0]);
        assert_eq!(hovered[2], idle[2]);
    }

    #[test]
    fn hovered_expansion_is_part_of_thumb_grab_region() {
        let layout = scrollbar_layout(&ctx(), 100, 25, 75, 0).unwrap();
        let hovered = visual_thumb(&layout, true);
        let x = hovered[0] + 0.5;
        let y = (hovered[1] + hovered[3]) / 2.0;
        assert!(x < layout.thumb[0]);
        assert_eq!(
            thumb_grab_offset(&layout, x, y, true),
            Some(y - layout.thumb[1])
        );
    }

    #[test]
    fn pointer_mapping_moves_toward_newest_when_dragged_down() {
        let layout = scrollbar_layout(&ctx(), 100, 25, 75, 75).unwrap();
        let middle = scroll_offset_for_pointer(&layout, layout.track[1] + layout.travel / 2.0, 0.0);
        let bottom = scroll_offset_for_pointer(&layout, layout.track[3] + 100.0, 0.0);
        assert!(middle < 75);
        assert_eq!(bottom, 0);
    }

    #[test]
    fn pointer_mapping_clamps_above_track_to_max_scroll() {
        let layout = scrollbar_layout(&ctx(), 100, 25, 75, 0).unwrap();
        assert_eq!(scroll_offset_for_pointer(&layout, -100.0, 0.0), 75);
    }
}
