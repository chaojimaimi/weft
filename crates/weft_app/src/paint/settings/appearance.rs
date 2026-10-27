use super::{find_field_error, push_quad, SettingsTarget};
use crate::paint::primitives::push_line;
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// F5: Render the Appearance category content.
    ///
    /// Contains the theme list, Logo Variant, Font Family, Font Size, Line
    /// Height, and Window Opacity. Theme rows show vector-drawn circles
    /// (current = filled accent, others = dim ring). Adjustment rows use the
    /// standard label/value/triangle layout.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_appearance_content(
        &self,
        verts: &mut Vec<f32>,
        s: crate::overlay::SettingsDrawParams<'_>,
        content_top: f32,
        content_x0: f32,
        content_x1: f32,
        value_x: f32,
        cw: f32,
        ch: f32,
        content_cols: usize,
        max_rows: usize,
        bg_uv: [f32; 4],
        selection_bg: [f32; 4],
        fg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        _theme_bg: [f32; 4],
    ) {
        // Reserve 6 rows for the adjustment fields after the theme list
        // (Variant, Font, Size, Line, Opacity, Semantic toggle).
        let theme_count =
            crate::settings_component::visible_appearance_theme_count(s.themes.len(), max_rows);

        // Theme list with vector-drawn circles.
        for (i, theme) in s.themes.iter().take(theme_count).enumerate() {
            let row_y = content_top + i as f32 * ch;
            let is_current = theme.name == s.theme_name;
            let is_selected = i == s.selection;
            // v1.13.17 S2: hover feedback — content-area rows use the shared
            // command-surface hover token; selection wins.
            let is_hovered = crate::settings_component::settings_row_hovered(
                s.hovered,
                SettingsTarget::Theme(i),
                is_selected,
            );
            if is_selected {
                push_quad(
                    verts,
                    [content_x0, row_y, content_x1, row_y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            } else if is_hovered {
                push_quad(
                    verts,
                    [content_x0, row_y, content_x1, row_y + ch],
                    bg_uv,
                    [0.0; 4],
                    self.settings_hover_bg(),
                );
            }
            // Vector circle: filled (current) or dim ring (others).
            let dot_cx = content_x0 + ch * 0.35;
            let dot_cy = row_y + ch * 0.5;
            let dot_r = ch * 0.16;
            let dot_color = if is_current {
                accent
            } else {
                [fg[0] * 0.3, fg[1] * 0.3, fg[2] * 0.3, 1.0]
            };
            let dot_lw = if is_current {
                1.5 * self.scale as f32
            } else {
                1.0 * self.scale as f32
            };
            let segments = 8;
            for s_idx in 0..segments {
                let a0 = s_idx as f32 * std::f32::consts::TAU / segments as f32;
                let a1 = (s_idx + 1) as f32 * std::f32::consts::TAU / segments as f32;
                push_line(
                    verts,
                    dot_cx + dot_r * a0.cos(),
                    dot_cy + dot_r * a0.sin(),
                    dot_cx + dot_r * a1.cos(),
                    dot_cy + dot_r * a1.sin(),
                    dot_lw,
                    dot_color,
                );
            }
            if is_current {
                push_quad(
                    verts,
                    [
                        dot_cx - dot_r * 0.4,
                        dot_cy - dot_r * 0.4,
                        dot_cx + dot_r * 0.4,
                        dot_cy + dot_r * 0.4,
                    ],
                    [0.0; 4],
                    [0.0; 4],
                    accent,
                );
            }
            let label_color = if is_current { accent } else { fg };
            self.push_text(
                verts,
                content_x0 + cw * 2.0,
                row_y,
                &theme.label,
                label_color,
                content_cols,
            );
        }

        // Adjustment rows: Logo Variant, Font Family, Font Size, Line Height,
        // Opacity, Semantic Colors toggle.
        let semantic_label = if s.semantic_output_enabled {
            "On"
        } else {
            "Off"
        };
        let adj_rows: [(String, String, &str); 6] = [
            (
                "Variant:".to_string(),
                s.logo_variant.label().to_string(),
                "",
            ),
            ("Font:".to_string(), s.font_family.to_string(), ""),
            (
                "Size:".to_string(),
                format!("{:.1} pt", s.font_size),
                "Font Size",
            ),
            (
                "Line:".to_string(),
                format!("{:.2}", s.line_height),
                "Line Height",
            ),
            (
                "Opacity:".to_string(),
                format!("{:.2}", s.window_opacity),
                "Window Opacity",
            ),
            (
                "Semantic:".to_string(),
                semantic_label.to_string(),
                "Semantic Output",
            ),
        ];
        for (i, (label, value, err_label)) in adj_rows.iter().enumerate() {
            let row_idx = theme_count + i;
            let row_y = content_top + row_idx as f32 * ch;
            let is_sel = row_idx == s.selection;
            self.push_settings_row(
                verts,
                row_y,
                label,
                value,
                is_sel,
                // v1.13.17 S2: hover follows the scene's ContentRow space
                // (== the selection index space).
                crate::settings_component::settings_row_hovered(
                    s.hovered,
                    SettingsTarget::ContentRow(row_idx),
                    is_sel,
                ),
                content_x0,
                content_x1,
                value_x,
                cw,
                ch,
                content_cols,
                bg_uv,
                selection_bg,
                fg,
                label_c,
                accent,
                if err_label.is_empty() {
                    None
                } else {
                    find_field_error(s.field_errors, err_label)
                },
            );
        }
    }
}
