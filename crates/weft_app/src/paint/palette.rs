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
    pub(crate) fn build_palette_vertices(
        &self,
        p: &crate::overlay::PaletteDrawParams<'_>,
    ) -> Vec<f32> {
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
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is too close to bg in Nord/Warp themes, making prompt
        // marks (❯), chevrons, and dim text invisible. label_c is always
        // readable across all themes.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        // Block separator: theme.separator (barely-visible warm dark).
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v1.0 Warp-style: thin low-opacity border.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];

        let pad_x = self.padding_x;
        let left = pad_x;
        let right = vp_w - pad_x;
        let cols = (((right - left) / cw).max(1.0)) as usize;

        // v0.8 stage 4: layout (popup rect, query/sep/results Y, column
        // anchors) is computed by the pure functions in `layout.rs`. The
        // renderer keeps responsibility for vertex building, theming, and
        // text rasterization.
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

        // v1.0 Warp-style: subtle drop shadow behind the popup.
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_pad,
                popup_top - shadow_pad,
                popup_x1 + shadow_pad,
                popup_bottom + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background + border.
        push_quad(
            &mut verts,
            [popup_x0, popup_top, popup_x1, popup_bottom],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        for (bx0, by0, bx1, by1) in [
            (popup_x0, popup_top, popup_x1, popup_top + 1.0),
            (popup_x0, popup_bottom - 1.0, popup_x1, popup_bottom),
            (popup_x0, popup_top, popup_x0 + 1.0, popup_bottom),
            (popup_x1 - 1.0, popup_top, popup_x1, popup_bottom),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Warp-style resize handles on right + top borders.
        self.draw_resize_handles(
            &mut verts,
            popup_x0,
            popup_top,
            popup_x1,
            popup_bottom,
            bg_uv,
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
                let accent = color_to_normalized(self.theme.accent);
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

        // Results rows.
        let mut y = layout.results_y;
        for i in start..end {
            if y + ch > popup_bottom {
                break;
            }
            let is_sel = i == p.selection;
            if is_sel {
                // v1.0 P3: unify with Settings selection_bg (accent*0.35 +
                // bg*0.65) — was [prompt_c, 0.20] which is low-contrast.
                let accent = color_to_normalized(self.theme.accent);
                let selection_bg = [
                    accent[0] * 0.35 + theme_bg[0] * 0.65,
                    accent[1] * 0.35 + theme_bg[1] * 0.65,
                    accent[2] * 0.35 + theme_bg[2] * 0.65,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [popup_x0 + 1.0, y, popup_x1 - 1.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let entry = &p.entries[i];
            let lcolor = if is_sel { fg } else { dim };
            // v1.0 fix: blend fg with theme_bg instead of pure fg*0.40.
            // The old fg*0.40 drops foreground towards black — on dark
            // themes with low fg values (Solarized Dark fg=0x93, One Dark
            // fg=0xab), the result is nearly identical to the popup
            // background, making "Workflow"/"Builtin"/"Theme" labels
            // invisible. Blending guarantees the color sits halfway between
            // fg and bg, ensuring readable contrast in ALL themes.
            let suffix_color = [
                fg[0] * 0.50 + theme_bg[0] * 0.50,
                fg[1] * 0.50 + theme_bg[1] * 0.50,
                fg[2] * 0.50 + theme_bg[2] * 0.50,
                1.0,
            ];

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
    pub(crate) fn build_palette_form_vertices(
        &self,
        form: &crate::overlay::PaletteFormView<'_>,
        popup_x0: f32,
        popup_x1: f32,
        vp_h: f32,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c — same fix as Settings
        // and Palette search mode. accent_dim is invisible in Nord/Warp.
        let prompt_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let dim = prompt_c;
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v1.0 P2: unify with Settings baseline — border α 0.35→0.20,
        // popup_bg 5%→8% lighten.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];

        let n_fields = form.fields.len();
        let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
        let popup_top = vp_h * 0.15;
        let popup_bottom = popup_top + popup_h;

        // v1.0 P2: add Warp-style shadow (was missing — form mode had no
        // shadow while search mode did, causing visual discontinuity).
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_pad,
                popup_top - shadow_pad,
                popup_x1 + shadow_pad,
                popup_bottom + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background + border.
        push_quad(
            &mut verts,
            [popup_x0, popup_top, popup_x1, popup_bottom],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        for (bx0, by0, bx1, by1) in [
            (popup_x0, popup_top, popup_x1, popup_top + 1.0),
            (popup_x0, popup_bottom - 1.0, popup_x1, popup_bottom),
            (popup_x0, popup_top, popup_x0 + 1.0, popup_bottom),
            (popup_x1 - 1.0, popup_top, popup_x1, popup_bottom),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

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
            if *is_current {
                // v1.0 P3: unify with Settings selection_bg (accent*0.35 +
                // bg*0.65) — was [accent_dim, 0.15] which is invisible in
                // Nord/Warp themes.
                let accent = color_to_normalized(self.theme.accent);
                let selection_bg = [
                    accent[0] * 0.35 + theme_bg[0] * 0.65,
                    accent[1] * 0.35 + theme_bg[1] * 0.65,
                    accent[2] * 0.35 + theme_bg[2] * 0.65,
                    1.0,
                ];
                push_quad(
                    &mut verts,
                    [val_x, y, popup_x1 - cw * 0.5, y + ch],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
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
