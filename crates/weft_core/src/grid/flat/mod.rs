//! FlatStorage — the compact history twin of the Cell grid (PLAN_S3).
//!
//! Port of Warp `flat_storage/mod.rs`, adapted to weft per PLAN_S3 §二:
//!
//! - D2 (write path): `push` takes an owned weft [`Row`] and encodes it —
//!   cell text as UTF-8 bytes, attribute change points into four logical
//!   interval maps (fg / bg+flags+underline slots / hyperlink id; underline
//!   slots live inside the widened bg+style value type per 评审 P1-3),
//!   `wrapped == false` rows terminate with `'\n'`, `WIDE_SPACER` cells
//!   produce no bytes, and default cells produce no bytes (interior runs are
//!   backfilled with blank bytes to keep offsets column-aligned).
//! - D3 (compat surface): the method names/signatures mirror the `Scrollback`
//!   ring this storage replaces (`push`/`extend`/`len`/`position`/
//!   `index_since`/`max_lines`/`set_max_lines`/`clear`/`is_empty`/`get`);
//!   `position` / `index_since` / `clear` transplant
//!   `grid/scrollback.rs:96-108` semantics bit for bit — `position` is a
//!   saturated count of rows ever pushed and survives `clear` (CSI 3J).
//! - Width authority: `Cell.width` (评审 P2); nothing recomputes widths from
//!   the text.
//!
//! T5: the storage is the sole history surface. `Grid::resize` runs the D4
//! protocol through it (`push_without_truncation` → `set_columns` →
//! `pop_rows`), the old Cell-ring `Scrollback` is deleted, and the frozen
//! 133 boundary + ownership mask migrate via the resize row map.

mod attribute_map;
mod content;
mod encode;
mod grapheme;
mod hyperlink;
pub(crate) mod index;
mod style;
mod window;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

/// T2 extras-gate equivalence tests (PLAN_v11217 §3.3). A CHILD module (via
/// `#[path]`, file `src/grid/flat/encode_fast_path_tests.rs`) so the tests
/// can reach the private flat structures directly while keeping this file
/// under the commit-gate 800-line ceiling.
#[cfg(test)]
#[path = "encode_fast_path_tests.rs"]
mod encode_fast_path_tests;

use super::cell::CellColor;
use super::row::Row;
use content::{ByteOffset, Content};
use hyperlink::HyperlinkIdMap;
use index::Index;
use style::{BgAndStyle, BgAndStyleMap, FgColorMap};

/// Grid history storage in flat form: a chunked UTF-8 byte stream, a
/// per-display-row index, and byte-offset attribute interval maps.
///
/// `pub` because `Grid.scrollback` is a public field and weft_app reads
/// `len()`/`max_lines()` across the crate boundary — the same surface the
/// `Scrollback` ring had.
pub struct FlatStorage {
    /// The grid content.
    content: Content,

    /// Maps row index → content byte range.
    index: Index,

    /// The width of the grid (columns the index wraps content at).
    columns: usize,

    /// Interval map: per-byte foreground color.
    fg_color_map: FgColorMap,

    /// Interval map: per-byte bg + style flags + underline slots.
    bg_and_style_map: BgAndStyleMap,

    /// Interval map: per-byte OSC 8 hyperlink id (None outside link spans).
    hyperlink_id_map: HyperlinkIdMap,

    /// Maximum retained rows; 0 disables storage entirely (push no-ops,
    /// mirroring `Scrollback::push`).
    max_lines: usize,

    /// Rows evicted by the `max_lines` limit. Monotonic; `clear` does not
    /// reset it, so block anchors never observe a counter going backwards.
    num_truncated_rows: u64,

    /// Logical insertion position: saturated count of rows ever pushed.
    /// `clear` (CSI 3J) deliberately does NOT reset it — screen-capture
    /// baselines and block anchors stay valid (D3 / scrollback.rs:89-95).
    position: u64,
}

impl FlatStorage {
    /// Constructs a new storage wrapping `columns` columns with a retention
    /// limit of `max_lines` rows (0 = disabled, like `Scrollback::new`).
    pub fn new(columns: usize, max_lines: usize) -> Self {
        Self {
            content: Content::new(),
            index: Index::new(columns, None),
            columns,
            fg_color_map: FgColorMap::new(CellColor::Default),
            bg_and_style_map: BgAndStyleMap::new(BgAndStyle::default()),
            hyperlink_id_map: HyperlinkIdMap::new(None),
            max_lines,
            num_truncated_rows: 0,
            position: 0,
        }
    }

    /// Encodes one row into the flat structures (D2 write path) and then
    /// applies the retention limit.
    ///
    /// Mirrors `Scrollback::push` exactly: `max_lines == 0` makes this a
    /// complete no-op (position included), and `position` advances before
    /// any eviction so it counts rows *pushed*, not rows retained.
    pub fn push(&mut self, row: Row) {
        if self.max_lines == 0 {
            return;
        }
        self.position = self.position.saturating_add(1);
        self.encode_row(&row);
        self.apply_max_rows();
    }

    /// Pushes every row from `iter` (D3 signature-compat with
    /// `Scrollback::extend`).
    pub fn extend<I: IntoIterator<Item = Row>>(&mut self, iter: I) {
        for row in iter {
            self.push(row);
        }
    }

    /// Encodes one row WITHOUT applying the retention limit (D4 step 2:
    /// the resize protocol pushes the whole viewport into storage before
    /// pulling rows back out; the limit applies once at the end).
    pub(crate) fn push_without_truncation(&mut self, row: Row) {
        self.position = self.position.saturating_add(1);
        self.encode_row(&row);
    }

    /// Pops the last `count` rows off the tail of storage (D4 step 5: the
    /// resize protocol pulls the new viewport back out, dropping the blank
    /// bottom rows past the cursor). Returns the materialized rows in
    /// document order; the tail (and its content bytes) is removed.
    /// Position / `num_truncated_rows` are untouched — a resize pop is not
    /// scrollback eviction.
    pub(crate) fn pop_rows(&mut self, count: usize) -> Vec<Row> {
        let start = self.len().saturating_sub(count);
        let rows: Vec<Row> = (start..self.len())
            .map(|i| self.get(i).expect("pop index must be in bounds"))
            .collect();
        let new_content_len = self.index.truncate(start);
        self.truncate_content_tail(new_content_len);
        rows
    }

    /// End offset (exclusive) of the row's content range — the D4 row-map
    /// walk reads one end per row instead of materializing ranges.
    pub(crate) fn content_range_end(&self, row: usize) -> Option<usize> {
        self.index
            .content_range_for_row(row)
            .map(|range| range.end.as_usize())
    }

    /// Whether the row soft-wraps into the next one (D4 cursor mapping:
    /// `input_needs_wrap` needs to know if the row hard-terminates).
    pub(crate) fn row_wraps(&self, row: usize) -> bool {
        self.index
            .get_entry(row)
            .is_some_and(|entry| !entry.has_trailing_newline)
    }

    /// Evicts the oldest rows until the `max_lines` limit holds.
    pub(crate) fn apply_max_rows(&mut self) {
        let excess_rows = self.index.len().saturating_sub(self.max_lines);
        if excess_rows > 0 {
            self.truncate_rows_front(excess_rows);
        }
    }

    /// Number of retained rows.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Logical insertion position (u64, saturated; survives `clear`).
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Index of the oldest retained row inserted at or after `position`.
    ///
    /// Bit-for-bit transplant of `scrollback.rs:102-108`.
    pub fn index_since(&self, position: u64) -> usize {
        if position > self.position {
            return 0; // The buffer was cleared and recreated.
        }
        let oldest = self.position.saturating_sub(self.len() as u64);
        position.max(oldest).saturating_sub(oldest) as usize
    }

    /// Configured retention limit.
    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// Updates the retention limit, keeping the NEWEST rows when shrinking
    /// (`Scrollback::resize` semantics — logical positions stay a contiguous
    /// suffix ending at `position`).
    ///
    /// The `cols` argument exists for D3 signature compatibility with
    /// `Scrollback::set_max_lines(usize, usize)`; per-row reshaping never
    /// happens here (width changes go through [`Self::set_columns`] /
    /// `Index::rebuild`).
    pub fn set_max_lines(&mut self, max_lines: usize, _cols: usize) {
        if max_lines == self.max_lines {
            return;
        }
        if max_lines == 0 {
            // Scrollback::resize(0): drop everything, keep position.
            self.max_lines = 0;
            self.truncate_rows_front(self.len());
            return;
        }
        let excess = self.len().saturating_sub(max_lines);
        if excess > 0 {
            self.truncate_rows_front(excess);
        }
        self.max_lines = max_lines;
    }

    /// Drops every retained row without re-basing content offsets and
    /// without touching `position` / `num_truncated_rows` (CSI 3J).
    pub fn clear(&mut self) {
        let new_content_len = self.index.truncate(0);
        self.truncate_content_tail(new_content_len);
    }

    /// Trims the content tail (and the attribute maps with it) to the given
    /// absolute offset — the shared tail-trim behind clear, the resize
    /// bridge, and single-row replacement.
    fn truncate_content_tail(&mut self, new_content_len: ByteOffset) {
        self.content.truncate(new_content_len);
        self.fg_color_map.truncate(new_content_len);
        self.bg_and_style_map.truncate(new_content_len);
        self.hyperlink_id_map.truncate(new_content_len);
    }

    /// T3 (D5-1): one-shot byte snapshot of the retained rows for the
    /// off-thread text search — the whole point is that the worker gets
    /// plain bytes plus RLE run tables instead of ~32B/cell clones. Run
    /// walk = the row-local half of `content_offset_to_point`, so hit →
    /// column mapping needs no `Index` on the worker side.
    pub(crate) fn search_rows_snapshot(&self) -> (Vec<u8>, Vec<crate::find::FindSnapshotRow>) {
        let mut bytes: Vec<u8> = Vec::new();
        let mut rows = Vec::with_capacity(self.len());
        for index in 0..self.len() {
            let Some(range) = self.index.content_range_for_row(index) else {
                break;
            };
            let Some(entry) = self.index.get_entry(index) else {
                break;
            };
            let range_len = (range.end - range.start).as_usize();
            // Drop the row terminator — the search walks cells only.
            let cell_bytes = if entry.has_trailing_newline {
                range_len - 1
            } else {
                range_len
            };
            let start = bytes.len();
            let cell_end = range.start + cell_bytes;
            self.content
                .append_range_to(&mut bytes, range.start..cell_end);
            let runs: Vec<(u16, u8, u16)> = self
                .index
                .grapheme_runs_for_row(index)
                .unwrap_or(&[])
                .iter()
                .map(|run| {
                    (
                        run.count.get(),
                        run.info.cell_width,
                        run.info.utf8_bytes.get(),
                    )
                })
                .collect();
            // T3 review P3-2: pin the flat invariant — the runs must
            // account for exactly the row's cell bytes.
            debug_assert_eq!(
                runs.iter()
                    .map(|(count, _, utf8_len)| *count as usize * *utf8_len as usize)
                    .sum::<usize>(),
                cell_bytes,
                "run byte sum must equal cell_bytes at row {index}"
            );
            rows.push(crate::find::FindSnapshotRow {
                start,
                cell_bytes,
                runs,
            });
        }
        (bytes, rows)
    }

    /// T3 (D5-2): byte length of a row's snapshot text, computed from the
    /// flat runs WITHOUT materializing the [`Row`] — the drag-selection
    /// replay walks every retained row per query, and per-row `Row`
    /// allocation dominated that cost.
    ///
    /// Mirrors `snapshot_line_map::snapshot_row_text_len` exactly: trailing
    /// blank graphemes are trimmed (a grapheme is blank iff its lead scalar
    /// is `' '`; `\0` never reaches flat content), and every kept grapheme
    /// contributes its full UTF-8 byte length (a multi-scalar cluster's run
    /// bytes ARE the cluster string the snapshot text uses).
    ///
    /// The rule stays cross-checked by snapshot_line_map's replay-vs-builder
    /// regression tests, which run this path against the real builder.
    pub(crate) fn row_snapshot_text_len(&self, index: usize) -> Option<usize> {
        let range = self.index.content_range_for_row(index)?;
        let runs = self.index.grapheme_runs_for_row(index)?;
        let base = range.start;
        let mut last_contentful: Option<usize> = None;
        let mut ordinal = 0usize;
        let mut off = 0usize;
        for run in runs {
            for _ in 0..run.count.get() {
                let end = base + off + run.info.utf8_bytes.get() as usize;
                let text = std::str::from_utf8(self.content[base + off..end].as_bytes()).ok();
                let lead = text.and_then(|s| s.chars().next());
                if lead.is_some_and(|c| c != ' ') {
                    last_contentful = Some(ordinal);
                }
                off += run.info.utf8_bytes.get() as usize;
                ordinal += 1;
            }
        }
        // Sum the bytes of the first `last_contentful + 1` graphemes.
        let keep = last_contentful.map_or(0, |o| o + 1);
        let mut total = 0usize;
        let mut seen = 0usize;
        for run in runs {
            if seen >= keep {
                break;
            }
            let take = (run.count.get() as usize).min(keep - seen);
            total += take * run.info.utf8_bytes.get() as usize;
            seen += take;
        }
        Some(total)
    }

    /// Re-wraps the index at a new column count. Content bytes, attribute
    /// maps, and offsets are untouched — that is the entire reflow story.
    pub(crate) fn set_columns(&mut self, new_columns: usize) {
        if self.columns == new_columns {
            return;
        }

        self.columns = new_columns;
        self.index = Index::rebuild(&self.index, new_columns);
    }

    /// Evicts the oldest `count` rows. Content/attribute structures are told
    /// the new start offset; nothing is re-based, so `num_truncated_rows` is
    /// the only observable trace besides the evicted rows themselves.
    pub(crate) fn truncate_rows_front(&mut self, count: usize) {
        if count == 0 {
            return;
        }

        let count = count.min(self.len());

        let new_start_offset = self.index.truncate_front(count);

        self.content.truncate_front(new_start_offset);
        self.fg_color_map.truncate_front(new_start_offset);
        self.bg_and_style_map.truncate_front(new_start_offset);
        self.hyperlink_id_map.truncate_front(new_start_offset);

        self.num_truncated_rows += count as u64;
    }

    /// Rows evicted so far by the retention limit (monotonic).
    #[allow(dead_code)] // retention diagnostics; no production reader yet.
    pub(crate) fn num_truncated_rows(&self) -> u64 {
        self.num_truncated_rows
    }
}

/// D4 step 3: where the cursor sits in the content stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorAnchor {
    /// Cursor is at the location with this byte offset.
    AtPoint(ByteOffset),
    /// Cursor is at the cell AFTER the grapheme starting at this byte
    /// offset — the encoding for a cursor past a row's content end, and
    /// for `wrap_pending` (Warp's `AtCellAfterPoint`).
    AfterCell(ByteOffset),
}

impl FlatStorage {
    /// Computes the cursor's content anchor from its (row, col) within the
    /// flat rows.
    ///
    /// - `col == 0`: anchor at the row start. For a hard row boundary this
    ///   is equivalent to Warp's `AtPoint` after their `cell_follows_newline`
    ///   check — the cursor stays on its own (possibly empty) row.
    /// - `col > 0` with content left of the cursor: anchor at the last
    ///   grapheme starting left of `col`, so the position tracks that
    ///   content through a re-wrap (Warp's `AtCellAfterPoint`).
    /// - `col > 0` on a row with no content left of the cursor (a blank
    ///   continuation row after a soft-wrapped line — a case Warp reaches
    ///   via cross-row `wrapping_sub`): glue to the previous row's last
    ///   content byte so the cursor cannot strand on a phantom blank row.
    pub(crate) fn cursor_anchor(&self, row: usize, col: usize) -> CursorAnchor {
        let Some(range) = self.index.content_range_for_row(row) else {
            return CursorAnchor::AtPoint(ByteOffset::zero());
        };
        if col == 0 {
            return CursorAnchor::AtPoint(range.start);
        }
        if let Some(start) = self.last_grapheme_start_before(row, col) {
            return CursorAnchor::AfterCell(start);
        }
        // Blank continuation row: glue to the previous row's last grapheme;
        // fall back to this row's start when there is none (empty history /
        // empty previous row).
        if row > 0 {
            if let Some(start) = self.last_grapheme_start_before(row - 1, usize::MAX) {
                return CursorAnchor::AfterCell(start);
            }
        }
        CursorAnchor::AtPoint(range.start)
    }

    /// Start offset of the last grapheme in `row` whose column is strictly
    /// left of `max_col` (`usize::MAX` = the row's last grapheme).
    fn last_grapheme_start_before(&self, row: usize, max_col: usize) -> Option<ByteOffset> {
        let range = self.index.content_range_for_row(row)?;
        let runs = self.index.grapheme_runs_for_row(row)?;
        let base = range.start;
        let mut byte_off = 0usize;
        let mut col = 0usize;
        let mut last: Option<ByteOffset> = None;
        for run in runs {
            let (run_cols, run_bytes) = (
                run.cols(),
                run.count.get() as usize * run.info.utf8_bytes.get() as usize,
            );
            if col >= max_col {
                break;
            }
            // Graphemes in this run cover columns [col, col + run_cols).
            let visible = run_cols.min(max_col - col);
            // Each grapheme is `width` columns; the last fully-visible one
            // starts at column `col + (visible - 1) * width` rounded down to
            // a grapheme start. With runs being uniform, the count of
            // visible graphemes is `visible / width`.
            let visible_graphemes = visible / run.info.cell_width as usize;
            if visible_graphemes > 0 {
                last = Some(
                    base + ByteOffset::from_usize(
                        byte_off + (visible_graphemes - 1) * run.info.utf8_bytes.get() as usize,
                    ),
                );
            }
            col += run_cols;
            byte_off += run_bytes;
        }
        last
    }

    /// D4 step 5: maps a cursor anchor back to a (row, col, wrap_pending)
    /// position at `new_cols` width. `AfterCell` re-finds the grapheme the
    /// anchor points at, then advances past it — turning into
    /// `wrap_pending` when that advance falls off a hard-terminated full
    /// row (Warp's `input_needs_wrap` recomputation).
    ///
    /// Deliberate divergence from Warp: their post-anchor advance is a fixed
    /// one-cell step, which can land the cursor ON a wide glyph's spacer
    /// cell; weft advances by the grapheme's full width so the cursor always
    /// rests on a writable lead cell (weft spacers are never cursor
    /// positions).
    pub(crate) fn cursor_point_from_anchor(
        &self,
        anchor: CursorAnchor,
        new_cols: usize,
    ) -> (usize, usize, bool) {
        let total = self.len();
        let fallback = (total.saturating_sub(1), 0, false);
        let offset = match anchor {
            CursorAnchor::AtPoint(offset) => offset,
            CursorAnchor::AfterCell(offset) => offset,
        };
        let Ok(point) = self.index.content_offset_to_point(offset) else {
            return fallback;
        };
        match anchor {
            CursorAnchor::AtPoint(_) => (point.row, point.col, false),
            CursorAnchor::AfterCell(_) => {
                // Width of the grapheme the anchor points at.
                let width = self
                    .index
                    .grapheme_runs_for_row(point.row)
                    .unwrap_or(&[])
                    .iter()
                    .scan(0usize, |col, run| {
                        let start = *col;
                        *col += run.cols();
                        Some((start, run.info.cell_width as usize))
                    })
                    .find(|(start, _)| *start == point.col)
                    .map(|(_, width)| width)
                    .unwrap_or(1);
                let next_col = point.col + width;
                if next_col < new_cols {
                    (point.row, next_col, false)
                } else if self.row_wraps(point.row) {
                    // Soft wrap: the position continues on the next row.
                    (point.row + 1, 0, false)
                } else {
                    // Hard-terminated full row: deferred wrap.
                    (point.row, new_cols.saturating_sub(1), true)
                }
            }
        }
        .pipe(|pos| {
            let (row, col, wrap) = pos;
            // Defensive clamps: the mapped position must exist.
            let row = row.min(total.saturating_sub(1));
            (row, col.min(new_cols.saturating_sub(1)), wrap)
        })
    }
}

/// Tiny combinator so the mapping tail above stays a single expression.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}
