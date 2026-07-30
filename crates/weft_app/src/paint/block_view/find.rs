use crate::paint::primitives::push_quad;

#[derive(Clone, Copy)]
pub(super) struct FindHighlightCanvas {
    pub(super) cell_width: f32,
    pub(super) cell_height: f32,
    pub(super) background_uv: [f32; 4],
    pub(super) color: [f32; 4],
}

pub(super) fn push_find_highlight(
    vertices: &mut Vec<f32>,
    canvas: FindHighlightCanvas,
    text: &str,
    hit: (usize, usize),
    cols: usize,
    chunk_index: usize,
    origin: [f32; 2],
) {
    let Some((_, display_col, display_len)) =
        crate::block_component::block_match_visual_ranges(text, hit.0, hit.1, cols)
            .into_iter()
            .find(|(candidate, _, _)| *candidate == chunk_index)
    else {
        return;
    };
    let x0 = origin[0] + display_col as f32 * canvas.cell_width;
    let x1 = x0 + display_len as f32 * canvas.cell_width;
    push_quad(
        vertices,
        [x0, origin[1], x1, origin[1] + canvas.cell_height],
        canvas.background_uv,
        [0.0; 4],
        canvas.color,
    );
}
