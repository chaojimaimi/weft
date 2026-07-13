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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiColors {
    pub canvas: Color,
    pub chrome: Color,
    pub raised: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub border_subtle: Color,
    pub focus: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    /// F3-5: Find/replace match highlight color (translucent yellow). The
    /// renderer applies a 0.50 alpha overlay so the matched text stays
    /// readable underneath. Kept distinct from `warning` so theme tweaks
    /// don't accidentally change find highlighting.
    pub find_match: Color,
}

impl UiColors {
    pub fn from_theme(theme: &Theme) -> Self {
        let bg = theme.background;
        let fg = theme.foreground;
        let dark = u16::from(bg.r) + u16::from(bg.g) + u16::from(bg.b) < 384;
        let chrome = if dark {
            scale(bg, 0.85)
        } else {
            mix(bg, Color::rgb(255, 255, 255), 0.50)
        };
        Self {
            canvas: bg,
            chrome,
            raised: mix(bg, Color::rgb(255, 255, 255), 0.08),
            text_primary: fg,
            text_secondary: mix(bg, fg, 0.70),
            border_subtle: mix(bg, fg, 0.20),
            focus: theme.accent,
            success: if dark {
                Color::rgb(135, 204, 92)
            } else {
                Color::rgb(50, 120, 35)
            },
            warning: if dark {
                Color::rgb(220, 166, 78)
            } else {
                Color::rgb(150, 95, 0)
            },
            error: if dark {
                Color::rgb(217, 92, 92)
            } else {
                Color::rgb(180, 45, 45)
            },
            find_match: if dark {
                Color::rgb(242, 199, 51)
            } else {
                Color::rgb(200, 150, 0)
            },
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

#[cfg(test)]
mod tests {
    use super::{
        clamp_sidebar_width, sidebar_edge_hit, ResponsiveClass, SidebarMetrics, UiColors,
        UiMetrics, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
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
    fn metrics_scale_from_logical_points() {
        let one = UiMetrics::for_scale(1.0);
        let two = UiMetrics::for_scale(2.0);
        assert_eq!(two.stroke, one.stroke * 2.0);
        assert_eq!(two.control_compact, one.control_compact * 2.0);
    }

    #[test]
    fn semantic_surfaces_and_text_are_distinct_in_dark_and_light_themes() {
        for theme in [Theme::weft_warm(), Theme::weft_light()] {
            let colors = UiColors::from_theme(&theme);
            assert_ne!(colors.chrome, colors.canvas);
            assert_ne!(colors.raised, colors.canvas);
            assert_ne!(colors.text_primary, colors.canvas);
            assert_ne!(colors.text_secondary, colors.canvas);
            assert_eq!(colors.focus, theme.accent);
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
}
