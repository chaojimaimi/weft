//! v1.18.10 (PLAN_v11810_FROZEN_BG_VERTEX_AFFINE §1.1): truth table for the
//! frozen-rect affine. Expected values are hand-computed literals — none of
//! them re-derives the formula under test (anti-tautological): ratios are
//! chosen so most results are exactly representable f32 values (asserted
//! with `==`), the 0.37x / 2.6x non-integer-ratio rows use ±1e-3 decimal
//! literals, and the shrink→grow round trip uses the plan's ±1e-3 window.
//!
//! This module is NOT gate-exempt (`*/tests.rs` patterns do not match) —
//! kept lean on purpose ([B1] line-budget note in the plan).

use crate::layout::Rect;
use crate::renderer::panes::affine::{extend_cached_vertices, frozen_rect_affine};

const EPS: f32 = 1e-3;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < EPS
}

/// ① identity (old == new) → p unchanged, exactly — including the
/// degenerate-rect identity, where the pure map would collapse (zero-size
/// axis → origin) but the SERVE helper must short-circuit on rect equality
/// and extend the bytes untouched.
#[test]
fn identity_maps_points_onto_themselves() {
    let rect: Rect = [10.0, 20.0, 110.0, 220.0];
    assert_eq!(frozen_rect_affine(rect, rect, [35.0, 95.0]), [35.0, 95.0]);
    assert_eq!(
        frozen_rect_affine(rect, rect, [110.0, 220.0]),
        [110.0, 220.0]
    );
    // Degenerate identity: the pure map collapses every point onto the
    // origin (documented zero-size behavior); the serve helper must NOT —
    // it short-circuits on `baked == current` first.
    let degenerate: Rect = [5.0, 5.0, 5.0, 5.0];
    assert_eq!(
        frozen_rect_affine(degenerate, degenerate, [7.0, 9.0]),
        [5.0, 5.0],
        "pure map: zero-size axis collapses to new.origin"
    );
    let mut out = Vec::new();
    extend_cached_vertices(&[1.0, 2.0, 3.0, 4.0], degenerate, degenerate, &mut out);
    assert_eq!(
        out,
        [1.0, 2.0, 3.0, 4.0],
        "identity serve must extend the bytes untouched (no chunking)"
    );
}

/// ② uniform re-scale: exact 3x grow; 2.6x / 0.37x non-integer ratios with
/// hand-computed decimal expectations (±1e-3).
#[test]
fn uniform_grow_and_shrink_follow_the_linear_map() {
    let old: Rect = [0.0, 0.0, 100.0, 100.0];
    // 3x grow, origin moved to (50, 25): 50 + 10*3, 25 + 60*3 — exact f32.
    let grown: Rect = [50.0, 25.0, 350.0, 325.0];
    assert_eq!(
        frozen_rect_affine(old, grown, [10.0, 60.0]),
        [80.0, 205.0],
        "exact 3x: new.size/old.size = 300/100 = 3.0"
    );
    // 2.6x grow, origin (10, 10): 10 + 30*2.6 = 88.0, 10 + 70*2.6 = 192.0.
    let grow_26: Rect = [10.0, 10.0, 270.0, 270.0];
    let p = frozen_rect_affine(old, grow_26, [30.0, 70.0]);
    assert!(close(p[0], 88.0) && close(p[1], 192.0), "got {p:?}");
    // 0.37x shrink, origin (200, 300): 200 + 10*0.37 = 203.7,
    // 300 + 60*0.37 = 322.2.
    let shrink_37: Rect = [200.0, 300.0, 237.0, 337.0];
    let p = frozen_rect_affine(old, shrink_37, [10.0, 60.0]);
    assert!(close(p[0], 203.7) && close(p[1], 322.2), "got {p:?}");
}

/// ③ single-axis degenerate: that axis collapses to `new`'s origin
/// component, no division by zero. 3a = baked-side zero width, 3b =
/// target-side zero width (identical expectation constants, both stated).
#[test]
fn zero_extent_axis_collapses_to_new_origin() {
    // 3a: old width 0 — every baked x sat on old.origin.x (50.0).
    let old: Rect = [50.0, 10.0, 50.0, 110.0];
    let new: Rect = [100.0, 0.0, 300.0, 200.0];
    assert_eq!(
        frozen_rect_affine(old, new, [50.0, 55.0]),
        [100.0, 90.0],
        "x collapses to new.origin.x = 100.0; y: 0 + (55-10)*(200/100) = 90.0"
    );
    // 3b: new width 0 — the whole axis lands on new.origin.x (77.0).
    let old: Rect = [0.0, 10.0, 100.0, 110.0];
    let new: Rect = [77.0, 5.0, 77.0, 205.0];
    assert_eq!(
        frozen_rect_affine(old, new, [30.0, 60.0]),
        [77.0, 105.0],
        "x collapses to new.origin.x = 77.0; y: 5 + (60-10)*(200/100) = 105.0"
    );
}

/// ④ both axes degenerate → pure translation to new.origin.
#[test]
fn both_axes_zero_map_is_a_pure_translation() {
    // Baked side collapsed: everything lands on new.origin.
    let old: Rect = [42.0, 24.0, 42.0, 24.0];
    let new: Rect = [10.0, 20.0, 310.0, 220.0];
    assert_eq!(frozen_rect_affine(old, new, [42.0, 24.0]), [10.0, 20.0]);
    // Target side collapsed: everything lands on the point rect.
    let old: Rect = [0.0, 0.0, 100.0, 100.0];
    let point: Rect = [7.0, 9.0, 7.0, 9.0];
    assert_eq!(frozen_rect_affine(old, point, [30.0, 80.0]), [7.0, 9.0]);
}

/// ⑤ negative delta (origin moves up-left) and the plan's shrink→grow
/// inverse round trip (±1e-3) — order-independent re-basing.
#[test]
fn negative_delta_and_shrink_grow_round_trip() {
    let old: Rect = [100.0, 100.0, 200.0, 200.0];
    let new: Rect = [0.0, 0.0, 100.0, 100.0];
    assert_eq!(
        frozen_rect_affine(old, new, [150.0, 120.0]),
        [50.0, 20.0],
        "ratio exactly 1.0: p - (100, 100)"
    );
    let small: Rect = [0.0, 0.0, 37.0, 37.0];
    let full: Rect = [0.0, 0.0, 100.0, 100.0];
    let shrunk = frozen_rect_affine(full, small, [10.0, 60.0]);
    assert!(
        close(shrunk[0], 3.7) && close(shrunk[1], 22.2),
        "got {shrunk:?}"
    );
    let back = frozen_rect_affine(small, full, shrunk);
    assert!(
        close(back[0], 10.0) && close(back[1], 60.0),
        "shrink→grow must round-trip ±1e-3, got {back:?}"
    );
}

/// ⑥ a point outside the baked rect maps proportionally OUT of the target
/// rect — no clamping (pane clip handles overflow at draw time).
#[test]
fn out_of_rect_bake_maps_proportionally_without_clamping() {
    let old: Rect = [0.0, 0.0, 100.0, 100.0];
    let new: Rect = [200.0, 300.0, 400.0, 500.0];
    let mapped = frozen_rect_affine(old, new, [-30.0, 150.0]);
    assert_eq!(
        mapped,
        [140.0, 600.0],
        "200 + (-30)*2 = 140.0, 300 + 150*2 = 600.0 — exact 2x"
    );
    assert!(
        mapped[0] < new[0] && mapped[1] > new[3],
        "the mapped point must leave new's bounds open (no clamp)"
    );
}

/// Helper-level mapped serve: the stream length is preserved, every 12-float
/// vertex actually moves (0.5x re-base), and the 10 non-position floats are
/// bit-identical (chunks_exact(12) per VERTEX, not per quad).
#[test]
fn mapped_serve_moves_only_vertex_positions_bit_exactly() {
    let baked: Rect = [0.0, 0.0, 400.0, 300.0];
    let current: Rect = [40.0, 30.0, 240.0, 180.0];
    // One band quad + one content quad, same shape as the freeze serve.
    let mut stream: Vec<f32> = Vec::new();
    for quad in [[0.0, 0.0, 400.0, 24.0], [8.0, 40.0, 392.0, 64.0]] {
        for (i, (x, y)) in [
            (quad[0], quad[1]),
            (quad[2], quad[1]),
            (quad[2], quad[3]),
            (quad[0], quad[1]),
            (quad[2], quad[3]),
            (quad[0], quad[3]),
        ]
        .into_iter()
        .enumerate()
        {
            stream.extend_from_slice(&[
                x,
                y,
                i as f32 * 0.25,
                1.0 - i as f32 * 0.125,
                0.25,
                0.5,
                0.75,
                1.0,
                0.1,
                0.2,
                0.3,
                0.4,
            ]);
        }
    }
    let mut out: Vec<f32> = Vec::new();
    extend_cached_vertices(&stream, baked, current, &mut out);
    assert_eq!(
        out.len(),
        stream.len(),
        "mapping must not change the stream length"
    );
    const VERTEX_FLOATS: usize = 12;
    for (vi, (v, b)) in out
        .chunks_exact(VERTEX_FLOATS)
        .zip(stream.chunks_exact(VERTEX_FLOATS))
        .enumerate()
    {
        for (k, (m, o)) in v[2..].iter().zip(b[2..].iter()).enumerate() {
            assert_eq!(
                m.to_bits(),
                o.to_bits(),
                "vertex {vi} tail float {k} perturbed by the affine"
            );
        }
        assert_ne!(v[0], b[0], "vertex {vi} x must move under the 0.5x re-base");
        assert_ne!(v[1], b[1], "vertex {vi} y must move under the 0.5x re-base");
    }
}
