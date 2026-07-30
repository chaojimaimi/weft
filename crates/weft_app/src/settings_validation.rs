//! Pure Settings draft validation shared by the controller and tests.

use std::collections::{HashMap, HashSet};

use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use weft_core::config::{Action, Config, FontConfig, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};
use weft_core::input::{KeyCode, Modifiers};

pub(crate) type FieldError = (String, String);

const PROGRAMMING_FONT_FAMILIES: &[&str] = &[
    "Menlo",
    "Hack",
    "JetBrains Mono",
    "SF Mono",
    "Monaco",
    "Fira Code",
    "Cascadia Mono",
    "Cascadia Code",
    "Iosevka",
    "Berkeley Mono",
    "MesloLGS NF",
    "Hack Nerd Font Mono",
    "Source Code Pro",
    "IBM Plex Mono",
    "Ubuntu Mono",
    "Courier New",
];

fn filtered_programming_fonts(
    current: &str,
    mut is_available: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let mut families: Vec<String> = PROGRAMMING_FONT_FAMILIES
        .iter()
        .copied()
        .filter(|family| *family == "Menlo" || *family == current || is_available(family))
        .map(str::to_owned)
        .collect();
    if !current.is_empty() && !families.iter().any(|family| family == current) {
        families.push(current.to_owned());
    }
    families
}

pub(crate) fn available_programming_fonts(current: &str) -> Vec<String> {
    let source = SystemSource::new();
    filtered_programming_fonts(current, |family| {
        source
            .select_best_match(&[FamilyName::Title(family.to_owned())], &Properties::new())
            .is_ok()
    })
}

pub(crate) fn runtime_font_config(config: &FontConfig) -> FontConfig {
    let defaults = FontConfig::default();
    let mut safe = config.clone();
    if !(8.0..=24.0).contains(&safe.size) {
        safe.size = defaults.size;
    }
    if !(1.0..=1.5).contains(&safe.line_height) {
        safe.line_height = defaults.line_height;
    }
    safe
}

pub(crate) fn runtime_scaled_font_config(config: &FontConfig, scale: f32) -> FontConfig {
    let mut safe = runtime_font_config(config);
    let scale = if (0.5..=3.0).contains(&scale) {
        scale
    } else {
        1.0
    };
    safe.size *= scale;
    safe
}

pub(crate) fn runtime_atlas_font_config(config: &FontConfig) -> FontConfig {
    let mut safe = config.clone();
    if !(4.0..=72.0).contains(&safe.size) {
        safe.size = FontConfig::default().size;
    }
    if !(1.0..=1.5).contains(&safe.line_height) {
        safe.line_height = FontConfig::default().line_height;
    }
    safe
}

pub(crate) fn runtime_opacity(opacity: f32) -> f32 {
    if (0.5..=1.0).contains(&opacity) {
        opacity
    } else {
        1.0
    }
}

pub(crate) fn runtime_minimum_contrast(minimum_contrast: f32) -> f32 {
    if minimum_contrast.is_finite() {
        minimum_contrast.clamp(1.0, 12.0)
    } else {
        weft_core::config::ThemeConfig::default().minimum_contrast
    }
}

pub(crate) fn runtime_sidebar_width(width: Option<f32>) -> Option<f32> {
    width.filter(|value| (SIDEBAR_MIN_WIDTH..=SIDEBAR_MAX_WIDTH).contains(value))
}

pub(crate) fn adjust_finite_value(
    current: f32,
    delta: i32,
    step: f32,
    min: f32,
    max: f32,
    fallback: f32,
) -> f32 {
    let base = if current.is_finite() {
        current
    } else {
        fallback
    };
    (base + delta as f32 * step).clamp(min, max)
}

pub(crate) fn directional_bool(current: bool, delta: i32) -> bool {
    match delta.cmp(&0) {
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => current,
    }
}

pub(crate) fn detect_keybinding_conflicts(
    views: &[crate::overlay::SettingsKeybindingView],
) -> HashSet<String> {
    let mut chord_actions: HashMap<&str, HashSet<&str>> = HashMap::new();
    for view in views {
        chord_actions
            .entry(view.binding.as_str())
            .or_default()
            .insert(view.action.as_str());
    }
    chord_actions
        .into_iter()
        .filter(|(_, actions)| actions.len() > 1)
        .map(|(chord, _)| chord.to_string())
        .collect()
}

fn push_float_range(errors: &mut Vec<FieldError>, label: &str, value: f32, min: f32, max: f32) {
    if !(min..=max).contains(&value) {
        errors.push((
            label.to_string(),
            format!("{value} is out of range [{min}, {max}]"),
        ));
    }
}

fn validate_keybindings(config: &Config, errors: &mut Vec<FieldError>) {
    let mut bindings: Vec<_> = config.keybindings.iter().collect();
    bindings.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut canonical: HashMap<(KeyCode, Modifiers), (&str, Action)> = HashMap::new();
    for (binding, action) in bindings {
        let Some(chord) = weft_core::config::parse_binding(binding) else {
            errors.push(("Keybindings".into(), format!("Invalid binding: {binding}")));
            continue;
        };
        if let Some((previous_binding, previous_action)) = canonical.get(&chord) {
            if previous_action != action {
                errors.push((
                    "Keybindings".into(),
                    format!("{previous_binding} conflicts with {binding}"),
                ));
            }
        } else {
            canonical.insert(chord, (binding.as_str(), *action));
        }
    }
}

pub(crate) fn validate_settings(config: &Config) -> Vec<FieldError> {
    let mut errors = Vec::new();
    push_float_range(&mut errors, "Font Size", config.font.size, 8.0, 24.0);
    push_float_range(
        &mut errors,
        "Line Height",
        config.font.line_height,
        1.0,
        1.5,
    );
    push_float_range(
        &mut errors,
        "Window Opacity",
        config.window.opacity,
        0.5,
        1.0,
    );
    push_float_range(
        &mut errors,
        "Minimum Contrast",
        config.theme.minimum_contrast,
        1.0,
        12.0,
    );
    if !(1_000..=100_000).contains(&config.scrollback.lines) {
        errors.push(("Scrollback".into(), "Must be 1000–100000 lines".into()));
    }
    if config.window.padding_x > 20 {
        errors.push(("Padding X".into(), "Must be at most 20 cells".into()));
    }
    if config.window.padding_y > 20 {
        errors.push(("Padding Y".into(), "Must be at most 20 cells".into()));
    }
    if !(400..=4_000).contains(&config.window.width) {
        errors.push(("Window Width".into(), "Must be 400–4000 px".into()));
    }
    if !(300..=4_000).contains(&config.window.height) {
        errors.push(("Window Height".into(), "Must be 300–4000 px".into()));
    }
    if let Some(width) = config.window.sidebar_width {
        push_float_range(
            &mut errors,
            "Sidebar Width",
            width,
            SIDEBAR_MIN_WIDTH,
            SIDEBAR_MAX_WIDTH,
        );
    }
    validate_keybindings(config, &mut errors);
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::SettingsKeybindingView;

    fn kb(action: &str, binding: &str) -> SettingsKeybindingView {
        SettingsKeybindingView {
            action: action.into(),
            binding: binding.into(),
            conflict: false,
        }
    }

    #[test]
    fn programming_font_catalog_filters_missing_and_preserves_custom_current() {
        let installed = ["Hack", "Fira Code"];
        let families = filtered_programming_fonts("My Mono", |family| installed.contains(&family));
        assert_eq!(families[0], "Menlo");
        assert!(families.iter().any(|family| family == "Hack"));
        assert!(families.iter().any(|family| family == "Fira Code"));
        assert!(families.iter().any(|family| family == "My Mono"));
        assert!(!families.iter().any(|family| family == "JetBrains Mono"));
    }

    #[test]
    fn directional_booleans_use_left_for_off_and_right_for_on() {
        assert!(!directional_bool(true, -1));
        assert!(!directional_bool(false, -1));
        assert!(directional_bool(false, 1));
        assert!(directional_bool(true, 1));
        assert!(directional_bool(true, 0));
        assert!(!directional_bool(false, 0));
    }

    #[test]
    fn programming_font_catalog_keeps_unavailable_selected_candidate() {
        let families = filtered_programming_fonts("JetBrains Mono", |_| false);
        assert_eq!(families, ["Menlo", "JetBrains Mono"]);
    }

    #[test]
    fn display_conflicts_require_different_actions_on_the_same_label() {
        let views = [kb("Copy", "cmd+x"), kb("Paste", "cmd+x")];
        assert!(detect_keybinding_conflicts(&views).contains("cmd+x"));
        let aliases = [kb("Paste", "cmd+v"), kb("Paste", "cmd+shift+v")];
        assert!(detect_keybinding_conflicts(&aliases).is_empty());
    }

    #[test]
    fn default_config_is_valid() {
        assert!(validate_settings(&Config::default()).is_empty());
    }

    #[test]
    fn invalid_runtime_geometry_uses_safe_values_without_mutating_source() {
        let font = FontConfig {
            size: f32::NAN,
            line_height: f32::INFINITY,
            ..FontConfig::default()
        };
        let safe = runtime_font_config(&font);
        assert_eq!(safe.size, FontConfig::default().size);
        assert_eq!(safe.line_height, FontConfig::default().line_height);
        assert!(font.size.is_nan());
        assert!(font.line_height.is_infinite());
        assert_eq!(runtime_opacity(f32::NAN), 1.0);
        assert_ne!(runtime_opacity(f32::NAN), runtime_opacity(0.95));
        assert_eq!(runtime_minimum_contrast(f32::NAN), 7.0);
        assert_eq!(runtime_sidebar_width(Some(f32::NAN)), None);
    }

    #[test]
    fn valid_runtime_geometry_is_preserved() {
        let font = FontConfig {
            size: 18.0,
            line_height: 1.4,
            ..FontConfig::default()
        };
        assert_eq!(runtime_font_config(&font).size, 18.0);
        assert_eq!(runtime_font_config(&font).line_height, 1.4);
        assert_eq!(runtime_opacity(0.75), 0.75);
        assert_eq!(runtime_minimum_contrast(5.5), 5.5);
        assert_eq!(runtime_sidebar_width(Some(300.0)), Some(300.0));
    }

    #[test]
    fn runtime_zoom_is_applied_after_base_font_validation() {
        let font = FontConfig::default();
        assert_eq!(runtime_scaled_font_config(&font, 0.5).size, 7.0);
        assert_eq!(runtime_scaled_font_config(&font, 3.0).size, 42.0);

        let invalid = FontConfig {
            size: f32::NAN,
            ..FontConfig::default()
        };
        assert_eq!(runtime_scaled_font_config(&invalid, 2.0).size, 28.0);
        assert_eq!(runtime_atlas_font_config(&font).size, font.size);
    }

    #[test]
    fn non_finite_and_every_numeric_boundary_are_rejected() {
        let mut config = Config::default();
        config.font.size = f32::NAN;
        config.font.line_height = f32::INFINITY;
        config.window.opacity = f32::NEG_INFINITY;
        config.theme.minimum_contrast = f32::NAN;
        config.scrollback.lines = 100;
        config.window.padding_x = 21;
        config.window.padding_y = 21;
        config.window.width = 4_001;
        config.window.height = 299;
        config.window.sidebar_width = Some(f32::NAN);
        let labels: Vec<_> = validate_settings(&config)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        for expected in [
            "Font Size",
            "Line Height",
            "Window Opacity",
            "Minimum Contrast",
            "Scrollback",
            "Padding X",
            "Padding Y",
            "Window Width",
            "Window Height",
            "Sidebar Width",
        ] {
            assert!(labels.iter().any(|label| label == expected), "{expected}");
        }
    }

    #[test]
    fn numeric_minimum_and_maximum_boundaries_are_inclusive() {
        let mut config = Config::default();
        config.font.size = 8.0;
        config.font.line_height = 1.0;
        config.window.opacity = 0.5;
        config.theme.minimum_contrast = 1.0;
        config.scrollback.lines = 1_000;
        config.window.padding_x = 20;
        config.window.padding_y = 20;
        config.window.width = 400;
        config.window.height = 300;
        config.window.sidebar_width = Some(SIDEBAR_MIN_WIDTH);
        assert!(validate_settings(&config).is_empty());

        config.font.size = 24.0;
        config.font.line_height = 1.5;
        config.theme.minimum_contrast = 12.0;
        config.window.opacity = 1.0;
        config.scrollback.lines = 100_000;
        config.window.width = 4_000;
        config.window.height = 4_000;
        config.window.sidebar_width = Some(SIDEBAR_MAX_WIDTH);
        assert!(validate_settings(&config).is_empty());
    }

    #[test]
    fn invalid_and_canonical_alias_conflicts_block_save_validation() {
        let mut config = Config::default();
        config
            .keybindings
            .insert("not-a-chord".into(), Action::Copy);
        config
            .keybindings
            .insert("cmd+return".into(), Action::NewTab);
        config
            .keybindings
            .insert("super+enter".into(), Action::CloseTab);
        let errors = validate_settings(&config);
        assert!(errors
            .iter()
            .any(|(label, message)| label == "Keybindings" && message.contains("Invalid")));
        assert!(errors
            .iter()
            .any(|(label, message)| label == "Keybindings" && message.contains("conflicts")));
    }

    #[test]
    fn canonical_aliases_for_the_same_action_are_allowed() {
        let mut config = Config::default();
        config
            .keybindings
            .insert("cmd+return".into(), Action::NewTab);
        config
            .keybindings
            .insert("super+enter".into(), Action::NewTab);
        assert!(validate_settings(&config).is_empty());
    }

    #[test]
    fn non_finite_draft_value_recovers_from_fallback_before_adjustment() {
        assert_eq!(adjust_finite_value(f32::NAN, 1, 0.5, 8.0, 24.0, 14.0), 14.5);
        assert_eq!(
            adjust_finite_value(f32::INFINITY, -1, 0.05, 0.5, 1.0, 1.0),
            0.95
        );
        assert_eq!(adjust_finite_value(24.0, 1, 0.5, 8.0, 24.0, 14.0), 24.0);
    }
}
