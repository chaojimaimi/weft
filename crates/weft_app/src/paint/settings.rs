//! Settings panel vertex builder extracted from renderer.rs (A5).

use crate::paint::primitives::{color_to_normalized, push_filled_triangle, push_line, push_quad};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
    /// Build the Settings panel (Cmd+,) as a centered modal overlay. Renders
    /// a title bar, a 4-tab tab bar (Appearance / Font /
    /// Keybindings / Window), the active tab's content, and a footer hint.
    ///
    /// Layout:
    ///   ┌────────────────────────────────────┐
    ///   │ Settings                           │  ← title (2 rows)
    ///   │ Appearance  Font  Keybindings  Win │  ← tab bar (1 row)
    ///   ├────────────────────────────────────┤
    ///   │  › Weft Warm (default)             │  ← content (scrollable)
    ///   │    Weft Light                      │
    ///   │    ...                             │
    ///   ├────────────────────────────────────┤
    ///   │ ↑↓ navigate  Enter apply  Tab …   │  ← footer (1 row)
    ///   └────────────────────────────────────┘
    pub(crate) fn build_settings_vertices(
        &self,
        s: crate::overlay::SettingsDrawParams<'_>,
    ) -> Vec<f32> {
        use crate::overlay::SettingsTab;

        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        if cw <= 0.0 || ch <= 0.0 || vp_w <= 0.0 || vp_h <= 0.0 {
            return verts;
        }

        let pairs: [(&str, &str); 6] = [
            ("↑↓", "navigate"),
            ("⏎", "apply"),
            ("⇥", "switch"),
            ("←→", "adjust"),
            ("esc", "close"),
            ("⌘⏎", "save"),
        ];
        let inner = cw * 0.3;
        let mut footer_pair_widths = [0.0; 6];
        for (index, (key, desc)) in pairs.iter().enumerate() {
            footer_pair_widths[index] =
                cw * (Self::text_col_width(key) + Self::text_col_width(desc)) as f32 + inner;
        }
        let layout = crate::layout::layout_settings(
            vp_w,
            vp_h,
            cw,
            ch,
            SettingsTab::ALL.len(),
            s.error.is_some(),
            &footer_pair_widths,
        )
        .expect("positive renderer geometry produces SettingsLayout");

        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let separator = color_to_normalized(self.theme.separator);
        // v1.0 fix: compute a "label" color that's always readable across all
        // themes. The old `dim` (= accent_dim) is too close to the background
        // in some themes (e.g. Nord: accent_dim #4c566a vs bg #2e3440). By
        // blending fg with bg (70% fg + 30% bg) we get a muted but always-
        // readable secondary text color that adapts to each theme.
        let label_c = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        // Warp-inspired: very thin 1px border at low opacity, subtle shadow.
        let border_c = [0.5, 0.5, 0.5, 0.20];
        // Popup background: 8% lighter — enough to distinguish from the
        // terminal canvas without being jarring.
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        let selection_bg = [
            accent[0] * 0.35 + theme_bg[0] * 0.65,
            accent[1] * 0.35 + theme_bg[1] * 0.65,
            accent[2] * 0.35 + theme_bg[2] * 0.65,
            1.0,
        ];

        let [box_x0, box_y0, box_x1, box_y1] = layout.box_rect;
        let content_x0 = layout.content_x0;
        let content_x1 = layout.content_x1;
        let content_cols = (((content_x1 - content_x0) / cw).max(1.0)) as usize;

        // Warp-style: very subtle shadow (low opacity, tight offset).
        let shadow_pad = ch * 0.15;
        push_quad(
            &mut verts,
            [
                box_x0 - shadow_pad,
                box_y0 - shadow_pad,
                box_x1 + shadow_pad,
                box_y1 + shadow_pad,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // Background (no top accent border — Warp keeps it minimal).
        push_quad(
            &mut verts,
            [box_x0, box_y0, box_x1, box_y1],
            bg_uv,
            [0.0; 4],
            popup_bg,
        );
        // Single 1px border on all four sides.
        for (bx0, by0, bx1, by1) in [
            (box_x0, box_y0, box_x1, box_y0 + 1.0),
            (box_x0, box_y1 - 1.0, box_x1, box_y1),
            (box_x0, box_y0, box_x0 + 1.0, box_y1),
            (box_x1 - 1.0, box_y0, box_x1, box_y1),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Title row — subtle (label_c, not accent) to keep visual hierarchy calm.
        let title_y = box_y0 + ch;
        self.push_text(
            &mut verts,
            content_x0,
            title_y,
            "Settings",
            label_c,
            content_cols,
        );

        // Tab bar: tabs side by side with 1px underline for active.
        let tab_w = layout.tab_width;
        let mut y = layout.tab_bar_y;
        for (i, tab) in SettingsTab::ALL.iter().enumerate() {
            let tx0 = box_x0 + i as f32 * tab_w;
            let label = tab.label();
            let is_active = *tab == s.active_tab;
            let color = if is_active { fg } else { label_c };
            // Active tab: 1px underline spanning the full slot width.
            if is_active {
                push_quad(
                    &mut verts,
                    [
                        tx0 + cw * 0.5,
                        y + ch * 0.9,
                        tx0 + tab_w - cw * 0.5,
                        y + ch * 0.9 + 1.0,
                    ],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }
            // Center the label within its slot so short labels (Font/Logo)
            // don't clump against the left edge of unequal-width tabs.
            // v1.2-fix: truncate to slot width (tab_w / cw cols), not the
            // full content area width. Previously passed content_cols which
            // let long labels overflow into adjacent slots when the panel
            // was small.
            let slot_cols = ((tab_w / cw) as usize).max(1);
            let label_chars = label.chars().count() as f32;
            let text_x = tx0 + ((tab_w - label_chars * cw) / 2.0).max(cw * 0.5);
            self.push_text(&mut verts, text_x, y, label, color, slot_cols);
        }
        y += ch * 1.5;

        // Separator line between tab bar and content.
        push_quad(
            &mut verts,
            [content_x0, y + ch * 0.3, content_x1, y + ch * 0.3 + 1.0],
            bg_uv,
            [0.0; 4],
            separator,
        );

        // Content area: render the active tab.
        // v1.0 fix: move footer up from ch*0.8 to ch*1.5 so it sits between
        // the separator line and the bottom border with balanced spacing.
        let footer_y = layout.footer_y;
        let content_base = if s.error.is_some() {
            layout.content_top - ch
        } else {
            layout.content_top
        };

        // v1.0 S2: error bar at the top of the content area when a save
        // failed. Renders a red background strip with the error message,
        // pushing the rest of the content down by one row so nothing
        // overlaps. Cleared by save_settings_draft on the next successful
        // save (or by closing the panel).
        let content_top = if let Some(err) = s.error {
            let err_bg = [0.65, 0.18, 0.18, 1.0];
            push_quad(
                &mut verts,
                [content_x0, content_base, content_x1, content_base + ch],
                bg_uv,
                [0.0; 4],
                err_bg,
            );
            // ⚠ prefix in white, then the message (truncated to fit).
            let msg = format!("\u{26a0} {}", err);
            self.push_text(
                &mut verts,
                content_x0 + cw * 0.3,
                content_base,
                &msg,
                [1.0, 1.0, 1.0, 1.0],
                content_cols,
            );
            content_base + ch
        } else {
            content_base
        };
        let max_rows = layout.max_rows;

        match s.active_tab {
            SettingsTab::Appearance => {
                // Theme list — Warp-style: vector-drawn circle for current
                // theme (avoids the ● glyph rendering as an oval because the
                // cell is wider than tall), subtle selection highlight.
                for (i, theme) in s.themes.iter().take(max_rows).enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_current = theme.name == s.theme_name;
                    let is_selected = i == s.selection;
                    if is_selected {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    // v1.2: draw a vector circle (current) or ring (others)
                    // at the row's left edge. Uses push_line segments to
                    // approximate a circle with radius based on ch (not cw),
                    // so it stays round regardless of cell aspect ratio.
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
                    // Approximate circle with 8 line segments.
                    let segments = 8;
                    for s_idx in 0..segments {
                        let a0 = s_idx as f32 * std::f32::consts::TAU / segments as f32;
                        let a1 = (s_idx + 1) as f32 * std::f32::consts::TAU / segments as f32;
                        if !is_current && s_idx % 2 == 0 {
                            // Ring: skip alternate segments for a dashed look.
                            // Actually, draw all segments but with dim color —
                            // a full ring is cleaner.
                        }
                        push_line(
                            &mut verts,
                            dot_cx + dot_r * a0.cos(),
                            dot_cy + dot_r * a0.sin(),
                            dot_cx + dot_r * a1.cos(),
                            dot_cy + dot_r * a1.sin(),
                            dot_lw,
                            dot_color,
                        );
                    }
                    // For current theme, fill the circle with a center dot.
                    if is_current {
                        push_quad(
                            &mut verts,
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
                    // Label starts after the dot area (reserve 2 cells width
                    // matching the old "● " / "  " prefix).
                    let label_color = if is_current { accent } else { fg };
                    self.push_text(
                        &mut verts,
                        content_x0 + cw * 2.0,
                        row_y,
                        theme.label,
                        label_color,
                        content_cols,
                    );
                }
            }
            SettingsTab::Font => {
                let rows = [
                    ("Family:", s.font_family),
                    ("Size:", &format!("{:.1} pt", s.font_size)),
                    ("Line height:", &format!("{:.2}", s.line_height)),
                ];
                // v1.2-fix: value_x aligned after the longest label so
                // triangles don't overlap label text (e.g. "Line height:").
                let value_x = content_x0 + cw * 14.0;
                for (i, (label, value)) in rows.iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_sel = i == s.selection;
                    if is_sel {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    let label_color = if is_sel { fg } else { label_c };
                    self.push_text(
                        &mut verts,
                        content_x0,
                        row_y,
                        label,
                        label_color,
                        content_cols,
                    );
                    let value_color = if is_sel { accent } else { fg };
                    self.push_text(&mut verts, value_x, row_y, value, value_color, content_cols);
                    // v1.2: filled triangle indicators ◀ ▶ on the selected row.
                    if is_sel {
                        let tri_color = accent;
                        let tri_h = ch * 0.14; // half-height of triangle
                        let tri_w = ch * 0.10; // half-width of triangle
                        let tri_cy = row_y + ch * 0.5;
                        let val_text_w = cw * Self::text_col_width(value) as f32;
                        let tri_lx = value_x - cw * 0.6;
                        let tri_rx = value_x + val_text_w + cw * 0.6;
                        // ◀ (pointing left): tip at tri_lx, base at tri_lx + tri_w
                        push_filled_triangle(
                            &mut verts,
                            tri_lx,
                            tri_cy,
                            tri_lx + tri_w,
                            tri_cy - tri_h,
                            tri_lx + tri_w,
                            tri_cy + tri_h,
                            tri_color,
                        );
                        // ▶ (pointing right): tip at tri_rx, base at tri_rx - tri_w
                        push_filled_triangle(
                            &mut verts,
                            tri_rx,
                            tri_cy,
                            tri_rx - tri_w,
                            tri_cy - tri_h,
                            tri_rx - tri_w,
                            tri_cy + tri_h,
                            tri_color,
                        );
                    }
                }
            }
            SettingsTab::Keybindings => {
                // v1.0 fix: scrollable list — render `[offset .. offset+max_rows]`
                // and auto-clamp offset so the selected row is always visible.
                let total = s.keybindings.len();
                // Reserve one row for the scroll indicator when the list is
                // scrollable (more rows than fit), so the indicator doesn't
                // overlap the last visible keybinding row.
                let scrollable = total > max_rows;
                let usable_rows = if scrollable {
                    max_rows.saturating_sub(1)
                } else {
                    max_rows
                };
                let visible = usable_rows.min(total);
                // Derive offset from selection: keep selection in view.
                let mut offset = s.scroll_offset.min(total);
                if s.selection < offset {
                    offset = s.selection;
                } else if s.selection >= offset + visible {
                    offset = s.selection + 1 - visible;
                }
                let end = (offset + visible).min(total);
                for (i, kb) in s.keybindings[offset..end].iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_selected = offset + i == s.selection;
                    if is_selected {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    self.push_text(
                        &mut verts,
                        content_x0,
                        row_y,
                        &kb.action,
                        label_c,
                        content_cols,
                    );
                    let binding_x = content_x1 - cw * 15.0;
                    self.push_text(
                        &mut verts,
                        binding_x,
                        row_y,
                        &kb.binding,
                        accent,
                        content_cols,
                    );
                }
                // v1.0 fix: scroll indicator on its own row below the list
                // (not overlapping the last keybinding row). Shows "↑ more"
                // / "↓ more" / both when scrollable in either direction.
                if scrollable {
                    let indicator_y = content_top + visible as f32 * ch;
                    let mut indicator = String::new();
                    if offset > 0 {
                        indicator.push('↑');
                    }
                    if end < total {
                        if !indicator.is_empty() {
                            indicator.push(' ');
                        }
                        indicator.push('↓');
                    }
                    if !indicator.is_empty() {
                        indicator.push_str(" more");
                        self.push_text(
                            &mut verts,
                            content_x0,
                            indicator_y,
                            &indicator,
                            label_c,
                            content_cols,
                        );
                    }
                }
            }
            SettingsTab::Window => {
                let rows = [
                    ("Opacity:", format!("{:.2}", s.window_opacity)),
                    ("Padding X:", format!("{} cells", s.window_padding_x)),
                    ("Padding Y:", format!("{} cells", s.window_padding_y)),
                    ("Scrollback:", format!("{} lines", s.scrollback_lines)),
                ];
                let value_x = content_x0 + cw * 14.0;
                for (i, (label, value)) in rows.iter().enumerate() {
                    let row_y = content_top + i as f32 * ch;
                    let is_sel = i == s.selection;
                    if is_sel {
                        push_quad(
                            &mut verts,
                            [content_x0, row_y, content_x1, row_y + ch],
                            bg_uv,
                            [0.0; 4],
                            selection_bg,
                        );
                    }
                    let label_color = if is_sel { fg } else { label_c };
                    self.push_text(
                        &mut verts,
                        content_x0,
                        row_y,
                        label,
                        label_color,
                        content_cols,
                    );
                    let value_color = if is_sel { accent } else { fg };
                    self.push_text(&mut verts, value_x, row_y, value, value_color, content_cols);
                    if is_sel {
                        let tri_color = accent;
                        let tri_h = ch * 0.14;
                        let tri_w = ch * 0.10;
                        let tri_cy = row_y + ch * 0.5;
                        let val_text_w = cw * Self::text_col_width(value) as f32;
                        let tri_lx = value_x - cw * 0.6;
                        let tri_rx = value_x + val_text_w + cw * 0.6;
                        push_filled_triangle(
                            &mut verts,
                            tri_lx,
                            tri_cy,
                            tri_lx + tri_w,
                            tri_cy - tri_h,
                            tri_lx + tri_w,
                            tri_cy + tri_h,
                            tri_color,
                        );
                        push_filled_triangle(
                            &mut verts,
                            tri_rx,
                            tri_cy,
                            tri_rx - tri_w,
                            tri_cy - tri_h,
                            tri_rx - tri_w,
                            tri_cy + tri_h,
                            tri_color,
                        );
                    }
                }
            }
            // v1.0 Logo: single row — Variant label + current value.
            // ←/→ cycles through LogoVariant::ALL (Cool/Warm/Light/Transparent).
            SettingsTab::Logo => {
                let row_y = content_top;
                let is_sel = s.selection == 0;
                if is_sel {
                    push_quad(
                        &mut verts,
                        [content_x0, row_y, content_x1, row_y + ch],
                        bg_uv,
                        [0.0; 4],
                        selection_bg,
                    );
                }
                let label_color = if is_sel { fg } else { label_c };
                self.push_text(
                    &mut verts,
                    content_x0,
                    row_y,
                    "Variant:",
                    label_color,
                    content_cols,
                );
                let value_x = content_x0 + cw * 14.0;
                let value_str = s.logo_variant.label();
                let value_color = if is_sel { accent } else { fg };
                self.push_text(
                    &mut verts,
                    value_x,
                    row_y,
                    value_str,
                    value_color,
                    content_cols,
                );
                if is_sel {
                    let tri_color = accent;
                    let tri_h = ch * 0.14;
                    let tri_w = ch * 0.10;
                    let tri_cy = row_y + ch * 0.5;
                    let val_text_w = cw * Self::text_col_width(value_str) as f32;
                    let tri_lx = value_x - cw * 0.6;
                    let tri_rx = value_x + val_text_w + cw * 0.6;
                    push_filled_triangle(
                        &mut verts,
                        tri_lx,
                        tri_cy,
                        tri_lx + tri_w,
                        tri_cy - tri_h,
                        tri_lx + tri_w,
                        tri_cy + tri_h,
                        tri_color,
                    );
                    push_filled_triangle(
                        &mut verts,
                        tri_rx,
                        tri_cy,
                        tri_rx - tri_w,
                        tri_cy - tri_h,
                        tri_rx - tri_w,
                        tri_cy + tri_h,
                        tri_color,
                    );
                }
            }
        }

        // v1.0 fix: Footer hint with two-color design — accent for shortcut
        // keys, label_c for descriptions. Each pair is rendered separately so
        // we can use different colors and control spacing precisely.
        push_quad(
            &mut verts,
            [
                content_x0,
                footer_y - ch * 0.4,
                content_x1,
                footer_y - ch * 0.4 + 1.0,
            ],
            bg_uv,
            [0.0; 4],
            separator,
        );
        // Compact pairs: "key description" with key in accent, desc in label_c.
        // v1.2 fix: left-to-right layout with right-edge truncation. Pairs
        // start from content_x0 (left edge, matching body text reading
        // direction) and stop when they would exceed content_x1 (right
        // edge). This keeps the leftmost pairs (↑↓ navigate, ⏎ apply) always
        // visible — they are the most-used operations. Pairs that don't fit
        // are simply not rendered (CPU-side cull, no half-glyph cropping).
        let gap = cw * 1.5; // gap between pairs
        let scale = 1.0; // match body text scale
        let mut fx = content_x0;
        for (key, desc) in &pairs {
            let key_w = cw * scale * Self::text_col_width(key) as f32;
            let desc_w = cw * scale * Self::text_col_width(desc) as f32;
            let pair_w = key_w + inner + desc_w;
            // Stop if this pair would exceed the right content boundary.
            if fx + pair_w > content_x1 {
                break;
            }
            self.push_text_scaled(&mut verts, fx, footer_y, key, accent, content_cols, scale);
            self.push_text_scaled(
                &mut verts,
                fx + key_w + inner,
                footer_y,
                desc,
                label_c,
                content_cols,
                scale,
            );
            fx += pair_w + gap;
        }

        verts
    }
}
