//! Scene model and hit testing for the Settings panel.
//!
//! Uses a typed `Scene<SettingsTarget>` instead of renderer-retained hit
//! state. Layout comes from `layout::layout_settings`;
//! the renderer's `build_settings_vertices` and the mouse handler share the
//! same geometry product.

use crate::layout::{Rect, SettingsLayout};
use crate::overlay::SettingsTab;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

/// A clickable region of the settings panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsTarget {
    /// F5: A sidebar category entry (Appearance / Terminal / Input / ...).
    SidebarCategory(SettingsTab),
    /// A theme row in the Appearance category (0-based screen-relative index).
    Theme(usize),
    /// Footer "⏎ apply" button.
    ApplyButton,
    /// Footer "esc close" button.
    CloseButton,
    /// Footer "⌘⏎ save" button.
    SaveButton,
    /// v1.5.1: A profile entry in the Settings profile toolbar. The index
    /// is into the per-frame sorted profile view (BTreeMap order). Click
    /// resolves the name from the same view; an out-of-bounds index is a
    /// no-op (safe hit test).
    ProfileEntry(usize),
    /// v1.5.1: The "New" button in the profile toolbar. Creates a new
    /// profile with a default name and switches to it.
    ProfileCreate,
    /// v1.5.1: The "Delete" button in the profile toolbar. Deletes the
    /// currently-active profile. The controller implements a two-click
    /// confirmation: the first click sets a pending-delete state, the
    /// second click within the same Settings session confirms.
    ProfileDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettingsFooterHint {
    pub keys: &'static [&'static str],
    pub description: &'static str,
}

pub(crate) const SETTINGS_FOOTER_HINTS: [SettingsFooterHint; 6] = [
    SettingsFooterHint {
        keys: &["↑", "↓"],
        description: "navigate",
    },
    SettingsFooterHint {
        keys: &["⏎"],
        description: "apply",
    },
    SettingsFooterHint {
        keys: &["⇥"],
        description: "switch",
    },
    SettingsFooterHint {
        keys: &["←", "→"],
        description: "adjust",
    },
    SettingsFooterHint {
        keys: &["esc"],
        description: "close",
    },
    SettingsFooterHint {
        keys: &["⌘", "⏎"],
        description: "save",
    },
];

pub(crate) const KEYCAP_PAD_X_CELLS: f32 = 0.35;
pub(crate) const KEYCAP_GAP_CELLS: f32 = 0.30;
pub(crate) const KEYCAP_DESCRIPTION_GAP_CELLS: f32 = 0.65;
pub(crate) const FOOTER_HINT_GAP_CELLS: f32 = 1.25;

pub(crate) fn settings_keycap_width(key: &str, cell_w: f32) -> f32 {
    let columns = if matches!(key, "↑" | "↓" | "←" | "→") {
        1.0
    } else {
        weft_core::grid::terminal_text_width(key) as f32
    };
    cell_w * (columns + KEYCAP_PAD_X_CELLS * 2.0)
}

pub(crate) fn settings_key_group_width(keys: &[&str], cell_w: f32) -> f32 {
    let key_width: f32 = keys
        .iter()
        .map(|key| settings_keycap_width(key, cell_w))
        .sum();
    key_width + cell_w * KEYCAP_GAP_CELLS * keys.len().saturating_sub(1) as f32
}

pub(crate) fn settings_footer_hint_width(hint: SettingsFooterHint, cell_w: f32) -> f32 {
    let description_cols = weft_core::grid::terminal_text_width(hint.description) as f32;
    settings_key_group_width(hint.keys, cell_w)
        + cell_w * (KEYCAP_DESCRIPTION_GAP_CELLS + description_cols)
}

pub(crate) fn settings_footer_widths(cell_w: f32) -> [f32; 6] {
    SETTINGS_FOOTER_HINTS.map(|hint| settings_footer_hint_width(hint, cell_w))
}

pub(crate) fn settings_value_x(
    base_x: f32,
    content_x0: f32,
    cell_w: f32,
    label_cols: usize,
) -> f32 {
    base_x.max(content_x0 + cell_w * (label_cols as f32 + 1.0))
}

/// Build the settings Scene from a shared layout product + dynamic state.
/// `theme_count` is the number of visible theme rows (already capped by
/// `layout.max_rows`). `tabs` is the ordered list of settings categories.
/// `cell_h` is the physical cell height (drives sidebar row heights).
/// `active_tab` is the currently-selected sidebar category (for highlight).
/// `profile_count` is the number of profile entries to register for hit
/// testing in the v1.5.1 profile toolbar (0 hides the profile entry hits
/// even when the toolbar rect is non-zero — the toolbar still draws the
/// "Base" label and +/- buttons).
pub(crate) fn build_settings_scene(
    layout: &SettingsLayout,
    tabs: &[SettingsTab],
    active_tab: SettingsTab,
    theme_count: usize,
    cell_h: f32,
    profile_count: usize,
) -> Scene<SettingsTarget> {
    let mut scene = Scene::default();
    let [box_x0, _box_y0, _box_x1, _box_y1] = layout.box_rect;

    scene.semantics.push(SemanticNode {
        role: SemanticRole::Dialog,
        label: "Settings".into(),
        bounds: layout.box_rect,
        focus: Some(FocusId::Settings),
        state: String::new(),
    });

    // F5: Sidebar category hit regions (vertical list).
    if layout.show_sidebar {
        for (i, tab) in tabs.iter().enumerate() {
            let row_y = layout.sidebar_top + i as f32 * cell_h;
            let row_rect: Rect = [
                layout.sidebar_rect[0],
                row_y,
                layout.sidebar_rect[2],
                row_y + cell_h,
            ];
            scene.hits.push(HitRegion::from_rect(
                row_rect,
                SettingsTarget::SidebarCategory(*tab),
            ));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::Tab,
                label: tab.label().into(),
                bounds: row_rect,
                focus: Some(FocusId::Settings),
                state: if *tab == active_tab {
                    "selected".into()
                } else {
                    String::new()
                },
            });
        }
    }

    // v1.5.1: Profile toolbar hit regions. The toolbar sits at the top of
    // the content area (above `content_top`). Hit regions:
    // - ProfileEntry(0..profile_count): click to switch to that profile.
    //   The "Base" entry is index 0; profiles follow at 1..=profile_count.
    // - ProfileCreate: the "+" button.
    // - ProfileDelete: the "−" button.
    //
    // The hit regions are only registered when the content area is
    // visible (wide mode or narrow content view). In narrow sidebar-only
    // mode the toolbar is zero-sized and the buttons are None.
    if layout.show_content {
        let [tx0, ty0, tx1, ty1] = layout.profile_toolbar_rect;
        if tx1 > tx0 {
            // Profile entry hits: divide the toolbar (minus the button area
            // on the right) into profile_count equal-width slots. Each slot
            // is a click target. The renderer draws the names; here we only
            // register the geometry.
            if profile_count > 0 {
                let btn_w = cell_h * 1.5;
                let gap = cell_h * 0.5;
                let buttons_total = btn_w * 2.0 + gap;
                let entries_area_w = (tx1 - tx0 - buttons_total).max(0.0);
                let slot_w = entries_area_w / profile_count as f32;
                for i in 0..profile_count {
                    let slot_x0 = tx0 + i as f32 * slot_w;
                    let slot_x1 = slot_x0 + slot_w;
                    scene.hits.push(HitRegion::from_rect(
                        [slot_x0, ty0, slot_x1, ty1],
                        SettingsTarget::ProfileEntry(i),
                    ));
                }
            }
            // + and − buttons.
            if let Some(btn) = layout.profile_create_button {
                scene
                    .hits
                    .push(HitRegion::from_rect(btn, SettingsTarget::ProfileCreate));
            }
            if let Some(btn) = layout.profile_delete_button {
                scene
                    .hits
                    .push(HitRegion::from_rect(btn, SettingsTarget::ProfileDelete));
            }
            scene.semantics.push(SemanticNode {
                role: SemanticRole::List,
                label: "Profile selector".into(),
                bounds: layout.profile_toolbar_rect,
                focus: Some(FocusId::Settings),
                state: String::new(),
            });
        }
    }

    // Theme row hits (Appearance only — theme_count is 0 for other categories).
    if layout.show_content && theme_count > 0 {
        for i in 0..theme_count {
            let row_y = layout.content_top + i as f32 * cell_h;
            let row_rect: Rect = [layout.content_x0, row_y, layout.content_x1, row_y + cell_h];
            scene
                .hits
                .push(HitRegion::from_rect(row_rect, SettingsTarget::Theme(i)));
            scene.semantics.push(SemanticNode {
                role: SemanticRole::ListItem,
                label: format!("Theme {}", i + 1),
                bounds: row_rect,
                focus: None,
                state: String::new(),
            });
        }
    }

    // Footer buttons.
    if let Some(apply) = layout.footer_buttons.apply {
        scene
            .hits
            .push(HitRegion::from_rect(apply, SettingsTarget::ApplyButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Apply settings".into(),
            bounds: apply,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }
    if let Some(close) = layout.footer_buttons.close {
        scene
            .hits
            .push(HitRegion::from_rect(close, SettingsTarget::CloseButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Close settings".into(),
            bounds: close,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }
    if let Some(save) = layout.footer_buttons.save {
        scene
            .hits
            .push(HitRegion::from_rect(save, SettingsTarget::SaveButton));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Button,
            label: "Save settings".into(),
            bounds: save,
            focus: Some(FocusId::Settings),
            state: String::new(),
        });
    }

    // Suppress unused-variable warning for box_x0 (kept for clarity).
    let _ = box_x0;

    scene
}

/// Resolve a physical-pixel point to the topmost settings target.
pub(crate) fn settings_target_at(
    scene: &Scene<SettingsTarget>,
    x: f32,
    y: f32,
) -> Option<SettingsTarget> {
    scene
        .hits
        .iter()
        .find(|hit| hit.contains_half_open(x, y))
        .map(|hit| hit.target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FooterButtonRects;

    const CELL_H: f32 = 20.0;

    fn sample_layout() -> SettingsLayout {
        SettingsLayout {
            box_rect: [200.0, 100.0, 1000.0, 700.0],
            sidebar_rect: [200.0, 140.0, 400.0, 660.0],
            sidebar_width: 200.0,
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 0);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Keybindings, 0, CELL_H, 0);
        // Row 3 (Keybindings) starts at sidebar_top + 3*20 = 140 + 60 = 200
        assert_eq!(
            settings_target_at(&scene, 300.0, 205.0),
            Some(SettingsTarget::SidebarCategory(SettingsTab::Keybindings)),
        );
        assert_eq!(scene.semantics[4].state, "selected");
    }

    #[test]
    fn footer_actions_have_accessibility_semantics_with_shared_bounds() {
        let layout = sample_layout();
        let scene = build_settings_scene(
            &layout,
            &SettingsTab::ALL,
            SettingsTab::Appearance,
            0,
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 5, CELL_H, 0);
        // Row 2 starts at content_top + 2*20 = 200 + 40 = 240
        assert_eq!(
            settings_target_at(&scene, 500.0, 245.0),
            Some(SettingsTarget::Theme(2)),
        );
    }

    #[test]
    fn theme_row_misses_when_sidebar_hidden() {
        // F5: in narrow drill-down sidebar mode, content is hidden so theme
        // rows should not be hit-testable.
        let tabs = SettingsTab::ALL.to_vec();
        let mut layout = sample_layout();
        layout.show_content = false;
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 5, CELL_H, 0);
        // Clicking where a theme row would be returns None (content hidden).
        assert_eq!(settings_target_at(&scene, 500.0, 245.0), None);
    }

    #[test]
    fn footer_button_hit() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 0);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 0);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 3);

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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 2);

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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 3);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H, 0);

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
}
