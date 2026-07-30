//! v1.5.1: Profile toolbar drawing for the Settings panel.
//!
//! Extracted from `paint/settings.rs` to keep that file within its
//! architecture-gate budget. Draws the profile selector toolbar at the
//! top of the Settings content area: the "Base" entry, sorted profile
//! names, and the "+" / "−" buttons.
//!
//! Layout (matches `layout::layout_settings` geometry):
//! ```text
//! ┌────────────────────────────────────────────┬─────┬─────┐
//! │ Base  work  presentation                    │  +  │  −  │
//! └────────────────────────────────────────────┴─────┴─────┘
//! ```
//!
//! The active entry gets an accent-colored underline. The "+" button
//! creates a new profile; "−" deletes the active one (two-click confirm
//! handled by `profiles_controller`).

use crate::layout::Rect;
use crate::overlay::SettingsProfileView;
use crate::paint::primitives::push_quad;
use crate::renderer::MetalRenderer;

fn toolbar_rule_rect(x0: f32, x1: f32, bottom: f32) -> Rect {
    let y1 = bottom.round();
    [x0, y1 - 1.0, x1, y1]
}

impl MetalRenderer {
    /// v1.5.1: Draw the profile selector toolbar at the top of the Settings
    /// content area. The toolbar is a single row of clickable entries
    /// (Base + sorted profile names) followed by "+" and "−" buttons on the
    /// right edge.
    ///
    /// The toolbar geometry comes from `layout.profile_toolbar_rect`,
    /// `layout.profile_create_button`, and `layout.profile_delete_button`.
    /// When the content area is hidden (narrow sidebar-only mode), the
    /// caller skips this function entirely.
    ///
    /// Colors are passed in from the parent `build_settings_vertices` to
    /// avoid recomputing theme tokens here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_profile_toolbar(
        &self,
        verts: &mut Vec<f32>,
        profiles: &[SettingsProfileView<'_>],
        toolbar_rect: Rect,
        create_btn: Option<Rect>,
        delete_btn: Option<Rect>,
        cw: f32,
        ch: f32,
        bg_uv: [f32; 4],
        theme_bg: [f32; 4],
        fg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        separator: [f32; 4],
    ) {
        let [tx0, ty0, tx1, ty1] = toolbar_rect;
        if tx1 <= tx0 || ty1 <= ty0 || profiles.is_empty() {
            return;
        }

        // Toolbar background: 6% lighter than canvas.
        let toolbar_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.06,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.06,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.06,
            1.0,
        ];
        push_quad(verts, toolbar_rect, bg_uv, [0.0; 4], toolbar_bg);

        // Bottom separator line.
        push_quad(
            verts,
            toolbar_rule_rect(tx0, tx1, ty1),
            bg_uv,
            [0.0; 4],
            separator,
        );

        // Compute button area width (must match layout_settings calculation).
        let btn_w = ch * 1.5;
        let gap = ch * 0.5;
        let buttons_total = btn_w * 2.0 + gap;
        let entries_area_w = (tx1 - tx0 - buttons_total).max(0.0);
        let slot_w = entries_area_w / profiles.len() as f32;

        // Draw each profile entry.
        for (i, profile) in profiles.iter().enumerate() {
            let slot_x0 = tx0 + i as f32 * slot_w;
            let slot_x1 = slot_x0 + slot_w;

            // Active entry: accent underline at the bottom of the slot.
            if profile.is_active {
                push_quad(
                    verts,
                    toolbar_rule_rect(slot_x0, slot_x1, ty1),
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }

            // Entry label: "Base" for index 0, otherwise the profile name.
            let label = if profile.name.is_empty() {
                "Base"
            } else {
                profile.name
            };
            let color = if profile.is_active { accent } else { label_c };
            let max_cols = ((slot_w / cw).max(1.0)) as usize;

            // Center the text in the slot.
            let text_w = cw * Self::text_col_width(label) as f32;
            let text_x = slot_x0 + ((slot_w - text_w) / 2.0).max(0.0);

            self.push_text(verts, text_x, ty0, label, color, max_cols);
        }

        // "+" (create) button.
        if let Some(btn) = create_btn {
            let [bx0, _, bx1, _] = btn;
            let btn_cx = (bx0 + bx1) / 2.0;
            // Center the "+" glyph (1 column wide).
            let text_x = btn_cx - cw * 0.5;
            self.push_text(verts, text_x, ty0, "+", fg, 1);
        }

        // "−" (delete) button using U+2212 minus sign.
        if let Some(btn) = delete_btn {
            let [bx0, _, bx1, _] = btn;
            let btn_cx = (bx0 + bx1) / 2.0;
            let text_x = btn_cx - cw * 0.5;
            self.push_text(verts, text_x, ty0, "\u{2212}", fg, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_rule_is_one_aligned_physical_pixel() {
        let rect = toolbar_rule_rect(10.25, 90.75, 42.6);
        assert_eq!(rect, [10.25, 42.0, 90.75, 43.0]);
    }
}
