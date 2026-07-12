//! Overlay vertex builders extracted from renderer.rs (A5).
//!
//! These remain `impl MetalRenderer` methods (strategy b) because they need
//! `self.theme`, `self.atlas` (via push_text/space_uv) and `self.layout_ctx`.
//! Hit testing for these overlays lives in the Scene components
//! (find_component / context_menu_component); these functions only produce
//! vertex data.

use crate::paint::primitives::{color_to_normalized, push_line, push_quad, push_triangle};
use crate::renderer::{
    block_duration_str, panel_display, strip_prompt_prefix, truncate_str, visible_panel_rows,
    FindDrawState, MetalRenderer, PanelDrawParams, TabBarDrawState,
};

impl MetalRenderer {
    /// Build the right-click context menu (F7) as a small popup at (x, y).
    pub(crate) fn build_context_menu_vertices(&self, x: f32, y: f32) -> Vec<f32> {
        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
        let fg = color_to_normalized(ui.text_primary);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is invisible in Nord/Warp themes.
        let prompt_c = color_to_normalized(ui.text_secondary);
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let items = [
            "Copy Command",
            "Copy Output",
            "Toggle Fold",
            "Send to Input",
        ];

        // v0.8 stage 4: layout (menu rect, per-item Y, separators, text X)
        // is computed by the pure function in `layout.rs`. The renderer keeps
        // responsibility for vertex building, theming, and text rasterization.
        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let layout = crate::layout::layout_context_menu(&ctx, x, y, self.scale as f32);
        let [menu_x0, menu_y0, menu_x1, menu_y1] = layout.menu_rect;
        let menu_w = menu_x1 - menu_x0;
        let text_x = layout.text_x;

        let popup_bg = color_to_normalized(ui.raised);
        let border_c = color_to_normalized(ui.border_subtle);

        // Background.
        push_quad(&mut verts, layout.menu_rect, bg_uv, [0.0; 4], popup_bg);
        // Border.
        for (bx0, by0, bx1, by1) in [
            (menu_x0, menu_y0, menu_x1, menu_y0 + 1.0),
            (menu_x0, menu_y1 - 1.0, menu_x1, menu_y1),
            (menu_x0, menu_y0, menu_x0 + 1.0, menu_y1),
            (menu_x1 - 1.0, menu_y0, menu_x1, menu_y1),
        ] {
            push_quad(&mut verts, [bx0, by0, bx1, by1], bg_uv, [0.0; 4], border_c);
        }

        // Items.
        for (i, label) in items.iter().enumerate() {
            let item_y = layout.item_y[i];
            let color = if i >= items.len() - 2 {
                prompt_c // "Toggle Fold" + "Send to Input" in accent
            } else {
                fg
            };
            self.push_text(
                &mut verts,
                text_x,
                item_y,
                label,
                color,
                (menu_w / cw * 0.9) as usize,
            );
            // Separator between items (except last).
            if i + 1 < items.len() {
                let sep_y = layout.separator_ys[i];
                push_quad(
                    &mut verts,
                    [menu_x0 + 2.0, sep_y, menu_x1 - 2.0, sep_y + 1.0],
                    bg_uv,
                    [0.0; 4],
                    separator,
                );
            }
        }

        verts
    }

    /// Build the FindInGrid overlay (v0.8 B3): a Warp-style popup card in
    /// the top-right corner showing the query + match count, plus a yellow
    /// translucent highlight over the current match's cells.
    ///
    /// v0.8 user testing asked for a Warp-style independent popup (instead
    /// of a full-width top banner) floating in the top-right corner. The
    /// card is a layered surface: drop shadow + tinted background + 1px
    /// border + accent-colored left stripe, with a single row of
    /// "Find: <query>  <status>" inside. Auto-focus is already handled at
    /// the app layer — `handle_find_key` captures keystrokes when `find_open`
    /// is true, so the input is functionally focused whenever the popup is
    /// visible.
    ///
    /// Drawing uses the same fg/bg vertex pipeline as the rest of the
    /// renderer: text is sampled from the glyph atlas, card surfaces are
    /// bg-only quads sampling the space glyph (mask 0 → solid bg color).
    pub(crate) fn build_find_vertices(&self, find: &FindDrawState) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let ctx = match &self.layout_ctx {
            Some(c) => *c,
            None => return Vec::new(),
        };
        let layout = crate::layout::layout_find(&ctx, find.total);

        let mut verts = Vec::new();
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv]; // V-flipped for layer

        // ── Popup geometry ──────────────────────────────────────────────
        // Target ~500px wide (Warp's default), capped by available content
        // width so it never overflows the left edge. Height: at least one
        // cell + 16px padding, at most 1.75× cell height for legibility.
        let [popup_x0, popup_y0, popup_x1, popup_y1] = layout.popup_rect;

        let theme_bg = color_to_normalized(self.theme.background);
        let accent = color_to_normalized(self.theme.accent);
        let sep = color_to_normalized(self.theme.separator);
        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 fix: replace accent_dim with label_c (70% fg + 30% bg) —
        // accent_dim is too close to bg in Nord (#4c566a vs #2e3440) and
        // Warp themes, making buttons/status text invisible. label_c is
        // always readable across all themes.
        let accent_dim = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];

        // ── Drop shadow (Warp-style: tight offset, low opacity) ──────────
        let shadow_offset = 2.0;
        push_quad(
            &mut verts,
            [
                popup_x0 - shadow_offset,
                popup_y0 - shadow_offset,
                popup_x1 + shadow_offset,
                popup_y1 + shadow_offset,
            ],
            bg_uv,
            [0.0; 4],
            [0.0, 0.0, 0.0, 0.15],
        );

        // ── Card background — v1.0: unified with Settings/Palette to 8%
        // lighten (was a dual-branch 45% darken / 60% lighten, which made
        // Find read as "darker than window" while other popups read as
        // "lighter than window" — visually inconsistent). Opaque (α=1.0)
        // to fully occlude underlying grid content.
        let card_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.08,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.08,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.08,
            1.0,
        ];
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            card_bg,
        );

        // ── 1px border — Warp-style: low opacity, subtle ──────────────────
        let border_w = 1.0;
        let border_bg = [0.5, 0.5, 0.5, 0.20];
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x1, popup_y0 + border_w],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x0, popup_y1 - border_w, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x0 + border_w, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );
        push_quad(
            &mut verts,
            [popup_x1 - border_w, popup_y0, popup_x1, popup_y1],
            bg_uv,
            [0.0; 4],
            border_bg,
        );

        // ── Accent-colored left stripe (3px) — v0.8 signature accent ────
        // Replaces the previous top accent stripe so the popup still reads
        // as branded without occupying vertical space at the card edge.
        let stripe_w = 3.0;
        push_quad(
            &mut verts,
            [
                popup_x0 + border_w,
                popup_y0 + border_w,
                popup_x0 + border_w + stripe_w,
                popup_y1 - border_w,
            ],
            bg_uv,
            [0.0; 4],
            [accent[0], accent[1], accent[2], 1.0],
        );

        // ── Match highlight (yellow translucent overlay on grid cells) ──
        // Drawn over the grid content area, independent of the popup card.
        if let Some((row, col, len)) = find.highlight {
            let hx0 = ctx.col_x(col);
            let hy0 = ctx.row_y(row);
            let hx1 = hx0 + len as f32 * cw;
            let hy1 = hy0 + ch;
            // Soft yellow with 0.5 alpha so the underlying text stays readable.
            push_quad(
                &mut verts,
                [hx0, hy0, hx1, hy1],
                bg_uv,
                [0.0; 4],
                [0.95, 0.78, 0.20, 0.50],
            );
        }

        // ── Popup text row ─────────────────────────────────────────────
        // Layout (left to right):
        //   [pad]Find: <query>│    <status> [↑][↓] [Aa] [.*][pad]
        //   │ = blinking cursor at end of query
        //   <status> = compact match count (right-aligned)
        //   ↑/↓ = prev/next match buttons (clickable)
        //   Aa = case-sensitive toggle (lit when case_sensitive is on)
        //   .* = regex toggle indicator (lit when regex_mode is on)
        //
        // Button hit-test rects (with generous click padding) are stored in
        // Geometry comes from `FindLayout`, shared with Scene hit testing.
        let text_x0 = layout.text_x0;
        let line_y = layout.line_y;
        let gap_w = cw;

        // ── Right-aligned button cluster (rightmost first) ───────────────
        // Each button: 2 chars wide glyph + 1 char gap on its left side.
        // Click padding: extend the hit-test rect 2px above/below the line
        // and 1px left/right so the clickable area is forgiving.
        // ".*" regex toggle (rightmost).
        let regex_label = ".*";
        let regex_w = Self::text_col_width(regex_label);
        let regex_x = layout.regex_x;
        let regex_color = if find.regex_mode { accent } else { accent_dim };
        self.push_text(
            &mut verts,
            regex_x,
            line_y,
            regex_label,
            regex_color,
            regex_w,
        );
        // "Aa" case-sensitive toggle.
        let case_label = "Aa";
        let case_w = Self::text_col_width(case_label);
        let case_x = layout.case_x;
        let case_color = if find.case_sensitive {
            accent
        } else {
            accent_dim
        };
        self.push_text(&mut verts, case_x, line_y, case_label, case_color, case_w);
        // "↓" down arrow (next match) — 1 char wide.
        let down_label = "↓";
        let down_w = Self::text_col_width(down_label);
        let down_x = layout.down_x;
        let down_color = if find.total > 0 { accent_dim } else { sep };
        self.push_text(&mut verts, down_x, line_y, down_label, down_color, down_w);
        // "↑" up arrow (previous match) — 1 char wide.
        let up_label = "↑";
        let up_w = Self::text_col_width(up_label);
        let up_x = layout.up_x;
        let up_color = if find.total > 0 { accent_dim } else { sep };
        self.push_text(&mut verts, up_x, line_y, up_label, up_color, up_w);
        // ── Status text (left of the up arrow, compact) ──────────────────
        // Compact format to avoid overflow:
        //   empty query → "" (nothing, keep it clean)
        //   no matches   → "no matches  "
        //   has matches  → "current/total  "
        //   truncated    → "total+  "
        //   block-only   → "N in blocks  "
        //   regex error  → "invalid regex  " (red — v0.9 U-P2)
        //
        // v0.9 fix: if the status text would push `status_x` so far left that
        // the query has < MIN_QUERY_BUDGET cols, skip rendering the status
        // text entirely. The query visibility is more important than status.
        let (status, status_color) = if find.regex_error.is_some() {
            ("invalid regex  ".to_string(), accent)
        } else if find.query.is_empty() {
            (String::new(), accent_dim)
        } else if find.truncated {
            (format!("{}+  ", find.total), accent_dim)
        } else if find.total == 0 {
            if find.block_matches > 0 {
                (format!("{} in blocks  ", find.block_matches), accent_dim)
            } else {
                ("no matches  ".to_string(), accent_dim)
            }
        } else {
            (
                format!("{}/{}  ", find.current.max(1), find.total),
                accent_dim,
            )
        };
        let status_w = Self::text_col_width(&status);
        let status_x = up_x - gap_w - status_w as f32 * cw;
        // Check if there's room for both status and a minimum-width query.
        let query_start_x_test = text_x0 + Self::text_col_width("Find: ") as f32 * cw;
        let avail_for_query = ((status_x - query_start_x_test) / cw).floor() as isize;
        const MIN_QUERY_BUDGET: isize = 10;
        let show_status = avail_for_query >= MIN_QUERY_BUDGET;
        if show_status {
            self.push_text(
                &mut verts,
                status_x,
                line_y,
                &status,
                status_color,
                status_w,
            );
        }

        // ── "Find: " label ───────────────────────────────────────────────
        let label = "Find: ";
        let label_cols = Self::text_col_width(label);
        self.push_text(&mut verts, text_x0, line_y, label, accent, label_cols);

        // ── Query text (truncated from left to fit) ──────────────────────
        // The cursor sits at the END of the query, so we show the tail when
        // the query is too long (prepend "…" to indicate truncation).
        let query_start_x = text_x0 + label_cols as f32 * cw;
        // v0.9 fix: when status is hidden (show_status == false), the query
        // extends to up_x (the left edge of the ↑ button). Otherwise it
        // extends to status_x.
        let query_right_x = if show_status { status_x } else { up_x };
        let query_max_w = ((query_right_x - query_start_x) / cw).floor().max(0.0) as usize;
        // Reserve 1 col for the cursor.
        let query_budget = query_max_w.saturating_sub(1);
        let query_full_w = Self::text_col_width(&find.query);
        let (query_display, cursor_x): (String, f32) = if query_full_w <= query_budget {
            // Full query fits — cursor goes right after the last char.
            let cx = query_start_x + query_full_w as f32 * cw;
            (find.query.clone(), cx)
        } else {
            // Truncate from left: walk chars in reverse, keep the tail.
            let mut kept: Vec<char> = Vec::new();
            let mut w = 1usize; // reserve 1 for "…"
            for c in find.query.chars().rev() {
                let cw_char = unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0);
                if w + cw_char > query_budget {
                    break;
                }
                kept.push(c);
                w += cw_char;
            }
            kept.reverse();
            let mut s = String::from("…");
            s.extend(kept.iter());
            let cx = query_start_x + w as f32 * cw;
            (s, cx)
        };
        let query_cols = Self::text_col_width(&query_display);
        self.push_text(
            &mut verts,
            query_start_x,
            line_y,
            &query_display,
            fg,
            query_cols,
        );

        // ── Blinking cursor (vertical bar at end of query) ───────────────
        // 600ms on, 600ms off — standard terminal cursor blink rate.
        let blink_phase = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() % 1200)
            .unwrap_or(0);
        if blink_phase < 600 {
            let cursor_w = 2.0_f32.max(cw * 0.12);
            push_quad(
                &mut verts,
                [cursor_x, line_y, cursor_x + cursor_w, line_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        let _ = ch;
        verts
    }

    /// Build the Tab-completion dropdown as a floating popup above the prompt
    /// input box. Split out from `build_prompt_vertices` for the overlay stack
    /// Draw a Warp-style resize drag handle on the right and/or top border
    /// of a popup. The handle is two small triangles pointing inward, with a
    /// short line between them — signaling "drag to resize".
    pub(crate) fn draw_resize_handles(
        &self,
        verts: &mut Vec<f32>,
        popup_x0: f32,
        popup_top: f32,
        popup_x1: f32,
        popup_bottom: f32,
        bg_uv: [f32; 4],
    ) {
        let handle_color = [0.55, 0.55, 0.55, 0.85];
        let s = 4.0; // triangle half-size
        let gap = 6.0; // gap between the two triangles (line length)

        // Right border: two triangles pointing inward (◀ ▶) + connecting line.
        let mid_y = (popup_top + popup_bottom) / 2.0;
        let rx = popup_x1;
        // Upper triangle: points toward center (tip at rx, base at rx-s).
        push_triangle(
            verts,
            [rx - s, mid_y - gap - s],
            [rx, mid_y - gap],
            [rx - s, mid_y - gap],
            handle_color,
            bg_uv,
        );
        // Lower triangle: points toward center.
        push_triangle(
            verts,
            [rx - s, mid_y + gap + s],
            [rx, mid_y + gap],
            [rx - s, mid_y + gap],
            handle_color,
            bg_uv,
        );
        // Connecting line.
        push_quad(
            verts,
            [rx - 1.5, mid_y - gap, rx, mid_y + gap],
            bg_uv,
            [0.0; 4],
            handle_color,
        );

        // Top border: two triangles pointing inward + connecting line.
        let mid_x = (popup_x0 + popup_x1) / 2.0;
        let ty = popup_top;
        // Left triangle: points toward center (tip at mid_x-gap).
        push_triangle(
            verts,
            [mid_x - gap - s, ty],
            [mid_x - gap - s, ty + s],
            [mid_x - gap, ty + s / 2.0],
            handle_color,
            bg_uv,
        );
        // Right triangle: points toward center.
        push_triangle(
            verts,
            [mid_x + gap + s, ty],
            [mid_x + gap + s, ty + s],
            [mid_x + gap, ty + s / 2.0],
            handle_color,
            bg_uv,
        );
        // Connecting line.
        push_quad(
            verts,
            [mid_x - gap, ty, mid_x + gap, ty + 1.5],
            bg_uv,
            [0.0; 4],
            handle_color,
        );
    }

    /// Build the Tab-completion dropdown as a floating popup above the prompt
    /// z-order (Completion) and hit-test regions.
    ///
    pub(crate) fn build_completion_vertices(
        &self,
        matches: &[weft_core::complete::Match],
        selected: usize,
        layout: crate::layout::CompletionLayout,
    ) -> Vec<f32> {
        let mut verts = Vec::new();
        if matches.is_empty() {
            return verts;
        }
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let label_color = [
            fg[0] * 0.85 + theme_bg[0] * 0.15,
            fg[1] * 0.85 + theme_bg[1] * 0.15,
            fg[2] * 0.85 + theme_bg[2] * 0.15,
            1.0,
        ];
        let sel_label_color = fg;
        // v1.0 fix: blend with bg (same fix as palette popup). The old
        // fg*0.40 was invisible in Solarized Dark / One Dark / Nord.
        let suffix_color = [
            fg[0] * 0.50 + theme_bg[0] * 0.50,
            fg[1] * 0.50 + theme_bg[1] * 0.50,
            fg[2] * 0.50 + theme_bg[2] * 0.50,
            1.0,
        ];

        let [popup_x0, popup_top, popup_x1, popup_bottom] = layout.popup_rect;
        let icon_x = layout.icon_x;
        let label_x = layout.label_x;
        let suffix_x = layout.suffix_x;
        let label_cols = layout.label_cols;
        let suffix_cols = layout.suffix_cols;

        let border_c = [0.5, 0.5, 0.5, 0.35];
        let popup_bg = [
            theme_bg[0] + (1.0 - theme_bg[0]) * 0.05,
            theme_bg[1] + (1.0 - theme_bg[1]) * 0.05,
            theme_bg[2] + (1.0 - theme_bg[2]) * 0.05,
            1.0,
        ];

        push_quad(&mut verts, layout.popup_rect, bg_uv, [0.0; 4], popup_bg);
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

        // Each row occupies exactly `ch` pixels. The bottom-most row starts at
        // `popup_bottom - ch` and extends to `popup_bottom` — fully inside the
        // popup. Subsequent rows step upward by `ch`.
        let mut y = popup_bottom - ch;
        for i in (layout.start..layout.end).rev() {
            if y < popup_top {
                break;
            }
            let is_sel = i == selected;
            let lcolor = if is_sel { sel_label_color } else { label_color };
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
            let (icon, icon_color, suffix) = match matches[i].kind {
                weft_core::complete::MatchKind::Path => {
                    if matches[i].is_dir {
                        ("📁", [0.90, 0.72, 0.30, 1.0], "Directory")
                    } else {
                        ("📄", [0.45, 0.65, 0.90, 1.0], "File")
                    }
                }
                weft_core::complete::MatchKind::History => {
                    ("»", [0.60, 0.60, 0.60, 1.0], "History")
                }
                weft_core::complete::MatchKind::Command => {
                    ("»", [0.55, 0.80, 0.55, 1.0], "Command")
                }
            };
            self.push_text(&mut verts, icon_x, y, icon, icon_color, 3);
            self.push_text(
                &mut verts,
                label_x,
                y,
                &matches[i].label,
                lcolor,
                label_cols,
            );
            // v0.8 U4: suffix is globally aligned (not per-row). The column
            // anchor derives from max_label_cols — the widest visible label —
            // so every row's suffix starts at the same X. Was per-row
            // `label_x + (this_row_label_w + gap) * cw`, which made suffixes
            // stagger when labels had different widths. The X coordinate
            // itself comes from `layout.suffix_x` (precomputed in layout.rs).
            self.push_text(&mut verts, suffix_x, y, suffix, suffix_color, suffix_cols);
            y -= ch;
        }

        verts
    }

    /// Build vertices for the right-side history panel overlay: a translucent
    /// background, a search box, and one row per finished block (newest first,
    /// filtered by the query), color-coded by exit code. The selected row is
    /// highlighted and, if expanded, its output is shown beneath. Drawn after
    /// the grid so it composites on top via the enabled alpha blend.
    pub(crate) fn build_panel_vertices(&self, p: &PanelDrawParams) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let _vp_w = self.viewport.0;
        let vp_h = self.viewport.1;
        let width_px = p.width_px;
        if width_px <= 0.0 || cw <= 0.0 || ch <= 0.0 {
            return Vec::new();
        }
        // v0.9 W5: panel is now a LEFT sidebar — anchor to the left edge.
        let panel_x = 0.0;
        let panel_cols = ((width_px / cw) as usize).max(1);
        let mut vertices = Vec::new();

        // v1.0 Warp-style: opaque panel background, slightly darkened.
        let theme_bg = color_to_normalized(self.theme.background);
        let panel_bg = [
            theme_bg[0] * 0.55,
            theme_bg[1] * 0.55,
            theme_bg[2] * 0.55,
            1.0,
        ];
        let separator_color = color_to_normalized(self.theme.separator);
        // v1.0: accent-based selection highlight (consistent with grid/block).
        // v1.0 P3: α 0.95→1.0 to match Settings/Palette selection_bg.
        let sel_bg = {
            let accent = color_to_normalized(self.theme.accent);
            [
                accent[0] * 0.35 + theme_bg[0] * 0.65,
                accent[1] * 0.35 + theme_bg[1] * 0.65,
                accent[2] * 0.35 + theme_bg[2] * 0.65,
                1.0,
            ]
        };
        let (su, sv, suw, svh) = self.space_uv();
        // V-swap to match grid rendering (CAMetalLayer flip compensation).
        let bg_uv = [su, sv + svh, su + suw, sv];
        // v0.9 W5: start the panel bg below the tab bar (chrome_top) so it
        // doesn't cover the tab bar, mirroring the content area.
        let chrome_top = self.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
        push_quad(
            &mut vertices,
            [panel_x, chrome_top, panel_x + width_px, vp_h],
            bg_uv,
            [0.0; 4],
            panel_bg,
        );

        // v0.9 fix: 1px separator on the right edge (Warp-style divider)
        // so the sidebar reads as a distinct surface, not floating content.
        push_quad(
            &mut vertices,
            [
                panel_x + width_px - 1.0,
                chrome_top,
                panel_x + width_px,
                vp_h,
            ],
            bg_uv,
            [0.0; 4],
            separator_color,
        );

        let fg = color_to_normalized(self.theme.foreground);
        // v1.0 P0: replace fg*0.6 dim with label_c (70% fg + 30% bg) —
        // consistent with Settings/Palette/Find and always readable.
        let dim = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];
        let green = [0.53, 0.80, 0.36, 1.0];
        let red = [0.85, 0.36, 0.36, 1.0];

        // v0.9 fix: Warp-style search input field — a distinct rounded-look
        // box with its own background and border, so it reads as an input
        // field rather than blending into the panel. When focused, the border
        // turns accent color and a blinking cursor is drawn.
        //
        // v1.2 A4: geometry now comes from the shared `layout_panel` product
        // so the renderer and mouse handler stay in sync (no duplicated magic
        // numbers).
        let panel_layout = crate::layout::layout_panel(chrome_top, cw, ch, width_px, vp_h);
        let [field_x0, field_y0, field_x1, field_y1] = panel_layout.search_field_rect;
        // v0.9 fix: add a "History" header above the search field for a
        // clearer panel identity (Warp-style section title).
        let header_y = chrome_top + ch * 0.4;
        self.push_text(
            &mut vertices,
            panel_x + cw * 0.5,
            header_y,
            "History",
            fg,
            panel_cols,
        );
        // Input field background: slightly lighter than panel bg.
        let field_bg = [
            panel_bg[0] + (1.0 - panel_bg[0]) * 0.08,
            panel_bg[1] + (1.0 - panel_bg[1]) * 0.08,
            panel_bg[2] + (1.0 - panel_bg[2]) * 0.08,
            1.0,
        ];
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            field_bg,
        );
        // Border: accent when focused (was accent_dim — invisible in
        // Nord/Warp themes), separator otherwise.
        let border_color = if p.search_focused {
            color_to_normalized(self.theme.accent)
        } else {
            separator_color
        };
        let border_w = if p.search_focused { 2.0 } else { 1.0 };
        // Top border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x1, field_y0 + border_w],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Bottom border
        push_quad(
            &mut vertices,
            [field_x0, field_y1 - border_w, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Left border
        push_quad(
            &mut vertices,
            [field_x0, field_y0, field_x0 + border_w, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );
        // Right border
        push_quad(
            &mut vertices,
            [field_x1 - border_w, field_y0, field_x1, field_y1],
            bg_uv,
            [0.0; 4],
            border_color,
        );

        // Text inside the field: show query, or placeholder "Search…" when empty.
        let text_y = field_y0 + (field_y1 - field_y0 - ch) * 0.5;
        let text_x = field_x0 + cw * 0.4;
        let text_cols = ((field_x1 - text_x - cw * 0.4) / cw) as usize;
        if p.query.is_empty() {
            self.push_text(&mut vertices, text_x, text_y, "Search…", dim, text_cols);
        } else {
            self.push_text(&mut vertices, text_x, text_y, p.query, fg, text_cols);
        }

        // Blinking cursor at the end of the query text when focused.
        if p.search_focused {
            let blink_phase = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() % 1200)
                .unwrap_or(0);
            if blink_phase < 600 {
                let query_w = p.query.chars().count() as f32 * cw;
                let cursor_x = text_x + query_w;
                let cursor_w = 2.0_f32.max(cw * 0.12);
                push_quad(
                    &mut vertices,
                    [cursor_x, text_y, cursor_x + cursor_w, text_y + ch],
                    bg_uv,
                    [0.0; 4],
                    fg,
                );
            }
        }

        // Display list: newest-first, filtered by query (capped to fit).
        let max_rows = visible_panel_rows(vp_h, self.cell_height());
        let display = panel_display(p.blocks, p.query, max_rows);
        let row_h = panel_layout.row_height;
        let mut y = panel_layout.list_top;
        let mut drawn = 0usize;

        for (i, block) in display.iter().enumerate() {
            if drawn >= max_rows || y + ch > vp_h {
                break;
            }
            let selected = i == p.selection;
            if selected {
                push_quad(
                    &mut vertices,
                    [panel_x, y - ch * 0.1, panel_x + width_px, y + ch],
                    bg_uv,
                    [0.0; 4],
                    sel_bg,
                );
            }
            let cmd_color = if selected {
                fg
            } else {
                match block.exit_code {
                    Some(0) => green,
                    Some(_) => red,
                    None => dim,
                }
            };
            let dur = block_duration_str(block);
            let dur_len = dur.chars().count();
            let cmd_cols = panel_cols.saturating_sub(dur_len + 2).max(1);
            // v0.9 fix: strip prompt prefix so old blocks (captured via
            // snapshot_command_line) show just the command, matching the
            // clean format of editor-submitted commands.
            let cleaned = strip_prompt_prefix(&block.command);
            let label = truncate_str(&cleaned, cmd_cols);
            self.push_text(
                &mut vertices,
                panel_x + cw * 0.5,
                y,
                &label,
                cmd_color,
                cmd_cols,
            );
            if !dur.is_empty() {
                let dur_x = panel_x + width_px - cw * 0.5 - dur_len as f32 * cw;
                self.push_text(&mut vertices, dur_x, y, &dur, dim, dur_len + 1);
            }
            y += row_h;
            drawn += 1;

            // Expanded output for the selected block.
            if Some(block.id) == p.expanded_id {
                let out_cols = panel_cols.saturating_sub(2).max(1);
                for line in block.output.lines().take(8) {
                    if drawn >= max_rows || y + ch > vp_h {
                        break;
                    }
                    let rendered = truncate_str(line, out_cols);
                    self.push_text(
                        &mut vertices,
                        panel_x + cw * 1.5,
                        y,
                        &rendered,
                        dim,
                        out_cols,
                    );
                    y += row_h;
                    drawn += 1;
                }
            }
        }

        vertices
    }

    /// v0.9 H1: Build the tab bar vertices (background + tab labels + close
    /// buttons). Returns `(vertices, tab_hits)` where `tab_hits` is the
    /// click hit-test data for the app's mouse handler.
    ///
    /// Layout:
    /// - Tab bar spans the full viewport width at the top.
    /// - Each tab is ~16 cells wide, with a 1px divider between tabs.
    /// - Active tab gets a brighter background + accent underline.
    /// - Close "×" button at the right of each tab.
    pub(crate) fn build_tab_bar_vertices(&self, tab_bar: &TabBarDrawState) -> Vec<f32> {
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let bar_h = self.tab_bar_height();
        let vp_w = self.viewport.0;
        let pad_x = self.padding_x;
        let chrome_left = self.layout_ctx.map(|c| c.chrome_left).unwrap_or(0.0);

        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme);
        let bg = color_to_normalized(ui.canvas);
        let fg = color_to_normalized(ui.text_primary);
        let accent = color_to_normalized(ui.focus);
        let separator = color_to_normalized(self.theme.separator);

        let bar_bg = color_to_normalized(ui.chrome);

        let mut vertices = Vec::new();

        // v1.2 architecture: renderer and App scroll/hit behavior consume the
        // same pure tab-strip layout product.
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: vp_w,
            bar_height: bar_h,
            cell_width: cw,
            padding_x: pad_x,
            chrome_left,
            traffic_lights_width: self.traffic_lights_width(),
            tab_count: tab_bar.tab_count,
            requested_scroll_offset: tab_bar.scroll_offset,
        });
        let tab_w = strip.tab_width;
        let overflowing = strip.overflowing;
        let arrow_w = strip.arrow_width;
        let plus_w = strip.plus_width;
        let tabs_start = strip.tabs_start;
        let scroll_offset = strip.scroll_offset;
        let vis_left = strip.visible_left;
        let vis_right = strip.visible_right;

        push_quad(&mut vertices, strip.bar_rect, [0.0; 4], [0.0; 4], bar_bg);

        let close_w = cw * 2.0;
        // v1.2-fix: reserve a small gap between label text and close button
        // so truncated "…" doesn't overlap the × icon.
        let label_gap = cw * 0.5;
        let label_w = tab_w - close_w - label_gap;
        let y0 = 0.0f32;
        let y1 = bar_h;

        for i in 0..tab_bar.tab_count {
            // Apply scroll offset to x position.
            let x0 = tabs_start + i as f32 * tab_w - scroll_offset;
            let x1 = x0 + tab_w;

            // CPU-side cull: skip tabs entirely outside the visible region.
            if x1 < vis_left || x0 > vis_right {
                continue;
            }

            let is_active = i == tab_bar.active_tab;
            let is_hovered = tab_bar.hovered_tab == Some(i);

            // Clamp rendering to [vis_left, vis_right] so tab backgrounds
            // don't bleed under the arrows or the "+" button.
            let draw_x0 = x0.max(vis_left);
            let draw_x1 = x1.min(vis_right);

            // Tab background: active tab gets the main bg; hovered tab gets
            // a subtle highlight (Warp-style hover feedback).
            if is_active {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    bg,
                );
                push_quad(
                    &mut vertices,
                    [draw_x0, y1 - 2.0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    accent,
                );
            } else if is_hovered {
                // v1.2: hover highlight — a subtle light overlay on inactive
                // tabs when the mouse is over them (matches demo behavior).
                let hover_bg = [
                    fg[0] * 0.08 + bar_bg[0] * 0.92,
                    fg[1] * 0.08 + bar_bg[1] * 0.92,
                    fg[2] * 0.08 + bar_bg[2] * 0.92,
                    1.0,
                ];
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x1, y1],
                    [0.0; 4],
                    [0.0; 4],
                    hover_bg,
                );
            }

            // Divider between tabs.
            if i > 0 && x0 >= vis_left {
                push_quad(
                    &mut vertices,
                    [draw_x0, y0, draw_x0 + 1.0, y1],
                    [0.0; 4],
                    [0.0; 4],
                    separator,
                );
            }

            // ── Tab label ──
            // Text starts at x0 + cw*0.5 (clamped to vis_left). Text must end
            // before the close button area: text_right = min(x1, vis_right)
            // - close_w. This ensures "…" truncation never overlaps ×.
            let label_x = (x0 + cw * 0.5).max(vis_left);
            let text_right = x1.min(vis_right) - close_w;
            let avail_text_w = (text_right - label_x).max(0.0);
            if avail_text_w >= cw {
                let max_cols = ((avail_text_w / cw) as usize).max(1);
                let label = tab_bar.labels.get(i).map(|s| s.as_str()).unwrap_or("");
                let display = truncate_str(label, max_cols.saturating_sub(1));
                let label_color = if is_active {
                    fg
                } else if is_hovered {
                    [fg[0] * 0.85, fg[1] * 0.85, fg[2] * 0.85, 1.0]
                } else {
                    [fg[0] * 0.6, fg[1] * 0.6, fg[2] * 0.6, 1.0]
                };
                self.push_text(
                    &mut vertices,
                    label_x,
                    y0 + (bar_h - ch) * 0.5,
                    &display,
                    label_color,
                    max_cols,
                );
            }

            // ── Close button ──
            // The × center cx = x0 + label_w + close_w*0.5. The × is shown
            // only when cx is inside the visible region [vis_left, vis_right]
            // — this applies to BOTH active and inactive tabs, so the × never
            // overlaps the scroll arrows, "+" button, or tab label text.
            // Inactive tabs additionally require hover.
            let close_cx = x0 + label_w + close_w * 0.5;
            let close_cy = y0 + bar_h * 0.5;
            let close_r = if is_active { ch * 0.16 } else { ch * 0.13 };
            let cx_in_view = close_cx - close_r >= vis_left && close_cx + close_r <= vis_right;
            let show_close = cx_in_view && (is_active || is_hovered);
            if show_close {
                let close_color = if is_active {
                    fg
                } else {
                    [fg[0] * 0.5, fg[1] * 0.5, fg[2] * 0.5, 1.0]
                };
                let line_w = crate::ui_tokens::UiMetrics::for_scale(self.scale).stroke;
                push_line(
                    &mut vertices,
                    close_cx - close_r,
                    close_cy - close_r,
                    close_cx + close_r,
                    close_cy + close_r,
                    line_w,
                    close_color,
                );
                push_line(
                    &mut vertices,
                    close_cx - close_r,
                    close_cy + close_r,
                    close_cx + close_r,
                    close_cy - close_r,
                    line_w,
                    close_color,
                );
            }

            // Hit rect: close_rect registered only when cx is in view.
            // Hit regions now live in the TabBar Scene (tab_bar_component.rs).
        }

        // v1.2: Scroll arrows — drawn when tabs overflow.
        // Opaque background quads under the arrows prevent partially-visible
        // tabs from showing through behind the arrow icons.
        if overflowing {
            let max_scroll = strip.max_scroll;
            let left_arrow_rect = strip.left_arrow_rect.expect("overflow has left arrow");
            let right_arrow_rect = strip.right_arrow_rect.expect("overflow has right arrow");

            // ── Left arrow (‹) ──
            let la_cx = tabs_start + arrow_w * 0.5;
            let la_cy = bar_h * 0.5;
            let la_r = ch * 0.14;
            let la_w = 1.5 * self.scale as f32;
            // Background: bar_bg + hover highlight if the mouse is over it.
            let la_bg = if tab_bar.arrow_left_hovered && scroll_offset > 0.0 {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(&mut vertices, left_arrow_rect, [0.0; 4], [0.0; 4], la_bg);
            let la_color = if scroll_offset > 0.0 {
                if tab_bar.arrow_left_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                la_cx + la_r,
                la_cy - la_r,
                la_cx - la_r,
                la_cy,
                la_w,
                la_color,
            );
            push_line(
                &mut vertices,
                la_cx - la_r,
                la_cy,
                la_cx + la_r,
                la_cy + la_r,
                la_w,
                la_color,
            );

            // ── Right arrow (›) ──
            let ra_cx = vis_right + arrow_w * 0.5;
            let ra_cy = bar_h * 0.5;
            let ra_r = ch * 0.14;
            let ra_w = 1.5 * self.scale as f32;
            let ra_bg = if tab_bar.arrow_right_hovered && scroll_offset < max_scroll {
                [
                    fg[0] * 0.12 + bar_bg[0] * 0.88,
                    fg[1] * 0.12 + bar_bg[1] * 0.88,
                    fg[2] * 0.12 + bar_bg[2] * 0.88,
                    1.0,
                ]
            } else {
                bar_bg
            };
            push_quad(&mut vertices, right_arrow_rect, [0.0; 4], [0.0; 4], ra_bg);
            let ra_color = if scroll_offset < max_scroll {
                if tab_bar.arrow_right_hovered {
                    fg
                } else {
                    [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
                }
            } else {
                [fg[0] * 0.25, fg[1] * 0.25, fg[2] * 0.25, 1.0]
            };
            push_line(
                &mut vertices,
                ra_cx - ra_r,
                ra_cy - ra_r,
                ra_cx + ra_r,
                ra_cy,
                ra_w,
                ra_color,
            );
            push_line(
                &mut vertices,
                ra_cx + ra_r,
                ra_cy,
                ra_cx - ra_r,
                ra_cy + ra_r,
                ra_w,
                ra_color,
            );
        }

        // v1.2: "+" button position:
        //   - Non-overflowing: right after the last tab (natural flow).
        //   - Overflowing: after the right scroll arrow (fixed position).
        let plus_x0 = strip.plus_rect[0];
        let plus_cx = plus_x0 + plus_w * 0.5;
        let plus_cy = bar_h * 0.5;
        let plus_r = ch * 0.22;
        let plus_line_w = 1.5 * self.scale as f32;
        // v1.2: hover highlight.
        if tab_bar.plus_hovered {
            let hover_bg = [
                fg[0] * 0.12 + bar_bg[0] * 0.88,
                fg[1] * 0.12 + bar_bg[1] * 0.88,
                fg[2] * 0.12 + bar_bg[2] * 0.88,
                1.0,
            ];
            push_quad(
                &mut vertices,
                [plus_x0, y0, plus_x0 + plus_w, y1],
                [0.0; 4],
                [0.0; 4],
                hover_bg,
            );
        }
        let plus_color = if tab_bar.plus_hovered {
            fg
        } else {
            [fg[0] * 0.7, fg[1] * 0.7, fg[2] * 0.7, 1.0]
        };
        push_line(
            &mut vertices,
            plus_cx - plus_r,
            plus_cy,
            plus_cx + plus_r,
            plus_cy,
            plus_line_w,
            plus_color,
        );
        push_line(
            &mut vertices,
            plus_cx,
            plus_cy - plus_r,
            plus_cx,
            plus_cy + plus_r,
            plus_line_w,
            plus_color,
        );
        vertices
    }
}
