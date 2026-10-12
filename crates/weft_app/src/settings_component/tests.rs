//! Settings scene/hit-test tests, moved verbatim out of `settings_component.rs`
//! (v1.13.9 backlog clear zero-behavior file-budget split; repo
//! test-module convention — gate-exempt sibling module).

use super::*;
use crate::layout::FooterButtonRects;

const CELL_H: f32 = 20.0;

fn sample_layout() -> SettingsLayout {
    SettingsLayout {
        box_rect: [200.0, 100.0, 1000.0, 700.0],
        sidebar_rect: [200.0, 140.0, 400.0, 660.0],
        sidebar_top: 140.0,
        show_sidebar: true,
        show_content: true,
        tab_bar_y: 140.0,
        tab_width: CELL_H,
        content_x0: 412.0,
        content_x1: 988.0,
        // v1.5.1: content_top is pushed down by the profile toolbar
        // (toolbar at 180-200, form rows start at 200).
        content_top: 200.0,
        max_rows: 20,
        footer_y: 670.0,
        footer_buttons: FooterButtonRects {
            apply: Some([212.0, 670.0, 280.0, 690.0]),
            close: Some([400.0, 670.0, 460.0, 690.0]),
            save: Some([600.0, 670.0, 680.0, 690.0]),
        },
        // v1.5.1: profile toolbar sits above content_top.
        profile_toolbar_rect: [412.0, 180.0, 988.0, 200.0],
        profile_create_button: Some([938.0, 180.0, 958.0, 200.0]),
        profile_delete_button: Some([968.0, 180.0, 988.0, 200.0]),
    }
}

#[test]
fn sidebar_category_hit_returns_correct_variant() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 0, 0),
        CELL_H,
        0,
    );
    // Row 1 (Terminal) starts at sidebar_top + 1*20 = 140 + 20 = 160
    assert_eq!(
        settings_target_at(&scene, 300.0, 165.0),
        Some(SettingsTarget::SidebarCategory(SettingsTab::Terminal)),
    );
}

#[test]
fn active_sidebar_category_highlighted() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Keybindings,
        (0, 0, 0),
        CELL_H,
        0,
    );
    // Row 4 (Keybindings; v1.12.19 inserted Blocks) = 140 + 80 = 220.
    assert_eq!(
        settings_target_at(&scene, 300.0, 225.0),
        Some(SettingsTarget::SidebarCategory(SettingsTab::Keybindings)),
    );
    assert_eq!(scene.semantics[5].state, "selected");
}

#[test]
fn footer_actions_have_accessibility_semantics_with_shared_bounds() {
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &SettingsTab::ALL,
        SettingsTab::Appearance,
        (0, 0, 0),
        CELL_H,
        0,
    );
    for (label, bounds) in [
        ("Apply settings", layout.footer_buttons.apply.unwrap()),
        ("Close settings", layout.footer_buttons.close.unwrap()),
        ("Save settings", layout.footer_buttons.save.unwrap()),
    ] {
        let semantic = scene
            .semantics
            .iter()
            .find(|node| node.label == label)
            .expect("footer action must be exposed to accessibility");
        assert_eq!(semantic.bounds, bounds);
    }
}

#[test]
fn theme_row_hit() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (5, 11, 0),
        CELL_H,
        0,
    );
    // Row 2 starts at content_top + 2*20 = 200 + 40 = 240
    assert_eq!(
        settings_target_at(&scene, 500.0, 245.0),
        Some(SettingsTarget::Theme(2)),
    );
}

#[test]
fn appearance_row_budget_is_shared_by_themes_and_adjustments() {
    assert_eq!(visible_appearance_theme_count(11, 16), 10);
    assert_eq!(visible_appearance_theme_count(11, 6), 0);
    assert_eq!(visible_appearance_theme_count(4, 20), 4);
}

#[test]
fn appearance_adjustment_rows_are_mouse_selectable() {
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &SettingsTab::ALL,
        SettingsTab::Appearance,
        (5, 11, 0),
        CELL_H,
        0,
    );
    let content_x = (layout.content_x0 + layout.content_x1) / 2.0;
    assert_eq!(
        settings_target_at(&scene, content_x, layout.content_top + CELL_H * 10.5),
        Some(SettingsTarget::ContentRow(10)),
    );
}

#[test]
fn theme_row_misses_when_sidebar_hidden() {
    // F5: in narrow drill-down sidebar mode, content is hidden so theme
    // rows should not be hit-testable.
    let tabs = SettingsTab::ALL.to_vec();
    let mut layout = sample_layout();
    layout.show_content = false;
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (5, 11, 0),
        CELL_H,
        0,
    );
    // Clicking where a theme row would be returns None (content hidden).
    assert_eq!(settings_target_at(&scene, 500.0, 245.0), None);
}

#[test]
fn footer_button_hit() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        0,
    );
    assert_eq!(
        settings_target_at(&scene, 250.0, 680.0),
        Some(SettingsTarget::ApplyButton),
    );
}

#[test]
fn narrow_drill_down_hides_sidebar_categories() {
    // F5: in narrow content mode, sidebar is hidden so category clicks
    // should not register.
    let tabs = SettingsTab::ALL.to_vec();
    let mut layout = sample_layout();
    layout.show_sidebar = false;
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        0,
    );
    // Clicking where a sidebar row would be returns None (sidebar hidden).
    assert_eq!(settings_target_at(&scene, 300.0, 165.0), None);
}

#[test]
fn settings_value_starts_after_long_label() {
    assert_eq!(settings_value_x(140.0, 10.0, 10.0, 20), 220.0);
    assert_eq!(settings_value_x(240.0, 10.0, 10.0, 5), 240.0);
}

#[test]
fn footer_keycaps_use_uniform_pair_spacing() {
    let cell_w = 10.0;
    assert_eq!(
        settings_key_group_width(&["↑", "↓"], cell_w),
        settings_key_group_width(&["←", "→"], cell_w)
    );
    assert_eq!(settings_footer_widths(cell_w).len(), 6);
    assert_eq!(
        settings_keycap_width("↑", cell_w),
        settings_keycap_width("↓", cell_w)
    );
    assert!(settings_key_group_width(&["⌘", "⏎"], cell_w) > cell_w * 2.0);
}

// ── v1.5.1: Profile toolbar hit tests ────────────────────────────

#[test]
fn profile_entry_hit_returns_indexed_target() {
    // With 3 profiles (Base + 2), the toolbar should register
    // ProfileEntry(0), ProfileEntry(1), ProfileEntry(2) hit regions
    // equally spaced across the entries area.
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        3,
    );

    // Compute slot bounds to verify each entry is hit-testable.
    // Toolbar: [412.0, 180.0, 988.0, 200.0] → 576 wide.
    // Buttons: 2 × (1.5 × 20) + 0.5 × 20 = 70 px → entries area = 506 px.
    // 3 slots → each ~168.7 px wide.
    let btn_w = CELL_H * 1.5;
    let gap = CELL_H * 0.5;
    let buttons_total = btn_w * 2.0 + gap;
    let entries_area_w = (988.0 - 412.0 - buttons_total).max(0.0);
    let slot_w = entries_area_w / 3.0;

    // Center of each slot.
    for i in 0..3 {
        let cx = 412.0 + i as f32 * slot_w + slot_w * 0.5;
        assert_eq!(
            settings_target_at(&scene, cx, 190.0),
            Some(SettingsTarget::ProfileEntry(i)),
            "ProfileEntry({i}) should be hit-testable at center ({cx}, 190)"
        );
    }
}

#[test]
fn profile_create_and_delete_buttons_hit_testable() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        2,
    );

    // "+" button at [938.0, 180.0, 958.0, 200.0]
    assert_eq!(
        settings_target_at(&scene, 948.0, 190.0),
        Some(SettingsTarget::ProfileCreate),
    );
    // "−" button at [968.0, 180.0, 988.0, 200.0]
    assert_eq!(
        settings_target_at(&scene, 978.0, 190.0),
        Some(SettingsTarget::ProfileDelete),
    );
}

#[test]
fn profile_toolbar_skipped_when_content_hidden() {
    // In narrow sidebar-only mode (show_content = false), the profile
    // toolbar must not register any hit regions even if profile_count
    // is non-zero — the caller skips the toolbar entirely.
    let tabs = SettingsTab::ALL.to_vec();
    let mut layout = sample_layout();
    layout.show_content = false;
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        3,
    );
    // Clicking where the toolbar would be returns None.
    assert_eq!(settings_target_at(&scene, 500.0, 190.0), None);
    assert_eq!(settings_target_at(&scene, 948.0, 190.0), None);
}

#[test]
fn profile_entry_zero_count_skips_entry_hits_but_keeps_buttons() {
    // When profile_count = 0, no ProfileEntry hit regions are
    // registered, but the +/- buttons remain (the toolbar still
    // draws the "Base" label and buttons).
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(
        &layout,
        &tabs,
        SettingsTab::Appearance,
        (0, 6, 0),
        CELL_H,
        0,
    );

    // No ProfileEntry hits — clicking in the entries area returns None
    // (the toolbar draws the Base label but it's not clickable as a
    // profile entry when there are no profiles).
    assert_eq!(
        settings_target_at(&scene, 500.0, 190.0),
        None,
        "ProfileEntry(0) should NOT be registered when profile_count = 0"
    );
    // But the + and − buttons ARE registered.
    assert_eq!(
        settings_target_at(&scene, 948.0, 190.0),
        Some(SettingsTarget::ProfileCreate),
    );
    assert_eq!(
        settings_target_at(&scene, 978.0, 190.0),
        Some(SettingsTarget::ProfileDelete),
    );
}

/// v1.5.2: When the Advanced tab is active, the Import Config and
/// Export Config rows (at content_top + 2*cell_h and + 3*cell_h)
/// must register `AdvancedImport` / `AdvancedExport` hit targets.
/// The first two rows (Debug Logging, Experimental) are not
/// clickable — Enter on them is a no-op, matching pre-v1.5.2
/// behavior.
#[test]
fn advanced_tab_registers_import_export_hit_regions() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(&layout, &tabs, SettingsTab::Advanced, (0, 4, 0), CELL_H, 0);
    // sample_layout() has content_top = 200, CELL_H = 20.
    // Row 0 (Debug Logging): y = 200 — no hit region.
    // Row 1 (Experimental): y = 220 — no hit region.
    // Row 2 (Import Config):  y = 240 — AdvancedImport.
    // Row 3 (Export Config): y = 260 — AdvancedExport.
    let content_x = (layout.content_x0 + layout.content_x1) / 2.0;
    assert_eq!(
        settings_target_at(&scene, content_x, 205.0),
        None,
        "Debug Logging row should not be clickable"
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 225.0),
        None,
        "Experimental row should not be clickable"
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 245.0),
        Some(SettingsTarget::AdvancedImport),
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 265.0),
        Some(SettingsTarget::AdvancedExport),
    );
}

/// v1.11.5 (PLAN_v1115 §M8): the Advanced tab's new rows 4-7 (Notify
/// Enabled / Threshold / Sound / OSC52 Clipboard) register ContentRow
/// hit targets so click-to-select works like every standard row.
#[test]
fn advanced_rows_4_to_7_register_content_row_hits() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(&layout, &tabs, SettingsTab::Advanced, (0, 8, 0), CELL_H, 0);
    let content_x = (layout.content_x0 + layout.content_x1) / 2.0;
    // Labels sit above cell centers in sample_layout (content_top=200):
    // row 4 → y≈285, row 5 → y≈305, row 6 → y≈325, row 7 → y≈345.
    assert_eq!(
        settings_target_at(&scene, content_x, 285.0),
        Some(SettingsTarget::ContentRow(4)),
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 305.0),
        Some(SettingsTarget::ContentRow(5)),
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 325.0),
        Some(SettingsTarget::ContentRow(6)),
    );
    assert_eq!(
        settings_target_at(&scene, content_x, 345.0),
        Some(SettingsTarget::ContentRow(7)),
    );
}

/// v1.5.2: Non-Advanced tabs must NOT register AdvancedImport /
/// AdvancedExport hit regions (the action rows only exist in the
/// Advanced category). This guards against the hit regions leaking
/// into other categories when the active tab changes.
#[test]
fn non_advanced_tabs_do_not_register_import_export_hits() {
    let tabs = SettingsTab::ALL.to_vec();
    let layout = sample_layout();
    let scene = build_settings_scene(&layout, &tabs, SettingsTab::Terminal, (0, 4, 0), CELL_H, 0);
    let content_x = (layout.content_x0 + layout.content_x1) / 2.0;
    // Rows 2 and 3 in the Terminal tab are Padding X and Padding Y —
    // they must not be mapped to AdvancedImport / AdvancedExport.
    assert_ne!(
        settings_target_at(&scene, content_x, 245.0),
        Some(SettingsTarget::AdvancedImport)
    );
    assert_ne!(
        settings_target_at(&scene, content_x, 265.0),
        Some(SettingsTarget::AdvancedExport)
    );
}
