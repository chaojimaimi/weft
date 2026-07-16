//! Procedural terminal glyphs that must touch cell edges.
//!
//! Font outlines often leave side bearings around box drawing and block
//! elements. In a terminal that turns one logical line into visible gaps and
//! makes pixel-art logos look blurry, so these core glyphs use exact cell
//! geometry instead.

const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const UP: u8 = 4;
const DOWN: u8 = 8;

pub(super) fn rasterize(ch: char, width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    match ch {
        '█' => Some(vec![u8::MAX; width as usize * height as usize]),
        // Atlas V is sampled upside-down by the renderer, so screen-top is
        // the lower half of the raw atlas cell and screen-bottom is the upper.
        '▀' => Some(fill_rect(width, height, 0, height / 2, width, height)),
        '▄' => Some(fill_rect(width, height, 0, 0, width, height.div_ceil(2))),
        '▌' => Some(fill_rect(width, height, 0, 0, width.div_ceil(2), height)),
        '▐' => Some(fill_rect(width, height, width / 2, 0, width, height)),
        '─' => Some(draw_box(width, height, LEFT | RIGHT, false)),
        '━' => Some(draw_box(width, height, LEFT | RIGHT, true)),
        '│' => Some(draw_box(width, height, UP | DOWN, false)),
        '┃' => Some(draw_box(width, height, UP | DOWN, true)),
        '┌' => Some(draw_box(width, height, RIGHT | DOWN, false)),
        '┐' => Some(draw_box(width, height, LEFT | DOWN, false)),
        '└' => Some(draw_box(width, height, RIGHT | UP, false)),
        '┘' => Some(draw_box(width, height, LEFT | UP, false)),
        '├' => Some(draw_box(width, height, RIGHT | UP | DOWN, false)),
        '┤' => Some(draw_box(width, height, LEFT | UP | DOWN, false)),
        '┬' => Some(draw_box(width, height, LEFT | RIGHT | DOWN, false)),
        '┴' => Some(draw_box(width, height, LEFT | RIGHT | UP, false)),
        '┼' => Some(draw_box(width, height, LEFT | RIGHT | UP | DOWN, false)),
        _ => None,
    }
}

fn fill_rect(width: u32, height: u32, x0: u32, y0: u32, x1: u32, y1: u32) -> Vec<u8> {
    let mut pixels = vec![0; width as usize * height as usize];
    for y in y0.min(height)..y1.min(height) {
        for x in x0.min(width)..x1.min(width) {
            pixels[(y * width + x) as usize] = u8::MAX;
        }
    }
    pixels
}

fn draw_box(width: u32, height: u32, segments: u8, heavy: bool) -> Vec<u8> {
    let mut pixels = vec![0; width as usize * height as usize];
    let thickness = if heavy { 2 } else { 1 }.min(width).min(height);
    let cx = width / 2;
    let cy = height / 2;
    let half = thickness / 2;
    let x0 = cx.saturating_sub(half);
    let x1 = (x0 + thickness).min(width);
    let y0 = cy.saturating_sub(half);
    let y1 = (y0 + thickness).min(height);

    if segments & LEFT != 0 {
        paint(&mut pixels, width, 0, y0, cx + 1, y1);
    }
    if segments & RIGHT != 0 {
        paint(&mut pixels, width, cx, y0, width, y1);
    }
    // Raw atlas Y is inverted at sampling time.
    if segments & UP != 0 {
        paint(&mut pixels, width, x0, cy, x1, height);
    }
    if segments & DOWN != 0 {
        paint(&mut pixels, width, x0, 0, x1, cy + 1);
    }
    pixels
}

fn paint(pixels: &mut [u8], width: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
    for y in y0..y1 {
        for x in x0..x1 {
            pixels[(y * width + x) as usize] = u8::MAX;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rasterize;

    #[test]
    fn horizontal_box_line_reaches_both_cell_edges() {
        let width = 8;
        let pixels = rasterize('─', width, 16).unwrap();
        assert!(pixels
            .chunks_exact(width as usize)
            .any(|row| { row.first() == Some(&u8::MAX) && row.last() == Some(&u8::MAX) }));
    }

    #[test]
    fn full_block_has_no_font_side_bearings() {
        let pixels = rasterize('█', 8, 16).unwrap();
        assert!(pixels.iter().all(|pixel| *pixel == u8::MAX));
    }

    #[test]
    fn adjacent_corners_share_exact_edge_pixels() {
        let left = rasterize('┌', 8, 16).unwrap();
        let right = rasterize('┐', 8, 16).unwrap();
        assert!(left.chunks_exact(8).any(|row| row[7] == u8::MAX));
        assert!(right.chunks_exact(8).any(|row| row[0] == u8::MAX));
    }
}
