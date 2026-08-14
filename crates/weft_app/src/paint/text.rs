//! Text rasterization helpers for the Metal renderer.
//!
//! These remain `impl MetalRenderer` methods (A5 strategy b) because they
//! need `&self.atlas` (glyph UV lookup) and `&self.theme` (syntax colors).
//! Splitting them out of `renderer.rs` keeps the main file focused on GPU
//! pipeline + grid rendering; overlay builders in `paint/overlays.rs` call
//! these via the same `self.push_text(...)` syntax with zero call-site edits.

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
                [px, y, px + cell_w, y + glyph_height],
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
                        [px, y, px + cell_w, y + self.cell_height() as f32],
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
