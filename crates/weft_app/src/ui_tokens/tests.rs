// ── Tests ───────────────────────────────────────────────────────────────

use super::{
    clamp_sidebar_width, compact_control_row_span, contrast_ratio, sidebar_edge_hit,
    sidebar_placement, sidebar_visual_width, sidebar_width_after_drag, InteractionState,
    ResponsiveClass, SidebarMetrics, UiColors, UiMetrics, MIN_WINDOW_WIDTH, SIDEBAR_MAX_WIDTH,
    SIDEBAR_MIN_WIDTH,
};
use weft_core::config::Theme;
use weft_core::grid::Color;

#[test]
fn responsive_breakpoints_are_stable() {
    assert_eq!(
        ResponsiveClass::from_logical_width(639.0),
        ResponsiveClass::Compact
    );
    assert_eq!(
        ResponsiveClass::from_logical_width(640.0),
        ResponsiveClass::Regular
    );
    assert_eq!(
        ResponsiveClass::from_logical_width(1099.0),
        ResponsiveClass::Regular
    );
    assert_eq!(
        ResponsiveClass::from_logical_width(1100.0),
        ResponsiveClass::Wide
    );
}

#[test]
fn compact_sidebar_is_overlay_and_regular_sidebar_pushes_content() {
    let compact = SidebarMetrics::for_logical_width(500.0);
    assert_eq!(compact.panel_width, 240.0);
    assert_eq!(compact.push_width, 0.0);

    let regular = SidebarMetrics::for_logical_width(900.0);
    assert_eq!(regular.panel_width, 240.0);
    assert_eq!(regular.push_width, regular.panel_width);

    let wide = SidebarMetrics::for_logical_width(1600.0);
    assert_eq!(wide.panel_width, 360.0);
    assert_eq!(wide.push_width, wide.panel_width);
}

#[test]
fn compact_sidebar_preserves_pty_but_reserves_visible_tab_chrome() {
    let compact = sidebar_placement(true, 480.0, 0.0);
    assert_eq!(compact.terminal_push_width, 0.0);
    assert_eq!(compact.tab_chrome_left, 480.0);
    assert!(compact.overlay);

    let regular = sidebar_placement(true, 480.0, 480.0);
    assert_eq!(regular.terminal_push_width, 480.0);
    assert_eq!(regular.tab_chrome_left, 480.0);
    assert!(!regular.overlay);

    let closed = sidebar_placement(false, 480.0, 0.0);
    assert_eq!(closed.terminal_push_width, 0.0);
    assert_eq!(closed.tab_chrome_left, 0.0);
    assert!(!closed.overlay);
}

#[test]
fn compact_sidebar_ignores_wide_override_until_viewport_recovers() {
    assert_eq!(sidebar_visual_width(360.0, Some(360.0)), 240.0);
    assert_eq!(sidebar_visual_width(900.0, Some(360.0)), 360.0);

    let viewport = MIN_WINDOW_WIDTH as f32;
    let chrome_left = sidebar_visual_width(viewport, Some(360.0));
    let layout = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
        viewport_width: viewport,
        bar_height: 28.0,
        cell_width: 8.0,
        padding_x: 10.0,
        chrome_left,
        traffic_lights_width: 72.0,
        tab_count: 4,
        requested_scroll_offset: 500.0,
    });
    assert!(layout.plus_rect[2] <= layout.bar_rect[2]);
    for rect in [layout.left_arrow_rect, layout.right_arrow_rect]
        .into_iter()
        .flatten()
    {
        assert!(rect[2] <= layout.bar_rect[2]);
    }
}

#[test]
fn metrics_scale_from_logical_points() {
    let one = UiMetrics::for_scale(1.0);
    let two = UiMetrics::for_scale(2.0);
    assert_eq!(two.stroke, one.stroke * 2.0);
    assert_eq!(two.control_compact, one.control_compact * 2.0);
}

#[test]
fn compact_control_span_reserves_whole_non_overlapping_rows() {
    assert_eq!(compact_control_row_span(20.0, 1.0), 2);
    assert_eq!(compact_control_row_span(40.0, 2.0), 2);
    assert_eq!(compact_control_row_span(64.0, 2.0), 1);
    assert_eq!(compact_control_row_span(0.0, 2.0), 1);
}

#[test]
fn semantic_surfaces_and_text_are_distinct_in_dark_and_light_themes() {
    for theme in [Theme::weft_warm(), Theme::weft_light()] {
        let colors = UiColors::from_theme(&theme);
        assert_ne!(colors.chrome, colors.canvas);
        assert_ne!(colors.raised, colors.canvas);
        assert_ne!(colors.panel, colors.canvas);
        assert!(contrast_ratio(colors.selection, colors.panel) >= 3.0);
        assert!(contrast_ratio(colors.selection_text, colors.selection) >= 4.5);
        assert_ne!(colors.text_primary, colors.canvas);
        assert_ne!(colors.text_secondary, colors.canvas);
        assert!(contrast_ratio(colors.focus, colors.canvas) >= 4.5);
        assert_ne!(colors.success, colors.error);
        assert_ne!(colors.warning, colors.error);
        // F3-5: find_match must be visible against the canvas background.
        assert_ne!(colors.find_match, colors.canvas);
    }
}

#[test]
fn sidebar_width_bounds_are_consistent_with_metrics() {
    // The clamp range must cover the Regular/Wide panel_width range so a
    // drag can't produce a width narrower than the responsive default.
    let regular = SidebarMetrics::for_logical_width(900.0);
    let wide = SidebarMetrics::for_logical_width(1600.0);
    assert!(regular.panel_width >= SIDEBAR_MIN_WIDTH);
    assert!(wide.panel_width <= SIDEBAR_MAX_WIDTH);
}

#[test]
fn clamp_sidebar_width_enforces_range() {
    // Below min → min, above max → max, in-range unchanged.
    assert_eq!(clamp_sidebar_width(0.0), SIDEBAR_MIN_WIDTH);
    assert_eq!(clamp_sidebar_width(100.0), SIDEBAR_MIN_WIDTH);
    assert_eq!(clamp_sidebar_width(SIDEBAR_MIN_WIDTH), SIDEBAR_MIN_WIDTH);
    assert_eq!(clamp_sidebar_width(300.0), 300.0);
    assert_eq!(clamp_sidebar_width(SIDEBAR_MAX_WIDTH), SIDEBAR_MAX_WIDTH);
    assert_eq!(clamp_sidebar_width(999.0), SIDEBAR_MAX_WIDTH);
    // NaN must not slip through (clamp keeps NaN, so callers must guard
    // their inputs; document that contract here).
    assert!(clamp_sidebar_width(f32::NAN).is_nan());
}

#[test]
fn sidebar_drag_width_grows_and_shrinks_in_logical_points() {
    assert_eq!(sidebar_width_after_drag(300.0, 300.0, 340.0, 1.0), 340.0);
    assert_eq!(sidebar_width_after_drag(300.0, 300.0, 260.0, 1.0), 260.0);
    assert_eq!(sidebar_width_after_drag(300.0, 600.0, 520.0, 2.0), 260.0);
}

#[test]
fn sidebar_drag_width_clamps_both_directions() {
    assert_eq!(
        sidebar_width_after_drag(300.0, 300.0, -100.0, 1.0),
        SIDEBAR_MIN_WIDTH
    );
    assert_eq!(
        sidebar_width_after_drag(300.0, 300.0, 900.0, 1.0),
        SIDEBAR_MAX_WIDTH
    );
}

#[test]
fn sidebar_edge_hit_detects_within_tolerance_and_vertical_extent() {
    let edge = 240.0_f32;
    let vp_h = 800.0_f32;
    let tol = 4.0_f32;

    // Exactly on edge.
    assert!(sidebar_edge_hit(edge, edge, tol, vp_h, 100.0));
    // Just inside tolerance (±4 px).
    assert!(sidebar_edge_hit(edge - 4.0, edge, tol, vp_h, 0.0));
    assert!(sidebar_edge_hit(edge + 4.0, edge, tol, vp_h, vp_h));
    // Just outside tolerance.
    assert!(!sidebar_edge_hit(edge - 4.001, edge, tol, vp_h, 100.0));
    assert!(!sidebar_edge_hit(edge + 4.001, edge, tol, vp_h, 100.0));
    // Above viewport top.
    assert!(!sidebar_edge_hit(edge, edge, tol, vp_h, -0.001));
    // Below viewport bottom.
    assert!(!sidebar_edge_hit(edge, edge, tol, vp_h, vp_h + 0.001));
    // Far from edge horizontally.
    assert!(!sidebar_edge_hit(500.0, edge, tol, vp_h, 100.0));
}

// ── F6: Increase Contrast ────────────────────────────────────────

#[test]
fn with_increase_contrast_false_returns_unchanged() {
    let theme = Theme::weft_warm();
    let colors = UiColors::from_theme(&theme);
    let adjusted = colors.with_increase_contrast(false);
    assert_eq!(adjusted.border_subtle, colors.border_subtle);
    assert_eq!(adjusted.text_secondary, colors.text_secondary);
    assert_eq!(adjusted.find_match, colors.find_match);
}

#[test]
fn with_increase_contrast_true_strengthens_border_and_text() {
    let theme = Theme::weft_warm();
    let colors = UiColors::from_theme(&theme);
    let adjusted = colors.with_increase_contrast(true);
    // border_subtle goes from mix(bg, fg, 0.20) to mix(bg, fg, 0.65).
    // The adjusted border should be closer to the foreground than the
    // original, i.e. more visible.
    assert_ne!(adjusted.border_subtle, colors.border_subtle);
    // text_secondary is promoted to the already contrast-corrected primary.
    assert_ne!(adjusted.text_secondary, colors.text_secondary);
    // Dark-theme find_match is brightened away from the canvas.
    assert_ne!(adjusted.find_match, colors.find_match);
    // Other colors are unchanged (struct update syntax ..self).
    assert_eq!(adjusted.canvas, colors.canvas);
    assert_eq!(adjusted.chrome, colors.chrome);
    assert_eq!(adjusted.panel, colors.panel);
    assert_eq!(adjusted.selection, colors.selection);
    assert_eq!(adjusted.selection_text, colors.selection_text);
    assert_eq!(adjusted.focus, colors.focus);
    assert_eq!(adjusted.error, colors.error);
}

#[test]
fn with_increase_contrast_border_closer_to_foreground() {
    // In dark theme, border_subtle is a mix of dark bg and light fg.
    // Increase Contrast pushes it closer to fg, so the resulting color
    // should have a higher sum of RGB channels (brighter in dark theme).
    let theme = Theme::weft_warm();
    let colors = UiColors::from_theme(&theme);
    let adjusted = colors.with_increase_contrast(true);
    let orig_sum = u16::from(colors.border_subtle.r)
        + u16::from(colors.border_subtle.g)
        + u16::from(colors.border_subtle.b);
    let adj_sum = u16::from(adjusted.border_subtle.r)
        + u16::from(adjusted.border_subtle.g)
        + u16::from(adjusted.border_subtle.b);
    assert!(
        adj_sum > orig_sum,
        "increase contrast should brighten border in dark theme: {orig_sum} → {adj_sum}"
    );
}

#[test]
fn secondary_text_remains_distinct_while_meeting_all_surface_floors() {
    for theme in [
        Theme::weft_warm(),
        Theme::weft_light(),
        Theme::solarized_dark(),
    ] {
        let colors = UiColors::from_theme(&theme);
        assert_ne!(colors.text_secondary, colors.text_primary);
        for surface in [colors.canvas, colors.chrome, colors.raised, colors.panel] {
            assert!(contrast_ratio(colors.text_secondary, surface) >= 4.5);
        }
    }
}

#[test]
fn saturated_custom_theme_chooses_the_non_degrading_contrast_direction() {
    use weft_core::grid::Color;

    let mut theme = Theme::weft_warm();
    theme.background = Color::rgb(255, 0, 0);
    theme.foreground = Color::rgb(0, 0, 0);
    theme.accent = Color::rgb(180, 30, 30);
    let colors = UiColors::from_theme(&theme);
    for surface in [colors.canvas, colors.chrome, colors.raised, colors.panel] {
        assert!(contrast_ratio(colors.text_primary, surface) >= 4.5);
        assert!(contrast_ratio(colors.text_secondary, surface) >= 4.5);
    }
    for status in [colors.focus, colors.success, colors.warning, colors.error] {
        assert!(contrast_ratio(status, colors.canvas) >= 4.5);
        assert!(contrast_ratio(status, colors.panel) >= 4.5);
    }
    assert!(contrast_ratio(colors.selection, colors.panel) >= 3.0);
    assert!(contrast_ratio(colors.selection_text, colors.selection) >= 4.5);
}

#[test]
fn r4_new_tokens_are_non_default() {
    let colors = UiColors::from_theme(&Theme::weft_warm());
    assert_ne!(colors.sunken, colors.canvas);
    assert_ne!(colors.text_muted, colors.text_primary);
    assert_ne!(colors.border_strong, colors.border_subtle);
    assert_ne!(colors.accent_hover, colors.focus);
    assert_ne!(colors.accent_pressed, colors.focus);
    // text_inverse is a contrast extreme for selection.
    assert!(
        colors.text_inverse == weft_core::grid::Color::rgb(255, 255, 255)
            || colors.text_inverse == weft_core::grid::Color::rgb(0, 0, 0)
    );
}

#[test]
fn r4_interaction_state_accent_progression() {
    let colors = UiColors::from_theme(&Theme::weft_warm());
    assert_eq!(colors.accent_for(InteractionState::Normal), colors.focus);
    assert_eq!(colors.accent_for(InteractionState::Focused), colors.focus);
    assert_ne!(colors.accent_for(InteractionState::Hover), colors.focus);
    assert_ne!(colors.accent_for(InteractionState::Pressed), colors.focus);
    assert_ne!(colors.accent_for(InteractionState::Disabled), colors.focus);
}

#[test]
fn r4_increase_contrast_strengthens_border_and_mutes() {
    let normal = UiColors::from_theme(&Theme::weft_warm());
    let ic = normal.with_increase_contrast(true);
    assert_ne!(ic.border_strong, normal.border_strong);
    assert_ne!(ic.text_muted, normal.text_muted);
}

#[test]
fn r4_text_muted_meets_large_text_contrast() {
    for theme in [
        Theme::weft_warm(),
        Theme::weft_light(),
        Theme::dracula(),
        Theme::solarized_dark(),
    ] {
        let colors = UiColors::from_theme(&theme);
        for surface in [colors.canvas, colors.chrome, colors.raised, colors.panel] {
            assert!(
                contrast_ratio(colors.text_muted, surface) >= 3.0,
                "text_muted contrast < 3.0"
            );
        }
    }
}

// ── v1.11.6 (PLAN_v1116 M6/D-f): theme.ui seed overrides ─────────

#[test]
fn theme_ui_success_override_still_passes_ensure_contrast_gate() {
    // A user `theme.ui.success` hex replaces the dual-branch INPUT but
    // still runs ensure_contrast(..., 4.5): the stored hex is NOT the
    // final painted color — a near-bg value must be lifted to ≥4.5:1
    // against the theme background.
    let mut theme = Theme::weft_warm();
    let raw_bg = theme.background;
    theme.ui.success = Some(Color::rgb(20, 20, 20)); // ≈ bg → fails 4.5
    let ui = UiColors::from_theme(&theme);
    assert_ne!(
        ui.success,
        Color::rgb(20, 20, 20),
        "gate must adjust the user success value"
    );
    assert!(
        contrast_ratio(ui.success, raw_bg) >= 4.5 - 1e-6,
        "user success override must reach the 4.5 gate, got {}",
        contrast_ratio(ui.success, raw_bg)
    );
    // The other gated keys behave the same way.
    theme.ui.warning = Some(Color::rgb(20, 20, 20));
    theme.ui.error = Some(Color::rgb(20, 20, 20));
    let ui = UiColors::from_theme(&theme);
    assert!(contrast_ratio(ui.warning, raw_bg) >= 4.5 - 1e-6);
    assert!(contrast_ratio(ui.error, raw_bg) >= 4.5 - 1e-6);
}

#[test]
fn theme_ui_none_falls_back_to_dual_branch() {
    // All-None (zero-config) keeps the old dual-branch hardcodes —
    // find_match has no gate in the legacy pipeline, so its None value
    // is exactly the literal (dark ochre / light yellow).
    // "light_text" in from_theme means the CANVAS is dark (text is
    // light) — weft_warm is dark → the yellow branch; weft_light is
    // light → the dark ochre branch.
    let dark_ui = UiColors::from_theme(&Theme::weft_warm());
    assert_eq!(dark_ui.find_match, Color::rgb(242, 199, 51));
    let light_ui = UiColors::from_theme(&Theme::weft_light());
    assert_eq!(light_ui.find_match, Color::rgb(65, 25, 0));
}
