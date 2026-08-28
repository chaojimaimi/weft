//! v1.11.3 (PLAN_v1113 §3.1/§3.2): underline geometry kernel + bold→bright
//! origin substitution, shared by the live-grid path (grid_instances) and
//! the block-history path (block_view).
//!
//! These live in their own module instead of `primitives.rs` so that file
//! stays within the architecture gate's 800-line budget; the call sites
//! import from here exactly as they would from primitives.

use crate::paint::primitives::resolve_cell_color;
use weft_core::grid::{CellColor, CellFlags, Color, UnderlineStyle};

/// Upper bound of rects emitted by [`underline_rects`] (Wavy = 8 step
/// quads; every other style emits fewer).
pub(crate) const MAX_UNDERLINE_RECTS: usize = 8;

/// v1.11.3 (PLAN_v1113 §3.1): shared underline geometry kernel — the ONE
/// place shape math lives, so the live-grid path (grid_instances) and the
/// block-history path (block_view) render byte-identical decorations.
///
/// Returns a stack array (upper bound [`MAX_UNDERLINE_RECTS`]) plus the
/// valid count. Each rect is [x0, y0, x1, y1]; the caller supplies the
/// baseline (bottom edge of the underline band) and thickness, which keeps
/// the two surfaces phase-consistent even though they measure rows
/// differently (grid: `y1 - UNDERLINE_HEIGHT`; block rows: row-pitch-based).
///
/// Shape spec: Single 1 solid bar; Double + second line at `-3.0` (the
/// contrast-test anchor, PLAN_v1113 §3.2); Wavy p = max(cw*0.5, 3), ±1px
/// step amplitude, 2 full periods per cell = 8 step quads (narrow cells
/// degrade to 4 quads / 1 period); Dotted dot = thickness, spacing cw/3;
/// Dashed dash = cw*0.6 gap = cw*0.4, phase from `x0` (continuous across
/// cell boundaries — `column` is reserved for documentation/future proof).
pub(crate) fn underline_rects(
    style: UnderlineStyle,
    x0: f32,
    width: f32,
    baseline_y: f32,
    thickness: f32,
    cw: f32,
    _column: usize,
) -> ([[f32; 4]; MAX_UNDERLINE_RECTS], usize) {
    let mut rects = [[0.0; 4]; MAX_UNDERLINE_RECTS];
    let mut n = 0usize;
    let y0 = baseline_y;
    let y1 = baseline_y + thickness;

    match style {
        UnderlineStyle::Single => {
            rects[0] = [x0, y0, x0 + width, y1];
            n = 1;
        }
        UnderlineStyle::Double => {
            rects[0] = [x0, y0, x0 + width, y1];
            // Second line 3px above (contrast anchor, PLAN_v1113 §3.2).
            rects[1] = [x0, y0 - 3.0, x0 + width, y1 - 3.0];
            n = 2;
        }
        UnderlineStyle::Wavy => {
            // 2 periods of wavelength p need 2p of width (p=cw/2 → exactly
            // one cell); under that, degrade to a single period = 4 quads.
            let p = (cw * 0.5).max(3.0);
            let quads = if width >= 2.0 * p { 8 } else { 4 };
            let seg = width / quads as f32;
            for (i, rect) in rects.iter_mut().enumerate().take(quads) {
                // Square-wave steps: first half of each period rides above
                // the baseline (−1px), second half below (+1px).
                let level = if (i / 2) % 2 == 0 { -1.0 } else { 1.0 };
                let qx0 = x0 + i as f32 * seg;
                *rect = [qx0, y0 + level, qx0 + seg, y1 + level];
            }
            n = quads;
        }
        UnderlineStyle::Dotted => {
            let spacing = (cw / 3.0).max(thickness);
            // 2–3 dots per cell: spaced cw/3, size = thickness.
            let count = ((width - thickness) / spacing).floor().clamp(1.0, 3.0) as usize;
            for i in 0..count {
                let dx = i as f32 * spacing;
                if dx + thickness <= width + 0.001 {
                    rects[n] = [x0 + dx, y0, x0 + dx + thickness, y1];
                    n += 1;
                }
            }
        }
        UnderlineStyle::Dashed => {
            // Dash/cap period = cw; phase = x0 mod cw keeps dashes
            // continuous across cell boundaries (PLAN_v1113 §3.1).
            let period = cw;
            let dash = cw * 0.6;
            let phase = x0.rem_euclid(period);
            let mut dash_x = if phase <= dash {
                x0 - phase
            } else {
                x0 - phase + period
            };
            let end = x0 + width;
            while dash_x < end && n < MAX_UNDERLINE_RECTS {
                let dashend = (dash_x + dash).min(end);
                // Clip to the cell: the same global dash is also emitted by
                // the left neighbor — each cell draws only its own slice.
                let clip_start = dash_x.max(x0);
                rects[n] = [clip_start, y0, dashend, y1];
                n += 1;
                dash_x += period;
            }
        }
    }
    (rects, n)
}

/// v1.11.3 (PLAN_v1113 §3.2): origin-level bold→bright substitution.
///
/// Runs BEFORE `resolve_cell_color`: palette index < 8 with the BOLD flag
/// and the `[compat] bold_is_bright` switch ON becomes the bright variant
/// (palette[i+8]). Rgb colors and palette indexes ≥ 8 pass through
/// untouched. Applied to the fg origin only — REVERSE swaps the resolved
/// colors afterwards, so a bold+reverse cell shows a bright background bar
/// (the original fg) exactly like xterm.
#[must_use]
pub(crate) fn bold_to_bright_origin(cc: CellColor, flags: CellFlags, enabled: bool) -> CellColor {
    match cc {
        CellColor::Palette(i) if enabled && flags.contains(CellFlags::BOLD) && i < 8 => {
            CellColor::Palette(i + 8)
        }
        other => other,
    }
}

/// v1.11.3 (PLAN_v1113 §3.2): resolve the underline decoration color —
/// the SGR 58 origin (DIM×0.5 applied like text, NO contrast boost —
/// application-owned), or the caller's text color fallback.
#[must_use]
pub(crate) fn underline_color(
    origin: Option<CellColor>,
    flags: CellFlags,
    default: [f32; 4],
    palette: &[Color; 256],
    fallback: [f32; 4],
) -> [f32; 4] {
    match origin {
        Some(origin) => {
            let c = resolve_cell_color(origin, default, palette);
            if flags.contains(CellFlags::DIM) {
                crate::paint::color_math::dim_half(c)
            } else {
                c
            }
        }
        None => fallback,
    }
}

#[cfg(test)]
mod underline_rects_tests {
    use super::*;

    const T: f32 = 2.0; // standard thickness
    const CW: f32 = 10.0; // standard cell width

    fn rects(style: UnderlineStyle, x0: f32, width: f32, cw: f32) -> ([[f32; 4]; 8], usize) {
        underline_rects(style, x0, width, 20.0, T, cw, 0)
    }

    #[test]
    fn single_is_one_solid_bar_at_baseline() {
        let (r, n) = rects(UnderlineStyle::Single, 0.0, CW, CW);
        assert_eq!(n, 1);
        assert_eq!(r[0], [0.0, 20.0, CW, 22.0]);
    }

    #[test]
    fn single_wide_cell_stretches_proportionally() {
        let (r, n) = rects(UnderlineStyle::Single, 10.0, 2.0 * CW, CW);
        assert_eq!(n, 1);
        assert_eq!(r[0], [10.0, 20.0, 10.0 + 2.0 * CW, 22.0]);
    }

    #[test]
    fn double_second_line_anchored_at_minus_3_relative_to_first() {
        let (r, n) = rects(UnderlineStyle::Double, 0.0, CW, CW);
        assert_eq!(n, 2);
        assert_eq!(r[0], [0.0, 20.0, CW, 22.0]);
        // contrast_tests.rs regression anchor: upper line 3px above the lower.
        assert_eq!(r[1], [0.0, 17.0, CW, 19.0]);
        assert!((r[1][1] - (r[0][1] - 3.0)).abs() < 1e-4);
    }

    #[test]
    fn double_wide_cell_keeps_both_lines() {
        let (r, n) = rects(UnderlineStyle::Double, 5.0, 2.0 * CW, CW);
        assert_eq!(n, 2);
        assert_eq!(r[0], [5.0, 20.0, 5.0 + 2.0 * CW, 22.0]);
        assert_eq!(r[1][1], 17.0);
    }

    #[test]
    fn wavy_standard_cell_emits_8_step_quads_covering_the_cell() {
        let (r, n) = rects(UnderlineStyle::Wavy, 0.0, CW, CW);
        assert_eq!(n, 8, "2 complete periods = 8 quads");
        let seg = CW / 8.0;
        for (i, q) in r.iter().take(8).enumerate() {
            assert!((q[0] - i as f32 * seg).abs() < 1e-4, "quad {i} x0");
            assert!((q[2] - (i as f32 + 1.0) * seg).abs() < 1e-4, "quad {i} x1");
            let level = if (i / 2) % 2 == 0 { -1.0 } else { 1.0 };
            assert!((q[1] - (20.0 + level)).abs() < 1e-4, "quad {i} y0");
            assert!((q[3] - (22.0 + level)).abs() < 1e-4, "quad {i} y1");
        }
    }

    #[test]
    fn wavy_narrow_cell_degrades_to_4_quads() {
        // cw = 4 → p = max(2,3) = 3 → width 4 < 4p → single period.
        let (r, n) = rects(UnderlineStyle::Wavy, 0.0, 4.0, 4.0);
        assert_eq!(n, 4, "narrow cell degrades to one period");
        let seg = 4.0 / 4.0;
        for (i, q) in r.iter().take(4).enumerate() {
            assert!((q[0] - i as f32 * seg).abs() < 1e-4);
            let level = if (i / 2) % 2 == 0 { -1.0 } else { 1.0 };
            assert!((q[1] - (20.0 + level)).abs() < 1e-4);
        }
    }

    #[test]
    fn wavy_wide_cell_still_8_quads_full_coverage() {
        let (r, n) = rects(UnderlineStyle::Wavy, 3.0, 2.0 * CW, CW);
        assert_eq!(n, 8);
        assert!((r[0][0] - 3.0).abs() < 1e-4);
        assert!((r[7][2] - (3.0 + 2.0 * CW)).abs() < 1e-4);
    }

    #[test]
    fn dotted_emits_thickness_sized_dots_spaced_cw_over_3() {
        let (r, n) = rects(UnderlineStyle::Dotted, 0.0, CW, CW);
        assert!((2..=3).contains(&n), "2-3 dots on a standard cell, got {n}");
        let spacing = CW / 3.0;
        for (i, q) in r.iter().take(n).enumerate() {
            assert!((q[1] - 20.0).abs() < 1e-4, "dot {i} on the baseline");
            assert!((q[3] - q[1]).abs() - T < 1e-4, "dot {i} size = thickness");
            assert!((q[0] - i as f32 * spacing).abs() < 1e-4, "dot {i} position");
        }
    }

    #[test]
    fn dotted_narrow_cell_keeps_at_least_one_dot() {
        let (r, n) = rects(UnderlineStyle::Dotted, 0.0, 3.0, 3.0);
        assert!((1..=3).contains(&n), "narrow cell still gets dots, got {n}");
        for q in r.iter().take(n) {
            assert!(q[2] <= 3.0 + 1e-3, "dots clipped to the cell");
        }
    }

    #[test]
    fn dashed_uses_dash_and_gap_proportions() {
        let (r, n) = rects(UnderlineStyle::Dashed, 0.0, CW, CW);
        assert_eq!(n, 1, "phase 0: exactly one dash per cell");
        assert!((r[0][2] - r[0][0] - CW * 0.6).abs() < 1e-4, "dash = 0.6cw");
        assert!((r[0][1] - 20.0).abs() < 1e-4);
    }

    #[test]
    fn dashed_wide_cell_emits_two_dashes_continuous() {
        let (r, n) = rects(UnderlineStyle::Dashed, 0.0, 2.0 * CW, CW);
        assert_eq!(n, 2, "two cells → two dashes");
        assert!((r[0][0] - 0.0).abs() < 1e-4);
        assert!((r[0][2] - CW * 0.6).abs() < 1e-4);
        assert!((r[1][0] - CW).abs() < 1e-4, "dash 2 starts after the gap");
        assert!((r[1][2] - (CW + CW * 0.6)).abs() < 1e-4);
    }

    #[test]
    fn dashed_phase_continues_across_cell_boundaries() {
        // A dash sequence started mid-gap must resume at the same absolute
        // phase: x0 = 25 lands inside cell 2's dash (k=2 → [20, 26]) — the
        // clipped remainder must equal it in both standalone and wide calls.
        let (r_a, _) = underline_rects(UnderlineStyle::Dashed, 25.0, 5.0, 20.0, T, CW, 0);
        let (r_b, _) = underline_rects(UnderlineStyle::Dashed, 20.0, 10.0, 20.0, T, CW, 0);
        // Both geometries land on the SAME global dash [20, 26].
        assert!((r_a[0][0] - 25.0).abs() < 1e-4, "clip to cell start");
        assert!((r_a[0][2] - 26.0).abs() < 1e-4, "global phase from x0");
        assert!((r_b[0][0] - 20.0).abs() < 1e-4);
        assert!((r_b[0][2] - 26.0).abs() < 1e-4);
    }

    #[test]
    fn dashed_offset_phase_starts_in_gap_shifts_first_dash() {
        // x0 = 8 → phase 8 ∈ (6, 10): in the gap → first dash at x0+2.
        let (r, n) = rects(UnderlineStyle::Dashed, 8.0, 10.0, CW);
        assert_eq!(n, 1);
        assert!((r[0][0] - 10.0).abs() < 1e-4, "dash starts after the gap");
        assert!((r[0][2] - 16.0).abs() < 1e-4);
    }

    #[test]
    fn all_styles_never_exceed_8_rects() {
        for style in [
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Wavy,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            let (_, n) = underline_rects(style, 0.0, 80.0, 20.0, T, CW, 0);
            assert!(n <= MAX_UNDERLINE_RECTS, "{style:?}: {n}");
        }
    }

    #[test]
    fn dashed_respects_absolute_phase_under_thickness_variation() {
        let (r, n) = underline_rects(UnderlineStyle::Dashed, 0.0, CW, 20.0, 3.0, CW, 0);
        assert_eq!(n, 1);
        assert_eq!(r[0][3], 23.0, "thickness parameter propagates");
    }
}

#[cfg(test)]
mod bold_to_bright_tests {
    use super::*;

    fn palette(i: u8) -> CellColor {
        CellColor::Palette(i)
    }

    #[test]
    fn off_identity_for_all_origins() {
        assert_eq!(
            bold_to_bright_origin(palette(1), CellFlags::BOLD, false),
            palette(1)
        );
        assert_eq!(
            bold_to_bright_origin(CellColor::Rgb(Color::rgb(1, 2, 3)), CellFlags::BOLD, false),
            CellColor::Rgb(Color::rgb(1, 2, 3))
        );
    }

    #[test]
    fn on_maps_only_bold_low_ansi_to_bright() {
        assert_eq!(
            bold_to_bright_origin(palette(1), CellFlags::BOLD, true),
            palette(9)
        );
        assert_eq!(
            bold_to_bright_origin(palette(7), CellFlags::BOLD, true),
            palette(15)
        );
        // Non-bold stays.
        assert_eq!(
            bold_to_bright_origin(palette(1), CellFlags::empty(), true),
            palette(1)
        );
        // High indexes and Rgb untouched.
        assert_eq!(
            bold_to_bright_origin(palette(8), CellFlags::BOLD, true),
            palette(8)
        );
        assert_eq!(
            bold_to_bright_origin(palette(196), CellFlags::BOLD, true),
            palette(196)
        );
        assert_eq!(
            bold_to_bright_origin(CellColor::Rgb(Color::rgb(4, 5, 6)), CellFlags::BOLD, true),
            CellColor::Rgb(Color::rgb(4, 5, 6))
        );
    }
}
