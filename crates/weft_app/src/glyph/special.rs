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
const QUAD_UPPER_LEFT: u8 = 1;
const QUAD_UPPER_RIGHT: u8 = 2;
const QUAD_LOWER_LEFT: u8 = 4;
const QUAD_LOWER_RIGHT: u8 = 8;

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
        '▁'..='▇' => Some(fill_lower_eighths(ch, width, height)),
        '▉'..='▏' => Some(fill_left_eighths(ch, width, height)),
        '▔' => Some(fill_rect(
            width,
            height,
            0,
            height.saturating_sub(height.div_ceil(8)),
            width,
            height,
        )),
        '▕' => Some(fill_rect(
            width,
            height,
            width.saturating_sub(width.div_ceil(8)),
            0,
            width,
            height,
        )),
        '▖' => Some(draw_quadrants(width, height, QUAD_LOWER_LEFT)),
        '▗' => Some(draw_quadrants(width, height, QUAD_LOWER_RIGHT)),
        '▘' => Some(draw_quadrants(width, height, QUAD_UPPER_LEFT)),
        '▙' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_LEFT | QUAD_LOWER_LEFT | QUAD_LOWER_RIGHT,
        )),
        '▚' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_LEFT | QUAD_LOWER_RIGHT,
        )),
        '▛' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_LEFT | QUAD_UPPER_RIGHT | QUAD_LOWER_LEFT,
        )),
        '▜' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_LEFT | QUAD_UPPER_RIGHT | QUAD_LOWER_RIGHT,
        )),
        '▝' => Some(draw_quadrants(width, height, QUAD_UPPER_RIGHT)),
        '▞' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_RIGHT | QUAD_LOWER_LEFT,
        )),
        '▟' => Some(draw_quadrants(
            width,
            height,
            QUAD_UPPER_RIGHT | QUAD_LOWER_LEFT | QUAD_LOWER_RIGHT,
        )),
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

fn fill_lower_eighths(ch: char, width: u32, height: u32) -> Vec<u8> {
    let eighths = ch as u32 - '▁' as u32 + 1;
    let filled = (height * eighths).div_ceil(8);
    fill_rect(width, height, 0, 0, width, filled)
}

fn fill_left_eighths(ch: char, width: u32, height: u32) -> Vec<u8> {
    let eighths = 8 - (ch as u32 - '▉' as u32 + 1);
    let filled = (width * eighths).div_ceil(8);
    fill_rect(width, height, 0, 0, filled, height)
}

fn draw_quadrants(width: u32, height: u32, quadrants: u8) -> Vec<u8> {
    let mut pixels = vec![0; width as usize * height as usize];
    let x_mid = width.div_ceil(2);
    let y_mid = height.div_ceil(2);
    if quadrants & QUAD_UPPER_LEFT != 0 {
        paint(&mut pixels, width, 0, height / 2, x_mid, height);
    }
    if quadrants & QUAD_UPPER_RIGHT != 0 {
        paint(&mut pixels, width, width / 2, height / 2, width, height);
    }
    if quadrants & QUAD_LOWER_LEFT != 0 {
        paint(&mut pixels, width, 0, 0, x_mid, y_mid);
    }
    if quadrants & QUAD_LOWER_RIGHT != 0 {
        paint(&mut pixels, width, width / 2, 0, width, y_mid);
    }
    pixels
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
    // The atlas is built in physical pixels. A fixed one-pixel stroke becomes
    // only half a logical pixel on Retina and nearly disappears for dim ANSI
    // box lines. Scale the light stroke from the cell width; heavy lines keep
    // a 2x relationship. At the common 1x 8px cell this remains 1px, while a
    // 2x 16px cell becomes 2px.
    let light = (width / 8).max(1);
    let thickness = if heavy { light * 2 } else { light }.min(width).min(height);
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
    fn retina_vertical_box_line_keeps_one_logical_pixel_weight() {
        let width = 16;
        let height = 32;
        let pixels = rasterize('│', width, height).unwrap();
        let center_row = &pixels
            [(height as usize / 2) * width as usize..(height as usize / 2 + 1) * width as usize];
        assert_eq!(
            center_row.iter().filter(|pixel| **pixel == u8::MAX).count(),
            2
        );
    }

    #[test]
    fn light_and_heavy_box_strokes_scale_consistently_at_1x_and_2x() {
        for (width, height, light, heavy) in [(8, 16, 1), (16, 32, 2)]
            .map(|(width, height, light)| (width, height, light, light * 2))
        {
            for (ch, expected) in [('│', light), ('┃', heavy)] {
                let pixels = rasterize(ch, width, height).unwrap();
                let row = &pixels[(height as usize / 2) * width as usize
                    ..(height as usize / 2 + 1) * width as usize];
                assert_eq!(row.iter().filter(|pixel| **pixel > 0).count(), expected);
            }
            for (ch, expected) in [('─', light), ('━', heavy)] {
                let pixels = rasterize(ch, width, height).unwrap();
                let column_ink = (0..height as usize)
                    .filter(|row| pixels[row * width as usize + width as usize / 2] > 0)
                    .count();
                assert_eq!(column_ink, expected);
            }
        }
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

    #[test]
    fn claude_logo_quadrants_have_no_font_bearing_gaps() {
        let width = 8;
        let height = 16;
        let upper = rasterize('▛', width, height).unwrap();
        let upper_right = rasterize('▝', width, height).unwrap();
        let upper_left = rasterize('▘', width, height).unwrap();

        for row in upper.chunks_exact(width as usize).skip(height as usize / 2) {
            assert!(row.iter().all(|pixel| *pixel == u8::MAX));
        }
        assert_eq!(upper_right[(height as usize - 1) * width as usize], 0);
        assert_eq!(upper_right[height as usize * width as usize - 1], u8::MAX);
        assert_eq!(upper_left[(height as usize - 1) * width as usize], u8::MAX);
        assert_eq!(upper_left[height as usize * width as usize - 1], 0);
    }

    #[test]
    fn all_solid_block_elements_use_procedural_rasterization() {
        for ch in '▁'..='▟' {
            if !matches!(ch, '░' | '▒' | '▓') {
                assert!(
                    rasterize(ch, 8, 16).is_some(),
                    "missing rasterizer for {ch}"
                );
            }
        }
    }
}
