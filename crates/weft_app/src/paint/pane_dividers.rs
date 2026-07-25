//! Pane divider + active-pane focus ring rendering for v1.3 split mode.
//!
//! Batch 7 (v1.3.1) visual polish. Drawn after the background panes and
//! before the overlay tail in `MetalRenderer::draw()`. Pure vertex emitter —
//! no renderer state dependency. Reuses `build_focus_ring` for the focus
//! indicator so Increase Contrast / focus-ring-thickness behavior stays
//! consistent with the panel + prompt focus rings.
//!
//! Divider derivation: given the split-tree-computed `pane_layouts` (one
//! `[x0,y0,x1,y1]` rect per pane), find the shared edges between adjacent
//! panes. Two panes are horizontally stacked (side-by-side, divided by a
//! vertical line) when one's right edge (`x1`) equals the other's left edge
//! (`x0`). Two panes are vertically stacked (top/bottom, divided by a
//! horizontal line) when one's bottom edge (`y1`) equals the other's top edge
//! (`y0`). Each such shared edge contributes one divider line.

use crate::layout::Rect;
use crate::paint::primitives::{
    build_focus_ring, focus_ring_alpha, focus_ring_thickness, push_quad,
};
use weft_core::pane_layout::PaneId;

/// Divider line axis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DividerAxis {
    /// A vertical line (divides left/right panes — horizontal split direction).
    Vertical,
    /// A horizontal line (divides top/bottom panes — vertical split direction).
    Horizontal,
}

/// Geometry needed to paint pane dividers + the active-pane focus ring.
#[derive(Default)]
pub(crate) struct DividerGeometry {
    /// Divider lines: `(axis, x_or_y)` — for `Vertical` the coord is the x of
    /// the line; for `Horizontal` the y.
    pub(crate) edges: Vec<(DividerAxis, f32)>,
    /// The active pane's rect (for focus ring). `None` in single-pane tabs.
    pub(crate) active_rect: Option<Rect>,
}

/// Derive divider edges + active-pane rect from the split-tree layout.
///
/// `pane_layouts` is the output of `SplitTree::layout(content_rect)` — one
/// `(PaneId, Rect)` per pane, in declaration order. `active_pane_id` selects
/// the focus-ring target.
///
/// Algorithm: for each pair of panes, check if they share an edge (one's x1
/// equals the other's x0 → vertical divider; one's y1 equals the other's y0
/// → horizontal divider). O(n²) over panes — fine, pane counts are tiny
/// (typically 2-4). Deduplicates edges (two adjacent panes both report the
/// same shared edge).
pub(crate) fn pane_divider_edges(
    pane_layouts: &[(PaneId, Rect)],
    active_pane_id: PaneId,
) -> DividerGeometry {
    let mut edges: Vec<(DividerAxis, f32)> = Vec::new();
    let mut active_rect: Option<Rect> = None;

    // Walk all pane pairs looking for shared edges.
    for (i, (id_a, rect_a)) in pane_layouts.iter().enumerate() {
        if *id_a == active_pane_id {
            active_rect = Some(*rect_a);
        }
        let [ax0, ay0, ax1, ay1] = *rect_a;
        for (id_b, rect_b) in pane_layouts.iter().skip(i + 1) {
            let [bx0, by0, bx1, by1] = *rect_b;
            // Skip self / non-adjacency quickly: panes must overlap on the
            // cross-axis to count as sharing an edge.
            // Vertical divider (a's right meets b's left, or vice versa):
            // require y-overlap.
            let y_overlap = ay0 < by1 && by0 < ay1;
            let x_overlap = ax0 < bx1 && bx0 < ax1;
            if y_overlap {
                let edge_x = if (ax1 - bx0).abs() < 0.5 {
                    Some(ax1)
                } else if (bx1 - ax0).abs() < 0.5 {
                    Some(bx1)
                } else {
                    None
                };
                if let Some(x) = edge_x {
                    let entry = (DividerAxis::Vertical, x);
                    if !edges.contains(&entry) {
                        edges.push(entry);
                    }
                }
            }
            if x_overlap {
                let edge_y = if (ay1 - by0).abs() < 0.5 {
                    Some(ay1)
                } else if (by1 - ay0).abs() < 0.5 {
                    Some(by1)
                } else {
                    None
                };
                if let Some(y) = edge_y {
                    let entry = (DividerAxis::Horizontal, y);
                    if !edges.contains(&entry) {
                        edges.push(entry);
                    }
                }
            }
            // Suppress unused warning on id_b (kept for clarity).
            let _ = id_b;
        }
    }

    DividerGeometry { edges, active_rect }
}

/// Draw all divider lines. Each divider is a 1px (physical) quad in the
/// separator color, spanning the content rect on the cross axis.
pub(crate) fn push_pane_dividers(
    verts: &mut Vec<f32>,
    edges: &[(DividerAxis, f32)],
    content: Rect,
    separator_color: [f32; 4],
) {
    let [cx0, cy0, cx1, cy1] = content;
    let uv = [0.0f32; 4];
    let fg = [0.0f32; 4];
    for (axis, coord) in edges {
        match axis {
            DividerAxis::Vertical => {
                // 1px vertical line at x=coord, spanning content height.
                push_quad(
                    verts,
                    [*coord, cy0, *coord + 1.0, cy1],
                    uv,
                    fg,
                    separator_color,
                );
            }
            DividerAxis::Horizontal => {
                // 1px horizontal line at y=coord, spanning content width.
                push_quad(
                    verts,
                    [cx0, *coord, cx1, *coord + 1.0],
                    uv,
                    fg,
                    separator_color,
                );
            }
        }
    }
}

/// Draw the active-pane focus ring. Reuses `build_focus_ring` so thickness
/// and Increase Contrast behavior match the panel + prompt rings.
pub(crate) fn push_active_pane_focus_ring(
    verts: &mut Vec<f32>,
    active_rect: Rect,
    accent_color: [f32; 4],
    increase_contrast: bool,
) {
    let mut color = accent_color;
    color[3] *= focus_ring_alpha(increase_contrast);
    build_focus_ring(
        verts,
        active_rect,
        color,
        focus_ring_thickness(increase_contrast),
    );
}

/// One-shot entry point for v1.3.1 pane overlays: derives divider edges from
/// `pane_layouts`, then draws dividers (theme separator color) + the active-
/// pane focus ring (theme accent). Self-contained so the renderer's call site
/// stays a single line — keeps `renderer.rs` within its architecture-gate
/// budget.
///
/// No-op when `pane_layouts` has fewer than 2 panes (single-pane tabs).
pub(crate) fn push_pane_overlays(
    verts: &mut Vec<f32>,
    pane_layouts: &[(PaneId, Rect)],
    active_pane_id: PaneId,
    content_rect: Rect,
    separator_color: [f32; 4],
    accent_color: [f32; 4],
    increase_contrast: bool,
) {
    if pane_layouts.len() < 2 {
        return;
    }
    let geo = pane_divider_edges(pane_layouts, active_pane_id);
    push_pane_dividers(verts, &geo.edges, content_rect, separator_color);
    if let Some(active) = geo.active_rect {
        push_active_pane_focus_ring(verts, active, accent_color, increase_contrast);
    }
}

// ── v1.3.2: drag-to-resize hit-testing ─────────────────────────────────

/// One draggable pane divider: the axis, its coordinate, and the two panes
/// whose shared edge this is. `first` is the top/left pane, `second` the
/// bottom/right — matching `SplitTree`'s first/second convention so the
/// caller can compute the new ratio directly.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DraggableDivider {
    pub axis: DividerAxis,
    /// The divider's coordinate (x for Vertical, y for Horizontal). Read by
    /// tests; production code computes the ratio from `bounds` + pointer.
    #[allow(dead_code)]
    pub coord: f32,
    pub first: PaneId,
    /// The bottom/right pane. Paired with `first` to uniquely identify the
    /// split via `set_ratio_for_pair`.
    pub second: PaneId,
    /// The union rect the split divides — used to compute the new ratio:
    /// `new_ratio = (pointer - bounds[0]) / (bounds[2] - bounds[0])` for
    /// vertical dividers, or the y analog for horizontal.
    pub bounds: Rect,
}

/// Hit-test a pointer against all draggable dividers derived from
/// `pane_layouts`. Returns the nearest divider within `tolerance` pixels, or
/// `None`. `tolerance` is in physical pixels (use 4.0 to match the sidebar
/// resize handle).
///
/// Unlike `pane_divider_edges` (which dedupes edges for painting), this
/// function tracks which pane pair each divider belongs to — needed to call
/// `SplitTree::set_ratio`.
pub(crate) fn pane_divider_at(
    pane_layouts: &[(PaneId, Rect)],
    x: f32,
    y: f32,
    tolerance: f32,
) -> Option<DraggableDivider> {
    let mut best: Option<(f32, DraggableDivider)> = None;
    for (i, (id_a, rect_a)) in pane_layouts.iter().enumerate() {
        let [ax0, ay0, ax1, ay1] = *rect_a;
        for (id_b, rect_b) in pane_layouts.iter().skip(i + 1) {
            let [bx0, by0, bx1, by1] = *rect_b;
            let y_overlap = ay0 < by1 && by0 < ay1;
            let x_overlap = ax0 < bx1 && bx0 < ax1;
            // Vertical divider (left/right panes): a.x1 == b.x0 or b.x1 == a.x0
            if y_overlap {
                let (coord, left_id, left_rect, right_id, right_rect) = if (ax1 - bx0).abs() < 0.5 {
                    (ax1, *id_a, *rect_a, *id_b, *rect_b)
                } else if (bx1 - ax0).abs() < 0.5 {
                    (bx1, *id_b, *rect_b, *id_a, *rect_a)
                } else {
                    continue;
                };
                let dist = (x - coord).abs();
                if dist <= tolerance {
                    let bounds = [
                        left_rect[0].min(right_rect[0]),
                        left_rect[1].min(right_rect[1]),
                        left_rect[2].max(right_rect[2]),
                        left_rect[3].max(right_rect[3]),
                    ];
                    let candidate = (
                        dist,
                        DraggableDivider {
                            axis: DividerAxis::Vertical,
                            coord,
                            first: left_id,
                            second: right_id,
                            bounds,
                        },
                    );
                    if best.as_ref().map_or(true, |(bd, _)| dist < *bd) {
                        best = Some(candidate);
                    }
                }
            }
            // Horizontal divider (top/bottom panes): a.y1 == b.y0 or b.y1 == a.y0
            if x_overlap {
                let (coord, top_id, top_rect, bot_id, bot_rect) = if (ay1 - by0).abs() < 0.5 {
                    (ay1, *id_a, *rect_a, *id_b, *rect_b)
                } else if (by1 - ay0).abs() < 0.5 {
                    (by1, *id_b, *rect_b, *id_a, *rect_a)
                } else {
                    continue;
                };
                let dist = (y - coord).abs();
                if dist <= tolerance {
                    let bounds = [
                        top_rect[0].min(bot_rect[0]),
                        top_rect[1].min(bot_rect[1]),
                        top_rect[2].max(bot_rect[2]),
                        top_rect[3].max(bot_rect[3]),
                    ];
                    let candidate = (
                        dist,
                        DraggableDivider {
                            axis: DividerAxis::Horizontal,
                            coord,
                            first: top_id,
                            second: bot_id,
                            bounds,
                        },
                    );
                    if best.as_ref().map_or(true, |(bd, _)| dist < *bd) {
                        best = Some(candidate);
                    }
                }
            }
        }
    }
    best.map(|(_, d)| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        [x0, y0, x1, y1]
    }

    #[test]
    fn single_pane_has_no_dividers() {
        let layouts = vec![(PaneId(1), rect(0.0, 0.0, 800.0, 600.0))];
        let geo = pane_divider_edges(&layouts, PaneId(1));
        assert!(geo.edges.is_empty());
        assert_eq!(geo.active_rect, Some(rect(0.0, 0.0, 800.0, 600.0)));
    }

    #[test]
    fn vertical_split_produces_one_vertical_edge() {
        // Two side-by-side panes: left [0,0,400,600], right [400,0,800,600].
        // Left's x1 (400) == right's x0 (400) → vertical divider at x=400.
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 400.0, 600.0)),
            (PaneId(2), rect(400.0, 0.0, 800.0, 600.0)),
        ];
        let geo = pane_divider_edges(&layouts, PaneId(1));
        assert_eq!(geo.edges.len(), 1);
        assert_eq!(geo.edges[0], (DividerAxis::Vertical, 400.0));
        assert_eq!(geo.active_rect, Some(rect(0.0, 0.0, 400.0, 600.0)));
    }

    #[test]
    fn horizontal_split_produces_one_horizontal_edge() {
        // Two stacked panes: top [0,0,800,300], bottom [0,300,800,600].
        // Top's y1 (300) == bottom's y0 (300) → horizontal divider at y=300.
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 800.0, 300.0)),
            (PaneId(2), rect(0.0, 300.0, 800.0, 600.0)),
        ];
        let geo = pane_divider_edges(&layouts, PaneId(2));
        assert_eq!(geo.edges.len(), 1);
        assert_eq!(geo.edges[0], (DividerAxis::Horizontal, 300.0));
        assert_eq!(geo.active_rect, Some(rect(0.0, 300.0, 800.0, 600.0)));
    }

    #[test]
    fn four_pane_grid_produces_two_vertical_and_two_horizontal_edges() {
        // 2x2 grid:
        //   [0,0,400,300]   [400,0,800,300]
        //   [0,300,400,600] [400,300,800,600]
        // Vertical edges at x=400 (top row + bottom row), horizontal at y=300
        // (left col + right col). Deduplicated → 1 vertical + 1 horizontal.
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 400.0, 300.0)),
            (PaneId(2), rect(400.0, 0.0, 800.0, 300.0)),
            (PaneId(3), rect(0.0, 300.0, 400.0, 600.0)),
            (PaneId(4), rect(400.0, 300.0, 800.0, 600.0)),
        ];
        let geo = pane_divider_edges(&layouts, PaneId(1));
        // 1 vertical (x=400, deduped across top+bottom row) + 1 horizontal (y=300).
        assert_eq!(
            geo.edges.len(),
            2,
            "expected 2 deduped edges, got {:?}",
            geo.edges
        );
        assert!(geo.edges.contains(&(DividerAxis::Vertical, 400.0)));
        assert!(geo.edges.contains(&(DividerAxis::Horizontal, 300.0)));
    }

    #[test]
    fn push_pane_dividers_emits_one_quad_per_edge() {
        let mut verts = Vec::new();
        let edges = vec![
            (DividerAxis::Vertical, 400.0),
            (DividerAxis::Horizontal, 300.0),
        ];
        push_pane_dividers(&mut verts, &edges, rect(0.0, 0.0, 800.0, 600.0), [1.0; 4]);
        // Each quad = 6 vertices × 12 floats = 72 floats. 2 edges → 144 floats.
        assert_eq!(verts.len(), 2 * 72);
    }

    #[test]
    fn focus_ring_emits_four_edges() {
        let mut verts = Vec::new();
        push_active_pane_focus_ring(
            &mut verts,
            rect(10.0, 10.0, 410.0, 310.0),
            [0.2, 0.5, 0.9, 1.0],
            false,
        );
        // build_focus_ring pushes 4 quads (top/bottom/left/right) = 4 × 72 floats.
        assert_eq!(verts.len(), 4 * 72);
    }

    // ── v1.3.2: pane_divider_at hit-testing ──────────────────────────────

    #[test]
    fn pane_divider_at_hits_vertical_divider_within_tolerance() {
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 400.0, 600.0)),
            (PaneId(2), rect(400.0, 0.0, 800.0, 600.0)),
        ];
        let hit = pane_divider_at(&layouts, 402.0, 300.0, 4.0);
        assert!(hit.is_some(), "should hit within 4px tolerance");
        let d = hit.unwrap();
        assert_eq!(d.axis, DividerAxis::Vertical);
        assert!((d.coord - 400.0).abs() < 0.01);
        assert_eq!(d.first, PaneId(1)); // left
        assert_eq!(d.second, PaneId(2)); // right
        assert_eq!(d.bounds, rect(0.0, 0.0, 800.0, 600.0));
    }

    #[test]
    fn pane_divider_at_misses_outside_tolerance() {
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 400.0, 600.0)),
            (PaneId(2), rect(400.0, 0.0, 800.0, 600.0)),
        ];
        // 10px away from the divider at x=400, tolerance=4 → miss.
        assert!(pane_divider_at(&layouts, 410.0, 300.0, 4.0).is_none());
    }

    #[test]
    fn pane_divider_at_hits_horizontal_divider() {
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 800.0, 300.0)),
            (PaneId(2), rect(0.0, 300.0, 800.0, 600.0)),
        ];
        let d = pane_divider_at(&layouts, 400.0, 299.0, 4.0).unwrap();
        assert_eq!(d.axis, DividerAxis::Horizontal);
        assert!((d.coord - 300.0).abs() < 0.01);
        assert_eq!(d.first, PaneId(1)); // top
        assert_eq!(d.second, PaneId(2)); // bottom
    }

    #[test]
    fn pane_divider_at_picks_nearest_when_two_dividers_near() {
        // Two vertical dividers: one at x=400 (dist 2), one at x=410 (dist 8).
        // tolerance=4 → only the x=400 one is in range, and it wins.
        let layouts = vec![
            (PaneId(1), rect(0.0, 0.0, 400.0, 600.0)),
            (PaneId(2), rect(400.0, 0.0, 410.0, 600.0)),
            (PaneId(3), rect(410.0, 0.0, 800.0, 600.0)),
        ];
        let d = pane_divider_at(&layouts, 402.0, 300.0, 4.0).unwrap();
        assert!((d.coord - 400.0).abs() < 0.5);
    }

    #[test]
    fn pane_divider_at_none_for_single_pane() {
        let layouts = vec![(PaneId(1), rect(0.0, 0.0, 800.0, 600.0))];
        assert!(pane_divider_at(&layouts, 400.0, 300.0, 4.0).is_none());
    }
}
