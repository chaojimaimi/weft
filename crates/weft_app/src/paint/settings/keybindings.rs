// Keybindings settings content arm — split out of paint/settings.rs to keep
// it within its architecture-gate ceiling (pages.rs v1.12.19 precedent:
// child-module privacy reaches the parent's private helpers).
//! v1.12.27b (P1-03): verbatim move of the `SettingsTab::Keybindings` arm
//! (settings.rs baseline :365-456) — zero behavior change. The one outer
//! local the arm read (`layout.footer_y`) arrives as the explicit
//! `footer_y` parameter (3-B-2 shared-variable contract).

use super::find_field_error;
use crate::paint::primitives::push_quad;
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_keybindings_content(
        &self,
        verts: &mut Vec<f32>,
        s: crate::overlay::SettingsDrawParams<'_>,
        content_top: f32,
        content_x0: f32,
        content_x1: f32,
        cw: f32,
        ch: f32,
        content_cols: usize,
        bg_uv: [f32; 4],
        selection_bg: [f32; 4],
        label_c: [f32; 4],
        accent: [f32; 4],
        warning_c: [f32; 4],
        footer_y: f32,
    ) {
        let mut list_top = content_top;
        let validation_error = find_field_error(s.field_errors, "Keybindings");
        if s.keybinding_conflict_count > 0 || validation_error.is_some() {
            let badge = validation_error.map_or_else(
                || format!("\u{26a0} {} conflict(s)", s.keybinding_conflict_count),
                |error| format!("\u{26a0} {error}"),
            );
            self.push_text(verts, content_x0, list_top, &badge, warning_c, content_cols);
            list_top += ch;
        }
        let total = s.keybindings.len();
        let available_rows = ((footer_y - ch * 0.5 - list_top) / ch).max(1.0) as usize;
        let scrollable = total > available_rows;
        let usable_rows = if scrollable {
            available_rows.saturating_sub(1)
        } else {
            available_rows
        };
        let visible = usable_rows.min(total);
        let mut offset = s.scroll_offset.min(total);
        if s.selection < offset {
            offset = s.selection;
        } else if s.selection >= offset + visible {
            offset = s.selection + 1 - visible;
        }
        let end = (offset + visible).min(total);
        for (i, kb) in s.keybindings[offset..end].iter().enumerate() {
            let row_y = list_top + i as f32 * ch;
            let is_selected = offset + i == s.selection;
            if is_selected {
                push_quad(
                    verts,
                    [content_x0, row_y, content_x1, row_y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let action_color = if kb.conflict { warning_c } else { label_c };
            self.push_text(
                verts,
                content_x0,
                row_y,
                &kb.action,
                action_color,
                content_cols,
            );
            let binding_x = content_x1 - cw * 15.0;
            let binding_color = if kb.conflict { warning_c } else { accent };
            self.push_text(
                verts,
                binding_x,
                row_y,
                &kb.binding,
                binding_color,
                content_cols,
            );
        }
        if scrollable {
            let indicator_y = list_top + visible as f32 * ch;
            let mut indicator = String::new();
            if offset > 0 {
                indicator.push('\u{2191}');
            }
            if end < total {
                if !indicator.is_empty() {
                    indicator.push(' ');
                }
                indicator.push('\u{2193}');
            }
            if !indicator.is_empty() {
                indicator.push_str(" more");
                self.push_text(
                    verts,
                    content_x0,
                    indicator_y,
                    &indicator,
                    label_c,
                    content_cols,
                );
            }
        }
    }
}
