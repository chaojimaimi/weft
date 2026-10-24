//! `paint/primitives` test module (v1.14.0 T5 file-budget split — the
//! rounded-rect + vertex-alpha additions pushed the production file over
//! the 800-line gate; sibling test module per the pty/tests.rs precedent).

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
    // v1.11.0: 每个主题用同一公式(fg × 0.65)，CWD 不随主题变——原 `output.cwd`
    // 键已删除（死配置，见 AUDIT_v1.10.39 / PLAN_v111）。
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
/// v1.7.0-E: Extended to also assert metadata ≠ output_default and
/// warning ≠ success/failure, closing the §2.5 "角色间满足固定的感知
/// 差异门槛" gap for the full OutputSemanticColors role set.
///
/// v1.11.0: the `cwd` role assertions were removed with the dead
/// `output.cwd` key (AUDIT_v1.10.39 / PLAN_v111) — CWD contrast is now
/// structural: `derive_cwd_gray` (fg × 0.65) guarantees the dimming,
/// covered by `derive_cwd_gray_dims_foreground_uniformly_and_theme_independently`.
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
        // V17 §2.4: output_default ≠ cwd ("普通结果不得等于 CWD 色") —
        // v1.11.0: CWD is derived (fg × 0.65), not a theme role; see
        // `derive_cwd_gray_dims_foreground_uniformly_and_theme_independently`.
        assert_ne!(o.success, o.failure, "{name}: success == failure");
        // v1.7.0-E: metadata must be distinct from output_default
        // ("label 与 value 可分，但整体弱于命令名" — metadata is a weaker
        // structural role and must not collapse into default or cwd).
        assert_ne!(
            o.metadata, o.output_default,
            "{name}: metadata == output_default"
        );
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

#[test]
fn scale_vertex_alpha_fades_every_layer_and_preserves_masks() {
    use crate::paint::primitives::scale_vertex_alpha;
    let mut verts = Vec::new();
    // One quad: fg opaque white, bg opaque shell color.
    push_quad(
        &mut verts,
        [0.0, 0.0, 10.0, 10.0],
        [0.0; 4],
        [1.0, 1.0, 1.0, 1.0],
        [0.1, 0.2, 0.3, 1.0],
    );
    scale_vertex_alpha(&mut verts, 0.5);
    assert_eq!(verts.len(), 72);
    // Every vertex: fg alpha at offset 7, bg alpha at offset 11.
    for v in verts.chunks_exact(12) {
        assert_eq!(v[7], 0.5);
        assert_eq!(v[11], 0.5);
    }
    // The "only bg shows" mask (fg alpha 0) must survive a fade: 0 × r.
    let mut masked = Vec::new();
    push_quad(
        &mut masked,
        [0.0, 0.0, 1.0, 1.0],
        [0.0; 4],
        [0.0; 4],
        [0.9, 0.9, 0.9, 0.25],
    );
    scale_vertex_alpha(&mut masked, 0.4);
    for v in masked.chunks_exact(12) {
        assert_eq!(v[7], 0.0);
        assert!((v[11] - 0.1).abs() < 1e-6);
    }
    // Full reveal is a no-op (early return).
    let mut full = vec![1.0f32; 24];
    let snapshot = full.clone();
    scale_vertex_alpha(&mut full, 1.0);
    assert_eq!(full, snapshot);
}

#[test]
fn rounded_rect_emits_center_edges_and_corner_fans() {
    // One rounded shape = 5 quads (5×6×12 = 360 floats) + 24 corner
    // triangles (24×3×12 = 864 floats) = 1224 floats.
    let mut verts = Vec::new();
    push_rounded_rect(
        &mut verts,
        [10.0, 10.0, 110.0, 60.0],
        10.0,
        [1.0; 4],
        [0.0; 4],
    );
    assert_eq!(verts.len(), 1224);
    // Center-column quad starts at x0 + radius.
    assert_eq!(verts[0], 20.0);
    assert_eq!(verts[1], 10.0);
    // The top-left fan's first triangle apex is the corner center.
    let fan_start = 5 * 72;
    assert!((verts[fan_start] - 20.0).abs() < 1e-4); // cx = x0 + r
    assert!((verts[fan_start + 1] - 20.0).abs() < 1e-4); // cy = y0 + r
                                                         // The first arc point sits exactly on the corner circle (180°).
    assert!((verts[fan_start + 12] - 10.0).abs() < 1e-4); // cx - r
    assert!((verts[fan_start + 13] - 20.0).abs() < 1e-4); // cy
}

#[test]
fn rounded_rect_clamps_radius_and_degrades_to_plain_quad() {
    // radius ≥ min(w,h)/2 clamps: 100×50 with radius 40 → 25.
    let mut verts = Vec::new();
    push_rounded_rect(
        &mut verts,
        [0.0, 0.0, 100.0, 50.0],
        40.0,
        [1.0; 4],
        [0.0; 4],
    );
    assert_eq!(verts.len(), 1224);
    assert_eq!(verts[0], 25.0); // center quad x0 = 0 + clamped radius
                                // Sub-half-pixel radius degrades to a single quad (72 floats) with
                                // the exact rect — square call sites keep their geometry.
    let mut plain = Vec::new();
    push_rounded_rect(&mut plain, [5.0, 5.0, 15.0, 15.0], 0.2, [1.0; 4], [0.0; 4]);
    assert_eq!(plain.len(), 72);
    assert_eq!(plain[0], 5.0);
    // Second vertex (bottom-left corner) pins the rect height exactly.
    assert_eq!(plain[13], 15.0);
    // Degenerate (zero-area) rect emits nothing.
    let mut empty = Vec::new();
    push_rounded_rect(&mut empty, [5.0, 5.0, 5.0, 15.0], 4.0, [1.0; 4], [0.0; 4]);
    assert!(empty.is_empty());
}

#[test]
fn scale_vertex_alpha_keeps_the_color_emoji_sentinel_above_the_threshold() {
    // rust-reviewer P2-1: the color-emoji sentinel (fg alpha 2.0) must
    // survive a fade ABOVE the shader's 1.5 threshold — dropping below
    // would route the emoji into the mask path and render it black.
    let mut verts = Vec::new();
    push_quad(
        &mut verts,
        [0.0, 0.0, 10.0, 10.0],
        [0.0; 4],
        [0.0, 0.0, 0.0, 2.0], // the sentinel
        [0.1, 0.1, 0.1, 1.0],
    );
    scale_vertex_alpha(&mut verts, 0.5);
    let faded = verts[7];
    assert!(
        faded > 1.5,
        "sentinel must stay on the color path, got {faded}"
    );
    assert!(
        (faded - 1.75).abs() < 1e-6,
        "band-linear remap, got {faded}"
    );
    // A near-zero reveal still stays just above the threshold.
    let mut near_zero = verts.clone();
    scale_vertex_alpha(&mut near_zero, 0.01);
    assert!(near_zero[7] > 1.5);
    // The quad's own bg alpha fades normally.
    assert!((verts[11] - 0.5).abs() < 1e-6);
}
