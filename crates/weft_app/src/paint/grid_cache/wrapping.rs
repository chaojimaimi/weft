use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

fn wrap_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    if cols == 0 {
        return std::iter::once(0..text.len()).collect();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width == 0 {
            continue;
        }
        if col + width > cols && grapheme.chars().all(char::is_whitespace) {
            col += width;
            continue;
        }
        if col + width > cols && byte > start {
            ranges.push(start..byte);
            start = byte;
            col = 0;
        }
        col += width;
    }
    ranges.push(start..text.len());
    ranges
}

/// Wrap prose by terminal display width while keeping graphemes atomic.
#[cfg(test)]
pub(crate) fn wrap_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    wrap_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Return source-relative byte ranges for cached block output. Keeping ranges
/// instead of owned wrapped strings avoids retaining a second copy of every
/// completed block while preserving the exact same wrapping behavior.
pub(crate) fn block_line_chunk_ranges(text: &str, cols: usize) -> Vec<Range<usize>> {
    if !is_terminal_structure_line(text) || cols == 0 {
        return wrap_line_chunk_ranges(text, cols);
    }
    let mut end = 0usize;
    let mut col = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width > 0 && col + width > cols {
            break;
        }
        col += width;
        end = byte + grapheme.len();
    }
    std::iter::once(0..end).collect()
}

/// Keep terminal-drawn structure atomic across history resizes; prose reflows.
pub(crate) fn block_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    block_line_chunk_ranges(text, cols)
        .into_iter()
        .map(|range| text[range].to_string())
        .collect::<Vec<_>>()
        .into_iter()
}

fn is_terminal_structure_line(text: &str) -> bool {
    let mut visible = 0usize;
    let mut box_drawing = 0usize;
    let mut vertical_separators = 0usize;
    for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
        visible += 1;
        box_drawing += usize::from(matches!(ch, '\u{2500}'..='\u{257f}'));
        vertical_separators += usize::from(matches!(ch, '│' | '┃' | '║' | '┆' | '┇' | '┊' | '┋'));
    }
    (visible >= 8 && box_drawing == visible) || vertical_separators >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_emoji_grapheme_clusters_atomic() {
        assert_eq!(
            wrap_line_chunks("A👩‍🔬B", 3).collect::<Vec<_>>(),
            ["A👩‍🔬", "B"]
        );
    }

    #[test]
    fn zero_width_graphemes_preserve_source_offsets_across_chunks() {
        let line = "ab\u{200b}cdef";
        let chunks = wrap_line_chunks(line, 2).collect::<Vec<_>>();
        assert_eq!(chunks.concat(), line);
        assert_eq!(chunks[0].chars().count(), 3);
        assert_eq!(chunks[1], "cd");
    }

    #[test]
    fn terminal_rules_and_table_rows_are_clipped() {
        assert_eq!(
            block_line_chunks("────────────────────", 8).collect::<Vec<_>>(),
            ["────────"]
        );
        let table = block_line_chunks("│ 磁盘 │ Data 426G / 926G │ 充裕 │", 16).collect::<Vec<_>>();
        assert_eq!(table.len(), 1);
        assert!(weft_core::grid::terminal_text_width(&table[0]) <= 16);
    }

    #[test]
    fn column_aligned_ls_output_wraps_without_loss_or_continuation_indent() {
        let line = "CLAUDE.md          projects          safari-proxy-research.md";
        let chunks = block_line_chunks(line, 24).collect::<Vec<_>>();
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), line);
        assert!(chunks
            .iter()
            .skip(1)
            .all(|chunk| chunk.chars().next().map_or(true, |ch| !ch.is_whitespace())));
    }

    #[test]
    fn prose_and_double_sentence_spaces_still_reflow() {
        assert_eq!(block_line_chunks("ordinary terminal prose", 8).count(), 3);
        let prose = "First sentence.  Second sentence.  Third sentence continues";
        assert!(block_line_chunks(prose, 24).count() > 1);
    }
}
