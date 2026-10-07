// v1.13.1 (PLAN_v1.13.1_SETTINGS_WRAP §二.1): word-aware wrapping for
// settings-panel free-standing text lines (help / status / diagnostics).
// Lives in its own module because paint/text.rs sits near the 800-line
// architecture gate and cannot absorb the implementation plus its tests.
//! Wrapping contract (plan, reviewer-pinned): the atom is a grapheme
//! (`unicode_segmentation`, same as `push_text`) and the width metric is
//! [`weft_core::grid::terminal_text_width`] — the exact function `push_text`
//! uses per grapheme (text.rs:97). Wrapping width and draw width can never
//! disagree. `terminal_char_width` is deliberately NOT used: its emoji
//! modifier / regional-indicator special cases are per-char, not per-grapheme,
//! and would drift from the drawn cells.

use unicode_segmentation::UnicodeSegmentation;

use crate::renderer::MetalRenderer;

/// Greedy word wrap on display columns. Returns the wrapped segments; empty
/// or all-whitespace input yields one empty segment; `max_cols == 0` yields
/// no segments. Words longer than `max_cols` hard-split at grapheme
/// boundaries (a single grapheme wider than `max_cols` still gets its own
/// segment — `push_text` clips it, the degenerate `max_cols == 1` + CJK case
/// has no better answer). Separators between words collapse to one column
/// inside a line and are dropped at wrap points.
pub(crate) fn wrap_text_cols(text: &str, max_cols: usize) -> Vec<&str> {
    if max_cols == 0 {
        return Vec::new();
    }
    let mut lines: Vec<&str> = Vec::new();
    // Pending line = [start, end) byte range + its display column count.
    let mut start = 0usize;
    let mut end = 0usize;
    let mut cols = 0usize;
    for word in text.split_whitespace() {
        // `word` borrows from `text`, so pointer subtraction yields its byte
        // offset — the only way split_whitespace exposes positions.
        let ws = word.as_ptr() as usize - text.as_ptr() as usize;
        let we = ws + word.len();
        // The separator between the pending line's last word and this one is
        // the raw slice text[end..ws] — it may be multiple spaces or a
        // full-width space, so it must be measured, not assumed 1 column
        // (segments are verbatim slices; accounting must match the draw).
        let sep_cols = if cols > 0 {
            weft_core::grid::terminal_text_width(&text[end..ws])
        } else {
            0
        };
        let word_cols: usize = word
            .graphemes(true)
            .map(weft_core::grid::terminal_text_width)
            .sum();
        if cols > 0 && cols + sep_cols + word_cols > max_cols {
            lines.push(&text[start..end]);
            cols = 0;
            // The flushed separator is dropped (wrap point).
        } else if cols > 0 {
            cols += sep_cols;
        }
        if word_cols <= max_cols {
            if cols == 0 {
                start = ws;
            }
            cols += word_cols;
            end = we;
        } else {
            // An overlong word cannot join the pending line — the wrap
            // condition above already flushed it (cols == 0 here). Hard-split
            // the word; its tail becomes the new pending line.
            let mut seg_start = ws;
            let mut seg_cols = 0usize;
            for grapheme in word.graphemes(true) {
                let gw = weft_core::grid::terminal_text_width(grapheme);
                let g_off = grapheme.as_ptr() as usize - text.as_ptr() as usize;
                if seg_cols > 0 && seg_cols + gw > max_cols {
                    lines.push(&text[seg_start..g_off]);
                    seg_start = g_off;
                    seg_cols = 0;
                }
                seg_cols += gw;
            }
            start = seg_start;
            end = we;
            cols = seg_cols;
        }
    }
    if cols > 0 {
        lines.push(&text[start..end]);
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

/// Vertical budget for a wrapped block starting at `y`: how many rows of
/// height `ch` fit strictly above `bottom`. `None` when not even one row
/// fits (callers skip drawing entirely — the pre-v1.13.1 guard shape).
pub(crate) fn line_budget(y: f32, bottom: f32, ch: f32) -> Option<usize> {
    if ch <= 0.0 || bottom - y < ch {
        return None;
    }
    Some(((bottom - y) / ch).floor() as usize)
}

impl MetalRenderer {
    /// Draws [`wrap_text_cols`] segments one cell height apart, clamped to
    /// `max_lines` rows when given. Returns the number of rows DRAWN —
    /// callers place subsequent content at `y + rows * ch`.
    #[allow(clippy::too_many_arguments)] // run geometry; mirrors push_text's arity
    pub(crate) fn push_text_wrapped(
        &self,
        vertices: &mut Vec<f32>,
        x: f32,
        y: f32,
        text: &str,
        fg: [f32; 4],
        max_cols: usize,
        max_lines: Option<usize>,
    ) -> usize {
        let segments = wrap_text_cols(text, max_cols);
        let visible = match max_lines {
            Some(n) => segments.len().min(n),
            None => segments.len(),
        };
        let ch = self.cell_height() as f32;
        for (i, segment) in segments.iter().take(visible).enumerate() {
            self.push_text(vertices, x, y + i as f32 * ch, segment, fg, max_cols);
        }
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(s: &str) -> usize {
        s.graphemes(true)
            .map(weft_core::grid::terminal_text_width)
            .sum()
    }

    #[test]
    fn ascii_wraps_on_word_boundaries() {
        assert_eq!(
            wrap_text_cols("alpha beta gamma", 10),
            vec!["alpha beta", "gamma"]
        );
        // Exactly-fitting lines never wrap.
        assert_eq!(wrap_text_cols("four", 4), vec!["four"]);
    }

    #[test]
    fn overlong_word_hard_splits_at_grapheme_boundaries() {
        assert_eq!(wrap_text_cols("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // The pending line is flushed before an overlong word starts.
        assert_eq!(
            wrap_text_cols("aa bbbbbbbbbb cc", 4),
            vec!["aa", "bbbb", "bbbb", "bb", "cc"]
        );
    }

    #[test]
    fn cjk_counts_double_columns_without_splitting_graphemes() {
        // 4 汉字 = 8 columns, max 4 → two per line, no byte-level splits.
        assert_eq!(wrap_text_cols("汉字测试", 4), vec!["汉字", "测试"]);
        // Mixed runs split between graphemes, never inside one.
        assert_eq!(wrap_text_cols("汉a汉a", 3), vec!["汉a", "汉a"]);
    }

    #[test]
    fn empty_and_zero_budget_edges() {
        assert_eq!(wrap_text_cols("", 5), vec![""]);
        assert_eq!(wrap_text_cols("   ", 5), vec![""]);
        assert!(wrap_text_cols("abc", 0).is_empty());
    }

    #[test]
    fn trailing_whitespace_produces_no_ghost_lines() {
        assert_eq!(wrap_text_cols("alpha ", 10), vec!["alpha"]);
        assert_eq!(wrap_text_cols("  alpha  beta ", 5), vec!["alpha", "beta"]);
    }

    #[test]
    fn every_segment_respects_the_column_budget() {
        // Metric-consistency pin: wrap width and push_text's draw width share
        // terminal_text_width, so every segment must fit its budget (the only
        // exemption is a lone grapheme wider than max_cols, impossible for
        // max_cols >= 2 because every grapheme is <= 2 columns).
        let corpus = [
            "Daily checks in the background; Off still allows manual checks.",
            "Connection failed: dial tcp 127.0.0.1:11434: connection refused",
            "汉字与 English 混排 wrap 的一致性检验文本",
            "Cmd+Shift+Click selects; Cmd+Option+Click safely opens.",
            // Separator runs are measured, not assumed 1 column (reviewer
            // P2-1): multi-space and full-width-space inputs must wrap.
            "ab  cd   ef",
            "ab\u{3000}cd",
            "two  words  with  double  spaces  everywhere  here",
        ];
        for text in corpus {
            for max_cols in 2..=24 {
                for segment in wrap_text_cols(text, max_cols) {
                    assert!(
                        width(segment) <= max_cols,
                        "{segment:?} ({} cols) exceeds budget {max_cols}",
                        width(segment)
                    );
                }
            }
        }
    }

    #[test]
    fn line_budget_requires_at_least_one_full_row() {
        assert_eq!(line_budget(10.0, 10.5, 1.0), None);
        assert_eq!(line_budget(10.0, 11.0, 1.0), Some(1));
        assert_eq!(line_budget(10.0, 13.9, 1.0), Some(3));
        assert_eq!(line_budget(10.0, 10.0, 1.0), None);
    }
}
