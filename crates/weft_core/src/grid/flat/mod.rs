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
//! T2: wired as `Grid.scrollback`. `materialize_all` / `replace_row` are the
//! two transitional bridges for the old reflow and the marker-cell trick —
//! both cold paths, both scheduled to die in T5/T6.

mod attribute_map;
mod content;
mod grapheme;
mod hyperlink;
pub(crate) mod index;
mod style;
mod window;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use super::cell::{CellColor, CellFlags, CellWidth};
use super::row::Row;
use content::{ByteOffset, Content};
use grapheme::Grapheme;
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

    /// T2 resize bridge (cold path): materializes every retained row as an
    /// owned [`Row`] and drains the storage, ready for the reflowed rows to
    /// be re-encoded via [`Self::extend`]. Position / counters stay
    /// monotonic (D3 评审 P2-1: anchors now survive a full reflow instead of
    /// collapsing with the old ring swap).
    pub(crate) fn materialize_all(&mut self) -> Vec<Row> {
        let rows: Vec<Row> = (0..self.len())
            .map(|i| self.get(i).expect("materialize index must be in bounds"))
            .collect();
        let new_content_len = self.index.truncate(0);
        self.truncate_content_tail(new_content_len);
        rows
    }

    /// T2 transitional adapter for the two `Scrollback::get_mut` consumers
    /// (the snapshot marker dance; T5 deletes the technique). Flat storage
    /// is append-only, so an in-place edit re-encodes the edited row and
    /// everything after it — O(rows), accepted because screen-exit runs it
    /// at most once per command.
    pub(crate) fn replace_row(&mut self, index: usize, row: Row) {
        debug_assert!(index < self.len(), "replace_row index out of bounds");
        let tail: Vec<Row> = ((index + 1)..self.len())
            .map(|i| self.get(i).expect("tail index must be in bounds"))
            .collect();
        let new_content_len = self.index.truncate(index);
        self.truncate_content_tail(new_content_len);
        self.encode_row(&row);
        for tail_row in tail {
            self.encode_row(&tail_row);
        }
    }

    /// T2 resize bridge: after the old reflow re-segments and re-encodes
    /// every row at the new width, the storage must track that width WITHOUT
    /// a second `Index::rebuild` (the rows are already wrapped).
    pub(crate) fn set_columns_no_rebuild(&mut self, columns: usize) {
        self.columns = columns;
    }

    /// Re-wraps the index at a new column count. Content bytes, attribute
    /// maps, and offsets are untouched — that is the entire reflow story.
    #[allow(dead_code)] // T5: resize protocol (D4 step 4) is the caller.
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
    #[allow(dead_code)] // T5: anchor-eviction diagnostics read this.
    pub(crate) fn num_truncated_rows(&self) -> u64 {
        self.num_truncated_rows
    }

    /// Materializes the row at `index` as an owned [`Row`] (0 = oldest).
    ///
    /// Walks the row's index entries over the content stream, back-filling
    /// attributes per byte offset, rebuilding wide-char spacers from grapheme
    /// widths, and reconstructing `RowExtras` for multi-scalar clusters and
    /// hyperlinks. This is the weft replacement for Warp's `RowIterator`
    /// (weft materializes owned rows for the bounded history window, D1).
    pub fn get(&self, index: usize) -> Option<Row> {
        let range = self.index.content_range_for_row(index)?;
        let entry = self.index.get_entry(index)?;

        let mut row = Row::new(self.columns);
        let start = range.start;
        let mut current = start;

        // Offset-keyed iterators must advance once per BYTE to stay in sync
        // with the content walk (multi-byte graphemes included).
        let mut fg_color_iter = self.fg_color_map.iter_from(start);
        let mut bg_and_style_iter = self.bg_and_style_map.iter_from(start);
        let mut hyperlink_id_iter = self.hyperlink_id_map.iter_from(start);

        let mut col = 0usize;

        for info in self.index.grapheme_infos_for_row(index)? {
            let byte_len = info.utf8_bytes.get() as usize;
            let text = &self.content[current..current + byte_len];
            let grapheme = Grapheme::new_from_str_and_info(text, info);

            if grapheme.starts_new_row() {
                break;
            }

            let fg_color = next_attribute(&mut fg_color_iter, byte_len);
            let bg_and_style = next_attribute(&mut bg_and_style_iter, byte_len);
            let hyperlink_id = next_attribute(&mut hyperlink_id_iter, byte_len);

            let cell_width = grapheme.cell_width() as usize;
            if cell_width == 0 {
                current += byte_len;
                continue;
            }

            if col + cell_width > self.columns {
                tracing::warn!(
                    row = index,
                    col,
                    "flat row materialization ran past the row width"
                );
                break;
            }

            let mut chars = grapheme.chars();
            let cell = &mut row.cells[col];
            cell.character = chars.next().expect("grapheme is non-empty");
            cell.fg = fg_color;
            cell.bg = bg_and_style.bg;
            // D2 drops DIRTY from the persisted style mask, but the flag is
            // weft's write-marker: reflow (`row_content_end`) and the
            // snapshot extents distinguish a written blank from a
            // never-written one by it, and the VT print path sets it on
            // every written cell. Re-add it on every cell a byte produced.
            cell.flags = bg_and_style.flags | CellFlags::DIRTY;
            cell.underline_style = bg_and_style.underline_style;
            cell.underline_color = bg_and_style.underline_color;
            cell.width = if cell_width == 2 {
                CellWidth::Full
            } else {
                CellWidth::Half
            };

            if chars.next().is_some() {
                // Multi-scalar cluster: store the whole cluster string in
                // RowExtras and flag the lead cell — the same shape the VT
                // print path produces (CellFlags::EXTRA contract, row_extras.rs).
                cell.flags |= CellFlags::EXTRA;
                row.extras.set_grapheme(col, Arc::from(text));
            }
            if let Some(id) = hyperlink_id {
                cell.flags |= CellFlags::HYPERLINK;
                row.extras.set_hyperlink(col, Some(id));
            }

            if cell_width == 2 {
                // The trailing half of a wide glyph: spacer flag plus the
                // lead's hyperlink id, so both halves stay hoverable.
                let spacer = &mut row.cells[col + 1];
                spacer.flags |= CellFlags::WIDE_SPACER;
                if let Some(id) = hyperlink_id {
                    spacer.flags |= CellFlags::HYPERLINK;
                    row.extras.set_hyperlink(col + 1, Some(id));
                }
            }

            col += cell_width;
            current += byte_len;
        }

        // weft's `wrapped` flag is exactly "no trailing newline in content".
        row.wrapped = !entry.has_trailing_newline;
        // `ends_with_leading_wide_char_spacer` (rebuild-only) materializes as
        // the trailing blank default cell — weft has no separate flag, and a
        // blank cell is the visual result of that slot.

        Some(row)
    }

    /// D2 encoder: one weft [`Row`] → content bytes + attribute change
    /// points + one index entry. Port of Warp's `push_rows_internal` inner
    /// loop with weft's specifics (extras clusters, `Row.wrapped`, ' '
    /// default cell).
    fn encode_row(&mut self, row: &Row) {
        // Attribute state carries across rows: unchanged output across a row
        // boundary costs no new map entries.
        let mut fg_color = self.fg_color_map.tail();
        let mut bg_and_style = self.bg_and_style_map.tail();
        let mut hyperlink_id = self.hyperlink_id_map.tail();

        let start_offset = ByteOffset::from_usize(self.content.end_offset());
        let mut entry_builder = self.index.start_row();

        // `offset` tracks where the next processed byte would land — it
        // advances for every cell (processed or not); content only grows for
        // processed cells, and skipped-cell backfills reconcile the two.
        let mut offset = start_offset;
        // Column index of the last cell that produced bytes (isize so the
        // pre-first-cell backfill range works out).
        let mut last_cell: isize = -1;

        for (idx, cell) in row.cells.iter().enumerate() {
            let idx = idx as isize;

            // Wide-char spacer cells carry no bytes — the lead glyph owns
            // the width (D2: WIDE_SPACER 不产字节).
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                last_cell = idx;
                continue;
            }

            let cluster = row.extras.grapheme_at(idx as usize);
            // A default cell is a NEVER-written blank. A written blank keeps
            // its DIRTY flag — the same discriminator reflow's
            // `row_content_end` uses — so an exactly-full wrapped row keeps
            // its boundary space as a real byte.
            let mut needs_processing =
                cluster.is_some() || cell.character != ' ' || !cell.flags.is_empty();

            let fg = cell.fg;
            if fg != fg_color {
                needs_processing = true;
                fg_color = fg;
                self.fg_color_map.push_attribute_change(offset.., fg);
            }

            let style = BgAndStyle::from_cell(cell);
            if style != bg_and_style {
                needs_processing = true;
                bg_and_style = style;
                self.bg_and_style_map.push_attribute_change(offset.., style);
            }

            let hl = row.extras.hyperlink_id_at(idx as usize);
            if hl != hyperlink_id {
                needs_processing = true;
                hyperlink_id = hl;
                self.hyperlink_id_map.push_attribute_change(offset.., hl);
            }

            let grapheme = Grapheme::new_from_cell(cell, cluster);
            offset += grapheme.len();

            if needs_processing {
                // Skipped default cells before a content-ful cell still have
                // to occupy bytes so byte offsets stay aligned with columns.
                for _ in last_cell..(idx - 1) {
                    entry_builder
                        .process_grapheme_info_unchecked(Grapheme::empty_cell().sizing_info());
                    self.content.push_grapheme(&Grapheme::empty_cell());
                }
                last_cell = idx;
                entry_builder.process_grapheme_info_unchecked(grapheme.sizing_info());
                self.content.push_grapheme(&grapheme);
            }
        }

        // D2 行界: `wrapped == true` is a soft-wrapped continuation (no
        // newline byte); anything else hard-terminates with '\n'.
        if !row.wrapped {
            entry_builder.add_trailing_newline();
            self.content.push_grapheme(&Grapheme::newline());
        }

        entry_builder.append_to_index(&mut self.index);
    }
}

/// Consumes `byte_len - 1` items from an offset-keyed attribute iterator and
/// returns the value at the grapheme's first byte. `nth(len - 1)` (not
/// `next()`) is what keeps multi-byte graphemes aligned with the per-byte
/// map semantics (same trick as Warp's RowIterator).
fn next_attribute<T: Copy>(iter: &mut impl Iterator<Item = T>, byte_len: usize) -> T {
    iter.nth(byte_len - 1)
        .expect("attribute iterator should cover the row's bytes")
}
