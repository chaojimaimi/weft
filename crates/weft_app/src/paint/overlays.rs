//! Context menu, Find and Completion vertex builders.

use crate::paint::primitives::{color_to_normalized, push_quad, push_triangle};
use crate::renderer::MetalRenderer;

/// Per-frame FindInGrid draw state (v0.8 B3). Set by the app before `draw()`.
#[derive(Clone, Debug, Default)]
pub struct FindDrawState {
    /// Live query string (rendered in the bar input).
    pub query: String,
    /// 1-based index of the current match, or 0 when there are no matches.
    pub current: usize,
    /// Total NAVIGABLE matches. In grid view this is grid matches; in block
    /// view this is block matches (Enter cycles through them).
    pub total: usize,
    /// True when `total` hit MAX_MATCHES — surfaced as "too many matches".
    pub truncated: bool,
    /// Current GRID match's `(viewport_row, col, len_in_cells)` — `None` when
    /// no match is selected or in block view. The renderer highlights this
    /// rectangle.
    pub highlight: Option<(usize, usize, usize)>,
    /// F2 P1-1: Terminal cursor `(row, col)` so the highlight can leave a gap
    /// over the cursor cell, keeping the cursor visible underneath. `None`
    /// when the cursor position is unknown or shouldn't be excluded.
    pub cursor_pos: Option<(usize, usize)>,
    /// F2 P1-1: Whether the grid cursor is drawn this frame (encodes
    /// `cursor_visible && blink_on && prompt.is_none()`). When false, the
    /// highlight covers the cursor cell as before.
    pub show_cursor: bool,
    /// Current BLOCK match's `(block_id, line, is_command, col, len)` — used
    /// to highlight the match in block view. `None` in grid view or when no
    /// match is selected.
    pub block_highlight: Option<(u64, usize, bool, usize, usize)>,
    /// Non-navigable matches found in block content (block view only). When
    /// `total == 0` and this is > 0, the status shows "N matches in blocks"
    /// so the user knows the search did find things (just not navigable).
    pub block_matches: usize,
    /// Regex mode toggle (v0.9 U-P2: now actually wired — when true, the
    /// query is compiled as a `regex::Regex` and matched via `find_iter`).
    /// When true, the ".*" indicator lights up in accent color.
    pub regex_mode: bool,
    /// Case-sensitive toggle. When true, the "Aa" indicator lights up in
    /// accent color and the search matches exact character case.
    pub case_sensitive: bool,
    /// Regex compile error message (v0.9 U-P2). When `Some`, the FindUI
    /// shows "invalid regex" in red instead of the match count. Cleared
    /// when the query compiles successfully or regex mode is toggled off.
    pub regex_error: Option<String>,
}

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
        // F2 P1-1: split the highlight around the cursor cell so the cursor
        // stays visible underneath when `show_cursor` is true and the cursor
        // sits inside the highlighted range.
        if let Some((row, col, len)) = find.highlight {
            let hy0 = ctx.row_y(row);
            let hy1 = hy0 + ch;
            let highlight_color = [0.95f32, 0.78, 0.20, 0.50];
            for (seg_col, seg_len) in split_highlight_around_cursor(
                col,
                len,
                find.cursor_pos.filter(|(r, _)| *r == row),
                find.show_cursor,
            ) {
                let sx0 = ctx.col_x(seg_col);
                let sx1 = sx0 + seg_len as f32 * cw;
                push_quad(
                    &mut verts,
                    [sx0, hy0, sx1, hy1],
                    bg_uv,
                    [0.0; 4],
                    highlight_color,
                );
            }
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
}

/// F2 P1-1: Compute the column segments of a Find highlight after excluding
/// the cursor cell, so the terminal cursor stays visible underneath the
/// yellow translucent overlay.
///
/// Returns a list of `(start_col, len)` pairs. When the cursor should not be
/// excluded (`show_cursor == false` or `cursor_pos` is `None` / outside the
/// range), returns a single segment covering the whole highlight.
///
/// Pure function — no rendering side effects, so it can be unit-tested.
fn split_highlight_around_cursor(
    col: usize,
    len: usize,
    cursor_pos: Option<(usize, usize)>,
    show_cursor: bool,
) -> Vec<(usize, usize)> {
    if !show_cursor {
        return vec![(col, len)];
    }
    let (_, cur_col) = match cursor_pos {
        Some(c) => c,
        None => return vec![(col, len)],
    };
    // Cursor cell is [cur_col, cur_col+1). Skip it only when it overlaps
    // the highlight range [col, col+len).
    if cur_col < col || cur_col >= col + len {
        return vec![(col, len)];
    }
    let mut segs = Vec::with_capacity(2);
    // Left segment: [col, cur_col)
    if cur_col > col {
        segs.push((col, cur_col - col));
    }
    // Right segment: [cur_col+1, col+len)
    let right_start = cur_col + 1;
    if right_start < col + len {
        segs.push((right_start, col + len - right_start));
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_highlight_no_cursor_returns_whole_range() {
        assert_eq!(
            split_highlight_around_cursor(5, 3, None, true),
            vec![(5, 3)]
        );
    }

    #[test]
    fn split_highlight_show_cursor_false_returns_whole_range() {
        assert_eq!(
            split_highlight_around_cursor(5, 3, Some((0, 6)), false),
            vec![(5, 3)]
        );
    }

    #[test]
    fn split_highlight_cursor_outside_range_returns_whole_range() {
        assert_eq!(
            split_highlight_around_cursor(5, 3, Some((0, 10)), true),
            vec![(5, 3)]
        );
        assert_eq!(
            split_highlight_around_cursor(5, 3, Some((0, 4)), true),
            vec![(5, 3)]
        );
    }

    #[test]
    fn split_highlight_cursor_at_start_leaves_right_segment() {
        // col=5, len=3 → range [5,8). cursor at col 5 → skip [5,6), keep [6,8)
        assert_eq!(
            split_highlight_around_cursor(5, 3, Some((0, 5)), true),
            vec![(6, 2)]
        );
    }

    #[test]
    fn split_highlight_cursor_at_end_leaves_left_segment() {
        // col=5, len=3 → range [5,8). cursor at col 7 → keep [5,7), skip [7,8)
        assert_eq!(
            split_highlight_around_cursor(5, 3, Some((0, 7)), true),
            vec![(5, 2)]
        );
    }

    #[test]
    fn split_highlight_cursor_in_middle_splits_into_two() {
        // col=5, len=4 → range [5,9). cursor at col 6 → [5,6) + [7,9)
        assert_eq!(
            split_highlight_around_cursor(5, 4, Some((0, 6)), true),
            vec![(5, 1), (7, 2)]
        );
    }

    #[test]
    fn split_highlight_single_cell_with_cursor_returns_empty() {
        // col=5, len=1 → range [5,6). cursor at col 5 → no segments
        assert_eq!(
            split_highlight_around_cursor(5, 1, Some((0, 5)), true),
            vec![]
        );
    }
}
