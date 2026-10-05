//! Text rasterization helpers for the Metal renderer.
//!
//! These remain `impl MetalRenderer` methods (A5 strategy b) because they
//! need `&self.atlas` (glyph UV lookup) and `&self.theme` (syntax colors).
//! Splitting them out of `renderer.rs` keeps the main file focused on GPU
//! pipeline + grid rendering; overlay builders in `paint/overlays.rs` call
//! these via the same `self.push_text(...)` syntax with zero call-site edits.

use crate::ime::ImeCursorArea;
use crate::layout::LayoutCtx;
use crate::paint::primitives::push_quad;
use crate::renderer::MetalRenderer;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use weft_core::syntax::TokenKind;

impl MetalRenderer {
    /// Column width of a character (0 for zero-width combining marks,
    /// 1 for ASCII and ambiguous terminal symbols, 2 for CJK full-width.
    pub(crate) fn char_col_width(c: char) -> usize {
        weft_core::grid::terminal_char_width(c)
    }

    /// Total column width of a string — sum of each char's display width.
    /// Use this instead of `chars().count()` whenever a width/position
    /// calculation must match what `push_text` actually renders (CJK chars
    /// occupy 2 columns each, not 1).
    pub(crate) fn text_col_width(s: &str) -> usize {
        weft_core::grid::terminal_text_width(s)
    }

    /// UV rect of the space glyph (background-only quads need mask 0).
    pub(crate) fn space_uv(&self) -> (f32, f32, f32, f32) {
        self.atlas
            .get(' ')
            .map(|g| {
                let (u, v) = g.uv_origin;
                let (uw, vh) = g.uv_size;
                (u, v, uw, vh)
            })
            .unwrap_or((0.0, 0.0, 0.0, 0.0))
    }

    /// Lay out a string left-to-right, honoring wide-character (CJK) widths.
    /// `max_cols` is a *column* budget (not a character count): a CJK char
    /// consumes 2 columns, ASCII consumes 1. Glyphs must already be in the
    /// atlas (warmed up by the caller). Text exceeding `max_cols` columns is
    /// truncated (callers that need wrapping use `push_text_wrapped`).
    pub(crate) fn push_text(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
    ) {
        self.push_text_with_height(
            vertices,
            [x, y],
            text,
            fg,
            max_cols,
            self.cell_height() as f32,
            crate::glyph::GlyphStyle::REGULAR,
        );
    }

    /// v1.10.12: `style` selects the atlas face (bold/italic) for the whole
    /// run. UI text (`push_text`) stays regular; block-view history cells
    /// pass their SGR flags-derived style.
    #[allow(clippy::too_many_arguments)] // run geometry + style; mirrors push_text's arity
    pub(crate) fn push_text_with_height(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        glyph_height: f32,
        style: crate::glyph::GlyphStyle,
    ) {
        let cw = self.cell_width() as f32;
        let [x, y] = origin;
        // v1.12.2 (PLAN_S2_render A3): snap text quad Y edges to integer
        // physical pixels (the whole coordinate chain is already physical —
        // viewport, padding, atlas rasterization). Fractional row origins
        // (pane splits, chrome offsets) otherwise sample the glyph atlas
        // with a subpixel offset → blurry text on Retina. X is deliberately
        // NOT snapped (plan scope: Y only); background/color-block quads
        // are snapped nowhere to avoid vertical seams.
        let y0 = y.floor();
        let y1 = (y + glyph_height).floor();
        let mut col = 0usize;
        let mut px = x;
        for grapheme in text.graphemes(true) {
            let w = weft_core::grid::terminal_text_width(grapheme);
            if w == 0 {
                continue;
            }
            if col + w > max_cols {
                break; // column budget exhausted
            }
            let glyph = weft_core::grid::terminal_grapheme_glyph(grapheme);
            let Some(g) = self.atlas.get_style(glyph, style).or_else(|| {
                // v1.10.12: styled face not rasterized yet — degrade to the
                // regular face instead of silently dropping the grapheme.
                (style != crate::glyph::GlyphStyle::REGULAR)
                    .then(|| {
                        self.atlas
                            .get_style(glyph, crate::glyph::GlyphStyle::REGULAR)
                    })
                    .flatten()
            }) else {
                col += w;
                px += w as f32 * cw;
                continue;
            };
            let (u, v) = g.uv_origin;
            let (uw, vh) = g.uv_size;
            let cell_w = w as f32 * cw;
            // v1.10.4: color emoji live in the RGBA color atlas — the shader
            // routes them there via the fg.a=2.0 sentinel (fg itself is
            // meaningless for color glyphs; the color comes from the atlas).
            let fg = if g.is_color { [0.0, 0.0, 0.0, 2.0] } else { fg };
            push_quad(
                vertices,
                [px, y0, px + cell_w, y1],
                [u, v + vh, u + uw, v],
                fg,
                [0.0; 4],
            );
            col += w;
            px += cell_w;
        }
    }

    /// Draw one visual row of a prompt line from pre-tokenized spans.
    ///
    /// `line` is the full logical line; `char_start`/`char_end` are the visual
    /// row's char range into it (from PromptVisualRow). Spans come from
    /// `syntax::tokenize_spans(line, true)` — a whole-logical-line
    /// tokenization, so string/command state survives visual wrapping.
    /// `selection` is relative to this visual row (as before).
    #[allow(clippy::too_many_arguments)] // run geometry + syntax state; mirrors push_text_with_height's arity
    pub(crate) fn push_line_spans_on_canvas(
        &self,
        vertices: &mut Vec<f32>,
        origin: [f32; 2],
        line: &str,
        char_start: usize,
        char_end: usize,
        spans: &[(Range<usize>, TokenKind)],
        max_cols: usize,
        canvas: [f32; 4],
        selection: Option<(usize, usize, [f32; 4])>,
    ) {
        let cw = self.cell_width() as f32;
        let [x, y] = origin;
        // v1.12.2 (PLAN_S2_render A3): same Y snap as `push_text_with_height`
        // — this span path directly emits text quads (prompt/input canvas),
        // so it is a third text-quad outlet beside the two named choke
        // points; leaving it fractional would keep prompt text blurry.
        let y0 = y.floor();
        let y1 = (y + self.cell_height() as f32).floor();
        let mut col = 0usize;
        let mut px = x;
        let mut char_index; // relative to this visual row; set per segment below
        'span_loop: for (range, kind) in spans {
            let seg_start = range.start.max(char_start);
            let seg_end = range.end.min(char_end);
            if seg_start >= seg_end {
                continue;
            }
            // Character offset of this segment within the visual row.
            char_index = seg_start - char_start;
            let seg: String = line
                .chars()
                .skip(seg_start)
                .take(seg_end - seg_start)
                .collect();
            let source_color = crate::paint::primitives::syntax_color(*kind, &self.theme);
            for grapheme in seg.graphemes(true) {
                let grapheme_end = char_index + grapheme.chars().count();
                let background = crate::paint::primitives::text_background_for_range(
                    canvas,
                    selection,
                    char_index..grapheme_end,
                );
                // Note: unlike the grid/block-view paths, this call site is
                // NOT exempted for terminal graphic glyphs (box drawing /
                // block elements). It renders user-typed, syntax-highlighted
                // prompt/command text — readable content — not TUI border
                // cells, so the minimum-contrast boost always applies here.
                let color = crate::paint::primitives::ensure_minimum_text_contrast(
                    source_color,
                    background,
                    self.minimum_contrast,
                );
                let w = weft_core::grid::terminal_text_width(grapheme);
                if w == 0 {
                    char_index = grapheme_end;
                    continue;
                }
                if col + w > max_cols {
                    break 'span_loop;
                }
                let glyph = weft_core::grid::terminal_grapheme_glyph(grapheme);
                if let Some(g) = self.atlas.get(glyph) {
                    let (u, v) = g.uv_origin;
                    let (uw, vh) = g.uv_size;
                    let cell_w = w as f32 * cw;
                    // v1.10.4: color emoji live in the RGBA color atlas —
                    // same fg.a=2.0 sentinel as `push_text_with_height`.
                    let color = if g.is_color {
                        [0.0, 0.0, 0.0, 2.0]
                    } else {
                        color
                    };
                    push_quad(
                        vertices,
                        [px, y0, px + cell_w, y1],
                        [u, v + vh, u + uw, v],
                        color,
                        [0.0; 4],
                    );
                    px += cell_w;
                }
                col += w;
                char_index = grapheme_end;
            }
        }
    }
}

// ── v1.12.24.1 (FIX_V1.12.24_NOTE_CJK): note-card layout + display window ──
//
// The note editor's buffer/caret math was char-count based since v1.7.3-C
// (N-4: caret drawn at half the CJK text extent, no trailing scroll) and the
// IME candidate window anchored at the terminal cursor (P1-3). Both are fixed
// here as pure functions; the card geometry constants are shared with the
// renderer so the anchor cannot drift from the card the user actually sees.

/// v1.12.24.1 (P1-3): note-card layout constants — single source of truth for
/// `overlays.rs::build_note_editor_vertices` (which draws the card with these)
/// and `note_editor_ime_area` below (which anchors the macOS IME candidate
/// window to it). The `note_window_tests` module pins the anchor output to the
/// same formula line-for-line (评审 P3: 机器钉住一致性).
pub(crate) const NOTE_CARD_WIDTH_FRACTION: f32 = 0.6;
/// Card width clamp, in cells.
pub(crate) const NOTE_CARD_MIN_WIDTH_CELLS: f32 = 40.0;
pub(crate) const NOTE_CARD_MAX_WIDTH_CELLS: f32 = 80.0;
/// Card top offset below the content top edge (physical pixels).
pub(crate) const NOTE_CARD_TOP_OFFSET: f32 = 10.0;
/// Accent stripe width on the card's left edge (physical pixels).
pub(crate) const NOTE_CARD_ACCENT_STRIPE_W: f32 = 3.0;
/// Input-line inner horizontal padding (physical pixels).
pub(crate) const NOTE_CARD_INNER_PAD: f32 = 8.0;
/// Input-line height: `cell_h * LINE_H_CELLS`, at least `cell_h + LINE_H_MIN_PX`.
pub(crate) const NOTE_CARD_LINE_H_CELLS: f32 = 1.75;
pub(crate) const NOTE_CARD_LINE_H_MIN_PX: f32 = 16.0;
/// Input-line label prefix (pure ASCII, so `.len()` == display cells).
pub(crate) const NOTE_CARD_LABEL: &str = "Note: ";

/// v1.12.24.1: note-card width from the shared layout consts — 60% of the
/// viewport width clamped to [40cw, 80cw]. One formula for the renderer
/// (overlays.rs) and the IME anchor so they cannot diverge.
pub(crate) fn note_card_width(vp_w: f32, cw: f32) -> f32 {
    (vp_w * NOTE_CARD_WIDTH_FRACTION)
        .max(cw * NOTE_CARD_MIN_WIDTH_CELLS)
        .min(cw * NOTE_CARD_MAX_WIDTH_CELLS)
}

/// v1.12.24.1 (N-4): cell-based display window for the note editor buffer.
/// The v1.7.3 char-count math drew the caret at half the text extent for CJK
/// (each char = 2 cells) and never scrolled, so the caret parked once the
/// card's cell budget filled. Returns the visible slice plus the caret's
/// cell offset within that slice (window follows the caret).
pub(crate) struct NoteDisplayWindow {
    /// Visible buffer slice starting at `window_start` (whole chars only).
    pub display: String,
    /// Cells from the window start to the caret (always ≤ the content budget).
    pub caret_cell: usize,
    /// Cells scrolled off the left edge (trailing scroll). Production only
    /// consumes `display` + `caret_cell`; this field stays so the window
    /// math's contract is documented and the unit tests can pin the
    /// trailing-scroll invariants (frame_trace::Hover precedent).
    #[allow(dead_code)]
    pub window_start: usize,
}

/// `buffer` is the full editor text, `cursor_byte` its caret offset,
/// `preedit_cells` the active IME composition's width (rendered at the
/// caret), `max_cells` the card's buffer display budget. Per the fix plan's
/// four invariants: the caret stays visible, the display never contains half
/// a character (a straddling char rolls out whole), zero-width marks stay
/// with their base char, and the window scrolls trailing the caret.
pub(crate) fn note_display_window(
    buffer: &str,
    cursor_byte: usize,
    preedit_cells: usize,
    max_cells: usize,
) -> NoteDisplayWindow {
    // The last column hosts the caret bar, so buffer + preedit share
    // max_cells - 1 cells of visible content (v1.7.3 reserved the same cell).
    let content_cells = max_cells.saturating_sub(1);

    // Per-char (byte range, terminal width). CJK counts 2 cells, zero-width
    // combining marks count 0. The caret's cell offset is the total width of
    // the chars entirely before cursor_byte.
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    let mut caret_cells = 0usize;
    let mut total_cells = 0usize;
    for (byte_idx, ch) in buffer.char_indices() {
        let end = byte_idx + ch.len_utf8();
        let w = weft_core::grid::terminal_char_width(ch);
        if end <= cursor_byte {
            caret_cells += w;
        }
        total_cells += w;
        spans.push((byte_idx, end, w));
    }

    // Trailing scroll: keep caret + preedit inside the budget, but never
    // scroll the caret itself past the left edge.
    let raw = (caret_cells + preedit_cells)
        .saturating_sub(content_cells)
        .min(caret_cells);

    // Snap the left edge forward to a cluster start — the first char, or any
    // non-zero-width char (zero-width marks attach to the preceding base).
    // A straddling char (or its trailing mark) therefore rolls out of the
    // window whole: no half chars, no bare combining marks.
    let mut window_cells = total_cells;
    let mut window_byte = buffer.len();
    let mut prefix = 0usize;
    for (i, &(start, _, w)) in spans.iter().enumerate() {
        if (i == 0 || w > 0) && prefix >= raw {
            window_cells = prefix;
            window_byte = start;
            break;
        }
        prefix += w;
    }

    // Visible slice: whole chars from the window start up to the budget.
    // Breaking at a base char drops its trailing marks with it.
    let mut display = String::new();
    let mut used = 0usize;
    for &(start, end, w) in spans.iter().skip_while(|&&(s, _, _)| s < window_byte) {
        if w > 0 && used + w > content_cells {
            break;
        }
        display.push_str(&buffer[start..end]);
        used += w;
    }

    NoteDisplayWindow {
        display,
        caret_cell: caret_cells.saturating_sub(window_cells),
        window_start: window_cells,
    }
}

/// v1.12.24.1 (P1-3): IME candidate-window anchor for the note card —
/// approximates the card's input area (palette precedent: "query_x is close
/// enough"), no exact caret tracking needed. Takes the `LayoutCtx` by
/// reference (it is `Copy`; the redraw call site passes `&ctx`) so tests can
/// build one via the pub `::new`. Width/height = one cell, matching the
/// palette anchor's shape. The viewport-based card math mirrors the renderer
/// deliberately — `ctx.width()` would be the content-clip width, off by
/// 2×padding + chrome (评审 P1 修正).
pub(crate) fn note_editor_ime_area(ctx: &LayoutCtx) -> ImeCursorArea {
    let cw = ctx.cell_w;
    let ch = ctx.cell_h;
    let vp_w = ctx.viewport.0;
    let popup_x0 = (vp_w - note_card_width(vp_w, cw)) * 0.5;
    let popup_y0 = ctx.top() + NOTE_CARD_TOP_OFFSET;
    let line_h = (ch * NOTE_CARD_LINE_H_CELLS).max(ch + NOTE_CARD_LINE_H_MIN_PX);
    let line_y = popup_y0 + (line_h - ch) * 0.5;
    // Input starts after the accent stripe + inner pad + "Note: " label.
    let x = popup_x0
        + NOTE_CARD_ACCENT_STRIPE_W
        + NOTE_CARD_INNER_PAD
        + NOTE_CARD_LABEL.len() as f32 * cw;
    ImeCursorArea {
        x,
        y: line_y,
        width: cw,
        height: ch,
    }
}

// ── v1.12.2 (PLAN_S2_render A3): text Y physical-pixel snapping ───────

#[cfg(test)]
mod snap_tests {
    use crate::renderer::MetalRenderer;
    use weft_core::config::Theme;

    /// Mirror the golden/offscreen skip precedent: no Metal device → skip.
    fn renderer_headless_or_skip() -> Option<MetalRenderer> {
        metal::Device::system_default()?;
        Some(MetalRenderer::new_headless_paint(Theme::weft_warm()))
    }

    /// Text quad vertices must land on integer physical pixels in Y. The
    /// full coordinate chain (viewport, padding, atlas) is already physical;
    /// a fractional row origin (pane split / chrome offset) used to sample
    /// the glyph atlas with a subpixel offset → blurry text on Retina.
    /// Naming paradigm follows pane_dividers.rs ("snaps fractional coords
    /// to integer pixels"). X is intentionally unsnapped (plan scope: Y).
    #[test]
    fn push_text_with_height_snaps_fractional_y_to_integer_pixels() {
        let Some(renderer) = renderer_headless_or_skip() else {
            eprintln!("skipping push_text_with_height snap test: no Metal device");
            return;
        };
        let mut verts = Vec::new();
        renderer.push_text_with_height(
            &mut verts,
            [0.0, 10.4],
            "A",
            [1.0; 4],
            8,
            renderer.cell_height() as f32,
            crate::glyph::GlyphStyle::REGULAR,
        );
        assert!(
            !verts.is_empty(),
            "'A' must be prewarmed into the headless atlas (no glyph → no quads)"
        );
        // Each push_quad vertex is 12 floats: x, y, u, v, fg(4), bg(4);
        // 6 vertices per quad. Vertex N's y lives at 12*N + 1.
        for n in 0..(verts.len() / 12) {
            let y = verts[n * 12 + 1];
            assert_eq!(y.fract(), 0.0, "vertex {n} y not integer: {y}");
        }
        // And the snapped value is the floor of the fractional origin.
        assert_eq!(verts[1], 10.0, "y0 must floor 10.4 → 10.0");
    }

    /// Outlet #3 (rust-reviewer M1 P2): `push_line_spans_on_canvas` is the
    /// third text-quad outlet (prompt/input canvas). With `selection: None`
    /// it emits glyph quads only, so every vertex Y must be integer.
    #[test]
    fn push_line_spans_on_canvas_snaps_fractional_y_to_integer_pixels() {
        let Some(renderer) = renderer_headless_or_skip() else {
            eprintln!("skipping push_line_spans_on_canvas snap test: no Metal device");
            return;
        };
        let mut verts = Vec::new();
        let line = "abc";
        let spans = weft_core::syntax::tokenize_spans(line, true);
        renderer.push_line_spans_on_canvas(
            &mut verts,
            [0.0, 10.4],
            line,
            0,
            3,
            &spans,
            8,
            [0.0, 0.0, 80.0, 24.0],
            None,
        );
        assert!(
            !verts.is_empty(),
            "prewarmed ascii must produce quads (no glyph → no quads)"
        );
        for n in 0..(verts.len() / 12) {
            let y = verts[n * 12 + 1];
            assert_eq!(y.fract(), 0.0, "vertex {n} y not integer: {y}");
        }
        assert_eq!(verts[1], 10.0, "y0 must floor 10.4 → 10.0");
    }
}

// ── v1.12.24.1 (FIX_V1.12.24_NOTE_CJK): pure-logic tests (plan-mandated) ──

#[cfg(test)]
mod note_window_tests {
    use super::*;
    use crate::layout::LayoutCtx;

    const CW: f32 = 17.5;
    const CH: f32 = 35.0;

    fn ime_ctx() -> LayoutCtx {
        LayoutCtx::new((1600.0, 1000.0), CW, CH, 12.0, 8.0)
    }

    /// N-4 test 1: ASCII 短文不滚动（window_start=0，caret_cell=长度）。
    #[test]
    fn ascii_short_text_does_not_scroll() {
        let win = note_display_window("hello", 5, 0, 40);
        assert_eq!(win.window_start, 0);
        assert_eq!(win.caret_cell, 5);
        assert_eq!(win.display, "hello");
    }

    /// N-4 test 2: 纯 CJK 光标在末尾 → caret_cell=8（4 字符 × 2 列）。
    #[test]
    fn cjk_caret_cell_counts_two_columns_per_char() {
        let buf = "你好世界";
        let win = note_display_window(buf, buf.len(), 0, 40);
        assert_eq!(win.window_start, 0);
        assert_eq!(win.caret_cell, 8);
        assert_eq!(win.display, "你好世界");
    }

    /// N-4 test 3: 溢出尾随——20 个 CJK（40 列）预算 9 列 → 窗口尾随滚动，
    /// display 为最后 4 个字符，caret 钉在窗口右缘（预算-1 列）。
    #[test]
    fn overflow_scrolls_trailing_and_pins_caret_at_right_edge() {
        let buf = "你".repeat(20);
        let win = note_display_window(&buf, buf.len(), 0, 9);
        assert_eq!(win.window_start, 32);
        assert_eq!(win.display, "你".repeat(4));
        assert_eq!(win.caret_cell, 8);
    }

    /// N-4 test 4: 光标在长文中段——窗口跟随光标，左缘对齐字符边界
    ///（不允许 display 含半个字符，跨界整字符滚出）。
    #[test]
    fn mid_text_cursor_window_follows_caret_on_char_boundaries() {
        let buf = "你".repeat(20);
        let cursor_byte = "你".repeat(5).len();
        let win = note_display_window(&buf, cursor_byte, 0, 9);
        assert_eq!(win.window_start, 2);
        assert_eq!(win.caret_cell, 8);
        assert_eq!(win.display, "你".repeat(4));
        assert_eq!(
            weft_core::grid::terminal_text_width(&win.display),
            8,
            "display must stay whole chars within the cell budget"
        );
    }

    /// N-4 test 5: 空缓冲 + 有 preedit——caret_cell=0，preedit_cells 参与
    /// 窗口预算（缓冲无处可滚，窗口保持 0）。
    #[test]
    fn empty_buffer_with_preedit_keeps_caret_at_zero() {
        let win = note_display_window("", 0, 6, 9);
        assert_eq!(win.caret_cell, 0);
        assert_eq!(win.window_start, 0);
        assert!(win.display.is_empty());
    }

    /// N-4 test 6: 组合字符——"e" + U+0301 附标（宽 0）不得与其基字符被
    /// 窗口边界分离（防残缺字形渲染）。
    #[test]
    fn zero_width_mark_never_separates_from_its_base_char() {
        let buf = "e\u{0301}fgh";
        let cursor_byte = buf.len();
        // 预算充足：附标随基字符完整可见。
        let win = note_display_window(buf, cursor_byte, 0, 5);
        assert_eq!(win.window_start, 0);
        assert_eq!(win.display, buf);
        // 预算逼出滚动（原始边界落在基字符与附标之间）：附标必须随基字符
        // 整体滚出，display 不得以裸附标开头。
        let win = note_display_window(buf, cursor_byte, 0, 3);
        assert_eq!(win.window_start, 2);
        assert_eq!(win.display, "gh");
        assert_eq!(win.caret_cell, 2);
        assert!(
            !win.display.starts_with('\u{0301}'),
            "a bare combining mark must never lead the window"
        );
    }

    /// P1-3 test: 锚点 x 在卡片 "Note: " 标签之后、y 在顶部卡片输入行、
    /// 尺寸非零（1600px 视口 / 17.5×35 cell）。
    #[test]
    fn note_editor_ime_area_anchors_after_label_on_top_card_row() {
        let ctx = ime_ctx();
        let area = note_editor_ime_area(&ctx);
        let target_w = note_card_width(1600.0, CW);
        let popup_x0 = (1600.0 - target_w) * 0.5;
        let text_x = popup_x0 + NOTE_CARD_ACCENT_STRIPE_W + NOTE_CARD_INNER_PAD;
        assert_eq!(
            area.x,
            text_x + NOTE_CARD_LABEL.len() as f32 * CW,
            "anchor must sit right after the 'Note: ' label"
        );
        let popup_y0 = ctx.top() + NOTE_CARD_TOP_OFFSET;
        let line_h = (CH * NOTE_CARD_LINE_H_CELLS).max(CH + NOTE_CARD_LINE_H_MIN_PX);
        assert_eq!(area.y, popup_y0 + (line_h - CH) * 0.5);
        assert!(
            area.y >= popup_y0 && area.y < popup_y0 + line_h,
            "anchor must land on the card's input row"
        );
        assert!(area.width > 0.0 && area.height > 0.0);
    }

    /// 一致性单测（评审 P3）：共享常数与锚定公式机器钉死——常数漂移=刻意
    /// 视觉变更；锚定输出必须与渲染公式（此处按原始字面量重算）逐位一致。
    #[test]
    fn note_card_consts_pin_anchor_and_render_to_one_formula() {
        assert_eq!(NOTE_CARD_WIDTH_FRACTION, 0.6);
        assert_eq!(NOTE_CARD_MIN_WIDTH_CELLS, 40.0);
        assert_eq!(NOTE_CARD_MAX_WIDTH_CELLS, 80.0);
        assert_eq!(NOTE_CARD_TOP_OFFSET, 10.0);
        assert_eq!(NOTE_CARD_ACCENT_STRIPE_W, 3.0);
        assert_eq!(NOTE_CARD_INNER_PAD, 8.0);
        assert_eq!(NOTE_CARD_LINE_H_CELLS, 1.75);
        assert_eq!(NOTE_CARD_LINE_H_MIN_PX, 16.0);
        assert_eq!(NOTE_CARD_LABEL, "Note: ");
        let ctx = ime_ctx();
        let area = note_editor_ime_area(&ctx);
        // Two-step form mirrors the renderer formula's max→min order without
        // tripping clippy::manual_clamp on the test-only literal chain.
        let target_w = (1600.0_f32 * 0.6).max(CW * 40.0);
        let target_w = target_w.min(CW * 80.0);
        let popup_x0 = (1600.0 - target_w) * 0.5;
        let popup_y0 = ctx.top() + 10.0;
        let line_h = (CH * 1.75).max(CH + 16.0);
        assert_eq!(area.x, popup_x0 + 3.0 + 8.0 + "Note: ".len() as f32 * CW);
        assert_eq!(area.y, popup_y0 + (line_h - CH) * 0.5);
        assert_eq!((area.width, area.height), (CW, CH));
    }
}
