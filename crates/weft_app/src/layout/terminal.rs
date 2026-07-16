//! Prompt and block-view terminal geometry.

use super::{LayoutCtx, Rect};

// ── Prompt (editor input box) ─────────────────────────────────────────
//
// The input box sits at the bottom of the viewport. Its height grows with
// the number of editor lines (1 pad row + N text rows + 1 pad row). The
// prompt glyph "❯ " occupies 2 columns on line 0 only — subsequent lines
// start at `left`. The caret X depends on the cumulative display width of
// the chars before the cursor (CJK chars are 2 cols wide), so the renderer
// computes `cursor_offset_cols` and passes it in.

/// Layout for the prompt input box. Mirrors the geometry computed inline
/// by `MetalRenderer::build_prompt_vertices` (renderer.rs:1370).
#[derive(Clone, Copy, Debug)]
pub struct PromptLayout {
    /// Outer box rect `[x0, y0, x1, y1]`.
    pub box_rect: Rect,
    /// Y of the first text row (one pad row below `box_y0`).
    pub text_y0: f32,
    /// X of the box's left edge (also where line 1+ starts).
    pub left: f32,
    /// Box width in character columns.
    pub box_cols: usize,
    /// X where line 0's text starts (after "❯ ", = `left + 2*cw`).
    pub first_line_text_x: f32,
    /// Caret X (top-left of the bar).
    pub cursor_x: f32,
    /// Caret Y (top edge).
    pub cursor_y: f32,
    /// Caret bar width in physical pixels.
    pub bar_w: f32,
    /// F2 P0-1: number of text rows visible in the clamped box. The renderer
    /// only draws lines `[scroll_offset, scroll_offset + visible_rows)`.
    pub visible_rows: usize,
    /// Physical height of one editor text row.
    pub row_height: f32,
}

/// Compute the prompt layout. `n_lines` is the editor buffer's line count
/// (clamped to ≥1). `cursor_line`/`cursor_offset_cols` locate the caret;
/// `cursor_offset_cols` is the cumulative display-column width of the
/// chars before the cursor on that line (CJK = 2 cols), pre-computed by
/// the renderer via `char_col_width`. `scroll_offset` is the editor
/// buffer's internal scroll offset (F2 P0-1) — cursor Y is adjusted so
/// the caret lands in the visible window.
pub fn layout_prompt(
    ctx: &LayoutCtx,
    n_lines: usize,
    cursor_line: usize,
    cursor_offset_cols: usize,
    scroll_offset: usize,
) -> PromptLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let vp_h = ctx.viewport.1;

    let n_lines = n_lines.max(1);
    // F2 P0-1: clamp the box to 30% of the viewport so multi-line input
    // scrolls internally instead of squeezing history content. The raw
    // height is 1 pad row + N text rows + 1 pad row.
    let raw_box_h = ch * (n_lines as f32 + 2.0);
    let max_box_h = vp_h * 0.30;
    let box_h = raw_box_h.min(max_box_h);
    // Visible text rows that fit in the clamped box (≥1).
    let visible_rows = (((box_h / ch).floor() - 2.0).max(1.0) as usize)
        .max(1)
        .min(n_lines);

    let box_y1 = (vp_h - ctx.padding_y).max(0.0);
    let box_y0 = (box_y1 - box_h).max(0.0);
    let box_x0 = ctx.left();
    let box_x1 = (vp_w - ctx.padding_x).max(box_x0);

    let text_y0 = box_y0 + ch;
    let left = box_x0;
    let box_cols = (((box_x1 - left) / cw).max(1.0)) as usize;

    let prompt_chars = 2usize; // "❯ "
    let first_line_text_x = left + prompt_chars as f32 * cw;

    // F2 P0-1: cursor Y accounts for the scroll offset so the caret lands
    // inside the visible window.
    let cursor_row_in_window = cursor_line.saturating_sub(scroll_offset) as f32;
    let cy = text_y0 + cursor_row_in_window * ch;
    let text_start_x = if cursor_line == 0 {
        first_line_text_x
    } else {
        left
    };
    let cx = text_start_x + cursor_offset_cols as f32 * cw;
    let bar_w = (cw * 0.12).max(2.0);

    PromptLayout {
        box_rect: [box_x0, box_y0, box_x1, box_y1],
        text_y0,
        left,
        box_cols,
        first_line_text_x,
        cursor_x: cx,
        cursor_y: cy,
        bar_w,
        visible_rows,
        row_height: ch,
    }
}

/// Map a physical Y coordinate to a document line in the currently visible
/// prompt window. Padding/hint-row clicks clamp to the nearest visible line.
pub fn prompt_line_at_y(
    layout: &PromptLayout,
    y: f32,
    scroll_offset: usize,
    n_lines: usize,
) -> usize {
    let n_lines = n_lines.max(1);
    let start = scroll_offset.min(n_lines - 1);
    let visible = layout.visible_rows.min(n_lines - start).max(1);
    let row =
        if layout.row_height.is_finite() && layout.row_height > 0.0 && layout.text_y0.is_finite() {
            ((y - layout.text_y0) / layout.row_height).floor() as isize
        } else {
            0
        };
    start + row.clamp(0, visible as isize - 1) as usize
}

/// Visible block rows above a prompt whose height is capped by
/// [`layout_prompt`]. This is shared by drawing and scroll clamping.
pub fn block_visible_rows(ctx: &LayoutCtx, prompt_lines: usize, cwd_header_active: bool) -> usize {
    if ctx.cell_h <= 0.0 || ctx.height() <= 0.0 {
        return 1;
    }
    let prompt = layout_prompt(ctx, prompt_lines, 0, 0, 0);
    let block = layout_block_view(ctx, prompt.box_rect[1], cwd_header_active);
    let height = (block.clip_bottom - ctx.top()).max(0.0);
    ((height / block.pitch).floor() as usize).max(1)
}

/// The fixed CWD band exists only while the owned editor prompt is visible.
/// A running command retains Terminal.cwd but renders a live block instead.
pub fn block_cwd_header_active(editor_mode: bool, cwd_present: bool) -> bool {
    editor_mode && cwd_present
}

// ── Block view (Warp-style history) ────────────────────────────────────
//
// Top-level geometry for the scrollable block-history region. Per-row Y
// coordinates are data-driven (each row's distance accumulates from the
// cumulative output line count of all blocks below it), so the renderer
// walks the block list and tracks `cursor_dist` itself. What we extract
// here is the *frame* geometry: pitch, left/right/cols, the fixed CWD
// line position (Editor mode), and the clip region.

/// Layout for the block view's outer frame. Mirrors the top-level geometry
/// computed inline by `MetalRenderer::build_block_view_vertices`
/// (renderer.rs:2126).
#[derive(Clone, Copy, Debug)]
pub struct BlockViewLayout {
    /// Row pitch in physical pixels (= ch * 1.1).
    pub pitch: f32,
    /// Left edge of the content area (= padding_x).
    pub left: f32,
    /// Right edge of the content area (= vp_w - padding_x).
    pub right: f32,
    /// Content width in character columns.
    pub cols: usize,
    /// Top of the clip region (viewport top padding).
    pub clip_top: f32,
    /// Bottom of the clip region (= content_bottom_y). Rows below this
    /// are clipped.
    pub clip_bottom: f32,
    /// Y of the fixed CWD header row. Only meaningful when
    /// `cwd_header_active == true` was passed to [`layout_block_view`].
    pub fixed_cwd_y: f32,
}

/// Compute the block view's frame layout. `region_bottom_y` is the bottom
/// edge of the block region (top of the input box in Editor mode, or the
/// screen bottom in CommandExecuting). `cwd_header_active` should be true
/// when a CWD line will be rendered (Editor mode + no live block).
pub fn layout_block_view(
    ctx: &LayoutCtx,
    region_bottom_y: f32,
    cwd_header_active: bool,
) -> BlockViewLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;

    let pitch = ch * 1.1;
    let left = ctx.left();
    let right = vp_w - ctx.padding_x;
    let cols = (((right - left) / cw).max(1.0)) as usize;

    // v0.9 H1: clip_top must include chrome_top (tab bar height) so the
    // sticky header and scrollable content start below the tab bar. ctx.top()
    // = padding_y + chrome_top; when there's no tab bar, chrome_top is 0 and
    // this reduces to the old `ctx.padding_y`.
    let clip_top = ctx.top();
    let (content_bottom_y, fixed_cwd_y) = if cwd_header_active {
        // Editor mode: CWD line + divider pinned to the bottom; scrollable
        // content sits ABOVE the CWD line (2 pitches up: one for CWD text,
        // one for the separator that the renderer draws above it).
        (region_bottom_y - 2.0 * pitch, region_bottom_y - pitch)
    } else {
        // CommandExecuting (live block) or no CWD: scrollable content goes
        // all the way to region_bottom_y.
        (region_bottom_y, 0.0)
    };

    BlockViewLayout {
        pitch,
        left,
        right,
        cols,
        clip_top,
        clip_bottom: content_bottom_y,
        fixed_cwd_y,
    }
}
