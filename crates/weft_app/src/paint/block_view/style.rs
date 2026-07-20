use crate::paint::primitives::resolve_cell_color;
use crate::renderer::MetalRenderer;
use weft_core::blocks::StyledLine;
use weft_core::grid::Color;

pub(super) struct BlockOutputTextPaint<'a> {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) text: &'a str,
    pub(super) style: Option<&'a StyledLine>,
    pub(super) char_offset: usize,
    pub(super) fallback: [f32; 4],
    pub(super) max_cols: usize,
    pub(super) palette: &'a [Color; 256],
    pub(super) row_pitch: f32,
}

fn fills_terminal_cell_edges(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259f}')
}

impl MetalRenderer {
    pub(super) fn push_block_output_text(
        &self,
        vertices: &mut Vec<f32>,
        paint: BlockOutputTextPaint<'_>,
    ) {
        let cw = self.cell_width() as f32;
        let mut col = 0;
        let mut x = paint.x;
        for (index, ch) in paint.text.chars().enumerate() {
            let width = weft_core::grid::terminal_char_width(ch);
            if width == 0 {
                continue;
            }
            if col + width > paint.max_cols {
                break;
            }
            let color = paint
                .style
                .and_then(|line| line.foreground_at(paint.char_offset + index))
                .map(|origin| resolve_cell_color(origin, paint.fallback, paint.palette))
                .unwrap_or(paint.fallback);
            let mut encoded = [0; 4];
            let glyph_height = if fills_terminal_cell_edges(ch) {
                paint.row_pitch
            } else {
                self.cell_height() as f32
            };
            self.push_text_with_height(
                vertices,
                [x, paint.y],
                ch.encode_utf8(&mut encoded),
                color,
                width,
                glyph_height,
            );
            col += width;
            x += width as f32 * cw;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fills_terminal_cell_edges;

    #[test]
    fn block_and_box_glyphs_bridge_block_view_row_leading() {
        for glyph in ['█', '▀', '▄', '▌', '┌', '─', '│', '┘'] {
            assert!(fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
        for glyph in ['A', '中', '●'] {
            assert!(!fills_terminal_cell_edges(glyph), "glyph={glyph}");
        }
    }
}
