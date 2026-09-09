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
        // Geometric Shapes: procedural edge-to-edge rendering with 4x4
        // supersampled antialiasing (see supersample / draw_* below). Shapes
        // are centered and sized by min(width, height) so a wide (double-cell)
        // slot keeps circles round instead of stretching them.
        '●' => Some(draw_circle_fill(width, height, 0.85)),
        '⬤' => Some(draw_circle_fill(width, height, 0.95)),
        '○' => Some(draw_circle_ring(width, height, 0.85, 0.24 * 0.85 / 2.0)),
        '•' => Some(draw_circle_fill(width, height, 0.45)),
        '◦' => Some(draw_circle_ring(width, height, 0.45, 0.30 * 0.45 / 2.0)),
        '■' => Some(draw_square_fill(width, height, 0.85)),
        '□' => Some(draw_square_frame(width, height, 0.85, 0.12 * 0.85)),
        '▲' => Some(draw_triangle(width, height, TriDir::Up, 0.85)),
        '▼' => Some(draw_triangle(width, height, TriDir::Down, 0.85)),
        '▶' => Some(draw_triangle(width, height, TriDir::Right, 0.85)),
        '◀' => Some(draw_triangle(width, height, TriDir::Left, 0.85)),
        '◆' => Some(draw_diamond(width, height, 0.85, true)),
        '◇' => Some(draw_diamond(width, height, 0.85, false)),
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

/// Antialiasing supersample factor. Each output pixel is the coverage ratio
/// (0–255) of 16 subpixel inclusion tests.
const AA: u32 = 4;

/// Generic 4×4 supersampled rasterizer.
///
/// `f` receives normalized coordinates centered on the cell so that
/// `min(width, height)` is one unit: `(0, 0)` is the cell center, the x/y
/// axes run along the cell edges, and +y points downward (toward higher pixel
/// rows). `f` returns `true` when the sample point lies inside the shape.
fn supersample(width: u32, height: u32, f: impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let mut pixels = vec![0u8; width as usize * height as usize];
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let unit = (width.min(height)) as f32;
    let inv = 1.0 / AA as f32;
    for py in 0..height {
        for px in 0..width {
            let mut hits = 0u32;
            for j in 0..AA {
                let sy = (py as f32) + (j as f32 + 0.5) * inv;
                let ny = (sy - cy) / unit;
                for i in 0..AA {
                    let sx = (px as f32) + (i as f32 + 0.5) * inv;
                    let nx = (sx - cx) / unit;
                    if f(nx, ny) {
                        hits += 1;
                    }
                }
            }
            pixels[(py * width + px) as usize] = ((hits * 255 + (AA * AA) / 2) / (AA * AA)) as u8;
        }
    }
    pixels
}

/// Filled circle. `ratio` is the diameter relative to `min(width, height)`.
fn draw_circle_fill(width: u32, height: u32, ratio: f32) -> Vec<u8> {
    let r = ratio / 2.0;
    let r2 = r * r;
    supersample(width, height, move |x, y| x * x + y * y <= r2)
}

/// Ring (hollow circle). `ratio` is the diameter; `stroke` is the line width
/// expressed as a fraction of the circle radius.
fn draw_circle_ring(width: u32, height: u32, ratio: f32, stroke: f32) -> Vec<u8> {
    let r = ratio / 2.0;
    let outer = r + stroke / 2.0;
    let inner = (r - stroke / 2.0).max(0.0);
    let o2 = outer * outer;
    let i2 = inner * inner;
    supersample(width, height, move |x, y| {
        let d2 = x * x + y * y;
        d2 <= o2 && d2 >= i2
    })
}

/// Filled axis-aligned square. `ratio` is the edge length relative to
/// `min(width, height)`.
fn draw_square_fill(width: u32, height: u32, ratio: f32) -> Vec<u8> {
    let h = ratio / 2.0;
    supersample(width, height, move |x, y| x.abs() <= h && y.abs() <= h)
}

/// Hollow square frame. `ratio` is the edge length; `stroke` is the line width
/// as a fraction of the edge.
fn draw_square_frame(width: u32, height: u32, ratio: f32, stroke: f32) -> Vec<u8> {
    let h = ratio / 2.0;
    let t = stroke / 2.0;
    supersample(width, height, move |x, y| {
        let ax = x.abs();
        let ay = y.abs();
        ax <= h && ay <= h && (ax >= h - t || ay >= h - t)
    })
}

#[derive(Clone, Copy)]
enum TriDir {
    Up,
    Down,
    Left,
    Right,
}

/// Filled triangle. `ratio` is the bounding-box edge relative to
/// `min(width, height)`; `dir` points the apex toward that edge.
///
/// The raw bitmap is stored bottom-up (row 0 = visual bottom, row h-1 = visual
/// top) and V-flipped at paint time, so `Up` (▲, point at the visual top) must
/// place the apex at the BACK of the buffer (large y, higher row index).
/// `Down` mirrors this. Left/Right are unaffected by the vertical flip.
fn draw_triangle(width: u32, height: u32, dir: TriDir, ratio: f32) -> Vec<u8> {
    let h = ratio / 2.0;
    supersample(width, height, move |x, y| match dir {
        TriDir::Up => y >= -h && y <= h && x >= (y - h) / 2.0 && x <= (h - y) / 2.0,
        TriDir::Down => y >= -h && y <= h && x >= -(y + h) / 2.0 && x <= (y + h) / 2.0,
        TriDir::Left => x >= -h && x <= h && y >= -(x + h) / 2.0 && y <= (x + h) / 2.0,
        TriDir::Right => x >= -h && x <= h && y >= (x - h) / 2.0 && y <= (h - x) / 2.0,
    })
}

/// Diamond (square rotated 45°). `ratio` is the diagonal relative to
/// `min(width, height)`. When `filled` is false a hollow frame is drawn whose
/// line width is ~10% of the diagonal.
fn draw_diamond(width: u32, height: u32, ratio: f32, filled: bool) -> Vec<u8> {
    let h = ratio / 2.0;
    if filled {
        supersample(width, height, move |x, y| x.abs() + y.abs() <= h)
    } else {
        let band = ratio * 0.10 / 2.0; // half of the ~10%-of-diagonal stroke
        supersample(width, height, move |x, y| {
            let d = x.abs() + y.abs();
            d <= h && d >= h - band
        })
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

    // ---- Geometric Shapes (Fix A) ----

    /// Mean ink coverage across a mask, 0.0–1.0.
    fn coverage(pixels: &[u8]) -> f32 {
        let sum: u32 = pixels.iter().map(|&p| p as u32).sum();
        sum as f32 / (pixels.len() as f32 * 255.0)
    }

    #[test]
    fn geometric_circle_ink_coverage() {
        let pixels = rasterize('●', 24, 24).unwrap();
        let cov = coverage(&pixels);
        assert!(
            (0.50..=0.65).contains(&cov),
            "circle coverage {cov} outside [0.50, 0.65] (expected ~0.567)"
        );
    }

    #[test]
    fn bullet_smaller_than_circle() {
        let big = coverage(&rasterize('●', 24, 24).unwrap());
        let small = coverage(&rasterize('•', 24, 24).unwrap());
        assert!(
            small < big * 0.4,
            "bullet coverage {small} not < circle {big} * 0.4"
        );
    }

    #[test]
    fn ring_has_hollow_center() {
        let w = 24;
        let h = 24;
        let pixels = rasterize('○', w, h).unwrap();
        let cx = w as f32 / 2.0;
        let cy = h as f32 / 2.0;

        // Central third of the cell should be transparent.
        let mut center_sum = 0u32;
        let mut center_n = 0u32;
        // Annular band at 0.38–0.45 of unit should carry ink.
        let mut band_sum = 0u32;
        let mut band_n = 0u32;
        for y in 0..h {
            for x in 0..w {
                let dx = (x as f32 + 0.5 - cx) / w as f32;
                let dy = (y as f32 + 0.5 - cy) / h as f32;
                let r = (dx * dx + dy * dy).sqrt();
                let v = pixels[(y * w + x) as usize] as u32;
                if r < 1.0 / 3.0 {
                    center_sum += v;
                    center_n += 1;
                }
                if (0.38..=0.45).contains(&r) {
                    band_sum += v;
                    band_n += 1;
                }
            }
        }
        let center_mean = center_sum as f32 / center_n as f32;
        let band_mean = band_sum as f32 / band_n as f32;
        assert!(center_mean < 30.0, "ring center not hollow: {center_mean}");
        assert!(band_mean > 100.0, "ring band lacks ink: {band_mean}");
    }

    #[test]
    fn aa_edges_have_midtones() {
        let pixels = rasterize('●', 24, 24).unwrap();
        let has_midtone = pixels.iter().any(|&v| v > 30 && v < 225);
        assert!(has_midtone, "no antialiased midtone pixels found");
    }

    #[test]
    fn triangle_orientation_respects_bottom_up_atlas() {
        let w = 24;
        let h = 24;
        let quarter = h / 4;

        // The raw bitmap is bottom-up: vec rows 0..quarter are the VISUAL BOTTOM
        // and rows 3*quarter.. are the VISUAL TOP (flipped at paint time).
        let up = rasterize('▲', w, h).unwrap();
        let up_front: u32 = up[..quarter as usize * w as usize]
            .iter()
            .map(|&p| p as u32)
            .sum();
        let up_back: u32 = up[(3 * quarter as usize) * w as usize..]
            .iter()
            .map(|&p| p as u32)
            .sum();
        assert!(
            up_back < up_front,
            "▲ visual-top rows {up_back} should have less ink than visual-bottom rows {up_front}"
        );

        let down = rasterize('▼', w, h).unwrap();
        let down_front: u32 = down[..quarter as usize * w as usize]
            .iter()
            .map(|&p| p as u32)
            .sum();
        let down_back: u32 = down[(3 * quarter as usize) * w as usize..]
            .iter()
            .map(|&p| p as u32)
            .sum();
        assert!(
            down_back > down_front,
            "▼ visual-top rows {down_back} should have more ink than visual-bottom rows {down_front}"
        );
    }

    #[test]
    fn circle_not_stretched_in_wide_slot() {
        let small = rasterize('●', 24, 24).unwrap();
        let wide = rasterize('●', 48, 24).unwrap();
        let off = (48 - 24) / 2;
        for y in 0..24 {
            for k in 0..24 {
                let w_idx = y * 48 + (off + k) as usize;
                let s_idx = y * 24 + k as usize;
                assert_eq!(
                    wide[w_idx], small[s_idx],
                    "mismatch at y={y} k={k} (wide slot stretched the circle)"
                );
            }
        }
    }
}
