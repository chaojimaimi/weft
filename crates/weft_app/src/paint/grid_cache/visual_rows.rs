//! M5-a (PLAN_M5 §二/§五): L1/L2 layered block-layout tables plus THE shared
//! word-aware wrap state machine.
//!
//! Layering (mirrors Warp's CharCellTextIndex split, with our semantics):
//! - L1 [`ContentTable`] — content layer, keyed by output identity: one
//!   [`GEntry`] per grapheme cluster (byte offset / terminal width / flags)
//!   plus per-surviving-line [`LMeta`]. Built once per output, never sees
//!   `cols`.
//! - L2 [`WidthTable`] — width layer, keyed by `cols`: one [`VisualRow`] per
//!   visual row (line, cluster window, byte length), a `line_row_base`
//!   prefix per surviving line, and `hint_rows` for command resume hints.
//!   `hint_rows + rows.len()` is the O(1) output-row read (PLAN_M5 §三).
//!
//! Zero-drift guarantee (G4): the word-aware wrap machine below is the ONLY
//! implementation — `paint/grid_cache/wrapping.rs` feeds it from the raw
//! text iterator ([`text_graphemes`]), the L2 rebuild feeds it from the L1
//! tables. There is no second copy of the state machine to drift.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use weft_core::blocks::Block;

use super::wrapping::{classify_structure_line, StructureKind, PROGRESS_GAUGE_CLIP_TOLERANCE};

// ── Shared wrap state machine (extracted verbatim from wrapping.rs) ─────

/// One grapheme cluster as the state machine sees it: a byte span, its
/// terminal display width ([`weft_core::grid::terminal_text_width`] on the
/// WHOLE cluster — flag / emoji-modifier clusters differ from per-char
/// sums) and whether every `char` in the cluster is whitespace.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphemeInfo {
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) width: u8,
    pub(crate) is_whitespace: bool,
}

/// Line-terminal wrap/clip policy. `Structure` carries the verbatim
/// `classify_structure_line` result — a `None` classification IS the prose
/// soft-wrap path (matching `block_line_chunk_ranges`' decision structure);
/// `ScreenOrigin` is the hard-clip variant for primary-screen TUI rows
/// (v1.10.26 B-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapMode {
    /// Structure line: `None` soft-wraps as prose; PureBox / TableRow clip;
    /// ProgressGauge keeps the cols+3 tolerance branch.
    Structure(StructureKind),
    /// Screen-origin (TUI frame) row — always a single right-clipped chunk.
    ScreenOrigin,
}

/// Enumerate `text`'s extended grapheme clusters as [`GraphemeInfo`] — the
/// legacy path's data source. Same call sites as wrapping.rs:
/// `grapheme_indices(true)` + `terminal_text_width(整簇)`.
pub(crate) fn text_graphemes(text: &str) -> impl Iterator<Item = GraphemeInfo> + '_ {
    text.grapheme_indices(true)
        .map(|(byte, grapheme)| GraphemeInfo {
            byte_start: byte,
            byte_end: byte + grapheme.len(),
            width: grapheme_width(grapheme),
            is_whitespace: grapheme.chars().all(char::is_whitespace),
        })
}

/// Terminal display width of one whole cluster, clamped to u8 (a real
/// cluster never exceeds width 2; saturation beats a panic on garbage).
pub(crate) fn grapheme_width(grapheme: &str) -> u8 {
    u8::try_from(weft_core::grid::terminal_text_width(grapheme)).unwrap_or(u8::MAX)
}

/// THE word-aware wrap machine (v1.11.10 semantics, extracted verbatim from
/// the old `wrap_line_chunk_ranges`). Wraps the cluster stream into
/// contiguous byte ranges whose terminal display width fits `cols`, keeping
/// graphemes atomic (CJK counts 2 columns):
///
/// - word-aware breaks: an overflowing trailing word (a run of
///   non-whitespace after the last whitespace) that fits alone on the next
///   line moves there whole instead of being split mid-word;
/// - whitespace clusters terminate the trailing word (P1-2) and are absorbed
///   into the chunk before the break — a chunk's DISPLAYED width may exceed
///   `cols` (trailing overflow whitespace, P2-1); consumers must not assume
///   row width ≤ cols;
/// - zero-width clusters ride along byte-wise: they never consume a column
///   and never touch the word state;
/// - a single cluster wider than `cols` (CJK @ cols=1) hard-cuts via the
///   `byte > start` guard instead of pushing an empty range (P1-1).
///
/// `Structure` / `ScreenOrigin` modes reproduce `block_line_chunk_ranges` /
/// `screen_origin_line_chunk_ranges` decision prefixes exactly, including
/// the ProgressGauge `cols + 3` clip tolerance and the cols == 0
/// whole-line degenerate branch.
pub(crate) fn wrap_line_ranges(
    entries: &[GraphemeInfo],
    cols: usize,
    mode: WrapMode,
) -> Vec<Range<usize>> {
    match mode {
        WrapMode::Structure(kind) => {
            if cols == 0 || kind == StructureKind::None {
                return prose_wrap_ranges(entries, cols);
            }
            if kind == StructureKind::ProgressGauge {
                // Progress gauges (ollama pull, brew upgrade, …) carry ETA /
                // speed / size info that users need to see. A *small* overflow
                // (≤ 3 cols) is the common off-by-few case where a program
                // emits cols+N chars due to ambiguous-width accounting;
                // clipping avoids a dangling 1-2 char tail ("9s") on the next
                // row. A *large* overflow means the window was narrowed (or
                // the program hasn't caught up to the new SIGWINCH); wrapping
                // preserves the trailing ETA/speed so the user can still read
                // it instead of seeing it truncated.
                //
                // Cluster widths are additive (whole-line
                // terminal_text_width == Σ per-cluster widths, verified over
                // flags/ZWJ/VS16/skin-tone input), so the table sum equals
                // the legacy whole-line measurement exactly.
                let total_width: usize = entries.iter().map(|e| usize::from(e.width)).sum();
                if total_width > cols.saturating_add(PROGRESS_GAUGE_CLIP_TOLERANCE) {
                    return prose_wrap_ranges(entries, cols);
                }
            }
            std::iter::once(0..grapheme_prefix_end(entries, cols)).collect()
        }
        WrapMode::ScreenOrigin => {
            if cols == 0 {
                // v1.10.26 Batch B review nit: cols == 0 must be the WHOLE
                // line as one chunk, aligned with wrap semantics.
                return std::iter::once(0..line_byte_len(entries)).collect();
            }
            std::iter::once(0..grapheme_prefix_end(entries, cols)).collect()
        }
    }
}

fn prose_wrap_ranges(entries: &[GraphemeInfo], cols: usize) -> Vec<Range<usize>> {
    if cols == 0 {
        return std::iter::once(0..line_byte_len(entries)).collect();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    // v1.11.10 (M-A): trailing-word state — byte offset where the current
    // word started, and the chunk-relative column it started at.
    let mut word_start: Option<usize> = None;
    let mut col_at_word_start: Option<usize> = None;
    for entry in entries {
        let width = usize::from(entry.width);
        if width == 0 {
            continue;
        }
        if entry.is_whitespace {
            // Overflowing or not, whitespace always terminates the trailing
            // word (P1-2): a word ending exactly at the column boundary
            // followed by absorbed overflow spaces must not keep its word
            // state, or the next short word would be cut at the wrong
            // point (`zz aaaaaaa␣␣XY` @ cols=10 → break must land at "XY").
            col += width;
            word_start = None;
            col_at_word_start = None;
            continue;
        }
        if word_start.is_none() {
            word_start = Some(entry.byte_start);
            col_at_word_start = Some(col);
        }
        if col + width > cols {
            // Overflow. Word-aware fallback decision:
            if let (Some(ws), Some(c0)) = (word_start, col_at_word_start) {
                // The whole word moved to the next line must fit there alone.
                // checked_sub: a hard cut below leaves the word state stale
                // (col resets to 0 while c0 does not), so col < c0 is
                // reachable. The legacy arithmetic underflowed there (debug
                // panic — latent in v1.12.2, exposed by the M5-a property
                // test on e.g. "中 中a" @ cols=1; release wrapped into
                // garbage that almost always failed the fit check). Treat
                // the corrupted state as not-fitting → hard cut, identical
                // to legacy wherever col >= c0 (the defined domain).
                let word_fits_alone = col
                    .checked_sub(c0)
                    .is_some_and(|word_cols| word_cols + width <= cols);
                if ws > start && word_fits_alone {
                    // chunk1 keeps everything through the trailing padding
                    // whitespace; the word (including the part already past
                    // `cols` and the current grapheme) opens the next chunk.
                    ranges.push(start..ws);
                    start = ws;
                    // New chunk's current column = the width the word has
                    // already consumed. The word is now the chunk head, so
                    // col_at_word_start becomes 0 — a stale pre-fallback
                    // column must not leak into later overflow decisions.
                    col -= c0;
                    col_at_word_start = Some(0);
                    // No continue: this grapheme counts into the new chunk
                    // via `col += width` below.
                } else {
                    // Character hard cut — the `byte > start` guard stays
                    // (P1-1): a single grapheme wider than cols (CJK w=2 @
                    // cols=1) must not push an empty range; advance col only,
                    // the next grapheme breaks.
                    if entry.byte_start > start {
                        ranges.push(start..entry.byte_start);
                        start = entry.byte_start;
                        col = 0;
                    }
                }
            } else if entry.byte_start > start {
                ranges.push(start..entry.byte_start);
                start = entry.byte_start;
                col = 0;
            }
        }
        col += width;
    }
    ranges.push(start..line_byte_len(entries));
    ranges
}

/// Longest grapheme-atomic prefix whose terminal display width fits `cols`;
/// returns the prefix's end byte index. Shared prefix math for the
/// structural clip and the screen-origin clip (extracted from wrapping.rs'
/// `grapheme_prefix_end`). Zero-width clusters advance the end without
/// consuming columns. `cols == 0` is handled by the call sites.
fn grapheme_prefix_end(entries: &[GraphemeInfo], cols: usize) -> usize {
    let mut end = 0usize;
    let mut col = 0usize;
    for entry in entries {
        let width = usize::from(entry.width);
        if width > 0 && col + width > cols {
            break;
        }
        col += width;
        end = entry.byte_end;
    }
    end
}

fn line_byte_len(entries: &[GraphemeInfo]) -> usize {
    entries.last().map_or(0, |e| e.byte_end)
}

// ── L1 content layer (key = output identity + len) ──────────────────────

/// Cluster-wide all-whitespace flag (the wrap machine's blank test).
pub(crate) const IS_WHITESPACE: u8 = 1 << 0;
/// Marks an ASCII TAB cluster: zero-width under the grid's
/// `terminal_char_width` measure. The wrap machine measures it via
/// `terminal_text_width` like any cluster — no tab expansion anywhere.
pub(crate) const IS_TAB_ZERO_W: u8 = 1 << 1;

/// One grapheme cluster of a surviving output line: 8 bytes (u32 + u8 +
/// u8 + padding), ~6.4MB for an 800k-cluster block — same order as Warp's
/// CharCellTextIndex (PLAN_M5 §二 内存).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GEntry {
    /// Byte offset of the cluster within its LINE (line-relative, matching
    /// the legacy per-line chunk ranges; output-relative =
    /// `CachedLine::byte_start` + this).
    pub(crate) byte_offset: u32,
    /// `terminal_text_width(整簇)` — never a per-char sum.
    pub(crate) width: u8,
    /// IS_WHITESPACE / IS_TAB_ZERO_W.
    pub(crate) flags: u8,
}

/// Per-surviving-source-line metadata. "Surviving" = after the SAME trailing
/// trim as `trimmed_output_line_count` (single source, applied at build).
#[derive(Clone, Copy, Debug)]
pub(crate) struct LMeta {
    /// Index of the line's first cluster in [`ContentTable::graphemes`].
    pub(crate) grapheme_start: u32,
    /// Number of clusters in the line.
    pub(crate) grapheme_len: u32,
    /// `line.chars().count()` — for the M5-b `chunk_char_offset` consumers.
    pub(crate) char_count: u32,
    /// Line byte length (the line's clusters tile `0..byte_len`).
    pub(crate) byte_len: u32,
    /// Verbatim `classify_structure_line` result (not a copy); Prose ≙ `None`.
    pub(crate) structure: StructureKind,
}

/// L1 content table for one finished block's output. `screen_origin` (the
/// block-level wrap policy) rides along so the L2 rebuild is a pure table
/// computation.
#[derive(Clone, Debug)]
pub(crate) struct ContentTable {
    pub(crate) graphemes: Vec<GEntry>,
    pub(crate) line_meta: Vec<LMeta>,
    pub(crate) screen_origin: bool,
    /// Trailing lines dropped by the trim — equivalence-test assertions only.
    pub(crate) trailing_trim_lines: u32,
}

/// Build L1 from a block output. Grapheme enumeration and width judgement
/// use the same call sites as the legacy wrap path.
pub(crate) fn build_content_table(output: &str, screen_origin: bool) -> ContentTable {
    let raw_lines: Vec<&str> = output.lines().collect();
    let trimmed_len = trimmed_output_line_count(&raw_lines);
    let mut graphemes: Vec<GEntry> = Vec::new();
    let mut line_meta: Vec<LMeta> = Vec::with_capacity(trimmed_len);
    for line in &raw_lines[..trimmed_len] {
        let grapheme_start = to_u32(graphemes.len());
        let mut char_count = 0u32;
        let mut byte_offset = 0usize;
        for grapheme in line.graphemes(true) {
            let mut flags = 0u8;
            if grapheme.chars().all(char::is_whitespace) {
                flags |= IS_WHITESPACE;
            }
            if grapheme == "\t" {
                flags |= IS_TAB_ZERO_W;
            }
            graphemes.push(GEntry {
                byte_offset: to_u32(byte_offset),
                width: grapheme_width(grapheme),
                flags,
            });
            byte_offset += grapheme.len();
            char_count += to_u32(grapheme.chars().count());
        }
        line_meta.push(LMeta {
            grapheme_start,
            grapheme_len: to_u32(graphemes.len()) - grapheme_start,
            char_count,
            byte_len: to_u32(line.len()),
            structure: classify_structure_line(line),
        });
        debug_assert_eq!(
            line_meta.last().unwrap().char_count as usize,
            line.chars().count(),
            "per-cluster char counts must sum to the line's char count"
        );
    }
    let table = ContentTable {
        graphemes,
        line_meta,
        screen_origin,
        trailing_trim_lines: to_u32(raw_lines.len() - trimmed_len),
    };
    debug_assert_eq!(
        table.trailing_trim_lines as usize,
        raw_lines.len() - trimmed_len,
        "L1 must trim with the single trailing-trim source"
    );
    table
}

// ── L2 width layer (key = cols, single active entry) ────────────────────

/// One visual row: 16 bytes, all `u32` — trailing-whitespace absorption can
/// push a row past `cols`; u16 would silently overflow (PLAN_M5 §二 P1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VisualRow {
    /// Surviving-line index (same numbering as `CachedLine.idx`).
    pub(crate) line_idx: u32,
    /// First cluster of the row — ABSOLUTE index into
    /// [`ContentTable::graphemes`].
    pub(crate) g_start: u32,
    /// Cluster count of the row.
    pub(crate) g_len: u32,
    /// Row byte length within the line (== legacy chunk range length).
    pub(crate) byte_len: u32,
}

const _: () = assert!(std::mem::size_of::<VisualRow>() == 16);
const _: () = assert!(std::mem::size_of::<GEntry>() == 8);

/// L2 width table for one (output, cols) pair. Single active entry: a cols
/// change drops the old table (rebuild) — no per-cols copies (PLAN_M5 §二).
#[derive(Clone, Debug)]
pub(crate) struct WidthTable {
    pub(crate) rows: Vec<VisualRow>,
    /// Per surviving line, the row index its first row occupies. Prefix
    /// layout (`len == line_meta.len() + 1`, `last == rows.len()`): line
    /// i's rows are `rows[base[i]..base[i+1]]`, and consumers can binary
    /// search a visual row back to its source line.
    pub(crate) line_row_base: Vec<u32>,
    /// Wrapped command resume hint rows — wrapped by the SAME machine.
    /// Must be included in output-row reads or scrollbar/find geometry
    /// drifts (PLAN_M5 §三).
    pub(crate) hint_rows: u32,
}

/// Rebuild L2 from the L1 tables — the machine never touches text
/// ("不摸文本"): every input comes from `GEntry` width/flags and `LMeta`
/// byte spans. Hints are short static strings, so they feed from the raw
/// text adapter (same machine).
pub(crate) fn build_width_table(l1: &ContentTable, hints: &[&str], cols: usize) -> WidthTable {
    let mut rows: Vec<VisualRow> = Vec::new();
    let mut line_row_base = Vec::with_capacity(l1.line_meta.len() + 1);
    line_row_base.push(0u32);
    let mut scratch: Vec<GraphemeInfo> = Vec::new();
    for (line_idx, meta) in l1.line_meta.iter().enumerate() {
        // Screen-origin blocks clip every row (v1.10.26 B-1); shell-output
        // blocks soft-wrap prose lines and apply the structure clip rules
        // PER LINE — the verbatim classification lives in L1.
        let mode = if l1.screen_origin {
            WrapMode::ScreenOrigin
        } else {
            WrapMode::Structure(meta.structure)
        };
        let g0 = meta.grapheme_start as usize;
        let entries = &l1.graphemes[g0..g0 + meta.grapheme_len as usize];
        // Cluster byte spans: each cluster's end is the next cluster's
        // offset; the line's last cluster ends at `meta.byte_len`.
        let ends = entries
            .iter()
            .skip(1)
            .map(|e| e.byte_offset as usize)
            .chain(std::iter::once(meta.byte_len as usize));
        scratch.clear();
        scratch.extend(entries.iter().zip(ends).map(|(e, byte_end)| GraphemeInfo {
            byte_start: e.byte_offset as usize,
            byte_end,
            width: e.width,
            is_whitespace: e.flags & IS_WHITESPACE != 0,
        }));
        let ranges = wrap_line_ranges(&scratch, cols, mode);
        // Byte ranges always land on cluster boundaries (the machine only
        // breaks at cluster starts / prefix ends), so the window walk below
        // is exact.
        let mut cursor = 0usize;
        for range in ranges {
            if entries.is_empty() {
                // An empty line still owns exactly one empty row (the
                // legacy `0..0` chunk).
                rows.push(VisualRow {
                    line_idx: to_u32(line_idx),
                    g_start: to_u32(g0),
                    g_len: 0,
                    byte_len: 0,
                });
                continue;
            }
            while (entries[cursor].byte_offset as usize) < range.start {
                cursor += 1;
            }
            let mut end_cursor = cursor;
            while end_cursor < entries.len()
                && (entries[end_cursor].byte_offset as usize) < range.end
            {
                end_cursor += 1;
            }
            let row = VisualRow {
                line_idx: to_u32(line_idx),
                g_start: to_u32(g0 + cursor),
                g_len: to_u32(end_cursor - cursor),
                byte_len: to_u32(range.end - range.start),
            };
            debug_assert_eq!(
                l1.graphemes[row.g_start as usize].byte_offset as usize, range.start,
                "row start must sit on a cluster boundary"
            );
            debug_assert_eq!(row.line_idx as usize, line_idx);
            rows.push(row);
            cursor = end_cursor;
        }
        line_row_base.push(to_u32(rows.len()));
    }
    let mut hint_rows = 0u32;
    for hint in hints {
        let entries: Vec<GraphemeInfo> = text_graphemes(hint).collect();
        let count = wrap_line_ranges(
            &entries,
            cols,
            WrapMode::Structure(classify_structure_line(hint)),
        )
        .len();
        hint_rows = hint_rows.saturating_add(to_u32(count));
    }
    let table = WidthTable {
        rows,
        line_row_base,
        hint_rows,
    };
    debug_assert_eq!(
        table.line_row_base.last().copied(),
        Some(to_u32(table.rows.len())),
        "line_row_base must prefix-sum to the row count"
    );
    table
}

/// `completed_block_output_rows` semantics on the L1/L2 tables (PLAN_M5 §三
/// 最小接线) — `block_content_metrics`' fallback read, `hint_rows +
/// rows.len()`; collapsed blocks report 0. Reads the STORED tables whenever
/// the entry's output identity still holds:
/// - cols-fresh entry (collapsed-state mismatch only): O(1) read of the
///   stored L2 (the tables are collapse-independent);
/// - cols-miss entry: L1 is cols-independent, so only L2 is rebuilt — never
///   touches text;
/// - no live entry: both layers are built from the block output.
///
/// Every branch yields the number the legacy per-line re-wrap computed
/// (G4-pinned by the visual_rows tests).
pub(crate) fn completed_output_rows(
    block: &Block,
    cols: usize,
    cached: Option<&super::CachedBlockLayout>,
) -> usize {
    if block.collapsed {
        return 0;
    }
    if let Some(c) = cached {
        // The entry's L1/L2 describe THIS output only if the identity
        // snapshot still matches (same check `ensure_cached` keys on;
        // Arc<str> is immutable, so ptr + len ⇒ same bytes).
        let same_output = c.output_len == block.output.len()
            && c.output_identity == block.output.as_ptr() as usize;
        if same_output && c.cols == cols {
            return c.width.hint_rows as usize + c.width.rows.len();
        }
        if same_output {
            let width = build_width_table(
                &c.content,
                crate::block_component::command_resume_hints(block),
                cols,
            );
            return width.hint_rows as usize + width.rows.len();
        }
    }
    let content = build_content_table(&block.output, block.screen_origin);
    let width = build_width_table(
        &content,
        crate::block_component::command_resume_hints(block),
        cols,
    );
    width.hint_rows as usize + width.rows.len()
}

/// Trailing-line trim shared by the cache and the L1 builder. Moved here
/// from grid_cache.rs (re-exported there so every existing path holds).
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

fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Property/unit tests for the L1/L2 tables and the shared machine.
/// Split into a companion file to stay under the 800-line gate
/// (same `#[cfg(test)] #[path]` pattern as `vt/screen_exit`).
#[cfg(test)]
#[path = "visual_rows_tests.rs"]
mod tests;
