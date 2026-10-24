//! Low-level vertex primitives shared by all overlay builders.
//!
//! Extracted verbatim from `renderer.rs` (A5 phase). Each function pushes
//! raw `f32` vertex data into a caller-owned buffer — no `MetalRenderer`
//! dependency. Call sites use `crate::paint::primitives::push_quad(...)`.

use crate::layout::Rect;
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

/// Corner fan resolution for [`push_rounded_rect`] — 6 segments per 90°
/// corner arc (15° per triangle), the plan's T5 default (4 corners × 6
/// segments + 5 rects ≈ 28 draw units per shape).
pub(crate) const ROUNDED_RECT_SEGMENTS: usize = 6;

/// v1.14.0 (PLAN_v1140 T5): filled rounded rectangle as a polygon
/// decomposition — **no shader change**.
///
/// Structure: 1 center rect + 4 edge strips + 4 corner triangle fans of
/// `corner_segments` segments each (default 6). `radius` is clamped to
/// `min(w, h) / 2` (the plan's 钳制 rule); a sub-half-pixel radius degrades
/// to a plain quad so existing square call sites keep their exact geometry.
/// Overdraw is intentional and cheap: rounded shapes appear only on
/// single-digit floating surfaces per frame.
pub(crate) fn push_rounded_rect(
    vertices: &mut Vec<f32>,
    rect: Rect,
    radius: f32,
    color: [f32; 4],
    bg_uv: [f32; 4],
) {
    let [x0, y0, x1, y1] = rect;
    let w = x1 - x0;
    let h = y1 - y0;
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let r = radius.clamp(0.0, w.min(h) * 0.5);
    if r < 0.5 {
        push_quad(vertices, rect, bg_uv, [0.0; 4], color);
        return;
    }
    // Interior: center column + the four side strips.
    push_quad(vertices, [x0 + r, y0, x1 - r, y1], bg_uv, [0.0; 4], color);
    push_quad(
        vertices,
        [x0, y0 + r, x0 + r, y1 - r],
        bg_uv,
        [0.0; 4],
        color,
    );
    push_quad(
        vertices,
        [x1 - r, y0 + r, x1, y1 - r],
        bg_uv,
        [0.0; 4],
        color,
    );
    push_quad(
        vertices,
        [x0 + r, y0, x1 - r, y0 + r],
        bg_uv,
        [0.0; 4],
        color,
    );
    push_quad(
        vertices,
        [x0 + r, y1 - r, x1 - r, y1],
        bg_uv,
        [0.0; 4],
        color,
    );
    // Corner arcs. Screen coords are y-down; each (center, start°) pair
    // sweeps the 90° arc that bulges toward the rect's corner.
    let centers = [
        (x0 + r, y0 + r, 180.0f32), // top-left: 180° → 270°
        (x1 - r, y0 + r, 270.0),    // top-right: 270° → 360°
        (x1 - r, y1 - r, 0.0),      // bottom-right: 0° → 90°
        (x0 + r, y1 - r, 90.0),     // bottom-left: 90° → 180°
    ];
    let segments = ROUNDED_RECT_SEGMENTS as f32;
    for (cx, cy, start_deg) in centers {
        for i in 0..ROUNDED_RECT_SEGMENTS {
            let a0 = (start_deg + 90.0 * i as f32 / segments).to_radians();
            let a1 = (start_deg + 90.0 * (i as f32 + 1.0) / segments).to_radians();
            push_triangle(
                vertices,
                [cx, cy],
                [cx + r * a0.cos(), cy + r * a0.sin()],
                [cx + r * a1.cos(), cy + r * a1.sin()],
                color,
                bg_uv,
            );
        }
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

/// v1.14.0 (PLAN_v1140 T1): multiply every overlay vertex's fg AND bg alpha
/// by `reveal`, uniformly across the whole built vertex buffer.
///
/// Why a post-pass instead of per-color multiplication at each push site
/// (plan: "统一在 push 前 color 构造处乘，逐层核对防漏乘闪烁"): the overlay
/// builders push quads/lines/triangles in ONE fixed 12-float vertex format
/// (x, y, u, v, fg rgba, bg rgba), so scaling offsets 7 and 11 per vertex
/// provably covers every layer — shell, shadow, border, row backgrounds,
/// text, preedit, underlines and carets — with zero chance of a missed
/// site flickering between faded and unfaded. Two semantics survive the
/// pass: the mask trick (fg alpha 0 = "only bg shows") because 0 × reveal
/// stays 0, and the color-emoji sentinel (fg alpha 2.0) because it is
/// remapped within the shader's > 1.5 band instead of under it (P2-1).
pub(crate) fn scale_vertex_alpha(vertices: &mut [f32], reveal: f32) {
    if reveal >= 1.0 {
        return;
    }
    for v in vertices.chunks_exact_mut(12) {
        // rust-reviewer P2-1: the color-emoji sentinel (fg alpha 2.0, the
        // shader's `> 1.5` color-atlas route) must NEVER be scaled below
        // 1.5 — a masked emoji renders black. Remap the sentinel within its
        // [1.5, 2.0] band instead; the shader converts the band into the
        // emoji's own fade factor (`(fg.a - 1.5) / 0.5`), so the emoji still
        // fades with the surface while staying on the color path.
        v[7] = if v[7] > 1.5 {
            1.5 + (v[7] - 1.5) * reveal
        } else {
            v[7] * reveal
        };
        v[11] *= reveal;
    }
}

/// CWD gray: a theme-independent dimmed version of the command foreground.
///
/// Deliberately NOT a theme role — v1.11.0 removed the dead `output.cwd`
/// key (it was a per-theme "coordinated accent", so switching themes changed
/// the CWD hue; the painter never used it). Deriving from `foreground` × 0.65
/// gives every theme the same neutral gray, keeps the "CWD dim < command
/// bright" hierarchy, and auto-adapts to light themes (dark fg → darker
/// gray, still readable). See AUDIT_v1.10.39 / PLAN_v111.
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

// Tests live in the gate-exempt sibling module (repo test-module convention,
// pty/tests.rs precedent) so inline test lines stay out of the production
// budget.
#[cfg(test)]
#[path = "primitives_tests.rs"]
mod tests;
