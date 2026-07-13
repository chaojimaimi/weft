//! Command palette vertex builders extracted from renderer.rs (A5).
//!
//! These remain `impl MetalRenderer` methods (strategy b) because they need
//! `self.theme`, `self.atlas` (via push_text/space_uv), `self.layout_ctx`
//! and several layout/interaction fields. Hit testing for the palette lives
//! in the Scene component (palette_component); these functions only produce
//! vertex data.

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// Build the Command Palette as a centered floating window. Renders a
    /// search box at the top, a scrollable results list, and an optional
    /// variable-fill form when a workflow is selected.
    ///
    /// F4: shell + row backgrounds now use the shared `command_surface`
    /// builders so Palette, Completion, Find and ContextMenu share the same
    /// visual language (8% lifted bg, unified border, resize handles, and
    /// selection/hover/disabled row states).
    pub(crate) fn build_palette_vertices(
        &self,
        p: &crate::overlay::PaletteDrawParams<'_>,
    ) -> Vec<f32> {
        use crate::paint::command_surface::{
            build_command_surface_row_bg, build_command_surface_shell, palette_surface_state,
            CommandSurfaceRowState, CommandSurfaceShell,
        };

        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return verts;
        }

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let pad_x = self.padding_x;
        let left = pad_x;
        let right = vp_w - pad_x;
        let cols = (((right - left) / cw).max(1.0)) as usize;

        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");

        // If we're in form mode, render the form instead of the search list.
        let palette_layout = crate::palette_component::derive_palette_layout(
            &ctx,
            p.entries.len(),
            p.selection,
            p.form.map(|form| form.fields.len()),
            self.popup_max_rows,
            self.popup_width_scale,
        );
        if let Some(form) = p.form {
            let crate::palette_component::PaletteLayout::Form(rect) = palette_layout else {
                unreachable!("form input always derives form layout")
            };
            let v = self.build_palette_form_vertices(form, rect[0], rect[2], vp_h);
            return v;
        }

        // Search mode: query box + results list.
        let crate::palette_component::PaletteLayout::Search(layout) = palette_layout else {
            unreachable!("search input always derives search layout")
        };
        let [popup_x0, popup_top, popup_x1, popup_bottom] = layout.popup_rect;
        let query_y = layout.query_y;
        let query_x = layout.query_x;
        let sep_y = layout.sep_y;
        let label_x = layout.label_x;
        let suffix_x = layout.suffix_x;
        let start = layout.start;
        let end = layout.end;

        // F4: shared shell (shadow + bg + border + resize handles).
        let shadow_pad = ch * 0.15;
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(
                [popup_x0, popup_top, popup_x1, popup_bottom],
                shadow_pad,
                true,
                theme_bg,
                bg_uv,
            ),
        );

        // Banner / query row. When a sub-mode banner is active, show it
        // instead of the normal search prompt.
        if !p.banner.is_empty() {
            // Sub-mode: show banner + input buffer.
            self.push_text(&mut verts, query_x, query_y, p.banner, prompt_c, cols);
            let banner_cols = Self::text_col_width(p.banner);
            let input_x = query_x + (banner_cols + 1) as f32 * cw;
            let avail = (((popup_x1 - input_x) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, input_x, query_y, p.submode_input, fg, avail);
        } else {
            // Normal search mode.
            let query_label = "> ";
            self.push_text(&mut verts, query_x, query_y, query_label, prompt_c, cols);
            let qx = query_x + query_label.chars().count() as f32 * cw;
            let avail = (((popup_x1 - qx) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, qx, query_y, p.query, fg, avail);
            // v0.9 fix: blinking caret at end of query so the user sees the
            // input focus (matches the find bar + panel search box behavior).
            if self.cursor_blink_on {
                let qcols = Self::text_col_width(p.query);
                let cx = qx + qcols as f32 * cw;
                push_quad(
                    &mut verts,
                    [cx, query_y, cx + cw * 0.15, query_y + ch],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }
        }

        // Separator below query.
        push_quad(
            &mut verts,
            [popup_x0, sep_y, popup_x1, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            separator,
        );

        // F4: formal state — show "No results" when the query has no matches.
        let surface_state = palette_surface_state(p.query, p.entries.len(), true);
        if !surface_state.shows_results() && !p.query.is_empty() {
            let status = surface_state.status_text();
            let status_color = [
                fg[0] * 0.50 + theme_bg[0] * 0.50,
                fg[1] * 0.50 + theme_bg[1] * 0.50,
                fg[2] * 0.50 + theme_bg[2] * 0.50,
                1.0,
            ];
            let status_w = Self::text_col_width(&status);
            self.push_text(
                &mut verts,
                label_x,
                layout.results_y,
                &status,
                status_color,
                status_w,
            );
        }

        // Results rows.
        let suffix_color = [
            fg[0] * 0.50 + theme_bg[0] * 0.50,
            fg[1] * 0.50 + theme_bg[1] * 0.50,
            fg[2] * 0.50 + theme_bg[2] * 0.50,
            1.0,
        ];
        let mut y = layout.results_y;
        for i in start..end {
            if y + ch > popup_bottom {
                break;
            }
            let is_sel = i == p.selection;
            // F4: unified row background (selected/hovered/disabled).
            build_command_surface_row_bg(
                &mut verts,
                [popup_x0, y, popup_x1, y + ch],
                CommandSurfaceRowState {
                    selected: is_sel,
                    ..Default::default()
                },
                theme_bg,
                accent,
                bg_uv,
            );
            let entry = &p.entries[i];
            let lcolor = if is_sel { fg } else { dim };

            // Label + description.
            let label_avail = (((popup_x1 - label_x) / cw) as usize)
                .saturating_sub(12)
                .max(1);
            self.push_text(&mut verts, label_x, y, entry.label, lcolor, label_avail);

            // Kind suffix (right-aligned area).
            self.push_text(&mut verts, suffix_x, y, entry.kind_label, suffix_color, 10);
            y += ch;
        }

        verts
    }

    /// Render the palette's variable-fill form (sub-mode when a workflow is selected).
    ///
    /// F4: shell now uses the shared `command_surface` builder; field
    /// selection backgrounds use the unified row-state helper.
    pub(crate) fn build_palette_form_vertices(
        &self,
        form: &crate::overlay::PaletteFormView<'_>,
        popup_x0: f32,
        popup_x1: f32,
        vp_h: f32,
    ) -> Vec<f32> {
        use crate::paint::command_surface::{
            build_command_surface_row_bg, build_command_surface_shell, CommandSurfaceRowState,
            CommandSurfaceShell,
        };

        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let n_fields = form.fields.len();
        let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
        let popup_top = vp_h * 0.15;
        let popup_bottom = popup_top + popup_h;

        // F4: shared shell (shadow + bg + border, no resize handles).
        let shadow_pad = ch * 0.15;
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(
                [popup_x0, popup_top, popup_x1, popup_bottom],
                shadow_pad,
                false,
                theme_bg,
                bg_uv,
            ),
        );

        // Title row.
        let title_y = popup_top + ch * 0.5;
        let title = format!("{} — 填写参数", form.workflow_name);
        self.push_text(
            &mut verts,
            popup_x0 + cw * 0.5,
            title_y,
            &title,
            prompt_c,
            40,
        );

        // Separator.
        let sep_y = title_y + ch;
        push_quad(
            &mut verts,
            [popup_x0, sep_y, popup_x1, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            [0.65, 0.65, 0.65, 0.22],
        );

        // Fields.
        let mut y = sep_y + ch;
        for (name, value, is_current) in form.fields.iter() {
            let label_text = format!("{name}: ");
            let color = if *is_current { fg } else { dim };
            self.push_text(&mut verts, popup_x0 + cw * 0.5, y, &label_text, color, 20);

            // Value bracket area.
            let val_x = popup_x0 + cw * 0.5 + 12.0 * cw;
            // F4: unified row background for the current field.
            build_command_surface_row_bg(
                &mut verts,
                [val_x, y, popup_x1 - cw * 0.5, y + ch],
                CommandSurfaceRowState {
                    selected: *is_current,
                    ..Default::default()
                },
                theme_bg,
                accent,
                bg_uv,
            );
            let val_avail = (((popup_x1 - cw * 0.5 - val_x) / cw).max(1.0)) as usize;
            self.push_text(&mut verts, val_x, y, value, color, val_avail);

            y += ch;
        }

        // Footer hint.
        let hint_y = popup_bottom - ch * 0.8;
        let hint = "Enter 执行  Tab 下一项  Esc 返回";
        self.push_text(&mut verts, popup_x0 + cw * 0.5, hint_y, hint, dim, 40);

        verts
    }
}
