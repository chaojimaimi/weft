//! Context menu, Find and Completion vertex builders.

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::renderer::MetalRenderer;

// Find/note overlay vertex builders live in child modules (v1.13.9
// file-budget split, pure code motion; `impl MetalRenderer` cross-file
// blocks per the vt/modes.rs precedent).
mod find;
mod note;

/// Per-frame FindInGrid draw state (v0.8 B3). Set by the app before `draw()`.
#[derive(Clone, Debug, Default)]
pub struct FindDrawState {
    /// Live query string (rendered in the bar input).
    pub query: String,
    /// v1.12.26 (P1-02): active IME composition, rendered inline after the
    /// query tail (accent + underline, palette precedent). The find bar's
    /// caret always sits at the end, so no separate cursor field is needed —
    /// the caret slides right by the preedit extent instead.
    pub ime_preedit: String,
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
        // v1.13.17 S2: mouse-hovered row (draw-param mirror; `None` = no hover).
        hovered: Option<usize>,
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
            // v1.13.17 S2: hovered row feedback — same row-state token as the
            // palette; selection wins when both land on the same row.
            build_command_surface_row_bg(
                &mut verts,
                [popup_x0, y, popup_x1, y + ch],
                CommandSurfaceRowState {
                    selected: is_sel,
                    hovered: hovered == Some(i),
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
