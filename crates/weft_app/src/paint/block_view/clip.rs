//! v1.14.2 (PLAN_v1142 §2 A1): hard bottom clip for block-view row bands.
//!
//! The block view culls rows by intersection only (`row_top > clip_bottom`),
//! so with a fractional `block_scroll` (trackpad sub-row offsets) the
//! bottom-most glyph band straddles `content_bottom_y` and bleeds into the
//! input box below. Every row-painting emitter funnels its primitives through
//! this module's two kernels:
//!
//! - [`crop_bottom`] — the pure per-band decision (truth-table tested here);
//! - [`push_quad_clipped`] — `push_quad` with that decision applied.
//!
//! Clip semantics (全车统一): for a band `[y0, y1)` against the clip line —
//! `y0 >= clip` → cull; `y1 <= clip` → emit unchanged; otherwise keep the
//! upper part: the bottom edge moves to the clip line and the paired atlas V
//! is interpolated proportionally (crop, never stretch), matching how
//! mainstream terminals shear the boundary row.

use crate::paint::primitives::push_quad;

/// The three outcomes of clipping a primitive band `[y0, y1)` against the
/// hard clip line (= `content_bottom_y`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum BottomCrop {
    /// `y0 >= clip` — nothing visible; the caller skips the primitive.
    Below,
    /// `y1 <= clip` — fully inside; emit unchanged.
    Full,
    /// `y0 < clip < y1` — keep the upper part: the bottom edge moves to the
    /// clip line and the V coordinate interpolated proportionally.
    Crop { y1: f32, v1: f32 },
}

/// Decide how one primitive band `[y0, y1)` with paired V endpoints
/// `[v0, v1]` meets the hard clip line. `v0` is the V sampled at `y0`, `v1`
/// the V at `y1` (whatever the atlas orientation — only the linear pairing
/// matters); the cropped edge lerps between them at the visible fraction.
pub(crate) fn crop_bottom(y0: f32, y1: f32, v0: f32, v1: f32, clip_bottom: f32) -> BottomCrop {
    debug_assert!(y1 >= y0, "inverted band [{y0}, {y1})");
    if y0 >= clip_bottom {
        return BottomCrop::Below;
    }
    if y1 <= clip_bottom {
        return BottomCrop::Full;
    }
    let t = (clip_bottom - y0) / (y1 - y0);
    BottomCrop::Crop {
        y1: clip_bottom,
        v1: v0 + (v1 - v0) * t,
    }
}

/// `push_quad` with the A1 bottom clip applied: bands fully below the line
/// are skipped, crossing bands keep their upper part (bottom edge + V
/// interpolated together, so the texture crops without stretching). Solid
/// quads carry a zero V pair — the interpolation is the identity there.
pub(crate) fn push_quad_clipped(
    vertices: &mut Vec<f32>,
    dst: [f32; 4],
    uv: [f32; 4],
    fg: [f32; 4],
    bg: [f32; 4],
    clip_bottom: f32,
) {
    let [x0, y0, x1, y1] = dst;
    let [u0, v0, u1, v1] = uv;
    match crop_bottom(y0, y1, v0, v1, clip_bottom) {
        BottomCrop::Below => {}
        BottomCrop::Full => push_quad(vertices, dst, uv, fg, bg),
        BottomCrop::Crop { y1, v1 } => {
            push_quad(vertices, [x0, y0, x1, y1], [u0, v0, u1, v1], fg, bg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Truth-table helper: (band, clip) → outcome.
    fn crop(y0: f32, y1: f32, clip: f32) -> BottomCrop {
        crop_bottom(y0, y1, 0.0, 1.0, clip)
    }

    /// 不跨界恒等：a band fully inside the clip region emits unchanged.
    #[test]
    fn band_inside_clip_is_untouched() {
        assert_eq!(crop(100.0, 116.0, 600.0), BottomCrop::Full);
        // Exactly touching the line from above is still fully visible.
        assert_eq!(crop(584.0, 600.0, 600.0), BottomCrop::Full);
    }

    /// 半跨界：the visible fraction moves both the bottom edge and V.
    #[test]
    fn band_half_crossing_crops_y_and_interpolates_v() {
        // [100, 116) against clip 108 → t = 0.5 → V lerps 0.0→1.0 to 0.5.
        assert_eq!(
            crop_bottom(100.0, 116.0, 0.0, 1.0, 108.0),
            BottomCrop::Crop { y1: 108.0, v1: 0.5 }
        );
        // Quarter crossing: t = 0.25.
        assert_eq!(
            crop_bottom(100.0, 116.0, 0.0, 1.0, 104.0),
            BottomCrop::Crop {
                y1: 104.0,
                v1: 0.25
            }
        );
        // Non-unity V pairing (text quads sample [v+vh … v] — flipped): the
        // interpolation still lands between the two endpoints.
        let crossed = crop_bottom(100.0, 116.0, 0.9, 0.1, 108.0);
        assert_eq!(
            crossed,
            BottomCrop::Crop {
                y1: 108.0,
                v1: 0.9 + (0.1 - 0.9) * 0.5
            }
        );
    }

    /// 全跨界剔除：a band fully below the line is culled outright.
    #[test]
    fn band_below_is_culled() {
        assert_eq!(crop(601.0, 617.0, 600.0), BottomCrop::Below);
        // Exactly touching the line from below is already invisible.
        assert_eq!(crop(600.0, 616.0, 600.0), BottomCrop::Below);
    }

    /// Degenerate zero-height bands cannot divide by zero: a band that is a
    /// single point resolves by the two boundary comparisons alone.
    #[test]
    fn zero_height_band_never_divides_by_zero() {
        assert_eq!(crop(100.0, 100.0, 600.0), BottomCrop::Full);
        assert_eq!(crop(600.0, 600.0, 600.0), BottomCrop::Below);
    }

    /// `push_quad_clipped` forwards the exact vertex stream in the Full case
    /// and shears the bottom edge (with the interpolated V on all four
    /// bottom-edge vertices) in the Crop case; Below emits nothing.
    #[test]
    fn push_quad_clipped_matches_decision_per_case() {
        let mut verts = Vec::new();
        push_quad_clipped(
            &mut verts,
            [0.0, 590.0, 10.0, 600.0],
            [0.1, 0.2, 0.3, 0.4],
            [1.0; 4],
            [0.0; 4],
            600.0,
        );
        assert_eq!(verts.len(), 72, "Full case emits one whole quad");
        push_quad_clipped(
            &mut verts,
            [0.0, 596.0, 10.0, 612.0],
            [0.1, 0.8, 0.3, 0.2],
            [1.0; 4],
            [0.0; 4],
            600.0,
        );
        assert_eq!(verts.len(), 144, "Crop case also emits one quad");
        // Crop case (the second quad, offset 72): bottom-edge vertices
        // (vertex 1, 2, 4 carry y1) sit on the clip line; their V is the
        // interpolated 0.8 + (0.2-0.8)*0.25.
        for (vi, expect_v) in [(1usize, 0.65f32), (2, 0.65), (4, 0.65)] {
            assert_eq!(verts[72 + 12 * vi + 1], 600.0, "vertex {vi} y on the clip");
            assert!(
                (verts[72 + 12 * vi + 3] - expect_v).abs() < 1e-6,
                "vertex {vi} v"
            );
        }
        // Top-edge vertices keep the original geometry/V.
        assert_eq!(verts[72 + 1], 596.0);
        assert_eq!(verts[72 + 3], 0.8);
        push_quad_clipped(
            &mut verts,
            [0.0, 601.0, 10.0, 617.0],
            [0.0; 4],
            [1.0; 4],
            [0.0; 4],
            600.0,
        );
        assert_eq!(verts.len(), 144, "Below case emits nothing");
    }
}
