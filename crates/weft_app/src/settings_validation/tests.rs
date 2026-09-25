//! Settings validation tests (split from settings_validation.rs to keep the
//! production file within its architecture-gate budget; child-module privacy
//! reaches private items exactly like the inline module did).

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
    // v1.12.19 (T13c): 100 is now the LEGAL floor (io clamp
    // SCROLLBACK_MIN_LINES) — the invalid sample moved below it.
    config.scrollback.lines = 99;
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
    // v1.12.19 (T13c): the floor is now the io clamp constant (100).
    config.scrollback.lines = SCROLLBACK_MIN_LINES;
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
    // v1.12.19 (T13c): the ceiling is now the io clamp constant
    // (1_000_000) — file-side legal values beyond the old 100 000 no
    // longer open the panel pre-loaded with a field error.
    config.scrollback.lines = SCROLLBACK_MAX_LINES;
    config.window.width = 4_000;
    config.window.height = 4_000;
    config.window.sidebar_width = Some(SIDEBAR_MAX_WIDTH);
    assert!(validate_settings(&config).is_empty());
}

/// v1.12.19 (PLAN_v11217 §3.8 T13c): the panel-side hard validation and
/// the io-layer clamp must share one range — both boundary values and
/// one sample of each formerly-rejected region.
#[test]
fn scrollback_validation_matches_the_io_clamp_range() {
    for legal in [
        SCROLLBACK_MIN_LINES,
        1_000,
        100_000,
        500_000,
        SCROLLBACK_MAX_LINES,
    ] {
        let mut config = Config::default();
        config.scrollback.lines = legal;
        assert!(
            validate_settings(&config).is_empty(),
            "{legal} lines must validate"
        );
    }
    for illegal in [0, SCROLLBACK_MIN_LINES - 1, SCROLLBACK_MAX_LINES + 1] {
        let mut config = Config::default();
        config.scrollback.lines = illegal;
        let errors = validate_settings(&config);
        assert!(
            errors.iter().any(|(label, _)| label == "Scrollback"),
            "{illegal} lines must be rejected"
        );
    }
}

// ── v1.12.19 (PLAN_v11217 §3.8 T13a/T13b): session + blocks rows ───

/// The recovery-gate truth table pinned by the plan: ask → prompt,
/// auto → synchronous restore, never → synchronous ignore.
#[test]
fn recovery_gate_truth_table() {
    use RecoveryMode::{Ask, Auto, Never};
    assert_eq!(recovery_gate(Ask), RecoveryGate::Prompt);
    assert_eq!(recovery_gate(Auto), RecoveryGate::AutoRestore);
    assert_eq!(recovery_gate(Never), RecoveryGate::NeverIgnore);
}

/// The Settings cycle walks ask → auto → never and wraps both ways.
#[test]
fn recovery_mode_cycles_through_three_states() {
    assert_eq!(
        cycled_recovery_mode(RecoveryMode::Ask, 1),
        RecoveryMode::Auto
    );
    assert_eq!(
        cycled_recovery_mode(RecoveryMode::Auto, 1),
        RecoveryMode::Never
    );
    assert_eq!(
        cycled_recovery_mode(RecoveryMode::Never, 1),
        RecoveryMode::Ask,
        "wrap around"
    );
    assert_eq!(
        cycled_recovery_mode(RecoveryMode::Ask, -1),
        RecoveryMode::Never,
        "wrap around backwards"
    );
    assert_eq!(
        cycled_recovery_mode(RecoveryMode::Auto, 0),
        RecoveryMode::Auto
    );
}

/// The painted labels are the review-mandated wording; `never` must not
/// suggest a "fresh start" (normal session-tab restore still happens).
#[test]
fn recovery_mode_labels_use_the_specified_wording() {
    assert_eq!(
        recovery_mode_label(RecoveryMode::Ask),
        "Ask after unexpected quit"
    );
    assert_eq!(
        recovery_mode_label(RecoveryMode::Auto),
        "Auto-restore without prompting"
    );
    assert_eq!(
        recovery_mode_label(RecoveryMode::Never),
        "Skip crash-recovery prompt"
    );
}

/// Blocks-row truth table: retained limit ±50 clamped 0..=20 000,
/// output cap ±1 clamped 1..=64, both rows mark BLOCKS dirty, unknown
/// rows are a no-op.
#[test]
fn adjust_blocks_row_truth_table() {
    // Row 0 — retained limit.
    let (mut retained, mut cap) = (2_000usize, 1usize);
    assert_eq!(
        adjust_blocks_row(&mut retained, &mut cap, 0, 1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(retained, 2_050);
    assert_eq!(
        adjust_blocks_row(&mut retained, &mut cap, 0, -1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(retained, 2_000);
    // Boundaries: 0 (Unlimited) and 20 000.
    let (mut r0, mut c) = (20usize, 1usize);
    assert_eq!(
        adjust_blocks_row(&mut r0, &mut c, 0, -1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(r0, 0, "floor clamps at 0 (Unlimited)");
    assert_eq!(
        adjust_blocks_row(&mut r0, &mut c, 0, -1),
        None,
        "stays at floor"
    );
    let (mut rmax, _) = (19_975usize, 1usize);
    assert_eq!(
        adjust_blocks_row(&mut rmax, &mut c, 0, 1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(rmax, BLOCKS_RETAINED_LIMIT_MAX, "ceiling is 20 000");
    assert_eq!(
        adjust_blocks_row(&mut rmax, &mut c, 0, 1),
        None,
        "stays at ceiling"
    );

    // Row 1 — output cap, ±1, clamped 1..=64.
    let (mut retained2, mut cap1) = (100usize, 8usize);
    assert_eq!(
        adjust_blocks_row(&mut retained2, &mut cap1, 1, 1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(cap1, 9);
    assert_eq!(
        adjust_blocks_row(&mut retained2, &mut cap1, 1, -1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(cap1, 8);
    let (mut r, mut cmin) = (100usize, 2usize);
    assert_eq!(
        adjust_blocks_row(&mut r, &mut cmin, 1, -1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(
        cmin,
        weft_core::blocks::OUTPUT_CAP_MIN_MIB,
        "floor is 1 MiB"
    );
    assert_eq!(
        adjust_blocks_row(&mut r, &mut cmin, 1, -1),
        None,
        "already at floor → no-op"
    );
    let (mut r2, mut cmax) = (100usize, 63usize);
    assert_eq!(
        adjust_blocks_row(&mut r2, &mut cmax, 1, 1),
        Some(ConfigSectionMask::BLOCKS)
    );
    assert_eq!(
        cmax,
        weft_core::blocks::OUTPUT_CAP_MAX_MIB,
        "ceiling is 64 MiB"
    );
    assert_eq!(
        adjust_blocks_row(&mut r2, &mut cmax, 1, 1),
        None,
        "already at ceiling → no-op"
    );

    // Unknown rows never dirty anything.
    let (mut r3, mut c3) = (100usize, 1usize);
    assert_eq!(adjust_blocks_row(&mut r3, &mut c3, 2, 1), None);
    assert_eq!(adjust_blocks_row(&mut r3, &mut c3, 99, -1), None);
    assert_eq!((r3, c3), (100, 1));
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

/// Terminal-row write model: row 0 clamps to the io-layer scrollback
/// range (T13c), rows 1-2 nudge padding, row 3 nudges contrast, row 4
/// cycles session recovery, unknown rows are a no-op.
#[test]
fn adjust_terminal_row_truth_table() {
    let adjust =
        |lines: &mut usize,
         px: &mut u32,
         py: &mut u32,
         mc: &mut f32,
         rec: &mut RecoveryMode,
         row,
         delta| { adjust_terminal_row(lines, px, py, mc, rec, row, delta) };
    // Row 0 — scrollback ±1000 within the io range.
    let (mut lines, mut px, mut py, mut mc, mut rec) =
        (10_000usize, 0u32, 0u32, 7.0f32, RecoveryMode::Ask);
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 0, 1),
        Some(ConfigSectionMask::SCROLLBACK)
    );
    assert_eq!(lines, 11_000);
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 0, -1),
        Some(ConfigSectionMask::SCROLLBACK)
    );
    assert_eq!(lines, 10_000);
    // The T13c bounds: 100 is reachable from below and 1_000_000 from
    // above (the old panel range rejected both).
    let (mut lo, mut hi) = (150usize, 999_500usize);
    let (mut p, mut q) = (0u32, 0u32);
    let (mut a, mut b) = (7.0f32, 7.0f32);
    let (mut r1, mut r2) = (RecoveryMode::Ask, RecoveryMode::Ask);
    adjust(&mut lo, &mut p, &mut q, &mut a, &mut r1, 0, -1);
    adjust(&mut hi, &mut p, &mut q, &mut b, &mut r2, 0, 1);
    assert_eq!(lo, SCROLLBACK_MIN_LINES, "floor is the io constant");
    assert_eq!(hi, SCROLLBACK_MAX_LINES, "ceiling is the io constant");

    // Rows 1-2 — padding ±1 clamped at 0.
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 1, 1),
        Some(ConfigSectionMask::WINDOW)
    );
    assert_eq!(px, 1);
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 2, -1),
        Some(ConfigSectionMask::WINDOW)
    );
    assert_eq!(py, 0, "padding clamps at 0");

    // Row 4 — session recovery cycle marks SESSION.
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 4, 1),
        Some(ConfigSectionMask::SESSION)
    );
    assert_eq!(rec, RecoveryMode::Auto);

    // Unknown rows never dirty anything.
    assert_eq!(
        adjust(&mut lines, &mut px, &mut py, &mut mc, &mut rec, 5, 1),
        None
    );
}

// ── v1.11.5 (PLAN_v1115 §M8): Advanced-page cycles ────────────────

#[test]
fn notify_threshold_cycles_through_four_tiers() {
    // 30 → 60 (right) / 30 → 10 (left), wrapping both ends.
    assert_eq!(cycled_notify_threshold(30, 1), 60);
    assert_eq!(cycled_notify_threshold(30, -1), 10);
    assert_eq!(cycled_notify_threshold(120, 1), 10, "wrap around top");
    assert_eq!(cycled_notify_threshold(10, -1), 120, "wrap around bottom");
    assert_eq!(cycled_notify_threshold(30, 0), 30, "no-op");
    // Hand-edited out-of-tier value re-anchors at the default (30).
    assert_eq!(cycled_notify_threshold(45, 1), 60);
    assert_eq!(cycled_notify_threshold(0, 1), 60);
    // The exact tiers the Settings UI promises.
    assert_eq!(NOTIFY_THRESHOLD_TIERS_SECS, [10, 30, 60, 120]);
}

#[test]
fn osc52_mode_cycles_through_three_states() {
    use weft_core::config::Osc52Mode;
    assert_eq!(cycled_osc52_mode(Osc52Mode::Default, 1), Osc52Mode::Off);
    assert_eq!(
        cycled_osc52_mode(Osc52Mode::Off, 1),
        Osc52Mode::Unrestricted
    );
    assert_eq!(
        cycled_osc52_mode(Osc52Mode::Unrestricted, 1),
        Osc52Mode::Default,
        "wrap around"
    );
    assert_eq!(
        cycled_osc52_mode(Osc52Mode::Default, -1),
        Osc52Mode::Unrestricted,
        "wrap around backwards"
    );
    assert_eq!(cycled_osc52_mode(Osc52Mode::Off, 0), Osc52Mode::Off);
}
