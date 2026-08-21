//! Completion, palette, context-menu, and find geometry.

use super::{LayoutCtx, Rect, Spacing};

// ── Overlay layouts (v0.8 stage 4 — U2) ────────────────────────────────
//
// Pure coordinate functions: given a `LayoutCtx` + the immutable inputs an
// overlay needs, compute the rectangles and column anchors the renderer
// should draw into. No `MetalRenderer`, no `push_quad`, no theme lookup —
// just math. The unit tests below assert coordinates without touching the
// GPU. Each `*Layout` is consumed by the matching `build_*_vertices` in
// `renderer.rs`; that function keeps responsibility for vertex building,
// theming, and text rasterization.

/// Layout for the completion popup. Mirrors the geometry computed inline by
/// `MetalRenderer::build_completion_vertices` (renderer.rs:1606). The
/// renderer reads these fields instead of recomputing them.
#[derive(Clone, Copy, Debug)]
pub struct CompletionLayout {
    /// Popup rectangle `[x0, y0, x1, y1]` in physical pixels.
    pub popup_rect: Rect,
    /// Left edge of the icon column (icons drawn from here).
    pub icon_x: f32,
    /// Left edge of the label column (text drawn from here).
    pub label_x: f32,
    /// Left edge of the suffix column ("File"/"Directory"/…).
    pub suffix_x: f32,
    /// Visible label width in character columns (after popup width clamp).
    pub label_cols: usize,
    /// Fixed suffix column width (10 cols, "Directory" is the longest).
    pub suffix_cols: usize,
    /// Inclusive start index into `matches` of the first visible row.
    pub start: usize,
    /// Exclusive end index into `matches` of the last visible row.
    pub end: usize,
}

/// Compute the visible-row window for a completion popup.
///
/// `anchor_y` is the popup's bottom edge in physical pixels (the prompt
/// input box's top edge). The popup grows upward, so the number of rows
/// that fit is limited by how much vertical space is above the anchor.
///
/// Returns `(start, end, shown)`. `start` is clamped so the `selected`
/// row stays visible: it scrolls only when `selected` falls outside the
/// window, then snaps to keep `selected` at the bottom row.
pub fn completion_window(
    anchor_y: f32,
    cell_h: f32,
    popup_max_rows: usize,
    selected: usize,
    items_count: usize,
) -> (usize, usize, usize) {
    // Empty input → empty window. Caller (`build_completion_vertices`)
    // already early-returns when `matches.is_empty()`, but be defensive.
    if items_count == 0 {
        return (0, 0, 0);
    }
    let avail_rows = ((anchor_y / cell_h).ceil() as usize).saturating_sub(1);
    let max_rows = popup_max_rows.min(avail_rows.max(1));
    let start = selected.saturating_sub(max_rows - 1);
    let end = (start + max_rows).min(items_count);
    (start, end, end - start)
}

/// Compute the full completion layout given a precomputed visible-window
/// slice and the widest visible label width.
///
/// The split mirrors the original inline logic: the renderer first computes
/// `(start, end, shown)` via [`completion_window`], then walks
/// `matches[start..end]` to find `max_label_cols`, then calls this function
/// to derive the popup rectangle and column anchors.
#[allow(clippy::too_many_arguments)]
pub fn layout_completion(
    ctx: &LayoutCtx,
    start: usize,
    end: usize,
    max_label_cols: usize,
    anchor_y: f32,
    box_x0: f32,
    popup_width_scale: f32,
) -> CompletionLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let padding_x = ctx.padding_x;

    let shown = end - start;
    // Floor at 10 so an empty (or short) visible slice still has a sane
    // label column — matches the original `.unwrap_or(10)` default.
    let max_label_cols = max_label_cols.max(10);
    let suffix_cols = 10usize;
    let gap_cols = 2usize;
    // popup_cols = left_pad(1) + icon(2) + label + gap(2) + suffix(10) + right_pad(1)
    let popup_cols = 1 + 2 + max_label_cols + gap_cols + suffix_cols + 1;
    let popup_max_cols = ((vp_w * popup_width_scale) / cw) as usize;
    let popup_cols = popup_cols.clamp(25, popup_max_cols.max(25));
    let popup_w = popup_cols as f32 * cw;
    let popup_x0 = box_x0;
    let popup_x1 = (popup_x0 + popup_w).min(vp_w - padding_x);

    let pad = Spacing::sm(ctx); // cw * 0.5
    let icon_w = 2.0 * cw;
    let label_x = popup_x0 + pad + icon_w;
    let label_cols = popup_cols
        .saturating_sub(1 + 2 + gap_cols + suffix_cols + 1)
        .max(5);

    let top_pad = Spacing::row_sm(ctx); // ch * 0.5
    let popup_h = shown as f32 * ch + top_pad;
    let popup_top = anchor_y - popup_h;

    let suffix_x = label_x + (max_label_cols.min(label_cols) + gap_cols) as f32 * cw;
    let icon_x = popup_x0 + pad;

    CompletionLayout {
        popup_rect: [popup_x0, popup_top, popup_x1, anchor_y],
        icon_x,
        label_x,
        suffix_x,
        label_cols,
        suffix_cols,
        start,
        end,
    }
}

// ── Command Palette ───────────────────────────────────────────────────
//
// The palette has two modes:
//   • search mode — query box at top, results list below
//   • form mode   — workflow variable-fill form (no results list)
// Both share the same horizontally-centered popup rect and the same
// vertical anchor (`vp_h * 0.15` from the top).

/// Layout for the palette's search mode. Mirrors the geometry computed
/// inline by `MetalRenderer::build_palette_vertices` (renderer.rs:1759)
/// when no workflow form is active.
#[derive(Clone, Copy, Debug)]
pub struct PaletteSearchLayout {
    /// Outer popup rect `[x0, y0, x1, y1]`.
    pub popup_rect: Rect,
    /// Y of the query/banner row (text baseline region).
    pub query_y: f32,
    /// Y of the separator below the query row.
    pub sep_y: f32,
    /// Y of the first results row (top edge).
    pub results_y: f32,
    /// X of the query/banner text left edge.
    pub query_x: f32,
    /// X of the result label column.
    pub label_x: f32,
    /// X of the right-aligned kind suffix column.
    pub suffix_x: f32,
    /// Visible results window (inclusive start, exclusive end). Visible row
    /// count = `end - start`.
    pub start: usize,
    pub end: usize,
}

/// Compute the centered popup X range for the palette (shared by search
/// and form modes). Returns `(x0, x1)`; the Y range depends on mode.
/// `popup_width_scale` is typically 0.6.
pub fn palette_popup_x_range(ctx: &LayoutCtx, popup_width_scale: f32) -> (f32, f32) {
    let vp_w = ctx.viewport.0;
    let popup_w = vp_w * popup_width_scale;
    let popup_x0 = (vp_w - popup_w) / 2.0;
    (popup_x0, popup_x0 + popup_w)
}

/// Compute the search-mode layout. `entries_len` is the total result count;
/// `selection` is the currently highlighted row; `popup_max_rows` clamps
/// the visible window (matches `MetalRenderer.popup_max_rows`).
#[allow(clippy::too_many_arguments)]
pub fn layout_palette_search(
    ctx: &LayoutCtx,
    entries_len: usize,
    selection: usize,
    popup_max_rows: usize,
    popup_width_scale: f32,
) -> PaletteSearchLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_h = ctx.viewport.1;

    let (popup_x0, popup_x1) = palette_popup_x_range(ctx, popup_width_scale);
    let max_results = popup_max_rows.min(entries_len.max(1));
    let shown = max_results.min(entries_len);
    // +2 rows for header (query + separator) + 0.5ch top padding.
    let popup_h = (shown as f32 + 2.0) * ch + ch * 0.5;
    let popup_top = vp_h * 0.15;
    let popup_bottom = popup_top + popup_h;

    let query_y = popup_top + ch * 0.5;
    let sep_y = query_y + ch;
    let results_y = sep_y + ch;
    let query_x = popup_x0 + cw * 0.5;
    let label_x = popup_x0 + cw * 0.5;
    // Right-aligned suffix: 10 cols wide, 0.5cw margin from the right edge.
    let suffix_x = popup_x1 - cw * 0.5 - 10.0 * cw;

    let start = selection.saturating_sub(max_results.saturating_sub(1));
    let end = (start + max_results).min(entries_len);

    PaletteSearchLayout {
        popup_rect: [popup_x0, popup_top, popup_x1, popup_bottom],
        query_y,
        sep_y,
        results_y,
        query_x,
        label_x,
        suffix_x,
        start,
        end,
    }
}

/// Compute the form-mode popup rect for the palette's workflow
/// variable-fill sub-mode. `n_fields` is the number of form fields
/// (excludes the 3 chrome rows: title, separator, submit hint).
pub fn layout_palette_form_rect(ctx: &LayoutCtx, n_fields: usize, popup_width_scale: f32) -> Rect {
    let ch = ctx.cell_h;
    let vp_h = ctx.viewport.1;
    let (popup_x0, popup_x1) = palette_popup_x_range(ctx, popup_width_scale);
    let popup_top = vp_h * 0.15;
    let popup_h = (n_fields as f32 + 3.0) * ch + ch * 0.5;
    [popup_x0, popup_top, popup_x1, popup_top + popup_h]
}

// ── Context menu ───────────────────────────────────────────────────────
//
// Right-click menu on a block. v1.7.3-C expanded from 4 to 7 items
// (Copy Command / Copy Output / Toggle Fold / Send to Input / Toggle
// Bookmark / Add Note / Export Block). The menu is anchored at the click
// point and clamped to the viewport's right edge.

/// v1.7.3-C: Number of items in the block context menu. Update this and
/// `CONTEXT_MENU_ITEMS` in `main.rs` together.
/// v1.8.2: Added "Diagnose with AI" as the 8th item.
/// v1.10.34: Added "Copy Block" (cwd+command+output) as the 3rd item.
pub const CONTEXT_MENU_ITEM_COUNT: usize = 9;

/// Layout for the block context menu.
#[derive(Clone, Copy, Debug)]
pub struct ContextMenuLayout {
    /// Outer menu rect `[x0, y0, x1, y1]`.
    pub menu_rect: Rect,
    /// Per-item Y (top edge). Length = [`CONTEXT_MENU_ITEM_COUNT`].
    pub item_y: [f32; CONTEXT_MENU_ITEM_COUNT],
    /// Exact hit-test rectangles for each action.
    pub item_rects: [Rect; CONTEXT_MENU_ITEM_COUNT],
    /// X of the item label text.
    pub text_x: f32,
    /// Y of the separator after item `i` (no separator after the last item).
    pub separator_ys: [f32; CONTEXT_MENU_ITEM_COUNT - 1],
}

/// Compute the context menu layout. `(x, y)` is the click anchor. The menu
/// is left-clamped so its right edge stays inside the viewport.
pub fn layout_context_menu(ctx: &LayoutCtx, x: f32, y: f32, scale: f32) -> ContextMenuLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let vp_h = ctx.viewport.1;

    let item_h = ch * 1.2;
    let menu_w = 180.0 * scale;
    let menu_h = CONTEXT_MENU_ITEM_COUNT as f32 * item_h + ch * 0.4;

    // Clamp so the right edge stays inside the viewport (with 4px gutter).
    let menu_x0 = x.min(vp_w - menu_w - 4.0).max(0.0);
    // v0.9 fix: if the menu would extend below the viewport, flip it upward
    // so it opens above the click point instead of being clipped.
    let menu_y0 = if y + menu_h > vp_h - 4.0 {
        (y - menu_h).max(4.0)
    } else {
        y
    };
    let menu_x1 = menu_x0 + menu_w;
    let menu_y1 = menu_y0 + menu_h;

    let top_pad = ch * 0.2;
    let mut item_y = [0.0f32; CONTEXT_MENU_ITEM_COUNT];
    let mut separator_ys = [0.0f32; CONTEXT_MENU_ITEM_COUNT - 1];
    for i in 0..CONTEXT_MENU_ITEM_COUNT {
        item_y[i] = menu_y0 + top_pad + i as f32 * item_h;
        if i < CONTEXT_MENU_ITEM_COUNT - 1 {
            separator_ys[i] = item_y[i] + item_h;
        }
    }
    let text_x = menu_x0 + cw * 0.4;
    let item_rects = item_y.map(|item_top| [menu_x0, item_top, menu_x1, item_top + item_h]);

    ContextMenuLayout {
        menu_rect: [menu_x0, menu_y0, menu_x1, menu_y1],
        item_y,
        item_rects,
        text_x,
        separator_ys,
    }
}

#[cfg(test)]
impl ContextMenuLayout {
    pub fn item_at(self, x: f32, y: f32) -> Option<usize> {
        self.item_rects.iter().position(|rect| {
            let [x0, y0, x1, y1] = *rect;
            x >= x0 && x < x1 && y >= y0 && y < y1
        })
    }
}

// ── Find utility bar ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindLayout {
    pub popup_rect: Rect,
    pub line_y: f32,
    pub text_x0: f32,
    pub text_x1: f32,
    pub regex_x: f32,
    pub case_x: f32,
    pub down_x: f32,
    pub up_x: f32,
    pub regex_rect: Rect,
    pub case_rect: Rect,
    pub down_rect: Option<Rect>,
    pub up_rect: Option<Rect>,
}

pub fn layout_find(ctx: &LayoutCtx, total_matches: usize) -> FindLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let target_w = 500.0_f32;
    let right_margin = 20.0;
    let min_w = cw * 48.0;
    let popup_w = target_w.min(ctx.width() - right_margin - 20.0).max(min_w);
    let popup_h = (ch * 1.75).max(ch + 16.0);
    let popup_x1 = ctx.right() - right_margin;
    let popup_x0 = popup_x1 - popup_w;
    let popup_y0 = ctx.top() + 10.0;
    let popup_y1 = popup_y0 + popup_h;

    let border_w = 1.0;
    let stripe_w = 3.0;
    let inner_pad_x = 8.0;
    let text_x0 = popup_x0 + border_w + stripe_w + inner_pad_x;
    let text_x1 = popup_x1 - border_w - inner_pad_x;
    let line_y = popup_y0 + border_w + ((popup_h - border_w * 2.0 - ch) * 0.5).max(0.0);
    let click_pad = 2.0;
    let gap_w = cw;

    let text_width = |text: &str| weft_core::grid::terminal_text_width(text) as f32 * cw;
    let regex_w = text_width(".*");
    let case_w = text_width("Aa");
    let down_w = text_width("↓");
    let up_w = text_width("↑");
    let regex_x = text_x1 - regex_w;
    let case_x = regex_x - gap_w - case_w;
    let down_x = case_x - gap_w - down_w;
    let up_x = down_x - gap_w - up_w;
    let button_rect = |x: f32, width: f32| {
        [
            x - click_pad,
            line_y - click_pad,
            x + width + click_pad,
            line_y + ch + click_pad,
        ]
    };
    FindLayout {
        popup_rect: [popup_x0, popup_y0, popup_x1, popup_y1],
        line_y,
        text_x0,
        text_x1,
        regex_x,
        case_x,
        down_x,
        up_x,
        regex_rect: button_rect(regex_x, regex_w),
        case_rect: button_rect(case_x, case_w),
        down_rect: (total_matches > 0).then(|| button_rect(down_x, down_w)),
        up_rect: (total_matches > 0).then(|| button_rect(up_x, up_w)),
    }
}
