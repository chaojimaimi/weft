//! Context menu, Find and Completion vertex builders.

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::paint::text::{
    note_card_width, note_display_window, NOTE_CARD_ACCENT_STRIPE_W, NOTE_CARD_INNER_PAD,
    NOTE_CARD_LABEL, NOTE_CARD_LINE_H_CELLS, NOTE_CARD_LINE_H_MIN_PX, NOTE_CARD_TOP_OFFSET,
};
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
    /// Async grid scan is still running. Drives the shared Loading state so
    /// a large scrollback search never looks like a completed empty result.
    pub worker_busy: bool,
}

/// v1.7.3-C: Per-frame note editor draw state. Set by the app before
/// `draw()` when the inline note editor is open. Renders a top-center
/// card with "Note: [buffer|]" and a hint footer.
#[derive(Clone, Debug, Default)]
pub struct NoteEditorDrawState {
    /// Current buffer text.
    pub buffer: String,
    /// Byte offset of the caret in `buffer`.
    pub cursor: usize,
    /// v1.12.24 (N-1): active IME composition, rendered inline at the caret.
    pub ime_preedit: String,
}

impl FindDrawState {
    fn surface_state(&self) -> crate::paint::command_surface::CommandSurfaceState {
        crate::paint::command_surface::find_surface_state(
            &self.query,
            self.worker_busy,
            self.total,
            self.regex_error.as_deref(),
        )
    }
}

impl MetalRenderer {
    /// Build the right-click context menu (F7) as a small popup at (x, y).
    ///
    /// F4: shell now uses the shared `command_surface` builder so the menu
    /// shares the same 8% lifted bg + unified border as Palette/Find/
    /// Completion. Item text + separators remain menu-specific.
    pub(crate) fn build_context_menu_vertices(&self, x: f32, y: f32, selection: usize) -> Vec<f32> {
        use crate::paint::command_surface::{build_command_surface_shell, CommandSurfaceShell};

        let mut verts = Vec::new();
        let cw = self.cell_width() as f32;
        let ui = crate::ui_tokens::UiColors::from_theme(&self.theme)
            .with_increase_contrast(self.increase_contrast);
        let fg = color_to_normalized(ui.text_primary);
        let prompt_c = color_to_normalized(ui.text_secondary);
        let selection_bg = color_to_normalized(ui.selection);
        let selection_fg = color_to_normalized(ui.selection_text);
        let separator = color_to_normalized(self.theme.separator);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let items = [
            "Copy Command",
            "Copy Output",
            "Copy Block",
            "Toggle Fold",
            "Send to Input",
            "Toggle Bookmark",
            "Add Note",
            "Export Block",
            "Diagnose with AI",
        ];

        let ctx = self.layout_ctx.expect("LayoutCtx built at draw() entry");
        let layout = crate::layout::layout_context_menu(&ctx, x, y, self.scale as f32);
        let [menu_x0, _menu_y0, menu_x1, _menu_y1] = layout.menu_rect;
        let menu_w = menu_x1 - menu_x0;
        let text_x = layout.text_x;

        let theme_bg = color_to_normalized(self.theme.background);
        // F4: shared shell (bg + border, no shadow, no resize handles).
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(layout.menu_rect, 0.0, false, theme_bg, bg_uv),
        );

        // Items.
        let selected =
            crate::context_menu_component::clamped_context_menu_selection(selection, items.len());
        // v1.7.3-C: the reuse group (Bookmark / Add Note / Export) renders in
        // accent to set it apart from the editing actions. v1.10.34: fixed
        // semantic anchor — `len - 3` drifted when "Copy Block" (index 2) was
        // inserted and "Diagnose with AI" (last) must stay normal-fg.
        let accent_start = 5;
        for (i, label) in items.iter().enumerate() {
            let item_y = layout.item_y[i];
            if selected == Some(i) {
                push_quad(
                    &mut verts,
                    layout.item_rects[i],
                    bg_uv,
                    [0.0; 4],
                    selection_bg,
                );
            }
            let color = if selected == Some(i) {
                selection_fg
            } else if i >= accent_start {
                prompt_c
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

        // F4: shared shell (shadow + bg + border). Find uses a 2px shadow
        // pad and no resize handles (it's a utility bar, not a resizable
        // popup). Replaces the hand-rolled shadow/bg/border quads with the
        // unified command-surface shell so Find, Palette, Completion and
        // ContextMenu share the same visual language.
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(layout.popup_rect, 2.0, false, theme_bg, bg_uv),
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
        // Reserve 1 col for the cursor.
        let query_budget = query_max_w.saturating_sub(1);
        let query_full_w = Self::text_col_width(&find.query);
        let (query_display, cursor_x): (String, f32) = if query_full_w <= query_budget {
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

    /// v1.7.3-C: Build the inline note editor overlay — a top-center card
    /// showing "Note: [buffer|]" with a caret and a hint footer. Uses the
    /// same command-surface shell as Find/ContextMenu for visual consistency.
    pub(crate) fn build_note_editor_vertices(&self, state: &NoteEditorDrawState) -> Vec<f32> {
        use crate::paint::command_surface::{build_command_surface_shell, CommandSurfaceShell};

        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let ctx = match &self.layout_ctx {
            Some(c) => *c,
            None => return Vec::new(),
        };

        // Layout: top-center card, 60% of viewport width (clamped to
        // [40cw, 80cw]). Height = 1 input line + 1 hint line + padding.
        // v1.12.24.1 (P1-3): geometry constants are the shared `paint::text`
        // note-card consts (single source of truth with the IME anchor).
        let vp_w = ctx.viewport.0;
        let target_w = note_card_width(vp_w, cw);
        let popup_x0 = (vp_w - target_w) * 0.5;
        let popup_x1 = popup_x0 + target_w;
        let popup_y0 = ctx.top() + NOTE_CARD_TOP_OFFSET;
        let line_h = (ch * NOTE_CARD_LINE_H_CELLS).max(ch + NOTE_CARD_LINE_H_MIN_PX);
        let hint_h = ch * 1.2;
        let popup_y1 = popup_y0 + line_h + hint_h + ch * 0.3;

        let mut verts = Vec::new();
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let theme_bg = color_to_normalized(self.theme.background);
        let accent = color_to_normalized(self.theme.accent);
        let fg = color_to_normalized(self.theme.foreground);
        let sep = color_to_normalized(self.theme.separator);
        let prompt_c = color_to_normalized(
            crate::ui_tokens::UiColors::from_theme(&self.theme)
                .with_increase_contrast(self.increase_contrast)
                .text_secondary,
        );

        // Shell (shadow + bg + border).
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(
                [popup_x0, popup_y0, popup_x1, popup_y1],
                2.0,
                false,
                theme_bg,
                bg_uv,
            ),
        );

        // Focus ring — the note editor captures keyboard input.
        {
            use crate::paint::primitives::{
                build_focus_ring, focus_ring_alpha, focus_ring_thickness,
            };
            let ring_color = [
                accent[0],
                accent[1],
                accent[2],
                focus_ring_alpha(self.increase_contrast),
            ];
            build_focus_ring(
                &mut verts,
                [
                    popup_x0 - 1.0,
                    popup_y0 - 1.0,
                    popup_x1 + 1.0,
                    popup_y1 + 1.0,
                ],
                ring_color,
                focus_ring_thickness(self.increase_contrast),
            );
        }

        // Accent left stripe — branded accent.
        let stripe_w = NOTE_CARD_ACCENT_STRIPE_W;
        push_quad(
            &mut verts,
            [popup_x0, popup_y0, popup_x0 + stripe_w, popup_y1],
            bg_uv,
            [0.0; 4],
            accent,
        );

        // Input line: "Note: " + buffer + caret.
        let inner_pad = NOTE_CARD_INNER_PAD;
        let text_x = popup_x0 + stripe_w + inner_pad;
        let text_w = popup_x1 - popup_x0 - stripe_w - inner_pad * 2.0;
        let line_y = popup_y0 + (line_h - ch) * 0.5;

        let label = NOTE_CARD_LABEL;
        let label_cells = label.len(); // ASCII
        self.push_text(&mut verts, text_x, line_y, label, prompt_c, label_cells);

        let buf_x = text_x + label_cells as f32 * cw;
        let buf_max_cells = ((text_x + text_w - buf_x) / cw).floor() as usize;
        // v1.12.24.1 (N-4): cell-based trailing display window — the v1.7.3
        // char-count math drew the caret at half the CJK text extent and
        // never scrolled once the card's cell budget filled.
        let preedit_cells = Self::text_col_width(&state.ime_preedit);
        let win = note_display_window(&state.buffer, state.cursor, preedit_cells, buf_max_cells);
        if !win.display.is_empty() {
            let buf_cols = Self::text_col_width(&win.display);
            self.push_text(&mut verts, buf_x, line_y, &win.display, fg, buf_cols);
        }

        // Caret: blink-driven block cursor at the caret position (clamped
        // to the visible display region so it doesn't overflow the popup).
        let caret_x = buf_x + win.caret_cell as f32 * cw;
        if self.cursor_blink_on {
            push_quad(
                &mut verts,
                [caret_x, line_y, caret_x + cw * 0.5, line_y + ch],
                bg_uv,
                [0.0; 4],
                fg,
            );
        }

        // v1.12.24 (N-1): IME preedit inline at the caret (palette precedent),
        // accent-colored + underlined. Width = text_col_width (CJK = 2 cols);
        // visible truncation rides push_text's column budget.
        if !state.ime_preedit.is_empty() {
            let preedit_max = buf_max_cells.saturating_sub(win.caret_cell);
            let cols = Self::text_col_width(&state.ime_preedit).min(preedit_max) as f32;
            let underline = [caret_x, line_y + ch - 1.0, caret_x + cols * cw, line_y + ch];
            let preedit = &state.ime_preedit;
            self.push_text(&mut verts, caret_x, line_y, preedit, accent, preedit_max);
            push_quad(&mut verts, underline, bg_uv, [0.0; 4], accent);
        }

        // Separator between input and hint.
        let sep_y = popup_y0 + line_h;
        push_quad(
            &mut verts,
            [popup_x0 + 2.0, sep_y, popup_x1 - 2.0, sep_y + 1.0],
            bg_uv,
            [0.0; 4],
            sep,
        );

        // Hint footer: "Enter to save · Esc to cancel"
        let hint_y = sep_y + ch * 0.15;
        let hint = "\u{23ce} save  \u{238b} cancel";
        self.push_text(
            &mut verts,
            text_x,
            hint_y,
            hint,
            prompt_c,
            (text_w / cw * 0.9) as usize,
        );

        verts
    }

    /// Build the Tab-completion dropdown as a floating popup above the prompt
    /// z-order (Completion) and hit-test regions.
    ///
    /// F4: shell + row backgrounds now use the shared `command_surface`
    /// builders so Completion, Palette, Find and ContextMenu share the same
    /// visual language (8% lifted bg, unified border, resize handles, and
    /// selection/hover/disabled row states).
    pub(crate) fn build_completion_vertices(
        &self,
        matches: &[weft_core::complete::Match],
        selected: usize,
        layout: crate::layout::CompletionLayout,
    ) -> Vec<f32> {
        use crate::paint::command_surface::{
            build_command_surface_row_bg, build_command_surface_shell, completion_surface_state,
            CommandSurfaceRowState, CommandSurfaceShell,
        };

        let mut verts = Vec::new();
        let ch = self.cell_height() as f32;
        let theme_bg = color_to_normalized(self.theme.background);
        let fg = color_to_normalized(self.theme.foreground);
        let accent = color_to_normalized(self.theme.accent);
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];

        let label_color = crate::paint::color_math::mix_fg_over_bg(fg, theme_bg, 0.15);
        let sel_label_color = fg;
        let suffix_color = crate::paint::color_math::mix_fg_over_bg(fg, theme_bg, 0.50);

        let [popup_x0, popup_top, popup_x1, popup_bottom] = layout.popup_rect;
        let icon_x = layout.icon_x;
        let label_x = layout.label_x;
        let suffix_x = layout.suffix_x;
        let label_cols = layout.label_cols;
        let suffix_cols = layout.suffix_cols;

        // F4: shared shell (bg + border + resize handles, no drop shadow).
        build_command_surface_shell(
            &mut verts,
            CommandSurfaceShell::canonical(layout.popup_rect, 0.0, true, theme_bg, bg_uv),
        );

        // F4: formal state. When there are no matches, show the Empty status
        // text instead of returning a bare popup (so the user sees why the
        // popup appeared).
        let state = completion_surface_state(matches.len());
        if !state.shows_results() {
            let status = state.status_text();
            if !status.is_empty() {
                let status_color = [
                    fg[0] * 0.50 + theme_bg[0] * 0.50,
                    fg[1] * 0.50 + theme_bg[1] * 0.50,
                    fg[2] * 0.50 + theme_bg[2] * 0.50,
                    1.0,
                ];
                let cw = self.cell_width() as f32;
                let cols = (((popup_x1 - popup_x0) / cw).max(1.0)) as usize;
                self.push_text(
                    &mut verts,
                    popup_x0 + cw,
                    popup_bottom - ch,
                    &status,
                    status_color,
                    cols,
                );
            }
            return verts;
        }

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
    use crate::paint::command_surface::CommandSurfaceState;

    #[test]
    fn find_draw_state_exposes_real_worker_loading_and_error_priority() {
        let mut state = FindDrawState {
            query: "needle".into(),
            worker_busy: true,
            ..Default::default()
        };
        assert_eq!(state.surface_state(), CommandSurfaceState::Loading);
        state.regex_error = Some("invalid pattern".into());
        assert_eq!(
            state.surface_state(),
            CommandSurfaceState::Error("invalid pattern".into())
        );
    }

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
