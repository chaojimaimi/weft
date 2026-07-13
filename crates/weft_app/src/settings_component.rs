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

/// Build the settings Scene from a shared layout product + dynamic state.
/// `theme_count` is the number of visible theme rows (already capped by
/// `layout.max_rows`). `tabs` is the ordered list of settings categories.
/// `cell_h` is the physical cell height (drives sidebar row heights).
/// `active_tab` is the currently-selected sidebar category (for highlight).
pub(crate) fn build_settings_scene(
    layout: &SettingsLayout,
    tabs: &[SettingsTab],
    _active_tab: SettingsTab,
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
            });
        }
    }

    // Footer buttons.
    if let Some(apply) = layout.footer_buttons.apply {
        scene
            .hits
            .push(HitRegion::from_rect(apply, SettingsTarget::ApplyButton));
    }
    if let Some(close) = layout.footer_buttons.close {
        scene
            .hits
            .push(HitRegion::from_rect(close, SettingsTarget::CloseButton));
    }
    if let Some(save) = layout.footer_buttons.save {
        scene
            .hits
            .push(HitRegion::from_rect(save, SettingsTarget::SaveButton));
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
}
