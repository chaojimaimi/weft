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
            self.push_text(
                vertices,
                x,
                paint.y,
                ch.encode_utf8(&mut encoded),
                color,
                width,
            );
            col += width;
            x += width as f32 * cw;
        }
    }
}
