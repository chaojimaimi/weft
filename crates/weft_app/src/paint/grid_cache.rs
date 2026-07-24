//! Grid paint model: block layout cache + line wrapping (A5 / M4 step 1).
//!
//! Finished blocks have immutable `output` (it's a detached snapshot), so
//! the per-line wrapping computation only needs to run once per block —
//! unless `cols` changes (resize) or the block's content/collapse state
//! changes. This cache eliminates the O(total_output_chars) per-frame cost
//! reintroduced when the `MAX_LAYOUT_LINES` cap was removed from historical
//! blocks.
//!
//! The live in-flight block is NOT cached (its output streams every frame).

use std::collections::HashMap;
use std::rc::Rc;

use unicode_segmentation::UnicodeSegmentation;
use weft_core::blocks::Block;

pub(crate) const MAX_LAYOUT_LINES_LIVE: usize = 2000;

pub(crate) fn trimmed_output_line_count(lines: &[&str]) -> usize {
    let mut len = lines.len();
    while len > 0 {
        let text = lines[len - 1].trim();
        if text.is_empty() || matches!(text, "%" | "$" | "#") {
            len -= 1;
        } else {
            break;
        }
    }
    len
}

/// Iterator yielding wrapped row chunks of `text` at `cols` columns. Each
/// yielded `String` fits within `cols` columns (respecting wide-char widths).
/// The first yielded chunk is the top row, subsequent chunks are continuation
/// rows below it.
pub(crate) fn wrap_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    let mut chunks: Vec<String> = Vec::new();
    if cols == 0 {
        chunks.push(text.to_string());
        return chunks.into_iter();
    }
    let mut current = String::new();
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let w = weft_core::grid::terminal_text_width(grapheme);
        if w == 0 {
            continue;
        }
        if col + w > cols {
            chunks.push(std::mem::take(&mut current));
            col = 0;
        }
        current.push_str(grapheme);
        col += w;
    }
    chunks.push(current);
    chunks.into_iter()
}

/// Block history normally reflows prose, but terminal-drawn structure must
/// retain its row identity. Wrapping a full-width rule produces several
/// identical prompt dividers after a resize; wrapping a table row detaches
/// cells from their border. Keep those rows atomic and clip them to the
/// current viewport instead.
pub(crate) fn block_line_chunks(text: &str, cols: usize) -> impl Iterator<Item = String> {
    if !is_terminal_structure_line(text) || cols == 0 {
        return wrap_line_chunks(text, cols).collect::<Vec<_>>().into_iter();
    }

    let mut clipped = String::new();
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let width = weft_core::grid::terminal_text_width(grapheme);
        if width == 0 {
            continue;
        }
        if col + width > cols {
            break;
        }
        clipped.push_str(grapheme);
        col += width;
    }
    vec![clipped].into_iter()
}

fn is_terminal_structure_line(text: &str) -> bool {
    let mut visible = 0usize;
    let mut box_drawing = 0usize;
    let mut vertical_separators = 0usize;
    for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
        visible += 1;
        if matches!(ch, '\u{2500}'..='\u{257f}') {
            box_drawing += 1;
        }
        if matches!(ch, '│' | '┃' | '║' | '┆' | '┇' | '┊' | '┋') {
            vertical_separators += 1;
        }
    }
    (visible >= 8 && box_drawing == visible) || vertical_separators >= 2
}

/// Pre-computed wrapping data for a single output line of a block.
#[derive(Clone)]
pub(crate) struct CachedLine {
    /// 0-based line index within the block's output (before trimming).
    pub(crate) idx: usize,
    /// Byte offset of this line's start within `block.output`.
    pub(crate) byte_start: usize,
    /// Byte offset of this line's end (exclusive) within `block.output`.
    pub(crate) byte_end: usize,
    /// Pre-wrapped chunks (owned via `Rc` for cheap sharing between the
    /// cache and the per-frame `LaidRow` entries). Usually 1 element;
    /// more for lines that exceed `cols` columns.
    pub(crate) chunks: Rc<[String]>,
}

/// Cached layout for a single finished block.
#[derive(Clone)]
pub(crate) struct CachedBlockLayout {
    /// Snapshot of `block.output.len()` — if the current block's output
    /// length differs, the cache is stale.
    pub(crate) output_len: usize,
    /// Identity of the immutable output allocation. Continuation updates can
    /// replace a block's output with equal-length text while retaining its
    /// BlockId; length alone would then reuse stale byte ranges and chunks.
    pub(crate) output_identity: usize,
    /// Snapshot of `block.command.len()`.
    pub(crate) command_len: usize,
    /// Snapshot of `block.collapsed` — toggling invalidates.
    pub(crate) collapsed: bool,
    /// `cols` used to compute wrapping — resize invalidates.
    pub(crate) cols: usize,
    /// Whether the block has any non-empty output lines (cached foldable
    /// check, avoids re-scanning the last 500 lines every frame).
    pub(crate) foldable: bool,
    /// Pre-trimmed, pre-wrapped line metadata. Trailing empty/prompt lines
    /// are already removed, matching the original trimming logic.
    pub(crate) lines: Vec<CachedLine>,
    /// R2-2: cached wrapped output row count (sum of chunks.len() across
    /// all lines + command resume hints). Excludes the header/gap/spacer
    /// because those depend on `header_rows` and `viewport_rows` which are
    /// per-frame parameters, not per-block cache keys. This lets
    /// `block_content_metrics` skip the O(m) re-wrap per block and read O(1).
    pub(crate) output_rows: usize,
}

/// Per-renderer block layout cache. Keyed by `BlockId.0`.
#[derive(Default)]
pub(crate) struct BlockLayoutCache {
    entries: HashMap<u64, CachedBlockLayout>,
    /// Step 1: cache hit/miss counters for frame_trace observability.
    /// A "hit" is an `ensure_cached` call that found a fresh entry;
    /// a "miss" is one that triggered a rebuild. Reset by
    /// `take_hit_miss_counts` at the end of each frame so the trace
    /// reports per-frame deltas, not cumulative totals.
    hits: usize,
    misses: usize,
}

impl BlockLayoutCache {
    /// Ensure `block` has a cached layout for `cols`. Recomputes only if
    /// the block is new, its output/command changed, `collapsed` was
    /// toggled, or `cols` changed (resize).
    pub(crate) fn ensure_cached(&mut self, block: &Block, cols: usize) {
        let id = block.id.0;
        let needs_rebuild = match self.entries.get(&id) {
            None => true,
            Some(c) => {
                c.output_len != block.output.len()
                    || c.output_identity != block.output.as_ptr() as usize
                    || c.command_len != block.command.len()
                    || c.collapsed != block.collapsed
                    || c.cols != cols
            }
        };
        if needs_rebuild {
            self.misses += 1;
            self.entries.insert(id, compute_block_layout(block, cols));
        } else {
            self.hits += 1;
        }
    }

    pub(crate) fn get(&self, id: u64) -> &CachedBlockLayout {
        self.entries
            .get(&id)
            .expect("ensure_cached must be called before get")
    }

    /// R2-2: like `get` but returns `None` for uncached blocks instead of
    /// panicking. Used by `block_content_metrics_with_cache` to fall back
    /// to direct computation when a block hasn't been cached yet (e.g. the
    /// block was finalized between the last paint and the scrollbar layout).
    pub(crate) fn get_if_cached(&self, id: u64) -> Option<&CachedBlockLayout> {
        self.entries.get(&id)
    }

    /// Step 1: drain the per-frame hit/miss counters for frame_trace.
    /// Returns `(hits, misses)` accumulated since the last call and resets
    /// the internal accumulators. Call this once per frame at build_end.
    pub(crate) fn take_hit_miss_counts(&mut self) -> (usize, usize) {
        let h = self.hits;
        let m = self.misses;
        self.hits = 0;
        self.misses = 0;
        (h, m)
    }
}

/// Compute the layout for a single block (expensive — call once, then cache).
fn compute_block_layout(block: &Block, cols: usize) -> CachedBlockLayout {
    // Foldable: does the block have ANY non-empty output line in the last 500?
    let foldable = block
        .output
        .lines()
        .rev()
        .take(500)
        .any(|l| !l.trim().is_empty());

    // Collect raw lines and trim trailing empty/prompt lines.
    let raw_lines: Vec<&str> = block.output.lines().collect();
    let trimmed_len = trimmed_output_line_count(&raw_lines);

    // Pre-compute wrapped chunks for each surviving line.
    let lines: Vec<CachedLine> = raw_lines[..trimmed_len]
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let byte_start = line.as_ptr() as usize - block.output.as_ptr() as usize;
            let byte_end = byte_start + line.len();
            let chunks: Rc<[String]> = Rc::from(block_line_chunks(line, cols).collect::<Vec<_>>());
            CachedLine {
                idx,
                byte_start,
                byte_end,
                chunks,
            }
        })
        .collect();

    // R2-2: cache the wrapped output row count so block_content_metrics can
    // read O(1) instead of re-wrapping every block every frame. This sums
    // command resume hints (static strings, opencode-only) + visible output
    // lines — exactly what completed_block_output_rows computes, minus the
    // per-frame header_rows/viewport_rows terms that don't belong in the cache.
    // Collapsed blocks report 0 output rows (matching completed_block_output_rows)
    // even though `lines` is still populated for foldable detection / uncollapse.
    let output_rows = if block.collapsed {
        0
    } else {
        let hint_rows: usize = crate::block_component::command_resume_hints(block)
            .iter()
            .map(|hint| block_line_chunks(hint, cols).count())
            .sum();
        let line_rows: usize = lines.iter().map(|l| l.chunks.len()).sum();
        hint_rows + line_rows
    };

    CachedBlockLayout {
        output_len: block.output.len(),
        output_identity: block.output.as_ptr() as usize,
        command_len: block.command.len(),
        collapsed: block.collapsed,
        cols,
        foldable,
        lines,
        output_rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::blocks::{Block, BlockId};

    fn mk_block_with_output(id: u64, command: &str, output: &str) -> Block {
        Block {
            id: BlockId(id),
            command: command.to_string(),
            cwd: None,
            output: output.into(),
            styled_output: None,
            exit_code: None,
            started_at: std::time::SystemTime::UNIX_EPOCH,
            finished_at: None,
            collapsed: false,
        }
    }

    #[test]
    fn block_layout_cache_computes_on_first_access() {
        let block = mk_block_with_output(1, "echo hello", "hello\nworld\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(layout.lines[0].idx, 0);
        assert_eq!(layout.lines[1].idx, 1);
        assert!(layout.foldable);
    }

    #[test]
    fn block_layout_cache_trims_trailing_empty() {
        let block = mk_block_with_output(1, "echo", "output\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing empty lines should be trimmed"
        );
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "output"
        );
    }

    #[test]
    fn block_layout_cache_trims_trailing_prompt() {
        let block = mk_block_with_output(1, "echo", "output\n%\n$\n#\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(
            layout.lines.len(),
            1,
            "trailing prompt lines should be trimmed"
        );
    }

    #[test]
    fn block_layout_cache_byte_offsets_correct() {
        let block = mk_block_with_output(1, "echo", "first\nsecond\nthird\n");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(
            &block.output[layout.lines[0].byte_start..layout.lines[0].byte_end],
            "first"
        );
        assert_eq!(
            &block.output[layout.lines[1].byte_start..layout.lines[1].byte_end],
            "second"
        );
        assert_eq!(
            &block.output[layout.lines[2].byte_start..layout.lines[2].byte_end],
            "third"
        );
    }

    #[test]
    fn block_layout_cache_wraps_long_lines() {
        // 20 chars at cols=10 → 2 chunks
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        let layout = compute_block_layout(&block, 10);
        assert_eq!(layout.lines.len(), 1);
        assert_eq!(
            layout.lines[0].chunks.len(),
            2,
            "20 chars at cols=10 → 2 chunks"
        );
        assert_eq!(layout.lines[0].chunks[0], "0123456789");
        assert_eq!(layout.lines[0].chunks[1], "abcdefghij");
    }

    #[test]
    fn wrapping_keeps_emoji_grapheme_clusters_atomic() {
        let chunks: Vec<_> = wrap_line_chunks("A👩‍🔬B", 3).collect();
        assert_eq!(chunks, ["A👩‍🔬", "B"]);
    }

    #[test]
    fn terminal_rule_is_clipped_instead_of_wrapped_after_resize() {
        let chunks: Vec<_> = block_line_chunks("────────────────────", 8).collect();
        assert_eq!(chunks, ["────────"]);
    }

    #[test]
    fn unicode_table_row_is_clipped_instead_of_split_after_resize() {
        let chunks: Vec<_> = block_line_chunks("│ 磁盘 │ Data 426G / 926G │ 充裕 │", 16).collect();
        assert_eq!(chunks.len(), 1);
        assert!(weft_core::grid::terminal_text_width(&chunks[0]) <= 16);
    }

    #[test]
    fn prose_still_wraps_after_resize() {
        let chunks: Vec<_> = block_line_chunks("ordinary terminal prose", 8).collect();
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn block_layout_cache_foldable_false_for_empty_output() {
        let block = mk_block_with_output(1, "true", "\n\n\n");
        let layout = compute_block_layout(&block, 80);
        assert!(!layout.foldable, "all-empty output should not be foldable");
        assert_eq!(layout.lines.len(), 0, "all lines trimmed");
    }

    #[test]
    fn block_layout_cache_ensure_cached_reuses() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        let layout1 = cache.get(1).clone();

        // Same content + cols → should NOT rebuild (same instance).
        cache.ensure_cached(&block, 80);
        let layout2 = cache.get(1).clone();
        assert_eq!(layout1.lines.len(), layout2.lines.len());
        assert_eq!(layout1.cols, layout2.cols);
    }

    #[test]
    fn block_layout_cache_rebuilds_on_output_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert_eq!(cache.get(1).lines.len(), 1);

        // Output grew → cache should detect and rebuild.
        let block2 = mk_block_with_output(1, "echo", "hello\nworld\n");
        cache.ensure_cached(&block2, 80);
        assert_eq!(
            cache.get(1).lines.len(),
            2,
            "output change should trigger rebuild"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_for_equal_length_replacement() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "abcdef\n");
        cache.ensure_cached(&block, 80);
        assert_eq!(cache.get(1).lines[0].byte_end, 6);

        // Same BlockId and byte length, but a different immutable allocation
        // and UTF-8 boundary. Reusing the old byte range could panic while
        // slicing the replacement output.
        let replacement = mk_block_with_output(1, "echo", "中文\n");
        assert_eq!(block.output.len(), replacement.output.len());
        cache.ensure_cached(&replacement, 80);
        let line = &cache.get(1).lines[0];
        assert_eq!(&replacement.output[line.byte_start..line.byte_end], "中文");
    }

    #[test]
    fn block_layout_cache_rebuilds_on_cols_change() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "0123456789abcdefghij");
        cache.ensure_cached(&block, 10);
        assert_eq!(
            cache.get(1).lines[0].chunks.len(),
            2,
            "20 chars / cols=10 → 2 chunks"
        );

        // Resize to cols=20 → should rebuild with 1 chunk.
        cache.ensure_cached(&block, 20);
        assert_eq!(
            cache.get(1).lines[0].chunks.len(),
            1,
            "20 chars / cols=20 → 1 chunk"
        );
    }

    #[test]
    fn block_layout_cache_rebuilds_on_collapse_toggle() {
        let mut cache = BlockLayoutCache::default();
        let block = mk_block_with_output(1, "echo", "hello\n");
        cache.ensure_cached(&block, 80);
        assert!(!cache.get(1).collapsed);

        let mut block2 = block.clone();
        block2.collapsed = true;
        cache.ensure_cached(&block2, 80);
        assert!(
            cache.get(1).collapsed,
            "collapse toggle should trigger rebuild"
        );
    }

    #[test]
    fn block_layout_cache_empty_output() {
        let block = mk_block_with_output(1, "true", "");
        let layout = compute_block_layout(&block, 80);
        assert_eq!(layout.lines.len(), 0);
        assert!(!layout.foldable);
    }
}
