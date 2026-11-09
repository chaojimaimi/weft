//! Running-command caret column helpers for the block-view formula-fallback
//! path (renderer `block_view_tui_cursor`, `None` branch).
//!
//! v1.13.5 T16a: extracted from ui_helpers (that file sat exactly on its
//! 800-line budget) and reworked — the old entry paid two O(capture) scans
//! per frame (`lines().count()` + `lines().nth()`); the tail entry the
//! renderer now calls scans ONLY the tail line, with the line index coming
//! from the tracker's O(1) newline ledger.

/// Caret column for line `line` of `output` (index form; test-only since
/// T16a — production anchors the tail via `tui_cursor_tail_display_col`).
///
/// The grid cursor is UNTRUSTWORTHY on this path: redraw-style progress
/// lines (brew) end every tick with CHA 0 / CPL — PTY capture `0G`×12,
/// `1F`×13, `C`×0, neither counted as cursor_ops — parking it at col 0 /
/// the previous line's start, and the 1:1 line map breaks on wrapped rows.
/// So the grid cursor is never read: anchor the end of `line`'s content;
/// the capture's rewrite compaction makes its tail == newest frame end ==
/// the caret position for every non-TUI command.
#[cfg(test)]
pub(crate) fn tui_cursor_display_col(output: &str, line: usize, cols: usize) -> usize {
    match output.lines().nth(line) {
        Some(text) => display_col_of_line_text(text, cols),
        None => 0,
    }
}

/// v1.13.5 T16a: caret column for the capture TAIL line — the only line the
/// formula fallback ever anchors. O(tail line) instead of two O(capture)
/// passes; the line index itself comes from the tracker's O(1) newline
/// ledger (`BlockTracker::live_cursor_tail_line`).
pub(crate) fn tui_cursor_tail_display_col(output: &str, cols: usize) -> usize {
    // rust-reviewer P1: a capture ending in '\n' (the near-universal
    // streaming shape) must anchor like the old `lines().nth(last)` — the
    // last COMPLETE line's end — not an empty phantom row at col 0 (which
    // would park the caret quad on top of that line's first glyph).
    let text = match output.strip_suffix('\n').unwrap_or(output).rfind('\n') {
        Some(i) => &output[i + 1..],
        None => output,
    };
    display_col_of_line_text(text, cols)
}

/// Width math shared by the two caret-column entries: the wrapped-chunk
/// column of the caret parked at `text`'s end (see `tui_cursor_display_col`'s
/// contract above).
///
/// Single row (incl. the Gauge clip band, ≤ cols +
/// PROGRESS_GAUGE_CLIP_TOLERANCE wide): end == cols is legal, paint parks
/// the caret at the grid's right edge; k > 1 still clamps to cols-1
/// (wrap_pending). Multi-row: `(k-1) * cols + last_chunk_width` hits the
/// last chunk's last cell exactly under the modulo map (chunk_idx ==
/// col / cols). Per-char width sum, exact for regular glyphs, same
/// convention as `block_view_line_end_col` (VS16/emoji clustering ±1 cell).
fn display_col_of_line_text(text: &str, cols: usize) -> usize {
    let w = |ch: char| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    let chunks: Vec<String> = crate::paint::grid_cache::block_line_chunks(text, cols).collect();
    // Last-chunk width == whole-line width when k <= 1 (single chunk / empty line).
    let last = match chunks.last() {
        Some(c) => c.chars().map(w).sum::<usize>(),
        None => 0,
    };
    match chunks.len() {
        0 | 1 => last.min(cols),
        k => (k - 1) * cols + last.min(cols.saturating_sub(1)),
    }
}

/// v1.18.9 B1' (PLAN_v1189 §2): wrap-pending clamp for the capture-cursor
/// anchor path ONLY. When the caret column is exactly the tail line's full
/// width AND that width is an exact multiple of `cols` (a multi-chunk row
/// whose end lands on a wrap boundary), the painter's chunk map
/// (`chunk_idx == cursor_col / cols` @ output_paint.rs:178) addresses a
/// nonexistent chunk `k` — the caret quad vanishes — while the real
/// terminal sits wrap-pending: its visual caret is the LAST cell of the
/// previous visual row. Clamp one cell back in exactly that shape;
/// everything else passes through bare:
/// - `k == 1` full-width row (width == cols): bare — T16c right-edge park
///   (the whole-line painter path clamps it inside the right edge);
/// - mid-row boundary (col % cols == 0, col < line_width): the row really
///   folded; the painter maps to the next visual row, col 0 — the true
///   position;
/// - ordinary columns: bare.
///
/// MUST NOT be wired into the formula path: `display_col_of_line_text`'s
/// single-chunk `cols` semantics are pinned by T16c geometry and the IME
/// preedit clamp.
pub(crate) fn wrap_pending_clamp(col: usize, cols: usize, line_width: usize) -> usize {
    if col > 0 && col == line_width && line_width > cols && col % cols == 0 {
        col - 1
    } else {
        col
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_cursor_wrapped_row_anchors_last_chunk_end() {
        assert_eq!(tui_cursor_display_col(&"x".repeat(180), 0, 110), 180);
    }

    #[test]
    fn tui_cursor_exact_multiple_anchors_last_cell() {
        assert_eq!(tui_cursor_display_col(&"x".repeat(220), 0, 110), 219);
    }

    #[test]
    fn tui_cursor_gauge_clip_band_parks_at_right_edge() {
        // bar+text mix → ProgressGauge (103 ≥ 4 block chars); 113 ≤ 110+3 → single chunk → parks at the right edge.
        let out = format!("{} 42% 1m59s", "█".repeat(103));
        assert_eq!(tui_cursor_display_col(&out, 0, 110), 110);
    }

    #[test]
    fn tui_cursor_cjk_wrap_anchors_content_end() {
        assert_eq!(tui_cursor_display_col(&"中".repeat(60), 0, 110), 120);
    }

    #[test]
    fn tui_cursor_single_chunk_short_line_anchors_at_content_end() {
        assert_eq!(tui_cursor_display_col("short line", 0, 80), 10);
    }

    #[test]
    fn tui_cursor_full_line_parks_at_right_edge() {
        assert_eq!(tui_cursor_display_col(&"x".repeat(80), 0, 80), 80);
    }

    #[test]
    fn tui_cursor_empty_line_anchors_col_zero() {
        assert_eq!(tui_cursor_display_col("a\n\nb", 1, 80), 0);
    }

    #[test]
    fn tui_cursor_out_of_bounds_line_anchors_col_zero() {
        assert_eq!(tui_cursor_display_col("abc", 5, 80), 0);
    }

    /// T16a: the tail entry agrees with the index entry on the LAST line —
    /// both anchor the same content end (reviewer P1: the trailing-newline
    /// form is pinned too — the old nth(last) anchored the last COMPLETE
    /// line's end, so the tail entry must too).
    #[test]
    fn tail_entry_matches_index_entry_on_last_line() {
        for (output, cols) in [
            ("one\ntwo", 80),
            ("a\n\nbbbb", 3),
            ("wrap".repeat(60).as_str(), 110),
            ("中".repeat(40).as_str(), 110),
            ("one\ntwo\n", 80),
            ("abc\n", 80),
        ] {
            let line = output.lines().count().saturating_sub(1);
            assert_eq!(
                tui_cursor_tail_display_col(output, cols),
                tui_cursor_display_col(output, line, cols),
                "tail mismatch for {output:?}"
            );
        }
    }

    /// Empty capture anchors col 0; a trailing newline anchors the last
    /// COMPLETE line's content end (reviewer P1 semantics — the caret must
    /// sit after that line's glyphs, not on top of its first one).
    #[test]
    fn tail_entry_handles_trailing_newline() {
        assert_eq!(tui_cursor_tail_display_col("abc\n", 80), 3);
        assert_eq!(tui_cursor_tail_display_col("", 80), 0);
        assert_eq!(tui_cursor_tail_display_col("\n\n", 80), 0);
    }

    /// v1.18.9 B1' (PLAN_v1189 §2 T5b): wrap_pending_clamp truth table —
    /// only the exact-multiple multi-chunk row END clamps one cell back
    /// (wrap-pending); k=1 full rows, mid-row boundaries, and ordinary
    /// columns all pass through bare.
    #[test]
    fn wrap_pending_clamp_only_bites_on_exact_multi_chunk_row_end() {
        // The clamp shape: caret at a multi-chunk row's full width, width an
        // exact multiple of cols → painter would address a nonexistent chunk
        // k → park one cell back (the terminal sits wrap-pending).
        assert_eq!(wrap_pending_clamp(220, 110, 220), 219);
        assert_eq!(wrap_pending_clamp(240, 80, 240), 239);
        // k=1 exact full row: bare (T16c right-edge park; gate needs > cols).
        assert_eq!(wrap_pending_clamp(80, 80, 80), 80);
        // Mid-row boundary (row really folded, col 0 of next visual row):
        // bare — the modulo map is exact there.
        assert_eq!(wrap_pending_clamp(110, 110, 220), 110);
        // Non-multiple row end: bare.
        assert_eq!(wrap_pending_clamp(180, 110, 180), 180);
        // Ordinary mid-row columns: bare.
        assert_eq!(wrap_pending_clamp(37, 110, 180), 37);
        // Empty line at col 0: bare.
        assert_eq!(wrap_pending_clamp(0, 80, 0), 0);
    }
}
