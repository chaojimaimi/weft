//! Inline IME marked-text overlay for passthrough terminal applications.

use crate::paint::primitives::{color_to_normalized, push_quad};
use crate::renderer::MetalRenderer;
use unicode_segmentation::UnicodeSegmentation;
use weft_core::grid::Grid;

#[derive(Clone, Copy)]
pub(crate) struct TuiPreeditDrawParams<'a> {
    pub(crate) text: &'a str,
    pub(crate) cursor: Option<(usize, usize)>,
}

/// v1.10.5: per-call paint parameters for a BlockView live-block preedit —
/// the marked text plus the resolved document anchor (pixel origin, clip
/// edge, block width) and the theme colors. Packed so
/// [`MetalRenderer::push_block_tui_preedit`] stays under the clippy
/// argument budget.
#[derive(Clone, Copy)]
pub(crate) struct BlockTuiPreeditParams<'a> {
    pub(crate) text: &'a str,
    pub(crate) cursor: Option<(usize, usize)>,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) right: f32,
    pub(crate) cols: usize,
    /// v1.10.5 (reviewer MEDIUM-1): the caret's column within the row —
    /// `push_text`'s `max_cols` budgets columns from the text origin, so
    /// the preedit must be limited to `cols - cursor_col` or its glyphs
    /// spill past the block's right edge near the end of a line.
    pub(crate) cursor_col: usize,
    pub(crate) bg_uv: [f32; 4],
    pub(crate) theme_bg: [f32; 4],
    pub(crate) accent: [f32; 4],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreeditVisualRow {
    pub(crate) grid_row: usize,
    pub(crate) grid_col: usize,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) text: String,
}

/// Wrap marked text from the TUI cursor without changing the bytes eventually
/// committed to the PTY. Rows outside the terminal viewport are omitted.
pub(crate) fn tui_preedit_rows(
    text: &str,
    start_row: usize,
    start_col: usize,
    rows: usize,
    cols: usize,
) -> Vec<PreeditVisualRow> {
    if text.is_empty() || rows == 0 || cols == 0 || start_row >= rows {
        return Vec::new();
    }

    let mut output = Vec::new();
    let mut row = start_row;
    let mut col = start_col.min(cols.saturating_sub(1));
    let mut chunk_col = col;
    let mut chunk_start = 0usize;
    let mut chunk = String::new();

    let flush = |output: &mut Vec<PreeditVisualRow>,
                 row: usize,
                 col: usize,
                 byte_start: usize,
                 byte_end: usize,
                 chunk: &mut String| {
        if !chunk.is_empty() && row < rows {
            output.push(PreeditVisualRow {
                grid_row: row,
                grid_col: col,
                byte_start,
                byte_end,
                text: std::mem::take(chunk),
            });
        }
    };

    for (byte, grapheme) in text.grapheme_indices(true) {
        if grapheme.contains(['\r', '\n']) {
            flush(&mut output, row, chunk_col, chunk_start, byte, &mut chunk);
            row += 1;
            if row >= rows {
                return output;
            }
            col = 0;
            chunk_col = 0;
            chunk_start = byte + grapheme.len();
            continue;
        }
        let width = weft_core::grid::terminal_text_width(grapheme);
        if col + width > cols && (col > 0 || !chunk.is_empty()) {
            if !chunk.is_empty() {
                flush(&mut output, row, chunk_col, chunk_start, byte, &mut chunk);
            }
            row += 1;
            if row >= rows {
                return output;
            }
            col = 0;
            chunk_col = 0;
            chunk_start = byte;
        }
        chunk.push_str(grapheme);
        col = (col + width).min(cols);
    }
    flush(
        &mut output,
        row,
        chunk_col,
        chunk_start,
        text.len(),
        &mut chunk,
    );
    output
}

impl MetalRenderer {
    pub(crate) fn build_tui_preedit_for_grid(
        &self,
        params: TuiPreeditDrawParams<'_>,
        grid: &Grid,
    ) -> Vec<f32> {
        self.build_tui_preedit_vertices(
            params,
            (grid.cursor.row, grid.cursor.col),
            (grid.num_rows, grid.num_cols),
        )
    }

    pub(crate) fn build_tui_preedit_vertices(
        &self,
        params: TuiPreeditDrawParams<'_>,
        grid_cursor: (usize, usize),
        grid_size: (usize, usize),
    ) -> Vec<f32> {
        let mut vertices = Vec::new();
        let Some(ctx) = self.layout_ctx else {
            return vertices;
        };
        let rows = tui_preedit_rows(
            params.text,
            grid_cursor.0,
            grid_cursor.1,
            grid_size.0,
            grid_size.1,
        );
        if rows.is_empty() {
            return vertices;
        }

        let cw = ctx.cell_w;
        let ch = ctx.cell_h;
        let (su, sv, suw, svh) = self.space_uv();
        let bg_uv = [su, sv + svh, su + suw, sv];
        let theme_bg = color_to_normalized(self.theme.background);
        let bg = [theme_bg[0], theme_bg[1], theme_bg[2], 0.96];
        let accent = color_to_normalized(self.theme.accent);
        let cursor = params.cursor.map(|(start, end)| {
            crate::paint::prompt::normalize_preedit_range(params.text, start, end)
        });

        let row_count = rows.len();
        for (row_index, row) in rows.into_iter().enumerate() {
            let x = ctx.col_x(row.grid_col);
            let y = ctx.row_y(row.grid_row);
            let width = Self::text_col_width(&row.text).max(1);
            push_quad(
                &mut vertices,
                [x, y, (x + width as f32 * cw).min(ctx.right()), y + ch],
                bg_uv,
                [0.0; 4],
                bg,
            );

            if let Some((start, end)) = cursor {
                let local_start = start.max(row.byte_start).min(row.byte_end);
                let local_end = end.max(local_start).min(row.byte_end);
                let prefix_cols = Self::text_col_width(&params.text[row.byte_start..local_start]);
                let selected_cols = Self::text_col_width(&params.text[local_start..local_end]);
                let marker_x = x + prefix_cols as f32 * cw;
                let cursor_belongs_to_row = start >= row.byte_start
                    && (start < row.byte_end
                        || (row_index + 1 == row_count && start == row.byte_end));
                if start == end && cursor_belongs_to_row {
                    push_quad(
                        &mut vertices,
                        [marker_x, y + ch - 2.0, marker_x + 2.0, y + ch],
                        bg_uv,
                        [0.0; 4],
                        accent,
                    );
                } else if selected_cols > 0 {
                    push_quad(
                        &mut vertices,
                        [
                            marker_x,
                            y + ch - 2.0,
                            marker_x + selected_cols as f32 * cw,
                            y + ch,
                        ],
                        bg_uv,
                        [0.0; 4],
                        accent,
                    );
                }
            }
            self.push_text(&mut vertices, x, y, &row.text, accent, width);
        }
        vertices
    }

    /// v1.10.5: draw a single-row IME preedit at a BlockView live-block
    /// caret (background band + accent text + composition caret). Shares the
    /// semantics of [`build_tui_preedit_vertices`](Self::build_tui_preedit_
    /// vertices) but takes an explicit pixel anchor — the grid cursor
    /// mapped into the live block's document row by the BlockView paint
    /// pass — instead of deriving the anchor from the grid coordinate
    /// system (which the BlockView does not render). Rows longer than the
    /// block width are clipped by `push_text`'s `max_cols`.
    pub(crate) fn push_block_tui_preedit(
        &self,
        verts: &mut Vec<f32>,
        params: BlockTuiPreeditParams<'_>,
    ) {
        let BlockTuiPreeditParams {
            text,
            cursor,
            x,
            y,
            right,
            cols,
            cursor_col,
            bg_uv,
            theme_bg,
            accent,
        } = params;
        if text.is_empty() {
            return;
        }
        let cw = self.cell_width() as f32;
        let ch = self.cell_height() as f32;
        let preedit_cols = Self::text_col_width(text);
        let preedit_w = (preedit_cols as f32 * cw).max(1.0);
        push_quad(
            verts,
            [x, y, (x + preedit_w).min(right), y + ch],
            bg_uv,
            [0.0; 4],
            [theme_bg[0], theme_bg[1], theme_bg[2], 0.96],
        );
        if let Some((start, end)) = cursor {
            let (start, end) = crate::paint::prompt::normalize_preedit_range(text, start, end);
            let prefix_cols = Self::text_col_width(&text[..start]);
            let selected_cols = Self::text_col_width(&text[start..end]);
            let marker_x = x + prefix_cols as f32 * cw;
            if start == end {
                push_quad(
                    verts,
                    [marker_x, y + ch - 2.0, marker_x + 2.0, y + ch],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            } else if selected_cols > 0 {
                push_quad(
                    verts,
                    [
                        marker_x,
                        y + ch - 2.0,
                        marker_x + selected_cols as f32 * cw,
                        y + ch,
                    ],
                    bg_uv,
                    [0.0; 4],
                    accent,
                );
            }
        }
        // Reviewer MEDIUM-1: max_cols budgets columns FROM the text origin
        // (the caret), so the remaining row width is `cols - cursor_col`.
        self.push_text(verts, x, y, text, accent, cols.saturating_sub(cursor_col));
    }
}

#[cfg(test)]
mod tests {
    use super::tui_preedit_rows;

    #[test]
    fn preedit_wraps_at_terminal_edge_without_losing_text() {
        let rows = tui_preedit_rows("shen'ru'fen'xi", 2, 7, 5, 10);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["she", "n'ru'fen'x", "i"]
        );
        assert_eq!(rows[0].grid_col, 7);
        assert_eq!(rows[1].grid_col, 0);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<String>(),
            "shen'ru'fen'xi"
        );
    }

    #[test]
    fn preedit_counts_cjk_as_two_terminal_cells() {
        let rows = tui_preedit_rows("a中b", 0, 3, 2, 5);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["a", "中b"]
        );
    }

    #[test]
    fn wide_preedit_character_moves_off_the_last_column() {
        let rows = tui_preedit_rows("中", 0, 4, 2, 5);
        assert_eq!(rows[0].grid_row, 1);
        assert_eq!(rows[0].grid_col, 0);
        assert_eq!(rows[0].text, "中");
    }

    #[test]
    fn preedit_keeps_zwj_combining_and_skin_tone_graphemes_atomic() {
        for text in ["A👩‍🔬B", "Ae\u{301}B", "A👍🏽B"] {
            let rows = tui_preedit_rows(text, 0, 3, 4, 5);
            assert_eq!(
                rows.iter().map(|row| row.text.as_str()).collect::<String>(),
                text
            );
            assert!(rows.iter().all(|row| !row.text.ends_with('\u{200d}')));
        }
    }

    #[test]
    fn preedit_newline_and_crlf_start_a_fresh_visual_row() {
        for text in ["ab\ncd", "ab\r\ncd"] {
            let rows = tui_preedit_rows(text, 1, 2, 5, 20);
            assert_eq!(
                rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
                ["ab", "cd"]
            );
            assert_eq!((rows[0].grid_row, rows[0].grid_col), (1, 2));
            assert_eq!((rows[1].grid_row, rows[1].grid_col), (2, 0));
            assert!(rows[0].byte_end <= rows[1].byte_start);
        }
    }

    #[test]
    fn preedit_byte_ranges_remain_utf8_boundaries_across_wraps() {
        let text = "a中👩‍🔬b";
        let rows = tui_preedit_rows(text, 0, 3, 4, 5);
        for row in rows {
            assert!(text.is_char_boundary(row.byte_start));
            assert!(text.is_char_boundary(row.byte_end));
            assert_eq!(&text[row.byte_start..row.byte_end], row.text);
        }
    }

    #[test]
    fn preedit_is_clipped_to_the_grid() {
        let rows = tui_preedit_rows("abcdef", 0, 3, 1, 4);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }
}
