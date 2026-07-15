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
        unicode_width::UnicodeWidthStr::width_cjk(key) as f32
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
    let description_cols = unicode_width::UnicodeWidthStr::width_cjk(hint.description) as f32;
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
pub(crate) fn build_settings_scene(
    layout: &SettingsLayout,
    tabs: &[SettingsTab],
    active_tab: SettingsTab,
    theme_count: usize,
    cell_h: f32,
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
            content_top: 200.0,
            max_rows: 20,
            footer_y: 670.0,
            footer_buttons: FooterButtonRects {
                apply: Some([212.0, 670.0, 280.0, 690.0]),
                close: Some([400.0, 670.0, 460.0, 690.0]),
                save: Some([600.0, 670.0, 680.0, 690.0]),
            },
        }
    }

    #[test]
    fn sidebar_category_hit_returns_correct_variant() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Keybindings, 0, CELL_H);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 5, CELL_H);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 5, CELL_H);
        // Clicking where a theme row would be returns None (content hidden).
        assert_eq!(settings_target_at(&scene, 500.0, 245.0), None);
    }

    #[test]
    fn footer_button_hit() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H);
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
        let scene = build_settings_scene(&layout, &tabs, SettingsTab::Appearance, 0, CELL_H);
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
}
