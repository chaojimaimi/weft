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
/// Passed to [`UiColors::accent_for`]. Batch 11 起在 tab_bar 首次消费；
/// `Pressed`/`Disabled`/`Focused` 暂未消费，保留以稳定 R4 API。
#[allow(dead_code)]
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
                // v1.11.6 (M6/D-f): a `theme.ui.success` override replaces
                // the dual-branch input and STILL runs the 4.5 gate — the
                // user hex is not necessarily the final painted color.
                if let Some(user) = theme.ui.success {
                    user
                } else if light_text {
                    Color::rgb(135, 204, 92)
                } else {
                    Color::rgb(50, 120, 35)
                },
                &[bg, panel],
                4.5,
            ),
            warning: ensure_contrast(
                if let Some(user) = theme.ui.warning {
                    user
                } else if light_text {
                    Color::rgb(220, 166, 78)
                } else {
                    Color::rgb(150, 95, 0)
                },
                &[bg, panel],
                4.5,
            ),
            error: ensure_contrast(
                if let Some(user) = theme.ui.error {
                    user
                } else if light_text {
                    Color::rgb(217, 92, 92)
                } else {
                    Color::rgb(180, 45, 45)
                },
                &[bg, panel],
                4.5,
            ),
            find_match: if let Some(user) = theme.ui.find_match {
                // v1.11.6 (M6/D-f): user override replaces the dual-branch
                // input. Unlike success/warning/error, this key never had
                // an ensure_contrast gate ("照旧" per D-f), so the user
                // value stays raw — the renderer composites it at 50% alpha.
                user
            } else if light_text {
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

// Tests live in the gate-exempt sibling module (repo test-module
// convention, pty/tests.rs precedent) so inline test lines stay out of
// the production-file budget.
#[cfg(test)]
#[path = "ui_tokens/tests.rs"]
mod tests;
