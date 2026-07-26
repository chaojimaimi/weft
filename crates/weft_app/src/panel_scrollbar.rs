//! History-sidebar scrollbar geometry and pointer mapping.

use crate::layout::Rect;
use crate::paint::primitives::snap_physical_rect;

#[derive(Clone, Copy, Debug)]
pub(crate) struct PanelScrollbarLayout {
    pub(crate) track: Rect,
    pub(crate) thumb: Rect,
    pub(crate) hit: Rect,
    pub(crate) travel: f32,
    pub(crate) max_scroll: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PanelScrollbarDragState {
    pub(crate) layout: PanelScrollbarLayout,
    pub(crate) grab_offset: f32,
}

pub(crate) fn panel_scrollbar_layout(
    panel: Rect,
    list_top: f32,
    total: usize,
    visible: usize,
    scroll: usize,
    min_thumb: f32,
) -> Option<PanelScrollbarLayout> {
    let max_scroll = total.saturating_sub(visible);
    let track_height = (panel[3] - list_top).max(0.0);
    if max_scroll == 0 || visible == 0 || track_height <= 0.0 {
        return None;
    }
    // Leave the outer 4px to the sidebar-width resize handle. Keeping the
    // scrollbar inward prevents one press from ambiguously owning both drags.
    let track = [panel[2] - 9.0, list_top, panel[2] - 7.0, panel[3]];
    let thumb_height = (track_height * visible as f32 / total as f32)
        .max(min_thumb)
        .min(track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let ratio = scroll.min(max_scroll) as f32 / max_scroll as f32;
    // v1.4.0: snap both thumb edges to integer physical pixels. Previously
    // this used `(travel * ratio * 2.0).round() * 0.5` to snap thumb_top to
    // 0.5 logical px (1 physical px at 2× scale), but the far edge
    // (`thumb_top + thumb_height`) was not snapped and could land on a
    // sub-pixel at 1× scale. The physical-pixel snap helper (pure `round()`)
    // is correct at every scale and snaps both edges in one call.
    let thumb_offset = travel * ratio;
    let thumb_top_raw = list_top + thumb_offset;
    let thumb_bottom_raw = thumb_top_raw + thumb_height;
    let (thumb_top, thumb_bottom) = snap_physical_rect(thumb_top_raw, thumb_bottom_raw);
    let thumb = [track[0], thumb_top, track[2], thumb_bottom];
    let hit = [panel[2] - 18.0, list_top, panel[2] - 5.0, panel[3]];
    Some(PanelScrollbarLayout {
        track,
        thumb,
        hit,
        travel,
        max_scroll,
    })
}

pub(crate) fn contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect[0] && x < rect[2] && y >= rect[1] && y < rect[3]
}

pub(crate) fn thumb_grab_offset(layout: &PanelScrollbarLayout, x: f32, y: f32) -> Option<f32> {
    contains(layout.thumb, x, y).then_some(y - layout.thumb[1])
}

pub(crate) fn scroll_offset_for_pointer(
    layout: &PanelScrollbarLayout,
    pointer_y: f32,
    grab_offset: f32,
) -> usize {
    if layout.travel <= f32::EPSILON {
        return 0;
    }
    let thumb_top =
        (pointer_y - grab_offset).clamp(layout.track[1], layout.track[1] + layout.travel);
    let ratio = (thumb_top - layout.track[1]) / layout.travel;
    (ratio * layout.max_scroll as f32)
        .round()
        .clamp(0.0, layout.max_scroll as f32) as usize
}

#[cfg(test)]
mod tests {
    use super::{panel_scrollbar_layout, scroll_offset_for_pointer};

    const PANEL: [f32; 4] = [0.0, 28.0, 240.0, 700.0];
    const LIST_TOP: f32 = 78.0;

    #[test]
    fn ten_thousand_rows_keep_newest_and_oldest_at_track_ends() {
        let newest = panel_scrollbar_layout(PANEL, LIST_TOP, 10_000, 25, 0, 20.0).unwrap();
        let oldest = panel_scrollbar_layout(PANEL, LIST_TOP, 10_000, 25, 9_975, 20.0).unwrap();
        assert_eq!(newest.thumb[1], newest.track[1]);
        assert_eq!(oldest.thumb[3], oldest.track[3]);
        assert_eq!(newest.max_scroll, 9_975);
        assert!(newest.thumb[3] - newest.thumb[1] >= 20.0);
        assert!(newest.hit[2] <= PANEL[2] - 4.0);
    }

    #[test]
    fn pointer_mapping_is_monotonic_and_clamped() {
        let layout = panel_scrollbar_layout(PANEL, LIST_TOP, 10_000, 25, 0, 20.0).unwrap();
        let top = scroll_offset_for_pointer(&layout, -100.0, 0.0);
        let middle = scroll_offset_for_pointer(&layout, layout.track[1] + layout.travel / 2.0, 0.0);
        let bottom = scroll_offset_for_pointer(&layout, 900.0, 0.0);
        assert_eq!(top, 0);
        assert!(middle > 0 && middle < layout.max_scroll);
        assert_eq!(bottom, layout.max_scroll);
    }

    // ── v1.4.0: physical-pixel alignment ──────────────────────────────

    #[test]
    fn thumb_edges_are_integer_physical_pixels_across_scroll_range() {
        // The previous `*2 → round → *0.5` only snapped thumb_top to 0.5px;
        // the far edge `thumb_top + thumb_height` was unaligned. The new
        // snap_physical_rect path guarantees both edges are integer.
        for scroll in [0, 1, 7, 100, 1234, 5000, 9_974, 9_975] {
            let layout = panel_scrollbar_layout(PANEL, LIST_TOP, 10_000, 25, scroll, 20.0).unwrap();
            assert!(
                layout.thumb[1].fract() == 0.0,
                "scroll={scroll}: thumb top not integer: {}",
                layout.thumb[1]
            );
            assert!(
                layout.thumb[3].fract() == 0.0,
                "scroll={scroll}: thumb bottom not integer: {}",
                layout.thumb[3]
            );
        }
    }

    #[test]
    fn thumb_edges_remain_integer_for_fractional_list_top() {
        // A fractional list_top would previously produce a fractional far
        // edge. After snap_physical_rect, both edges are integer.
        let panel: [f32; 4] = [0.0, 28.3, 240.7, 700.9];
        let list_top = 78.5;
        for scroll in [0, 100, 5000, 9_975] {
            let layout = panel_scrollbar_layout(panel, list_top, 10_000, 25, scroll, 20.0).unwrap();
            assert!(
                layout.thumb[1].fract() == 0.0,
                "scroll={scroll}: thumb top not integer: {}",
                layout.thumb[1]
            );
            assert!(
                layout.thumb[3].fract() == 0.0,
                "scroll={scroll}: thumb bottom not integer: {}",
                layout.thumb[3]
            );
        }
    }
}
