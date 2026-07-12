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
        TokenKind::Whitespace | TokenKind::Default => color_to_normalized(s.default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_color_distinct_and_default_fallback() {
        // v0.8: syntax colors are now theme-driven. Verify the warm theme
        // produces distinct colors for every token kind, and that
        // Default/Whitespace resolve to theme.syntax.default.
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
        ];
        let colors: Vec<[f32; 4]> = kinds.iter().map(|&k| syntax_color(k, &theme)).collect();
        // All 8 should be distinct (no two token kinds share a color).
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
}
