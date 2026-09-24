//! Prompt and block-view terminal geometry.

use super::{LayoutCtx, Rect};

/// Horizontal breathing room for terminal-owned chrome. This stays relative
/// to the active font so BlockView and the editor keep the same rhythm at
/// every font size without depending on a user's window-padding preference.
fn terminal_content_gutter(ctx: &LayoutCtx) -> f32 {
    ctx.cell_w * 1.5
}

fn inset_left(ctx: &LayoutCtx) -> f32 {
    let available_gutter = (ctx.right() - ctx.left() - ctx.cell_w).max(0.0);
    ctx.left() + terminal_content_gutter(ctx).min(available_gutter)
}

pub fn prompt_content_cols(ctx: &LayoutCtx) -> usize {
    let left = inset_left(ctx);
    (((ctx.right() - left) / ctx.cell_w).max(1.0)) as usize
}

pub fn block_content_x_bounds(ctx: &LayoutCtx) -> (f32, f32) {
    let frame_left = ctx.left();
    let frame_right = ctx.right();
    let available_gutter = ((frame_right - frame_left - ctx.cell_w).max(0.0) * 0.5).max(0.0);
    let gutter = terminal_content_gutter(ctx).min(available_gutter);
    (frame_left + gutter, frame_right - gutter)
}

/// Effective terminal content columns for a pane of width `pane_width`
/// with the given cell width. Accounts for the horizontal gutter that
/// BlockView reserves on each side, so PTY-reported cols match what
/// BlockView can actually display — preventing progress bars from wrapping
/// at the last few columns.
pub fn terminal_content_cols(pane_width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || pane_width <= 0.0 {
        return 1;
    }
    let available_gutter = ((pane_width - cell_w).max(0.0) * 0.5).max(0.0);
    let gutter = (cell_w * 1.5).min(available_gutter);
    let content_width = (pane_width - 2.0 * gutter).max(cell_w);
    ((content_width / cell_w).max(1.0)) as usize
}

/// v1.10.4: Full pane-width column count for alt-screen TUIs (vim/opencode/
/// htop/less). Unlike [`terminal_content_cols`], this does NOT subtract the
/// BlockView gutter — TUIs assume PTY cols = the visible terminal width and
/// need every column to paint borders/layouts edge-to-edge.
pub fn terminal_full_cols(pane_width: f32, cell_w: f32) -> usize {
    if cell_w <= 0.0 || pane_width <= 0.0 {
        return 1;
    }
    ((pane_width / cell_w).max(1.0)) as usize
}

// ── Prompt (editor input box) ─────────────────────────────────────────
//
// The input box sits at the bottom of the viewport. Its height grows with
// the number of editor lines (1 pad row + N text rows + 1 pad row). The
// prompt marker "> " occupies 2 columns on line 0 only — subsequent lines
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
    /// X of the inset text edge (also where line 1+ starts).
    pub left: f32,
    /// Box width in character columns.
    pub box_cols: usize,
    /// X where line 0's text starts (after "> ", = `left + 2*cw`).
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
    let vp_h = ctx.viewport.1;

    let n_lines = n_lines.max(1);
    // F2 P0-1: clamp the box to 30% of the viewport so multi-line input
    // scrolls internally instead of squeezing history content. The raw
    // height is 1 pad row + N text rows + 1 pad row.
    let raw_box_h = ch * (n_lines as f32 + 2.0);
    let max_box_h = vp_h * 0.30;
    // v1.3: in multi-pane tabs the prompt belongs to the active pane, so
    // the box must not exceed the pane's clipped height either.
    let box_h = raw_box_h.min(max_box_h).min(ctx.height());
    // Visible text rows that fit in the clamped box (≥1).
    let visible_rows = (((box_h / ch).floor() - 2.0).max(1.0) as usize)
        .max(1)
        .min(n_lines);

    // v1.3: pin the prompt to the active pane's clipped bounds so it does
    // not bleed across background panes in a split.
    let box_y1 = ctx.bottom();
    let box_y0 = (box_y1 - box_h).max(ctx.top());
    let box_x0 = ctx.left();
    let box_x1 = ctx.right();

    let text_y0 = box_y0 + ch;
    let left = inset_left(ctx);
    let box_cols = prompt_content_cols(ctx);

    let prompt_chars = 2usize; // "> "
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

/// Visible BlockView rows, optionally reserving a prompt whose height is capped
/// by [`layout_prompt`]. This is shared by drawing and scroll clamping.
pub fn block_visible_rows(
    ctx: &LayoutCtx,
    prompt_lines: Option<usize>,
    cwd_header_active: bool,
) -> usize {
    if ctx.cell_h <= 0.0 || ctx.height() <= 0.0 {
        return 1;
    }
    let region_bottom_y = prompt_lines
        .map(|lines| layout_prompt(ctx, lines, 0, 0, 0).box_rect[1])
        .unwrap_or_else(|| ctx.bottom());
    let block = layout_block_view(ctx, region_bottom_y, cwd_header_active);
    let height = (block.clip_bottom - ctx.top()).max(0.0);
    ((height / block.pitch).floor() as usize).max(1)
}

/// The fixed CWD band belongs to the editor. While a command runs, its CWD is
/// rendered as part of the in-flight block header instead of at window bottom.
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

/// v1.10.26 Batch D (D-3): structural chrome rows a <b>settled split head</b>
/// adds to the block view above the live tail. When a long primary-screen
/// TUI snapshot crosses `DEFAULT_OUTPUT_CAP`, `split_screen_history`
/// (vt/screen_exit/freeze.rs) settles 1MiB chunks as finished blocks; each
/// new block renders a Command row + Header band + Separator (see
/// `paint/block_view/layout_pass.rs`), i.e. this many rows inserted between
/// the older content and the live tail. A detached `block_scroll_anchor`
/// must advance by `heads × this` to keep the user's viewport stationary
/// (`tab/scroll.rs split_head_anchor_compensation`). This is a FLOOR, not
/// an exact count: a wrapped pending command renders
/// `command_line_chunks(...).len()` Command rows, under-compensating by
/// (chunks - 1) per such head — accepted for the rare >1MiB split. FollowBottom
/// is unaffected — the live tail keeps the view pinned to the bottom.
pub const BLOCK_SPLIT_HEAD_CHROME_ROWS: usize = 3;

/// Layout for the block view's outer frame. Mirrors the top-level geometry
/// computed inline by `MetalRenderer::build_block_view_vertices`
/// (renderer.rs:2126).
#[derive(Clone, Copy, Debug)]
pub struct BlockViewLayout {
    /// Outer left edge used by block surfaces, rails, and separators.
    pub frame_left: f32,
    /// Outer right edge used by block surfaces, rails, and separators.
    pub frame_right: f32,
    /// Row pitch in physical pixels (= ch), shared with the live Grid view.
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
/// whenever a CWD line will be rendered.
pub fn layout_block_view(
    ctx: &LayoutCtx,
    region_bottom_y: f32,
    cwd_header_active: bool,
) -> BlockViewLayout {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;

    // A primary-screen TUI switches from Grid to BlockView when history is
    // opened. Sharing the exact row pitch keeps the same transcript from
    // changing density and apparent glyph weight during that transition.
    let pitch = ch;
    let frame_left = ctx.left();
    // v1.3 multi-pane: honor the pane's clip rect / pane_origin via ctx.right()
    // instead of the full-viewport \`vp_w - padding_x\` (which would extend across
    // background panes). ctx.right() clamps to clip.x1 and adds pane_origin.0.
    let frame_right = ctx.right();
    let (left, right) = block_content_x_bounds(ctx);
    let cols = (((right - left) / cw).max(1.0)) as usize;

    // v0.9 H1: clip_top must include chrome_top (tab bar height) so the
    // sticky header and scrollable content start below the tab bar. ctx.top()
    // = padding_y + chrome_top; when there's no tab bar, chrome_top is 0 and
    // this reduces to the old `ctx.padding_y`.
    let clip_top = ctx.top();
    let (content_bottom_y, fixed_cwd_y) = if cwd_header_active {
        // CWD line + divider pinned to the bottom; scrollable
        // content sits ABOVE the CWD line (2 pitches up: one for CWD text,
        // one for the separator that the renderer draws above it).
        (region_bottom_y - 2.0 * pitch, region_bottom_y - pitch)
    } else {
        // No CWD: scrollable content goes
        // all the way to region_bottom_y.
        (region_bottom_y, 0.0)
    };

    BlockViewLayout {
        pitch,
        frame_left,
        frame_right,
        left,
        right,
        cols,
        clip_top,
        clip_bottom: content_bottom_y,
        fixed_cwd_y,
    }
}
