use crate::paint::primitives::push_quad;

/// Map a find hit (char range into the prompt-stripped command) onto the
/// rendered command chunks. Mirrors `block_match_visual_ranges` but takes
/// the actual rendered chunks (dual-capacity wrapping) instead of
/// re-wrapping, so chunk boundaries match the paint path exactly.
/// Returns (chunk_index, display_col, display_len) per affected chunk.
pub(super) fn find_chunk_visual_ranges(
    chunks: &[String],
    hit_start: usize,
    hit_end: usize,
) -> Vec<(usize, usize, usize)> {
    let mut source_col = 0usize;
    let mut ranges = Vec::new();
    for (ci, chunk) in chunks.iter().enumerate() {
        let chunk_len = chunk.chars().count();
        let start = hit_start.max(source_col);
        let end = hit_end.min(source_col + chunk_len);
        if start < end {
            let display_col: usize = chunk
                .chars()
                .take(start - source_col)
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            let display_len: usize = chunk
                .chars()
                .skip(start - source_col)
                .take(end - start)
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            if display_len > 0 {
                ranges.push((ci, display_col, display_len));
            }
        }
        source_col += chunk_len;
    }
    ranges
}

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

#[cfg(test)]
mod tests {
    use super::find_chunk_visual_ranges;

    #[test]
    fn find_chunk_visual_ranges_maps_across_chunks() {
        let chunks = vec!["docker ps --".to_string(), "filter 'x'".to_string()];
        // chunk0 = [0,12), chunk1 = [12,22)。匹配 [8,16) 跨两个 chunk。
        let ranges = find_chunk_visual_ranges(&chunks, 8, 16);
        // chunk0: [8,12) → display_col=8("docker " 前缀 8 列), display_len=4("ps --")
        // chunk1: [12,16) → chunk 内 [0,4) → display_col=0, display_len=4("filt")
        assert_eq!(ranges, vec![(0, 8, 4), (1, 0, 4)]);
    }

    #[test]
    fn find_chunk_visual_ranges_honors_cjk_width() {
        let chunks = vec!["ab中文".to_string()];
        // 匹配 "中文"(chunk 内 [2,4))
        let ranges = find_chunk_visual_ranges(&chunks, 2, 4);
        assert_eq!(ranges, vec![(0, 2, 4)]); // 2 个 CJK = 4 列
    }

    #[test]
    fn find_chunk_visual_ranges_skips_out_of_hit() {
        let chunks = vec!["abc".to_string(), "def".to_string()];
        assert!(!find_chunk_visual_ranges(&chunks, 0, 3).is_empty()); // chunk0 命中
        assert_eq!(find_chunk_visual_ranges(&chunks, 9, 12), vec![]); // 超出
    }
}
