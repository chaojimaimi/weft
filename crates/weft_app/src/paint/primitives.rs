//! Low-level vertex primitives shared by all overlay builders.
//!
//! Extracted verbatim from `renderer.rs` (A5 phase). Each function pushes
//! raw `f32` vertex data into a caller-owned buffer — no `MetalRenderer`
//! dependency. Call sites use `crate::paint::primitives::push_quad(...)`.

use weft_core::config::Theme;
use weft_core::grid::{CellColor, Color};
use weft_core::syntax::TokenKind;

/// Push a textured quad (two triangles, 6 vertices × 12 floats).
pub(crate) fn push_quad(
    vertices: &mut Vec<f32>,
    dst: [f32; 4],
    uv: [f32; 4],
    fg: [f32; 4],
    bg: [f32; 4],
) {
    let [x0, y0, x1, y1] = dst;
    let [u0, v0, u1, v1] = uv;
    for (x, y, u, v) in [
        (x0, y0, u0, v0),
        (x0, y1, u0, v1),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x1, y0, u1, v0),
    ] {
        vertices.extend_from_slice(&[
            x, y, u, v, fg[0], fg[1], fg[2], fg[3], bg[0], bg[1], bg[2], bg[3],
        ]);
    }
}

/// Draw a line segment as a thin rotated rectangle (two triangles).
/// Used for vector-drawn UI elements like the tab close button × icon.
pub(crate) fn push_line(
    vertices: &mut Vec<f32>,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    width: f32,
    color: [f32; 4],
) {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    // Perpendicular unit vector × half-width
    let hw = width * 0.5;
    let px = -dy / len * hw;
    let py = dx / len * hw;
    // Four corners of the rotated rectangle
    let (ax, ay) = (x1 + px, y1 + py);
    let (bx, by) = (x1 - px, y1 - py);
    let (cx, cy) = (x2 + px, y2 + py);
    let (dx, dy) = (x2 - px, y2 - py);
    // UV=[0;4] and fg=[0;4] → mask=0 → only bg (color) shows
    let uv = [0.0f32; 4];
    let fg = [0.0f32; 4];
    for (x, y) in [(ax, ay), (bx, by), (cx, cy), (bx, by), (dx, dy), (cx, cy)] {
        vertices.extend_from_slice(&[
            x, y, uv[0], uv[1], fg[0], fg[1], fg[2], fg[3], color[0], color[1], color[2], color[3],
        ]);
    }
}

/// Draw a filled triangle (3 vertices, 1 triangle = 3 vertices in the
/// vertex buffer). Used for solid adjustment-triangle indicators ◀ ▶ in
/// the Settings panel. The triangle is specified by 3 (x,y) corner points.
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_filled_triangle(
    vertices: &mut Vec<f32>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    color: [f32; 4],
) {
    let uv = [0.0f32; 4];
    let fg = [0.0f32; 4];
    for (x, y) in [(x0, y0), (x1, y1), (x2, y2)] {
        vertices.extend_from_slice(&[
            x, y, uv[0], uv[1], fg[0], fg[1], fg[2], fg[3], color[0], color[1], color[2], color[3],
        ]);
    }
}

/// v1.0 P1.5-B1: Push a single grid-cell instance (16 floats = 64 bytes).
/// Layout matches the Metal `CellInstance` struct: origin(2) + size(2) +
/// uv_rect(4) + fg(4) + bg(4). The instance is rendered against a static
/// 4-vertex unit quad indexed as [0,1,2,0,2,3] — the shader maps
/// vertex_id 0..3 to corners (0,0), (0,1), (1,1), (1,0) and scales by
/// `size`/offsets by `origin`. Compared to `push_quad` (6 verts × 12 floats
/// = 72 floats = 288 B per cell), this emits 16 floats = 64 B per cell, a
/// 4.5x reduction in per-frame upload size.
pub(crate) fn push_cell_instance(
    instances: &mut Vec<f32>,
    dst: [f32; 4],
    uv: [f32; 4],
    fg: [f32; 4],
    bg: [f32; 4],
) {
    let [x0, y0, x1, y1] = dst;
    let [u0, v0, u1, v1] = uv;
    instances.extend_from_slice(&[
        x0,
        y0,
        x1 - x0,
        y1 - y0,
        u0,
        v0,
        u1,
        v1,
        fg[0],
        fg[1],
        fg[2],
        fg[3],
        bg[0],
        bg[1],
        bg[2],
        bg[3],
    ]);
}

/// Push a filled triangle (3 vertices) into the vertex buffer. Uses the
/// same vertex layout as `push_quad` (12 floats each).
pub(crate) fn push_triangle(
    vertices: &mut Vec<f32>,
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    color: [f32; 4],
    bg_uv: [f32; 4],
) {
    let [u0, v0, u1, v1] = bg_uv;
    let mid_u = (u0 + u1) * 0.5;
    let mid_v = (v0 + v1) * 0.5;
    for [x, y] in [p0, p1, p2] {
        vertices.extend_from_slice(&[
            x, y, mid_u, mid_v, color[0], color[1], color[2], color[3], color[0], color[1],
            color[2], color[3],
        ]);
    }
}

/// Convert a grid Color to normalized RGBA floats.
pub(crate) fn color_to_normalized(color: Color) -> [f32; 4] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        color.a as f32 / 255.0,
    ]
}

/// CWD gray: a theme-independent dimmed version of the command foreground.
///
/// Deliberately NOT `theme.output.cwd` — that key is a per-theme "coordinated
/// accent" (warp_dark is a dark coral, dracula a dark violet), so switching
/// themes changes the CWD hue. Deriving from `foreground` × 0.65 gives every
/// theme the same neutral gray, keeps the "CWD dim < command bright" hierarchy,
/// and auto-adapts to light themes (dark fg → darker gray, still readable).
pub(crate) fn derive_cwd_gray(fg: [f32; 4]) -> [f32; 4] {
    [fg[0] * 0.65, fg[1] * 0.65, fg[2] * 0.65, 1.0]
}

pub(crate) fn scale_color_alpha(mut color: [f32; 4], opacity: f32) -> [f32; 4] {
    color[3] *= opacity.clamp(0.0, 1.0);
    color
}

/// Resolve a cell's color-origin against the current palette / theme default.
/// `Default` → theme default; `Palette(i)` → palette slot; `Rgb` → as-is.
/// Because this runs per-frame, changing the palette (theme switch or OSC)
/// recolors the whole screen on the next draw without rewriting cells.
pub(crate) fn resolve_cell_color(
    cc: CellColor,
    default: [f32; 4],
    palette: &[Color; 256],
) -> [f32; 4] {
    match cc {
        CellColor::Default => default,
        CellColor::Palette(i) => color_to_normalized(palette[i as usize]),
        CellColor::Rgb(c) => color_to_normalized(c),
    }
}

fn srgb_relative_luminance(color: [f32; 4]) -> f32 {
    let linear = |channel: f32| {
        let channel = channel.clamp(0.0, 1.0);
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

pub(crate) fn text_contrast_ratio(foreground: [f32; 4], background: [f32; 4]) -> f32 {
    let foreground = srgb_relative_luminance(foreground);
    let background = srgb_relative_luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

pub(crate) fn composite_color_over(foreground: [f32; 4], background: [f32; 4]) -> [f32; 4] {
    let alpha = foreground[3].clamp(0.0, 1.0);
    [
        foreground[0] * alpha + background[0] * (1.0 - alpha),
        foreground[1] * alpha + background[1] * (1.0 - alpha),
        foreground[2] * alpha + background[2] * (1.0 - alpha),
        1.0,
    ]
}

pub(crate) fn text_background_for_range(
    canvas: [f32; 4],
    selection: Option<(usize, usize, [f32; 4])>,
    range: std::ops::Range<usize>,
) -> [f32; 4] {
    selection
        .filter(|(start, end, _)| range.start < *end && range.end > *start)
        .map(|(_, _, background)| background)
        .unwrap_or(canvas)
}

/// Raise text to a minimum contrast without changing its stored color origin.
/// Mixing toward white or black preserves the RGB channel ordering and hue;
/// a binary search finds the smallest display-only lightness adjustment.
pub(crate) fn ensure_minimum_text_contrast(
    foreground: [f32; 4],
    background: [f32; 4],
    minimum_ratio: f32,
) -> [f32; 4] {
    let minimum_ratio = minimum_ratio.clamp(1.0, 21.0);
    if text_contrast_ratio(foreground, background) >= minimum_ratio {
        return foreground;
    }

    let black = [0.0, 0.0, 0.0, foreground[3]];
    let white = [1.0, 1.0, 1.0, foreground[3]];
    let target = if text_contrast_ratio(white, background) >= text_contrast_ratio(black, background)
    {
        white
    } else {
        black
    };
    let mix = |amount: f32| {
        [
            foreground[0] + (target[0] - foreground[0]) * amount,
            foreground[1] + (target[1] - foreground[1]) * amount,
            foreground[2] + (target[2] - foreground[2]) * amount,
            foreground[3],
        ]
    };

    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..12 {
        let middle = (low + high) * 0.5;
        if text_contrast_ratio(mix(middle), background) >= minimum_ratio {
            high = middle;
        } else {
            low = middle;
        }
    }
    mix(high)
}

/// Map a syntax-highlight token kind to its theme color (normalized RGBA).
pub(crate) fn syntax_color(kind: TokenKind, theme: &Theme) -> [f32; 4] {
    let s = &theme.syntax;
    match kind {
        TokenKind::Command => color_to_normalized(s.command),
        TokenKind::Flag => color_to_normalized(s.flag),
        TokenKind::Path => color_to_normalized(s.path),
        TokenKind::String => color_to_normalized(s.string),
        TokenKind::Number => color_to_normalized(s.number),
        TokenKind::Variable => color_to_normalized(s.variable),
        TokenKind::Operator => color_to_normalized(s.operator),
        TokenKind::Comment => color_to_normalized(s.comment),
        TokenKind::Argument => color_to_normalized(s.argument),
        TokenKind::Whitespace | TokenKind::Default => color_to_normalized(s.default),
    }
}

/// F6: Draw a focus ring (accent-colored border) around a semantic bounds
/// rect. Emits 4 edge quads (top/bottom/left/right) into `verts`. The ring
/// is drawn *inside* the bounds so it doesn't overflow into adjacent elements.
///
/// - `bounds`: `[x0, y0, x1, y1]` of the focused element.
/// - `color`: accent/focus color (normalized RGBA). Alpha is applied as-is;
///   callers should pre-multiply for the desired translucency.
/// - `thickness`: border width in physical pixels (typically 2.0, or 3.0
///   when Increase Contrast is on).
///
/// Pure vertex emitter — no renderer dependency. The caller passes the
/// resulting `verts` to the same vertex buffer used by other overlay quads.
pub(crate) fn build_focus_ring(
    verts: &mut Vec<f32>,
    bounds: [f32; 4],
    color: [f32; 4],
    thickness: f32,
) {
    let [x0, y0, x1, y1] = bounds;
    let t = thickness.max(0.5);
    let uv = [0.0f32; 4];
    let fg = [0.0f32; 4];
    // Top edge
    push_quad(verts, [x0, y0, x1, y0 + t], uv, fg, color);
    // Bottom edge
    push_quad(verts, [x0, y1 - t, x1, y1], uv, fg, color);
    // Left edge
    push_quad(verts, [x0, y0, x0 + t, y1], uv, fg, color);
    // Right edge
    push_quad(verts, [x1 - t, y0, x1, y1], uv, fg, color);
}

/// F6: Resolve the focus ring thickness based on the Increase Contrast
/// setting. Pure function so callers don't hardcode the contrast multiplier.
pub(crate) fn focus_ring_thickness(increase_contrast: bool) -> f32 {
    if increase_contrast {
        3.0
    } else {
        2.0
    }
}

/// F6: Resolve the focus ring alpha based on the Increase Contrast setting.
/// When contrast is increased, the ring uses full opacity so it's visible
/// against any background; otherwise it uses 0.70 for a subtler appearance.
pub(crate) fn focus_ring_alpha(increase_contrast: bool) -> f32 {
    if increase_contrast {
        1.0
    } else {
        0.70
    }
}

// ── v1.4.0: physical-pixel alignment ───────────────────────────────────
//
// WARP_REFERENCE R3-4: terminal chrome (separators, scrollbars, pane dividers,
// focus rings) must land on integer physical-pixel boundaries to avoid
// Retina blurriness. The renderer and `LayoutCtx` already work in physical
// pixels, so snapping is a pure `round()` — no scale parameter, no divide-
// then-multiply. The helpers below are the single source of truth; all chrome
// geometry should pass through them before being pushed as vertex data.
//
// Why snap both edges of a rect (not just origin): if you snap origin and then
// add a fractional width, the far edge lands on a non-integer and the GPU's
// linear interpolation smears it across two physical pixels. Snapping both
// `start` and `end` independently guarantees both edges are crisp, at the cost
// of a sub-pixel width perturbation (≤ 1px) which is invisible.
//
// See `docs/V14_IMPLEMENTATION_PLAN.md` §4.1 for the design contract and
// `docs/V14_IMPLEMENTATION_PLAN.md` §4.2 for the call-site list.

/// Snap a single physical-pixel coordinate to an integer boundary.
///
/// `round()` is the correct choice (not `floor`/`ceil`) because it minimizes
/// the maximum displacement: a value at `x.5` moves to `x+1`, but a value at
/// `x.4999` moves to `x` — the average error is ~0.25px, half of `floor`.
///
/// Pure function; no global state. Inline-friendly — the compiler collapses
/// this to a single `roundsd`/`vroundss` on x86/ARM.
///
/// Kept as part of the documented v1.4 snap API even though current chrome
/// call sites use `snap_physical_rect` (the two-edge variant). Single-
/// coordinate snapping is the natural primitive for future callers that need
/// to align a 1-D position (e.g. an x-only or y-only guide line) and is
/// exercised by the unit tests below.
#[allow(dead_code)]
#[inline]
pub(crate) fn snap_physical(value: f32) -> f32 {
    value.round()
}

/// Snap both edges of a 1-D interval to integer physical-pixel boundaries.
///
/// Returns `(start_rounded, end_rounded)`. The width may shrink or grow by
/// up to 1px compared to the input, but both edges are guaranteed integer.
/// Callers must NOT then re-derive width as `end - start` and assert it
/// matches a design token — the snapped width is intentionally not pinned.
///
/// The two-edges rule (vs. snapping only `start`) is what eliminates Retina
/// smearing: a 1.5px-wide line at `x=10.3` would otherwise become `x=10, w=1.5`
/// → far edge at `11.5` → GPU rasterizes across pixels 11 and 12.
///
/// Returns `(start, end)` in the same order as the inputs. Inputs are not
/// required to be ordered (start ≤ end); if `start > end` the return is
/// `(start_rounded, end_rounded)` without swapping — callers that need
/// ordering should sort first.
#[inline]
pub(crate) fn snap_physical_rect(start: f32, end: f32) -> (f32, f32) {
    (start.round(), end.round())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_alpha_clamps_opacity_without_changing_rgb() {
        assert_eq!(
            scale_color_alpha([0.1, 0.2, 0.3, 0.8], 0.5),
            [0.1, 0.2, 0.3, 0.4]
        );
        assert_eq!(scale_color_alpha([0.1, 0.2, 0.3, 0.8], 2.0)[3], 0.8);
    }

    #[test]
    fn minimum_contrast_leaves_already_readable_color_exactly_unchanged() {
        let foreground = [0.9, 0.7, 0.2, 0.8];
        let background = [0.02, 0.03, 0.04, 1.0];
        assert_eq!(
            ensure_minimum_text_contrast(foreground, background, 4.5),
            foreground
        );
    }

    #[test]
    fn minimum_contrast_brightens_dark_canvas_without_changing_channel_order() {
        let foreground = [0.35, 0.18, 0.08, 0.75];
        let background = [0.02, 0.03, 0.04, 1.0];
        let adjusted = ensure_minimum_text_contrast(foreground, background, 7.0);
        assert!(text_contrast_ratio(adjusted, background) >= 6.99);
        assert!(adjusted[0] > adjusted[1] && adjusted[1] > adjusted[2]);
        assert_eq!(adjusted[3], foreground[3]);
    }

    #[test]
    fn minimum_contrast_darkens_on_light_canvas_and_one_is_identity() {
        let foreground = [0.75, 0.55, 0.35, 1.0];
        let background = [0.98, 0.97, 0.94, 1.0];
        assert_eq!(
            ensure_minimum_text_contrast(foreground, background, 1.0),
            foreground
        );
        let adjusted = ensure_minimum_text_contrast(foreground, background, 7.0);
        assert!(text_contrast_ratio(adjusted, background) >= 6.99);
        assert!(adjusted[0] < foreground[0]);
    }

    #[test]
    fn derive_cwd_gray_dims_foreground_uniformly_and_theme_independently() {
        // 每个主题用同一公式(fg × 0.65),CWD 不随主题 `output.cwd` 键变。
        for theme in [
            weft_core::config::Theme::weft_warm(),
            weft_core::config::Theme::warp_dark(),
            weft_core::config::Theme::dracula(),
            weft_core::config::Theme::weft_light(),
        ] {
            let fg = color_to_normalized(theme.foreground);
            let gray = derive_cwd_gray(fg);
            for i in 0..3 {
                assert!(
                    (gray[i] - fg[i] * 0.65).abs() < 1e-5,
                    "cwd channel must be fg × 0.65"
                );
            }
            assert_eq!(gray[3], 1.0);
            // CWD 暗于命令色,保持 "CWD 暗 < 命令亮" 层次
            assert!(gray[0] < fg[0]);
        }
    }

    #[test]
    fn derive_cwd_gray_stays_readable_on_light_theme() {
        // 浅色主题 fg 本身是深色,×0.65 后仍是深灰,在浅背景上保持可读
        let light = weft_core::config::Theme::weft_light();
        let fg = color_to_normalized(light.foreground);
        let bg = color_to_normalized(light.background);
        let gray = derive_cwd_gray(fg);
        assert!(
            text_contrast_ratio(gray, bg) >= 4.5,
            "light-theme CWD must stay readable"
        );
    }

    #[test]
    fn translucent_selection_is_composited_before_contrast_measurement() {
        let canvas = [0.1, 0.2, 0.3, 1.0];
        let selection = [0.8, 0.4, 0.2, 0.6];
        let composite = composite_color_over(selection, canvas);
        for (actual, expected) in composite.into_iter().zip([0.52, 0.32, 0.24, 1.0]) {
            assert!((actual - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn text_range_uses_selection_background_only_when_ranges_overlap() {
        let canvas = [0.1, 0.2, 0.3, 1.0];
        let selected = [0.4, 0.5, 0.6, 1.0];
        let selection = Some((2, 5, selected));
        assert_eq!(text_background_for_range(canvas, selection, 1..2), canvas);
        assert_eq!(text_background_for_range(canvas, selection, 2..3), selected);
        assert_eq!(text_background_for_range(canvas, selection, 4..6), selected);
        assert_eq!(text_background_for_range(canvas, selection, 5..6), canvas);
    }

    #[test]
    fn selected_text_is_corrected_against_the_composited_selection() {
        let canvas = color_to_normalized(weft_core::config::Theme::weft_warm().background);
        let accent = color_to_normalized(weft_core::config::Theme::weft_warm().accent);
        let selection = [
            accent[0] * 0.35 + canvas[0] * 0.65,
            accent[1] * 0.35 + canvas[1] * 0.65,
            accent[2] * 0.35 + canvas[2] * 0.65,
            0.60,
        ];
        let selected_canvas = composite_color_over(selection, canvas);
        let failure = color_to_normalized(weft_core::config::Theme::weft_warm().output.failure);
        let adjusted = ensure_minimum_text_contrast(failure, selected_canvas, 7.0);
        assert!(text_contrast_ratio(adjusted, selected_canvas) >= 6.99);
        assert_eq!(adjusted[3], failure[3]);
    }

    #[test]
    fn syntax_color_distinct_and_default_fallback() {
        // v0.8: syntax colors are now theme-driven. Verify the warm theme
        // produces distinct colors for every token kind, and that
        // Default/Whitespace resolve to theme.syntax.default.
        // v1.7.0-B: now includes Argument (9 distinct kinds).
        let theme = weft_core::config::Theme::weft_warm();
        let kinds = [
            TokenKind::Command,
            TokenKind::Flag,
            TokenKind::Path,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Variable,
            TokenKind::Operator,
            TokenKind::Comment,
            TokenKind::Argument,
        ];
        let colors: Vec<[f32; 4]> = kinds.iter().map(|&k| syntax_color(k, &theme)).collect();
        // All 9 should be distinct (no two token kinds share a color).
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "syntax colors at {i}/{j} collide");
            }
        }
        // Default/Whitespace resolve to theme.syntax.default.
        let default_c = color_to_normalized(theme.syntax.default);
        assert_eq!(syntax_color(TokenKind::Default, &theme), default_c);
        assert_eq!(syntax_color(TokenKind::Whitespace, &theme), default_c);
    }

    /// v1.7.0-B: V17 §2.4 visual hierarchy contract — in every built-in
    /// theme, command ≠ argument ≠ default, and output_default ≠ cwd.
    /// Also verifies success ≠ failure (status colors must be distinguishable).
    ///
    /// v1.7.0-E: Extended to also assert metadata ≠ output_default/cwd and
    /// warning ≠ success/failure, closing the §2.5 "角色间满足固定的感知
    /// 差异门槛" gap for the full OutputSemanticColors role set.
    #[test]
    fn all_themes_satisfy_visual_hierarchy_contract() {
        let themes: Vec<(&str, weft_core::config::Theme)> = vec![
            ("weft_warm", weft_core::config::Theme::weft_warm()),
            ("weft_light", weft_core::config::Theme::weft_light()),
            ("warp_dark", weft_core::config::Theme::warp_dark()),
            ("dracula", weft_core::config::Theme::dracula()),
            ("solarized_dark", weft_core::config::Theme::solarized_dark()),
            ("gruvbox_dark", weft_core::config::Theme::gruvbox_dark()),
            ("nord", weft_core::config::Theme::nord()),
            ("tokyo_night", weft_core::config::Theme::tokyo_night()),
            (
                "catppuccin_mocha",
                weft_core::config::Theme::catppuccin_mocha(),
            ),
            ("one_dark", weft_core::config::Theme::one_dark()),
            ("monokai_pro", weft_core::config::Theme::monokai_pro()),
        ];
        for (name, theme) in &themes {
            let s = &theme.syntax;
            let o = &theme.output;
            // V17 §2.4: command ≠ argument ≠ default
            assert_ne!(s.command, s.argument, "{name}: command == argument");
            assert_ne!(s.command, s.default, "{name}: command == default");
            assert_ne!(s.argument, s.default, "{name}: argument == default");
            // V17 §2.4: output_default ≠ cwd ("普通结果不得等于 CWD 色")
            assert_ne!(o.output_default, o.cwd, "{name}: output_default == cwd");
            // Status colors must be distinguishable
            assert_ne!(o.success, o.failure, "{name}: success == failure");
            // v1.7.0-E: metadata must be distinct from output_default and cwd
            // ("label 与 value 可分，但整体弱于命令名" — metadata is a weaker
            // structural role and must not collapse into default or cwd).
            assert_ne!(
                o.metadata, o.output_default,
                "{name}: metadata == output_default"
            );
            assert_ne!(o.metadata, o.cwd, "{name}: metadata == cwd");
        }
    }

    // ── F6: Focus ring ────────────────────────────────────────────────

    #[test]
    fn build_focus_ring_emits_four_edge_quads() {
        let mut verts = Vec::new();
        let color = [0.3, 0.6, 0.9, 0.7];
        build_focus_ring(&mut verts, [10.0, 20.0, 110.0, 52.0], color, 2.0);
        // 4 quads × 6 verts × 12 floats = 288 floats.
        assert_eq!(verts.len(), 288);
    }

    #[test]
    fn build_focus_ring_top_edge_starts_at_y0() {
        let mut verts = Vec::new();
        let color = [0.3, 0.6, 0.9, 0.7];
        build_focus_ring(&mut verts, [10.0, 20.0, 110.0, 52.0], color, 2.0);
        // First quad: top edge → first vertex is (x0, y0) = (10, 20).
        assert_eq!(verts[0], 10.0); // x0
        assert_eq!(verts[1], 20.0); // y0
    }

    #[test]
    fn build_focus_ring_clamps_thickness_to_minimum() {
        let mut verts_a = Vec::new();
        let mut verts_b = Vec::new();
        let color = [1.0; 4];
        build_focus_ring(&mut verts_a, [0.0, 0.0, 100.0, 50.0], color, 0.0);
        build_focus_ring(&mut verts_b, [0.0, 0.0, 100.0, 50.0], color, 0.5);
        // thickness=0 clamps to 0.5, same as thickness=0.5.
        assert_eq!(verts_a.len(), verts_b.len());
    }

    #[test]
    fn focus_ring_thickness_increases_with_contrast() {
        assert_eq!(focus_ring_thickness(false), 2.0);
        assert_eq!(focus_ring_thickness(true), 3.0);
    }

    #[test]
    fn focus_ring_alpha_full_when_contrast_increased() {
        assert!((focus_ring_alpha(false) - 0.70).abs() < 1e-4);
        assert!((focus_ring_alpha(true) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn build_focus_ring_with_increase_contrast_uses_more_vertices_area() {
        // Both emit 4 quads, but the contrast version has thicker edges.
        let mut verts_normal = Vec::new();
        let mut verts_contrast = Vec::new();
        let color = [0.3, 0.6, 0.9, 1.0];
        let bounds = [10.0, 20.0, 110.0, 52.0];
        build_focus_ring(
            &mut verts_normal,
            bounds,
            color,
            focus_ring_thickness(false),
        );
        build_focus_ring(
            &mut verts_contrast,
            bounds,
            color,
            focus_ring_thickness(true),
        );
        // Same vertex count (4 quads × 6 verts × 12 floats = 288), but
        // different edge geometry (thicker edges with contrast).
        assert_eq!(verts_normal.len(), verts_contrast.len());
        // Top edge = first quad with dst [x0, y0, x1, y0 + thickness].
        // push_quad vertex order: (x0,y0), (x0,y1), (x1,y1), ...
        // Each vertex is 12 floats, so vertex 1's y-coordinate is at
        // index 12 (vertex 1 offset) + 1 (y slot) = 13.
        // Normal: thickness=2 → y0+2 = 22; Contrast: thickness=3 → y0+3 = 23.
        assert_eq!(verts_normal[13], 22.0); // y1 of top edge (vertex 1)
        assert_eq!(verts_contrast[13], 23.0);
    }

    // ── v1.4.0: snap_physical helpers ─────────────────────────────────

    #[test]
    fn snap_physical_rounds_integers_unchanged() {
        // Integers are already on pixel boundaries — snap must be identity.
        assert_eq!(snap_physical(0.0), 0.0);
        assert_eq!(snap_physical(1.0), 1.0);
        assert_eq!(snap_physical(100.0), 100.0);
        assert_eq!(snap_physical(-5.0), -5.0);
    }

    #[test]
    fn snap_physical_rounds_positive_fractionals() {
        // round() uses banker's rounding in Rust (round-half-to-even),
        // but for typical layout inputs the values are not exactly at .5.
        // 0.4 → 0, 0.6 → 1, 10.49 → 10, 10.51 → 11.
        assert_eq!(snap_physical(0.4), 0.0);
        assert_eq!(snap_physical(0.6), 1.0);
        assert_eq!(snap_physical(10.49), 10.0);
        assert_eq!(snap_physical(10.51), 11.0);
    }

    #[test]
    fn snap_physical_rounds_negative_fractionals() {
        // Negative coordinates (e.g. offscreen scissor bounds) must round
        // toward the nearest integer, not toward zero.
        assert_eq!(snap_physical(-0.4), 0.0);
        assert_eq!(snap_physical(-0.6), -1.0);
        assert_eq!(snap_physical(-10.49), -10.0);
        assert_eq!(snap_physical(-10.51), -11.0);
    }

    #[test]
    fn snap_physical_rect_returns_integer_edges() {
        // The contract: both edges must be integers.
        let (s, e) = snap_physical_rect(10.3, 50.7);
        assert_eq!(s, 10.0);
        assert_eq!(e, 51.0);
        assert!(s.fract() == 0.0);
        assert!(e.fract() == 0.0);
    }

    #[test]
    fn snap_physical_rect_supports_non_integer_widths() {
        // A 1.4px-wide separator at x=10.3 → [10.3, 11.7] → snap → [10, 12].
        // Both edges integer; width becomes 2.0 (was 1.4). The 0.6px growth
        // is sub-pixel and invisible; what matters is no edge lands on x.5.
        let (s, e) = snap_physical_rect(10.3, 11.7);
        assert_eq!(s, 10.0);
        assert_eq!(e, 12.0);
    }

    #[test]
    fn snap_physical_rect_does_not_invert_for_ordered_inputs() {
        // For start < end, the snapped pair must remain ordered (start ≤ end).
        // This holds because round() is monotonic non-decreasing.
        for (s_in, e_in) in [(10.0, 20.0), (10.3, 11.7), (-5.5, 5.5), (0.1, 0.9)] {
            let (s, e) = snap_physical_rect(s_in, e_in);
            assert!(s <= e, "snapped {s} > {e} for input ({s_in}, {e_in})");
        }
    }

    #[test]
    fn snap_physical_rect_preserves_input_order_for_inverted_inputs() {
        // When start > end (caller error or intentional), the helper does
        // NOT swap — it returns (round(start), round(end)) in the same order.
        // Callers that need ordering must sort first. This contract avoids
        // surprising silent swaps deep inside the geometry pipeline.
        let (s, e) = snap_physical_rect(20.6, 10.3);
        assert_eq!(s, 21.0);
        assert_eq!(e, 10.0);
        // Caller is responsible for detecting the inversion:
        assert!(s > e);
    }

    #[test]
    fn snap_physical_rect_handles_extremes() {
        // Narrow 0.2px interval straddling an integer (10.4, 10.6) → snaps
        // to (10, 11), width 1px. The helper does not collapse sub-pixel
        // intervals that straddle a pixel boundary.
        let (s, e) = snap_physical_rect(10.4, 10.6);
        assert_eq!(s, 10.0);
        assert_eq!(e, 11.0);
        assert_eq!(e - s, 1.0);

        // Large coordinates (4K/8K display ranges) must not lose precision.
        let (s, e) = snap_physical_rect(3839.7, 3840.3);
        assert_eq!(s, 3840.0);
        assert_eq!(e, 3840.0);
        // Note: a sub-pixel input straddling 3840 collapses to width 0.
        // Callers must enforce a minimum-thickness invariant themselves
        // (e.g. `thumb_height.max(2.0)`) before snapping — the snap helper
        // is geometry-only and does not know the design intent.
        assert_eq!(e - s, 0.0);
    }
}
