//! Shared settings-row paint helpers, split out of paint/settings.rs
//! (v1.13.17 S2 line-budget split; pages.rs/keybindings.rs precedent).
//!
//! Provenance (so a future "verbatim" grep is not misled): the
//! `push_settings_row` BODY moved out of paint/settings.rs unchanged, but
//! the function gained the `is_hovered` parameter + hover branch (v1.13.17
//! S2); `settings_hover_bg` is NEW in this release, not a moved item.

use super::ERROR_COLOR;
use crate::paint::primitives::{color_to_normalized, push_filled_triangle, push_quad};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// v1.13.17 S2: hovered-row background — the shared command-surface
    /// hover token (4% lift toward white) resolved against the raw canvas
    /// color, so every settings row family tints identically.
    pub(super) fn settings_hover_bg(&self) -> [f32; 4] {
        let theme_bg =
            color_to_normalized(crate::ui_tokens::UiColors::from_theme(&self.theme).canvas);
        crate::paint::color_math::lighten_to_white(theme_bg, 0.04)
    }

    /// F5: Render a standard label/value row with selection highlight,
    /// triangles on the selected row, and an optional inline field error.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_settings_row(
        &self,
        verts: &mut Vec<f32>,
        row_y: f32,
        label: &str,
        value: &str,
        is_sel: bool,
        // v1.13.17 S2: mouse-hovered row (pre-resolved via
        // `settings_row_hovered`; selection wins when both land on the row).
        is_hovered: bool,
        content_x0: f32,
        content_x1: f32,
        value_x: f32,
        cw: f32,
        ch: f32,
        content_cols: usize,
        bg_uv: [f32; 4],
        selection_bg: [f32; 4],
        fg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        error_msg: Option<&str>,
    ) {
        let value_x = crate::settings_component::settings_value_x(
            value_x,
            content_x0,
            cw,
            Self::text_col_width(label),
        );
        if is_sel {
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
        let label_color = if is_sel { fg } else { label_c };
        self.push_text(verts, content_x0, row_y, label, label_color, content_cols);
        let value_color = if is_sel { accent } else { fg };
        self.push_text(verts, value_x, row_y, value, value_color, content_cols);
        // Triangles on the selected row.
        if is_sel {
            let tri_h = ch * 0.14;
            let tri_w = ch * 0.10;
            let tri_cy = row_y + ch * 0.5;
            let val_text_w = cw * Self::text_col_width(value) as f32;
            let tri_lx = value_x - cw * 0.6;
            let tri_rx = value_x + val_text_w + cw * 0.6;
            push_filled_triangle(
                verts,
                tri_lx,
                tri_cy,
                tri_lx + tri_w,
                tri_cy - tri_h,
                tri_lx + tri_w,
                tri_cy + tri_h,
                accent,
            );
            push_filled_triangle(
                verts,
                tri_rx,
                tri_cy,
                tri_rx - tri_w,
                tri_cy - tri_h,
                tri_rx - tri_w,
                tri_cy + tri_h,
                accent,
            );
        }
        // Inline field error (red text after the value/triangles).
        if let Some(err) = error_msg {
            let val_text_w = cw * Self::text_col_width(value) as f32;
            let err_x = value_x + val_text_w + cw * 2.5;
            let err_cols =
                content_cols.saturating_sub(((err_x - content_x0) / cw).max(0.0) as usize);
            if err_cols > 3 {
                let msg = format!("\u{26a0} {}", err);
                self.push_text(verts, err_x, row_y, &msg, ERROR_COLOR, err_cols);
            }
        }
    }
}
