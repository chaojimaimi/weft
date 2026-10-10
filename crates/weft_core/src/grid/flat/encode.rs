//! FlatStorage row encoding (D2 `encode_row`) and materialization (D1
//! `get`). Split verbatim from `mod.rs` (v1.13.8 S2 zero-behavior
//! file-budget split): the F1 tail-truncation load-bearing comments
//! (flags.is_empty() ⇒ all-default attributes) travel with the code.

use std::sync::Arc;

use super::content::ByteOffset;
use super::grapheme::Grapheme;
use super::style::BgAndStyle;
use super::FlatStorage;
use crate::grid::cell::{Cell, CellColor, CellFlags, CellWidth};
use crate::grid::row::Row;

impl FlatStorage {
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
    pub(super) fn encode_row(&mut self, row: &Row) {
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

        // T2 (PLAN_v11217 §3.3): hoist the `RowExtras` fast-path predicate so
        // the per-cell lookups below are skipped entirely for plain rows (seq
        // short-line streams do 2 BTreeMap probes per blank cell otherwise).
        //
        // Correctness: `RowExtras.cells` empty ⟺ both lookups return `None`
        // for EVERY column — `grapheme_at` / `hyperlink_id_at` are exactly
        // `self.cells.get(&col).and_then(..)` (row_extras.rs:121-144), and
        // `is_empty` is `cells.is_empty()` (row_extras.rs:116). So when
        // `extras_empty` holds, substituting `None` for each lookup is
        // bit-for-bit equivalent to probing the map; when it does not hold,
        // the original unconditional lookups run unchanged. Non-empty-extras
        // rows take the identical code path as before this gate.
        let extras_empty = row.extras.is_empty();

        let default_style = BgAndStyle::from_cell(&Cell::default());
        let carried_default = extras_empty
            && fg_color == CellColor::Default
            && hyperlink_id.is_none()
            && bg_and_style == default_style;
        let last_idx: isize = if carried_default {
            row.last_written_cell()
        } else {
            row.cells.len() as isize - 1
        };

        // Returns "running state non-default" (reset-materialization test).
        let mut emit_cell = |idx: isize, cell: &Cell| -> bool {
            // Wide-char spacer cells carry no bytes — the lead glyph owns
            // the width (D2: WIDE_SPACER 不产字节).
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                last_cell = idx;
                return fg_color != CellColor::Default
                    || bg_and_style != default_style
                    || hyperlink_id.is_some();
            }

            let cluster = if extras_empty {
                None
            } else {
                row.extras.grapheme_at(idx as usize)
            };
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

            let hl = if extras_empty {
                None
            } else {
                row.extras.hyperlink_id_at(idx as usize)
            };
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

            fg_color != CellColor::Default
                || bg_and_style != default_style
                || hyperlink_id.is_some()
        };

        let mut state_nondefault = false;
        for idx in 0..=last_idx {
            state_nondefault = emit_cell(idx, &row.cells[idx as usize]);
        }
        // Reset-cell materialization: at most one suffix cell can matter.
        if carried_default && state_nondefault && last_idx + 1 < row.cells.len() as isize {
            emit_cell(last_idx + 1, &row.cells[(last_idx + 1) as usize]);
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
