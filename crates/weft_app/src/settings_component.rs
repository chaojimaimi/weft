//! Scene model and hit testing for the Settings panel.
//!
//! Replaces `renderer.settings_hits: Vec<SettingsHit>` with a typed
//! `Scene<SettingsTarget>`. Layout comes from `layout::layout_settings`;
//! the renderer's `build_settings_vertices` and the mouse handler share the
//! same geometry product.

use crate::layout::{Rect, SettingsLayout};
use crate::overlay::SettingsTab;
use crate::scene::{FocusId, HitRegion, Scene, SemanticNode, SemanticRole};

/// A clickable region of the settings panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsTarget {
    /// A tab-bar entry (Appearance / Font / Keybindings / Window / Logo).
    Tab(SettingsTab),
    /// A theme row in the Appearance tab (0-based screen-relative index).
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
/// `layout.max_rows`). `tabs` is the ordered list of settings tabs.
/// `cell_h` is the physical cell height (drives tab/theme row heights).
pub(crate) fn build_settings_scene(
    layout: &SettingsLayout,
    tabs: &[SettingsTab],
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

    // Tab bar hits.
    for (i, tab) in tabs.iter().enumerate() {
        let tx0 = box_x0 + i as f32 * layout.tab_width;
        let tab_rect: Rect = [
            tx0,
            layout.tab_bar_y,
            tx0 + layout.tab_width,
            layout.tab_bar_y + cell_h,
        ];
        scene
            .hits
            .push(HitRegion::from_rect(tab_rect, SettingsTarget::Tab(*tab)));
        scene.semantics.push(SemanticNode {
            role: SemanticRole::Tab,
            label: tab.label().into(),
            bounds: tab_rect,
            focus: Some(FocusId::Settings),
        });
    }

    // Theme row hits (Appearance only — theme_count is 0 for other tabs).
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
            tab_bar_y: 156.0,
            tab_width: 160.0,
            content_x0: 212.0,
            content_x1: 988.0,
            content_top: 200.0,
            content_bottom: 660.0,
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
    fn tab_hit_returns_correct_variant() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, 0, CELL_H);
        // Tab 1 starts at box_x0 + 1*tab_width = 200 + 160 = 360
        assert_eq!(
            settings_target_at(&scene, 400.0, 160.0),
            Some(SettingsTarget::Tab(SettingsTab::Font)),
        );
    }

    #[test]
    fn theme_row_hit() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, 5, CELL_H);
        // Row 2 starts at content_top + 2*20 = 200 + 40 = 240
        assert_eq!(
            settings_target_at(&scene, 300.0, 245.0),
            Some(SettingsTarget::Theme(2)),
        );
    }

    #[test]
    fn footer_button_hit() {
        let tabs = SettingsTab::ALL.to_vec();
        let layout = sample_layout();
        let scene = build_settings_scene(&layout, &tabs, 0, CELL_H);
        assert_eq!(
            settings_target_at(&scene, 250.0, 680.0),
            Some(SettingsTarget::ApplyButton),
        );
    }
}
