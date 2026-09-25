//! Pure Settings draft validation shared by the controller and tests.

use std::collections::{HashMap, HashSet};

use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use weft_core::config::{
    Action, Config, ConfigSectionMask, EditorConfig, FontConfig, PasteConfig, RecoveryMode,
    PASTE_SIZE_TIERS_KIB, SCROLLBACK_MAX_LINES, SCROLLBACK_MIN_LINES, SIDEBAR_MAX_WIDTH,
    SIDEBAR_MIN_WIDTH,
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

/// v1.11.5 (PLAN_v1115 §M8): legal Settings cycles for the Advanced page.
/// Notify threshold tiers (seconds, X7). An out-of-tier stored value
/// (hand-edited TOML, e.g. 45) re-anchors at the default (30) so the cycle
/// stays predictable.
pub(crate) const NOTIFY_THRESHOLD_TIERS_SECS: [u64; 4] = [10, 30, 60, 120];

/// v1.11.5 (PLAN_v1115 §M8, reviewer P2-5): single source for the Advanced
/// tab's row count (4 legacy rows + 4 notification/clipboard rows). Every
/// site that lays out, paints, hit-tests, or key-navigates the tab must
/// consume THIS — numeric literals drifted across five call sites were the
/// exact coupling hazard F15 warned about.
pub(crate) const ADVANCED_ROW_COUNT: usize = 8;

/// v1.11.5 (PLAN_v1115 §M8): next notify threshold after cycling `delta`
/// steps through the four tiers (10s/30s/60s/120s).
pub(crate) fn cycled_notify_threshold(current_secs: u64, delta: i32) -> u64 {
    let idx = NOTIFY_THRESHOLD_TIERS_SECS
        .iter()
        .position(|tier| *tier == current_secs)
        .unwrap_or(1); // 30 = default tier
    let next = (idx as i32 + delta).rem_euclid(NOTIFY_THRESHOLD_TIERS_SECS.len() as i32) as usize;
    NOTIFY_THRESHOLD_TIERS_SECS[next]
}

/// v1.11.5 (PLAN_v1115 §M8): next OSC 52 mode after cycling `delta` steps
/// through the three states (default → off → unrestricted). `unrestricted`
/// carries the silent-read warning in the UI rendering, not here.
pub(crate) fn cycled_osc52_mode(
    current: weft_core::config::Osc52Mode,
    delta: i32,
) -> weft_core::config::Osc52Mode {
    use weft_core::config::Osc52Mode;
    const MODES: [Osc52Mode; 3] = [Osc52Mode::Default, Osc52Mode::Off, Osc52Mode::Unrestricted];
    let idx = MODES.iter().position(|m| *m == current).unwrap_or(0);
    let next = (idx as i32 + delta).rem_euclid(MODES.len() as i32) as usize;
    MODES[next]
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

/// v1.12.19 (PLAN_v11217 §3.8 T13a/T13c): adjust the Settings Terminal-page
/// row `row` by `delta` — the write-model twin of the page's painted rows,
/// mirroring [`adjust_input_row`] so the mapping stays headless-tested.
/// Rows 0-3 are the legacy scrollback / padding X / padding Y / contrast
/// rows (row 0's clamp now uses the io-layer constants — T13c); row 4 is
/// the Session recovery cycle. Returns the config section to mark dirty,
/// or `None` for an unknown row.
pub(crate) fn adjust_terminal_row(
    scrollback_lines: &mut usize,
    padding_x: &mut u32,
    padding_y: &mut u32,
    minimum_contrast: &mut f32,
    recovery: &mut RecoveryMode,
    row: usize,
    delta: i32,
) -> Option<ConfigSectionMask> {
    match row {
        0 => {
            // Scrollback: ±1000 lines, clamped to the io-layer range
            // (T13c: the old [1_000, 100_000] range fought the 100..=1M
            // load clamp and tripped the validator above 100k).
            *scrollback_lines = ((*scrollback_lines as i64).saturating_add(delta as i64 * 1000))
                .clamp(SCROLLBACK_MIN_LINES as i64, SCROLLBACK_MAX_LINES as i64)
                as usize;
            Some(ConfigSectionMask::SCROLLBACK)
        }
        1 => {
            // Padding X: ±1 cell, clamped to [0, 20].
            *padding_x = ((*padding_x as i32 + delta).max(0) as u32).min(20);
            Some(ConfigSectionMask::WINDOW)
        }
        2 => {
            // Padding Y: ±1 cell, clamped to [0, 20].
            *padding_y = ((*padding_y as i32 + delta).max(0) as u32).min(20);
            Some(ConfigSectionMask::WINDOW)
        }
        3 => {
            *minimum_contrast = adjust_finite_value(*minimum_contrast, delta, 0.5, 1.0, 12.0, 7.0);
            Some(ConfigSectionMask::THEME)
        }
        4 => {
            *recovery = cycled_recovery_mode(*recovery, delta);
            Some(ConfigSectionMask::SESSION)
        }
        _ => None,
    }
}

/// v1.12.19 (PLAN_v11217 §3.8 T13a): what the startup recovery path does
/// when an unclean-shutdown snapshot exists. Pure decision core for
/// `recovery_controller::run_startup_recovery` — headless-tested below; the
/// controller only performs the parked-snapshot state machine the gate
/// prescribes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryGate {
    /// Show the deferred recovery prompt (factory behavior).
    Prompt,
    /// Skip the prompt; apply `RecoveryChoice::Restore` synchronously.
    AutoRestore,
    /// Skip the prompt; apply `RecoveryChoice::Ignore` synchronously (the
    /// snapshot is superseded by the current session's auto-snapshot, never
    /// deleted — permanent deletion stays a manual action).
    NeverIgnore,
}

/// v1.12.19 (PLAN_v11217 §3.8 T13a): map the user's `[session].recovery`
/// setting onto the startup-recovery action.
pub(crate) fn recovery_gate(mode: RecoveryMode) -> RecoveryGate {
    match mode {
        RecoveryMode::Ask => RecoveryGate::Prompt,
        RecoveryMode::Auto => RecoveryGate::AutoRestore,
        RecoveryMode::Never => RecoveryGate::NeverIgnore,
    }
}

/// v1.12.19 (PLAN_v11217 §3.8 T13a): Terminal-tab value labels for the
/// Session recovery cycle row. Wording note: `never` still performs the
/// NORMAL session-tab restore on launch — it only skips the crash-recovery
/// prompt — so the label must not suggest "fresh start".
pub(crate) fn recovery_mode_label(mode: RecoveryMode) -> &'static str {
    match mode {
        RecoveryMode::Ask => "Ask after unexpected quit",
        RecoveryMode::Auto => "Auto-restore without prompting",
        RecoveryMode::Never => "Skip crash-recovery prompt",
    }
}

/// v1.12.19 (PLAN_v11217 §3.8 T13a): next recovery mode after cycling
/// `delta` steps through ask → auto → never (wraps both ways; out-of-list
/// stored values cannot occur — `RecoveryMode::parse` is never-fail).
pub(crate) fn cycled_recovery_mode(current: RecoveryMode, delta: i32) -> RecoveryMode {
    const MODES: [RecoveryMode; 3] = [RecoveryMode::Ask, RecoveryMode::Auto, RecoveryMode::Never];
    let idx = MODES.iter().position(|m| *m == current).unwrap_or(0);
    let next = (idx as i32 + delta).rem_euclid(MODES.len() as i32) as usize;
    MODES[next]
}

/// v1.12.19 (PLAN_v11217 §3.8 T13b): upper bound for the Settings
/// "Retained limit" row. NEW bound owned by this plan (there is no existing
/// constant — the load path deliberately does not clamp `retained_limit`,
/// the v1.11.2 power-user semantics); 20 000 blocks ≈ 10x the default cap.
pub(crate) const BLOCKS_RETAINED_LIMIT_MAX: usize = 20_000;

/// v1.12.19 (PLAN_v11217 §3.8 T13b): adjust the Settings Blocks-page row
/// `row` by `delta`. Row 0 is the per-tab in-memory retention cap
/// (±50, clamped 0..=[`BLOCKS_RETAINED_LIMIT_MAX`]; 0 disables retention);
/// row 1 is the retained-output cap in MiB (±1, clamped to the io-layer
/// `OUTPUT_CAP_MIN/MAX_MIB` range). Returns the config section to mark
/// dirty, or `None` for an unknown row / an unchanged value.
pub(crate) fn adjust_blocks_row(
    retained_limit: &mut usize,
    output_cap_mib: &mut usize,
    row: usize,
    delta: i32,
) -> Option<ConfigSectionMask> {
    let mask = ConfigSectionMask::BLOCKS;
    match row {
        0 => {
            let next = ((*retained_limit as i64) + delta as i64 * 50)
                .clamp(0, BLOCKS_RETAINED_LIMIT_MAX as i64) as usize;
            if next == *retained_limit {
                None
            } else {
                *retained_limit = next;
                Some(mask)
            }
        }
        1 => {
            let next = ((*output_cap_mib as i64) + delta as i64).clamp(
                weft_core::blocks::OUTPUT_CAP_MIN_MIB as i64,
                weft_core::blocks::OUTPUT_CAP_MAX_MIB as i64,
            ) as usize;
            if next == *output_cap_mib {
                None
            } else {
                *output_cap_mib = next;
                Some(mask)
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
    bindings.sort_by_key(|(left, _)| *left);
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
    // v1.12.19 (PLAN_v11217 §3.8 T13c): align the hard validation with the
    // io-layer clamp constants (100 / 1_000_000) — previously a legal
    // file-side value >100 000 opened the panel already in error, and the
    // ←/→ stepper stopped at the narrower 1_000..=100_000 range.
    if !(SCROLLBACK_MIN_LINES..=SCROLLBACK_MAX_LINES).contains(&config.scrollback.lines) {
        errors.push((
            "Scrollback".into(),
            format!("Must be {SCROLLBACK_MIN_LINES}–{SCROLLBACK_MAX_LINES} lines"),
        ));
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
#[path = "settings_validation/tests.rs"]
mod tests;
