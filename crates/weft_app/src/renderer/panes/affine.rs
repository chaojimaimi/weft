//! v1.18.10 (PLAN_v11810_FROZEN_BG_VERTEX_AFFINE, 病灶④): frozen-rect
//! affine for background block panes.
//!
//! While the rewrap freeze is armed (zoom animation / live-resize window),
//! `panes.rs::build_background_pane_content` serves a background pane's
//! cached `entry.vertices` — absolutely baked under the rect recorded in
//! `entry.fingerprint.rect`. When the split tree reflows mid-window the
//! pane's CURRENT rect differs and the stale bytes paint mis-scaled,
//! misplaced block heads and content (the v1.18.10 symptom). The pure map
//! below re-bases every vertex position from the baked rect to the current
//! rect; the freeze's REBUILD skip itself is untouched (the shared
//! `block_layout_cache` churn defense — PLAN §1.3 forbidden zones).

use crate::layout::Rect;

/// Per-axis linear re-base `p' = new.origin + (p - old.origin) *
/// (new.size / old.size)`; `Rect` is `[x0, y0, x1, y1]` in absolute
/// viewport coords, so an axis spans `old[0]..old[2]` (x) / `old[1]..old[3]`
/// (y).
///
/// A degenerate extent on EITHER side of an axis (baked-side 0 — every
/// baked point sat on the origin line; target-side 0 — the axis collapsed)
/// maps that axis to `new`'s origin component instead of dividing by zero.
pub(crate) fn frozen_rect_affine(old: Rect, new: Rect, p: [f32; 2]) -> [f32; 2] {
    fn axis(old_a: f32, old_b: f32, new_a: f32, new_b: f32, pa: f32) -> f32 {
        let old_size = old_b - old_a;
        let new_size = new_b - new_a;
        if old_size == 0.0 || new_size == 0.0 {
            return new_a;
        }
        new_a + (pa - old_a) * (new_size / old_size)
    }
    [
        axis(old[0], old[2], new[0], new[2], p[0]),
        axis(old[1], old[3], new[1], new[3], p[1]),
    ]
}

/// Serve a cached background-pane vertex stream into `out`: an identity
/// rect (`baked == current`) extends the baked bytes untouched (the
/// zero-overhead short-circuit every cache sentinel leans on); a stale
/// rect serves the re-based copy via [`extend_rect_mapped`].
pub(crate) fn extend_cached_vertices(
    vertices: &[f32],
    baked: Rect,
    current: Rect,
    out: &mut Vec<f32>,
) {
    if baked == current {
        out.extend_from_slice(vertices);
    } else {
        extend_rect_mapped(vertices, baked, current, out);
    }
}

/// Append `vertices` to `out` with every vertex's position floats re-based
/// from `baked` to `current` through [`frozen_rect_affine`]. Chunks per
/// VERTEX — 12 floats (pos 2 + tex 2 + fg 4 + bg 4; the golden_tests.rs:583
/// convention), NOT per quad; only floats[0..2] pass through the affine,
/// the other 10 are copied bit-for-bit.
pub(crate) fn extend_rect_mapped(vertices: &[f32], baked: Rect, current: Rect, out: &mut Vec<f32>) {
    const VERTEX_FLOATS: usize = 12;
    debug_assert_eq!(
        vertices.len() % VERTEX_FLOATS,
        0,
        "block vertex streams are whole 12-float vertices"
    );
    let start = out.len();
    out.extend_from_slice(vertices);
    for v in out[start..].chunks_exact_mut(VERTEX_FLOATS) {
        let [x, y] = frozen_rect_affine(baked, current, [v[0], v[1]]);
        v[0] = x;
        v[1] = y;
    }
}
