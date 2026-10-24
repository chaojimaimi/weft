//! Find-overlay (FindInGrid popup) vertex builder, moved verbatim out of
//! `overlays.rs` (v1.13.9 backlog clear zero-behavior file-budget split;
//! `impl MetalRenderer` cross-file block per the vt/modes.rs precedent).

use super::{color_to_normalized, push_quad, split_highlight_around_cursor, FindDrawState};
use crate::renderer::MetalRenderer;

impl MetalRenderer {
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
        use crate::paint::command_surface::{build_command_surface_shell, CommandSurfaceShell};

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

        let [popup_x0, popup_y0, _popup_x1, popup_y1] = layout.popup_rect;

        let theme_bg = color_to_normalized(self.theme.background);
        let accent = color_to_normalized(self.theme.accent);
        let sep = color_to_normalized(self.theme.separator);
        let fg = color_to_normalized(self.theme.foreground);
        let accent_dim = [
            fg[0] * 0.70 + theme_bg[0] * 0.30,
            fg[1] * 0.70 + theme_bg[1] * 0.30,
            fg[2] * 0.70 + theme_bg[2] * 0.30,
            1.0,
        ];

        // F4: shared shell (shadow + bg + border). No resize handles (it's
        // a utility bar, not a resizable popup). v1.14.0 (T5): token-driven
        // material (compact radius + dual-layer soft shadow) — unified with
        // Palette, Completion and ContextMenu.
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(
                layout.popup_rect,
                crate::ui_tokens::SurfaceTokens::compact()
                    .scaled(self.scale as f32)
                    .for_environment(self.theme.is_dark(), self.increase_contrast),
                false,
                theme_bg,
                bg_uv,
            ),
        );

        // F6: Focus ring — the Find popup captures keyboard input while open,
        // so draw a focus ring to indicate it's the active input target.
        {
            use crate::paint::primitives::{
                build_focus_ring, focus_ring_alpha, focus_ring_thickness,
            };
            let [px0, py0, px1, py1] = layout.popup_rect;
            let accent = color_to_normalized(self.theme.accent);
            let ring_color = [
                accent[0],
                accent[1],
                accent[2],
                focus_ring_alpha(self.increase_contrast),
            ];
            build_focus_ring(
                &mut verts,
                [px0 - 1.0, py0 - 1.0, px1 + 1.0, py1 + 1.0],
                ring_color,
                focus_ring_thickness(self.increase_contrast),
            );
        }

        // ── Accent-colored left stripe (3px) — v0.8 signature accent ────
        // Replaces the previous top accent stripe so the popup still reads
        // as branded without occupying vertical space at the card edge.
        let stripe_w = 3.0;
        let border_w = 1.0;
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
            // F3-5: use the semantic find_match token instead of a hardcoded
            // yellow so the color adapts to the active theme.
            let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast);
            let fm = color_to_normalized(ui.find_match);
            let highlight_color = [fm[0], fm[1], fm[2], 0.50];
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
        // F4: derive the formal surface state (Ready/Loading/Empty/Error)
        // from the observable inputs. The existing inline status text is
        // kept (it has more nuance: truncated, block_matches, etc.), but the
        // error color now comes from the unified state so Find, Palette and
        // Completion all treat "invalid regex" as a formal Error state.
        let surface_state = find.surface_state();
        let (status, status_color) = if surface_state.is_error() {
            ("invalid regex  ".to_string(), accent)
        } else if find.query.is_empty() {
            (String::new(), accent_dim)
        } else if find.worker_busy {
            (format!("{}  ", surface_state.status_text()), accent_dim)
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
        // v1.12.26 (P1-02): reserve 1 col for the cursor PLUS the active
        // preedit's extent, so the tail truncation shrinks the query to
        // make room instead of the preedit overpainting the caret
        // (palette.rs's preedit-aware cx precedent). Width math is
        // text_col_width-based (N-4 lesson: char-count math drew CJK
        // carets at half the text extent).
        let preedit_cells = Self::text_col_width(&find.ime_preedit);
        let query_budget = query_max_w.saturating_sub(1).saturating_sub(preedit_cells);
        let query_full_w = Self::text_col_width(&find.query);
        let (query_display, mut cursor_x): (String, f32) = if query_full_w <= query_budget {
            // Full query fits — cursor goes right after the last char.
            let cx = query_start_x + query_full_w as f32 * cw;
            (find.query.clone(), cx)
        } else {
            // Truncate from left: walk chars in reverse, keep the tail.
            use unicode_segmentation::UnicodeSegmentation;
            let mut kept: Vec<&str> = Vec::new();
            let mut w = weft_core::grid::terminal_text_width("…");
            for grapheme in find.query.graphemes(true).rev() {
                let width = weft_core::grid::terminal_text_width(grapheme);
                if w + width > query_budget {
                    break;
                }
                kept.push(grapheme);
                w += width;
            }
            kept.reverse();
            let mut s = String::from("…");
            for grapheme in kept {
                s.push_str(grapheme);
            }
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

        // ── IME preedit inline after the query tail (v1.12.26 P1-02) ─────
        // Accent-colored + underlined (palette/note precedent). Over-wide
        // compositions truncate via push_text's column budget; the caret
        // below rides the SHOWN width so it stays glued to the composition.
        if !find.ime_preedit.is_empty() {
            let preedit_max = query_max_w.saturating_sub(1).saturating_sub(query_cols);
            if preedit_max > 0 {
                let shown = preedit_cells.min(preedit_max) as f32;
                self.push_text(
                    &mut verts,
                    cursor_x,
                    line_y,
                    &find.ime_preedit,
                    accent,
                    preedit_max,
                );
                let underline = [
                    cursor_x,
                    line_y + ch - 1.0,
                    cursor_x + shown * cw,
                    line_y + ch,
                ];
                push_quad(&mut verts, underline, bg_uv, [0.0; 4], accent);
                cursor_x += shown * cw;
            }
        }

        // ── Blinking cursor (vertical bar at end of query) ───────────────
        // v1.12.26 (P1-01): the caret follows the renderer-global blink
        // phase (`cursor_blink_on`), exactly like palette/note. The old
        // private wall-clock phase self-corrected but never painted when
        // no redraw was scheduled (e.g. a command executing froze the bar
        // on its last frame) — the shared phase plus the text-input
        // overlay wake keeps it blinking in every state.
        if self.cursor_blink_on {
            let cursor_w = 2.0_f32.max(cw * 0.12);
            push_quad(
                &mut verts,
                [cursor_x, line_y, cursor_x + cursor_w, line_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        verts
    }
}
