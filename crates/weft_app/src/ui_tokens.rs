//! v1.2 semantic UI tokens.
//!
//! Terminal ANSI colors remain independent; these tokens describe application
//! chrome and controls so every component derives hover/surface/text states in
//! the same way across themes.

use weft_core::config::Theme;
use weft_core::grid::Color;

pub const MIN_WINDOW_WIDTH: f64 = 360.0;
pub const MIN_WINDOW_HEIGHT: f64 = 240.0;

/// F3-3: Re-export the sidebar width bounds defined in weft_core::config so
/// app-level helpers and config load share one source of truth.
pub use weft_core::config::{SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};

/// F3-3: Clamp a candidate sidebar width (logical points) to the allowed
/// range. Pure function so drag-move, config load, and `set_sidebar_width`
/// share one source of truth.
pub fn clamp_sidebar_width(width: f32) -> f32 {
    width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

/// Resolve a sidebar drag in physical pixels to a clamped logical width.
/// The signed delta is intentional: dragging left must shrink the sidebar.
pub fn sidebar_width_after_drag(start_width: f32, start_x: f64, current_x: f64, scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0.0 {
        return clamp_sidebar_width(start_width);
    }
    let delta = ((current_x - start_x) / f64::from(scale)) as f32;
    if !delta.is_finite() {
        return clamp_sidebar_width(start_width);
    }
    clamp_sidebar_width(start_width + delta)
}

/// F3-3: Pure hit-test for the sidebar's right-edge resize handle. Returns
/// `true` when `x` is within `tolerance` px of `edge` and `y` is inside the
/// viewport's vertical extent. Extracted from `App::sidebar_resize_hit` so
/// the geometry is unit-testable without an `App` instance.
pub fn sidebar_edge_hit(x: f32, edge: f32, tolerance: f32, vp_h: f32, y: f32) -> bool {
    (x - edge).abs() <= tolerance && y >= 0.0 && y <= vp_h
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsiveClass {
    Compact,
    Regular,
    Wide,
}

impl ResponsiveClass {
    pub fn from_logical_width(width: f32) -> Self {
        if width < 640.0 {
            Self::Compact
        } else if width < 1100.0 {
            Self::Regular
        } else {
            Self::Wide
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidebarMetrics {
    pub panel_width: f32,
    pub push_width: f32,
}

/// Per-frame placement contract for the history sidebar. Compact windows use
/// an overlay drawer: the terminal keeps its full PTY width, while top chrome
/// still starts after the visible drawer and the drawer paints above terminal
/// overlays such as the prompt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidebarPlacement {
    pub terminal_push_width: f32,
    pub tab_chrome_left: f32,
    pub overlay: bool,
}

pub fn sidebar_placement(open: bool, panel_width: f32, push_width: f32) -> SidebarPlacement {
    if !open {
        return SidebarPlacement {
            terminal_push_width: 0.0,
            tab_chrome_left: 0.0,
            overlay: false,
        };
    }
    SidebarPlacement {
        terminal_push_width: push_width,
        tab_chrome_left: panel_width,
        overlay: push_width <= 0.0,
    }
}

/// Resolve the visible sidebar width without letting a Regular/Wide drag
/// override consume the entire Compact viewport. The override is retained by
/// config and becomes effective again after returning to a wider class.
pub fn sidebar_visual_width(logical_viewport_width: f32, override_width: Option<f32>) -> f32 {
    let metrics = SidebarMetrics::for_logical_width(logical_viewport_width);
    if ResponsiveClass::from_logical_width(logical_viewport_width) == ResponsiveClass::Compact {
        metrics.panel_width
    } else {
        override_width
            .map(clamp_sidebar_width)
            .unwrap_or(metrics.panel_width)
    }
}

impl SidebarMetrics {
    pub fn for_logical_width(width: f32) -> Self {
        let class = ResponsiveClass::from_logical_width(width);
        let panel_width = match class {
            ResponsiveClass::Compact => (width * 0.8).min(240.0),
            ResponsiveClass::Regular => (width * 0.24).clamp(240.0, 320.0),
            ResponsiveClass::Wide => (width * 0.24).clamp(280.0, 360.0),
        }
        .max(0.0);
        let push_width = match class {
            ResponsiveClass::Compact => 0.0,
            ResponsiveClass::Regular | ResponsiveClass::Wide => panel_width,
        };
        Self {
            panel_width,
            push_width,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiMetrics {
    pub stroke: f32,
    pub control_compact: f32,
}

impl UiMetrics {
    pub fn for_scale(scale: f64) -> Self {
        let s = scale.max(0.5) as f32;
        Self {
            stroke: 1.0 * s,
            control_compact: 28.0 * s,
        }
    }
}

/// Number of ordinary text rows reserved for an accessible compact control.
/// Keeping this integral lets BlockView scrolling, painting and hit-testing
/// share the same half-open vertical bands without stealing adjacent rows.
pub fn compact_control_row_span(row_pitch: f32, scale: f64) -> usize {
    if !row_pitch.is_finite() || row_pitch <= 0.0 {
        return 1;
    }
    (UiMetrics::for_scale(scale).control_compact / row_pitch)
        .ceil()
        .max(1.0) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiColors {
    pub canvas: Color,
    pub chrome: Color,
    pub raised: Color,
    /// R4: sunken surface — deeper than canvas for input wells/code blocks.
    pub sunken: Color,
    pub panel: Color,
    pub selection: Color,
    pub selection_text: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    /// R4: muted text — placeholders/hints, 3:1 large-text floor.
    pub text_muted: Color,
    /// R4: inverse text — readable on selection/highlight backgrounds.
    pub text_inverse: Color,
    pub border_subtle: Color,
    /// R4: strong border — active dividers, 40% toward foreground.
    pub border_strong: Color,
    pub focus: Color,
    /// R4: accent hover state — slightly brighter than focus.
    pub accent_hover: Color,
    /// R4: accent pressed state — slightly darker than focus.
    pub accent_pressed: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    /// F3-5: Find/replace match highlight color (translucent yellow). The
    /// renderer applies a 0.50 alpha overlay so the matched text stays
    /// readable underneath. Kept distinct from `warning` so theme tweaks
    /// don't accidentally change find highlighting.
    pub find_match: Color,
}

/// R4: Interaction state for hover/focus/pressed/disabled semantics.
/// Passed to [`UiColors::accent_for`] to derive the correct accent variant.
#[allow(dead_code)] // R4: scaffolding for hover/pressed paint migration (Batch 11+)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InteractionState {
    #[default]
    Normal,
    Hover,
    Pressed,
    Disabled,
    Focused,
}

impl UiColors {
    pub fn from_theme(theme: &Theme) -> Self {
        let bg = theme.background;
        let fg = theme.foreground;
        let canvas_target = preferred_contrast_extreme(&[bg]);
        let light_text = canvas_target == Color::rgb(255, 255, 255);
        let chrome = if light_text {
            scale(bg, 0.85)
        } else {
            mix(bg, Color::rgb(255, 255, 255), 0.50)
        };
        let raised = mix(bg, Color::rgb(255, 255, 255), 0.08);
        // Keep the panel in the same luminance family as the canvas. The old
        // unconditional `bg * 0.55` turned light/custom themes into a dark
        // sidebar, making one status-text color unable to pass on both.
        let panel = if light_text {
            scale(bg, 0.55)
        } else {
            mix(bg, Color::rgb(0, 0, 0), 0.06)
        };
        let text_surfaces = [bg, chrome, raised, panel];
        // UI chrome is independent from terminal ANSI text. Give primary UI
        // text enough headroom that a visibly distinct secondary token can
        // still meet AA on canvas, chrome and raised surfaces.
        let text_primary = ensure_contrast(fg, &text_surfaces, 7.0);
        let text_secondary = ensure_contrast(mix(bg, text_primary, 0.70), &text_surfaces, 4.5);
        let text_muted = ensure_contrast(mix(bg, text_primary, 0.50), &text_surfaces, 3.0);
        let (selection, selection_text) = selection_colors(panel, theme.accent, text_primary);
        let text_inverse = preferred_contrast_extreme(&[selection]);
        // R4: sunken is 12% toward the contrast extreme (deeper for dark themes,
        // lighter for light themes) — visually distinct from canvas/raised.
        let sunken = mix(bg, canvas_target, 0.12);
        let border_strong = mix(bg, fg, 0.40);
        let focus = ensure_contrast(theme.accent, &[bg, panel], 4.5);
        // R4: accent interaction states — hover brightens, pressed darkens.
        let accent_hover = mix(focus, canvas_target, 0.15);
        let accent_pressed = mix(focus, bg, 0.20);
        Self {
            canvas: bg,
            chrome,
            raised,
            sunken,
            panel,
            selection,
            selection_text,
            text_primary,
            text_secondary,
            text_muted,
            text_inverse,
            border_subtle: mix(bg, fg, 0.20),
            border_strong,
            focus,
            accent_hover,
            accent_pressed,
            success: ensure_contrast(
                if light_text {
                    Color::rgb(135, 204, 92)
                } else {
                    Color::rgb(50, 120, 35)
                },
                &[bg, panel],
                4.5,
            ),
            warning: ensure_contrast(
                if light_text {
                    Color::rgb(220, 166, 78)
                } else {
                    Color::rgb(150, 95, 0)
                },
                &[bg, panel],
                4.5,
            ),
            error: ensure_contrast(
                if light_text {
                    Color::rgb(217, 92, 92)
                } else {
                    Color::rgb(180, 45, 45)
                },
                &[bg, panel],
                4.5,
            ),
            find_match: if light_text {
                Color::rgb(242, 199, 51)
            } else {
                // The renderer composites this token at 50% alpha. A dark
                // ochre keeps the resulting light-theme highlight at 3:1
                // against the canvas while preserving readable match text.
                Color::rgb(65, 25, 0)
            },
        }
    }

    /// F6: Strengthen colors for the macOS Increase Contrast accessibility
    /// setting. When `increase_contrast` is true:
    /// - `border_subtle` is pushed from 20% → 65% toward the foreground so
    ///   borders meet the 3:1 non-text contrast floor on built-in themes.
    /// - `text_secondary` is promoted to `text_primary`, preserving maximum
    ///   text contrast while the accessibility setting is active.
    /// - `find_match` moves away from the canvas luminance so composited
    ///   search highlights gain contrast in both dark and light themes.
    ///
    /// Returns a new `UiColors` (the original is unchanged). Pure function
    /// so it can be unit-tested without a renderer.
    pub fn with_increase_contrast(self, increase_contrast: bool) -> Self {
        if !increase_contrast {
            return self;
        }
        let bg = self.canvas;
        let fg = self.text_primary;
        let find_target = preferred_contrast_extreme(&[bg]);
        Self {
            border_subtle: mix(bg, fg, 0.65),
            border_strong: mix(bg, fg, 0.80),
            text_secondary: fg,
            text_muted: self.text_secondary,
            find_match: mix(self.find_match, find_target, 0.20),
            ..self
        }
    }

    /// R4: Derive the accent color for a given interaction state.
    /// Normal/Focused → focus; Hover → accent_hover; Pressed → accent_pressed;
    /// Disabled → 50% desaturated toward canvas.
    #[allow(dead_code)] // R4: scaffolding for hover/pressed paint migration (Batch 11+)
    pub fn accent_for(&self, state: InteractionState) -> Color {
        match state {
            InteractionState::Normal | InteractionState::Focused => self.focus,
            InteractionState::Hover => self.accent_hover,
            InteractionState::Pressed => self.accent_pressed,
            InteractionState::Disabled => mix(self.focus, self.canvas, 0.50),
        }
    }
}

fn scale(color: Color, factor: f32) -> Color {
    Color::rgb(
        (f32::from(color.r) * factor).round().clamp(0.0, 255.0) as u8,
        (f32::from(color.g) * factor).round().clamp(0.0, 255.0) as u8,
        (f32::from(color.b) * factor).round().clamp(0.0, 255.0) as u8,
    )
}

fn mix(from: Color, to: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::rgb(
        channel(from.r, to.r),
        channel(from.g, to.g),
        channel(from.b, to.b),
    )
}

fn relative_luminance(color: Color) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

pub(crate) fn contrast_ratio(a: Color, b: Color) -> f64 {
    let a = relative_luminance(a);
    let b = relative_luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn ensure_contrast(color: Color, backgrounds: &[Color], minimum_ratio: f64) -> Color {
    let worst_ratio = |candidate: Color| {
        backgrounds
            .iter()
            .map(|background| contrast_ratio(candidate, *background))
            .fold(f64::INFINITY, f64::min)
    };
    let original_ratio = worst_ratio(color);
    if original_ratio >= minimum_ratio {
        return color;
    }
    let extremes = [Color::rgb(0, 0, 0), Color::rgb(255, 255, 255)];
    let mut best = (color, original_ratio);
    for step in 1..=100 {
        let amount = step as f32 / 100.0;
        let mut passing = None;
        for target in extremes {
            let candidate = mix(color, target, amount);
            let ratio = worst_ratio(candidate);
            if ratio > best.1 {
                best = (candidate, ratio);
            }
            let improves_passing = match passing.as_ref() {
                None => true,
                Some((_, passing_ratio)) => ratio > *passing_ratio,
            };
            if ratio >= minimum_ratio && improves_passing {
                passing = Some((candidate, ratio));
            }
        }
        if let Some((candidate, _)) = passing {
            return candidate;
        }
    }
    best.0
}

fn preferred_contrast_extreme(backgrounds: &[Color]) -> Color {
    let worst_ratio = |candidate: Color| {
        backgrounds
            .iter()
            .map(|background| contrast_ratio(candidate, *background))
            .fold(f64::INFINITY, f64::min)
    };
    let black = Color::rgb(0, 0, 0);
    let white = Color::rgb(255, 255, 255);
    if worst_ratio(white) > worst_ratio(black) {
        white
    } else {
        black
    }
}

fn selection_colors(panel: Color, accent: Color, preferred_text: Color) -> (Color, Color) {
    let initial = mix(panel, accent, 0.35);
    let extremes = [Color::rgb(0, 0, 0), Color::rgb(255, 255, 255)];
    let text_for = |background| {
        if contrast_ratio(preferred_text, background) >= 4.5 {
            preferred_text
        } else {
            preferred_contrast_extreme(&[background])
        }
    };
    for step in 0..=100 {
        let amount = step as f32 / 100.0;
        let mut passing = None;
        for target in extremes {
            let candidate = mix(initial, target, amount);
            let text = text_for(candidate);
            let indicator_ratio = contrast_ratio(candidate, panel);
            let text_ratio = contrast_ratio(text, candidate);
            if indicator_ratio >= 3.0 && text_ratio >= 4.5 {
                let score = indicator_ratio.min(text_ratio);
                let improves = match passing.as_ref() {
                    None => true,
                    Some((_, _, passing_score)) => score > *passing_score,
                };
                if improves {
                    passing = Some((candidate, text, score));
                }
            }
        }
        if let Some((background, text, _)) = passing {
            return (background, text);
        }
    }
    let background = preferred_contrast_extreme(&[panel]);
    (background, text_for(background))
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_sidebar_width, compact_control_row_span, contrast_ratio, sidebar_edge_hit,
        sidebar_placement, sidebar_visual_width, sidebar_width_after_drag, InteractionState,
        ResponsiveClass, SidebarMetrics, UiColors, UiMetrics, MIN_WINDOW_WIDTH, SIDEBAR_MAX_WIDTH,
        SIDEBAR_MIN_WIDTH,
    };
    use weft_core::config::Theme;

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
}
