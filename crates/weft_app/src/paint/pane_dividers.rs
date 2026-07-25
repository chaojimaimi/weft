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
}
