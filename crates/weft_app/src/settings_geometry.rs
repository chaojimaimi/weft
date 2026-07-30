//! Settings-only geometry and hit testing kept out of the shared controller.

use super::App;

impl App {
    pub(super) fn settings_target_at(
        &self,
        x: f32,
        y: f32,
    ) -> Option<crate::settings_component::SettingsTarget> {
        if !self.settings.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f32;
        let ch = renderer.cell_height() as f32;
        let (width, height) = renderer.viewport();
        let footer_widths = crate::settings_component::settings_footer_widths(cw);
        let layout = crate::layout::layout_settings(
            width,
            height,
            cw,
            ch,
            crate::overlay::SettingsTab::ALL.len(),
            self.settings.error.is_some(),
            &footer_widths,
            self.settings_is_narrow(),
            self.settings.drill_down,
        )?;
        let total_rows = self.settings_tab_row_count();
        let row_window = crate::settings_component::settings_scene_row_window(
            self.settings.tab,
            total_rows,
            self.settings_theme_views().len(),
            layout.max_rows,
            self.settings.scroll_offset,
        );
        let scene = crate::settings_component::build_settings_scene(
            &layout,
            crate::overlay::SettingsTab::ALL.as_slice(),
            self.settings.tab,
            row_window,
            ch,
            self.profile_names_sorted().len() + 1,
        );
        crate::settings_component::settings_target_at(&scene, x, y)
    }

    pub(super) fn point_inside_settings_box(&self, x: f32, y: f32) -> bool {
        let Some(layout) = self.settings_layout(&[0.0; 6]) else {
            return false;
        };
        let [left, top, right, bottom] = layout.box_rect;
        x >= left && x < right && y >= top && y < bottom
    }

    pub(super) fn settings_max_visible_rows(&self) -> usize {
        self.settings_layout(&[0.0; 6])
            .map_or(usize::MAX, |layout| layout.max_rows)
    }

    fn settings_layout(&self, footer_widths: &[f32; 6]) -> Option<crate::layout::SettingsLayout> {
        if !self.settings.open {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let (width, height) = renderer.viewport();
        crate::layout::layout_settings(
            width,
            height,
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
            crate::overlay::SettingsTab::ALL.len(),
            self.settings.error.is_some(),
            footer_widths,
            self.settings_is_narrow(),
            self.settings.drill_down,
        )
    }
}
