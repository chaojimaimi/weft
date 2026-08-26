//! Pure Settings draft validation shared by the controller and tests.

use std::collections::{HashMap, HashSet};

use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use weft_core::config::{
    Action, Config, ConfigSectionMask, EditorConfig, FontConfig, PasteConfig, PASTE_SIZE_TIERS_KIB,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
};
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

/// v1.11.1 (PLAN_v1111 §4.2): clamp a hand-edited `[paste]` section to safe
/// runtime values. A `size_threshold_kib` outside the legal tier list
/// (hand-edited TOML — the Settings row only cycles the six documented
/// steps) falls back to 16 KiB instead of being rejected, matching the
/// fallback-not-reject philosophy of `runtime_font_config`.
pub(crate) fn runtime_paste_config(config: &PasteConfig) -> PasteConfig {
    let mut safe = config.clone();
    if !PASTE_SIZE_TIERS_KIB.contains(&safe.size_threshold_kib) {
        safe.size_threshold_kib = weft_core::input::DEFAULT_PASTE_SIZE_THRESHOLD_KIB;
    }
    safe
}

/// v1.11.1 (PLAN_v1111 §4.6): next `size_threshold_kib` after cycling `delta`
/// steps through the six legal tiers. An out-of-tier stored value (hand-edited
/// TOML) re-anchors at the default tier first so the cycle stays predictable.
pub(crate) fn cycled_paste_threshold(current_kib: u32, delta: i32) -> u32 {
    let tiers = PASTE_SIZE_TIERS_KIB;
    let idx = tiers
        .iter()
        .position(|tier| *tier == current_kib)
        .unwrap_or_else(|| {
            tiers
                .iter()
                .position(|tier| *tier == weft_core::input::DEFAULT_PASTE_SIZE_THRESHOLD_KIB)
                .unwrap_or(0)
        });
    let next = (idx as i32 + delta).rem_euclid(tiers.len() as i32) as usize;
    tiers[next]
}

/// v1.11.1 (PLAN_v1111 §4.6): the Input page's paste-protection values,
/// grouped so the (already huge) `SettingsDrawParams` /
/// `build_overlay_stack` signatures gain one member instead of three.
#[derive(Clone, Copy)]
pub(crate) struct PasteRowsView {
    pub confirm_large: bool,
    pub confirm_control_chars: bool,
    pub size_threshold_kib: u32,
}

/// v1.11.1 (PLAN_v1111 §4.6): the Input page's five (label, value) rows in
/// display order — the read model whose indices match `adjust_input_row`'s
/// write mapping (asserted together in the tests below).
pub(crate) fn input_page_row_values(
    submit_on_ctrl_enter: bool,
    smart_select: bool,
    paste: PasteRowsView,
) -> [(&'static str, String); 5] {
    let on_off = |on: bool| if on { "On" } else { "Off" };
    [
        (
            "Submit on Ctrl+Enter:",
            on_off(submit_on_ctrl_enter).to_string(),
        ),
        ("Smart Select:", on_off(smart_select).to_string()),
        (
            "Confirm large paste:",
            on_off(paste.confirm_large).to_string(),
        ),
        (
            "Confirm control-char paste:",
            on_off(paste.confirm_control_chars).to_string(),
        ),
        (
            "Paste size threshold:",
            format!("{} KiB", paste.size_threshold_kib),
        ),
    ]
}

/// v1.11.1 (PLAN_v1111 §4.6): adjust the Settings Input-page row `row` by
/// `delta`. Rows 0-1 are the `[editor]` toggles; rows 2-3 are the `[paste]`
/// confirmation toggles; row 4 cycles the size threshold through the legal
/// tiers. Returns the config section to mark dirty, or `None` when the row is
/// unknown or the value did not change.
pub(crate) fn adjust_input_row(
    editor: &mut EditorConfig,
    paste: &mut PasteConfig,
    row: usize,
    delta: i32,
) -> Option<ConfigSectionMask> {
    let toggle = |value: &mut bool| {
        let next = directional_bool(*value, delta);
        if next != *value {
            *value = next;
            true
        } else {
            false
        }
    };
    match row {
        0 => toggle(&mut editor.submit_on_ctrl_enter).then_some(ConfigSectionMask::EDITOR),
        1 => toggle(&mut editor.smart_select).then_some(ConfigSectionMask::EDITOR),
        2 => toggle(&mut paste.confirm_large).then_some(ConfigSectionMask::PASTE),
        3 => toggle(&mut paste.confirm_control_chars).then_some(ConfigSectionMask::PASTE),
        4 => {
            let next = cycled_paste_threshold(paste.size_threshold_kib, delta);
            if next != paste.size_threshold_kib {
                paste.size_threshold_kib = next;
                Some(ConfigSectionMask::PASTE)
            } else {
                None
            }
        }
        _ => None,
    }
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

    // ── v1.11.1 runtime_paste_config (PLAN_v1111 §4.2) ─────────────────

    #[test]
    fn paste_config_every_legal_tier_is_preserved() {
        for kib in PASTE_SIZE_TIERS_KIB {
            let cfg = runtime_paste_config(&PasteConfig {
                size_threshold_kib: kib,
                ..PasteConfig::default()
            });
            assert_eq!(cfg.size_threshold_kib, kib, "tier {kib} must pass through");
        }
    }

    #[test]
    fn paste_config_out_of_tier_thresholds_fall_back_to_16kib_without_mutating_source() {
        for bad in [0u32, 7, 12, 17, 300, u32::MAX] {
            let source = PasteConfig {
                size_threshold_kib: bad,
                confirm_large: false,
                confirm_control_chars: true,
            };
            let safe = runtime_paste_config(&source);
            assert_eq!(safe.size_threshold_kib, 16, "{bad} KiB is not a legal tier");
            assert!(!safe.confirm_large);
            assert!(safe.confirm_control_chars);
            assert_eq!(
                source.size_threshold_kib, bad,
                "the stored config value must not be mutated"
            );
        }
    }

    #[test]
    fn cycled_paste_threshold_walks_all_tiers_and_wraps_both_ways() {
        // Forward from the default walks the documented order.
        assert_eq!(cycled_paste_threshold(16, 1), 32);
        assert_eq!(cycled_paste_threshold(256, 1), 8, "wraps at the top");
        // Backward wraps at the bottom.
        assert_eq!(cycled_paste_threshold(8, -1), 256);
        assert_eq!(cycled_paste_threshold(64, -1), 32);
        // An out-of-tier stored value re-anchors at the default step.
        assert_eq!(cycled_paste_threshold(12, 1), 32);
        assert_eq!(cycled_paste_threshold(12, -1), 8);
    }

    #[test]
    fn adjust_input_rows_map_to_their_config_sections() {
        let mut editor = EditorConfig::default();
        let mut paste = PasteConfig {
            confirm_large: false,
            ..PasteConfig::default()
        };

        // Rows 0-1 are the [editor] toggles.
        assert_eq!(
            adjust_input_row(&mut editor, &mut paste, 0, 1),
            Some(ConfigSectionMask::EDITOR)
        );
        assert!(editor.submit_on_ctrl_enter);
        // Smart Select defaults On, so ← flips it off while → would be a
        // no-op (covered by the noop test below).
        assert_eq!(
            adjust_input_row(&mut editor, &mut paste, 1, -1),
            Some(ConfigSectionMask::EDITOR)
        );
        assert!(!editor.smart_select);

        // Rows 2-3 are the [paste] toggles; row 2 flips its off default on,
        // row 3 flips its On default off.
        assert_eq!(
            adjust_input_row(&mut editor, &mut paste, 2, 1),
            Some(ConfigSectionMask::PASTE)
        );
        assert!(paste.confirm_large);
        assert_eq!(
            adjust_input_row(&mut editor, &mut paste, 3, -1),
            Some(ConfigSectionMask::PASTE)
        );
        assert!(!paste.confirm_control_chars);

        // Row 4 cycles the threshold.
        assert_eq!(
            adjust_input_row(&mut editor, &mut paste, 4, 1),
            Some(ConfigSectionMask::PASTE)
        );
        assert_eq!(paste.size_threshold_kib, 32);
    }

    #[test]
    fn adjust_input_row_is_a_noop_when_the_value_already_matches() {
        let mut editor = EditorConfig::default();
        let mut paste = PasteConfig::default();
        // Smart Select defaults On; → on an already-On row changes nothing.
        assert_eq!(adjust_input_row(&mut editor, &mut paste, 1, 1), None);
        assert!(editor.smart_select);
        // Unknown rows never dirty anything.
        assert_eq!(adjust_input_row(&mut editor, &mut paste, 5, 1), None);
        assert_eq!(adjust_input_row(&mut editor, &mut paste, 99, -1), None);
        assert_eq!(paste, PasteConfig::default());
        assert!(!editor.submit_on_ctrl_enter);
        assert!(editor.smart_select);
    }

    #[test]
    fn input_page_read_model_and_write_model_stay_aligned() {
        // The painted labels and the ←/→ write mapping must cover the same
        // five rows in the same order (paint/settings renders row i from
        // input_page_row_values()[i]; adjust_input_row writes row i).
        let paste = PasteRowsView {
            confirm_large: false,
            confirm_control_chars: true,
            size_threshold_kib: 16,
        };
        let rows = input_page_row_values(false, true, paste);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[4], ("Paste size threshold:", "16 KiB".to_string()));
        for (row, (label, value)) in rows.iter().take(4).enumerate() {
            assert!(
                value == "On" || value == "Off",
                "row {row} ({label}) must render an On/Off value"
            );
            // Each toggle row is adjustable in at least one direction.
            let mut editor = EditorConfig::default();
            let mut paste_cfg = PasteConfig {
                confirm_large: false,
                ..PasteConfig::default()
            };
            let changed = adjust_input_row(&mut editor, &mut paste_cfg, row, -1).is_some()
                || adjust_input_row(&mut editor, &mut paste_cfg, row, 1).is_some();
            assert!(changed, "toggle row {row} ({label}) must be adjustable");
        }
    }
}
