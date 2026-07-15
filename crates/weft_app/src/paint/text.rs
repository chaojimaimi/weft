//! Text rasterization helpers for the Metal renderer.
//!
//! These remain `impl MetalRenderer` methods (A5 strategy b) because they
//! need `&self.atlas` (glyph UV lookup) and `&self.theme` (syntax colors).
//! Splitting them out of `renderer.rs` keeps the main file focused on GPU
//! pipeline + grid rendering; overlay builders in `paint/overlays.rs` call
//! these via the same `self.push_text(...)` syntax with zero call-site edits.

use crate::paint::primitives::{push_quad, syntax_color};
use crate::renderer::MetalRenderer;
use weft_core::syntax;

impl MetalRenderer {
    /// Column width of a character (0 for zero-width combining marks,
    /// 1 for ASCII/narrow, 2 for CJK full-width including ambiguous-width
    /// characters like ①②③ which are rendered full-width in CJK context).
    pub(crate) fn char_col_width(c: char) -> usize {
        unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0)
    }

    /// Total column width of a string — sum of each char's display width.
    /// Use this instead of `chars().count()` whenever a width/position
    /// calculation must match what `push_text` actually renders (CJK chars
    /// occupy 2 columns each, not 1).
    pub(crate) fn text_col_width(s: &str) -> usize {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width_cjk(c).unwrap_or(0))
            .sum()
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
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let mut col = 0usize;
        let mut px = x;
        for c in text.chars() {
            let w = Self::char_col_width(c);
            if w == 0 {
                continue; // skip combining marks / zero-width
            }
            if col + w > max_cols {
                break; // column budget exhausted
            }
            let Some(g) = self.atlas.get(c) else {
                col += w;
                px += w as f32 * cw;
                continue;
            };
            let (u, v) = g.uv_origin;
            let (uw, vh) = g.uv_size;
            let cell_w = w as f32 * cw;
            push_quad(
                vertices,
                [px, y, px + cell_w, y + ch],
                [u, v + vh, u + uw, v],
                fg,
                [0.0; 4],
            );
            col += w;
            px += cell_w;
        }
    }

    /// Lay out a line left-to-right, coloring each shell token by its kind
    /// (syntax highlight). `default_fg` is used for Whitespace/Default tokens.
    /// Wide-character aware: CJK chars occupy 2 columns. Glyphs must already
    /// be in the atlas (warmed up by the caller).
    pub(crate) fn push_line_tokenized(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        line: &str,
        max_cols: usize,
    ) {
        let cw = self.cell_width() as f32;
        let mut col = 0usize;
        let mut px = x;
        for token in syntax::tokenize(line) {
            if col >= max_cols {
                break;
            }
            let color = syntax_color(token.kind, &self.theme);
            for c in token.text.chars() {
                let w = Self::char_col_width(c);
                if w == 0 {
                    continue;
                }
                if col + w > max_cols {
                    break;
                }
                if let Some(g) = self.atlas.get(c) {
                    let (u, v) = g.uv_origin;
                    let (uw, vh) = g.uv_size;
                    let cell_w = w as f32 * cw;
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
            }
        }
    }
}
