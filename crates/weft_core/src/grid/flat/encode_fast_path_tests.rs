//! T2 encode_row extras-gate equivalence tests (PLAN_v11217 §3.3 测试验收).
//!
//! `FlatStorage::encode_row` gates the two `RowExtras` lookups behind a
//! hoisted `extras_empty` predicate. These tests pin gate-vs-ungated
//! equivalence with a slow REFERENCE encoder ([`ungated_encode_row`]) that
//! probes the extras map unconditionally — the exact pre-T2 semantics — and
//! compares the full observable encoding state: content bytes, per-row index
//! boundaries + grapheme run tables, and all three attribute maps, plus
//! materialized round-trips and hand-computed golden bytes.
//!
//! A child module (via `#[path]`, same pattern as
//! `weft_app/src/tab/pane_pump_budget_tests.rs`) so it can reach the private
//! flat structures without growing `mod.rs`/`tests.rs` past the commit-gate
//! 800-line ceiling.

use std::sync::Arc;

use super::content::ByteOffset;
use super::grapheme::Grapheme;
use super::style::BgAndStyle;
use super::testing::assert_rows_equal;
use super::FlatStorage;
use crate::grid::cell::{CellColor, CellFlags, CellWidth};
use crate::grid::row::Row;

// ── slow reference (pre-T2) encoder ─────────────────────────────────────

/// The pre-T2 `encode_row`, kept verbatim as the equivalence reference: the
/// two `RowExtras` lookups run UNCONDITIONALLY for every non-spacer cell.
/// If either this copy or the gated production copy drifts semantically,
/// the snapshot comparisons below fail.
fn ungated_encode_row(storage: &mut FlatStorage, row: &Row) {
    // Attribute state carries across rows: unchanged output across a row
    // boundary costs no new map entries.
    let mut fg_color = storage.fg_color_map.tail();
    let mut bg_and_style = storage.bg_and_style_map.tail();
    let mut hyperlink_id = storage.hyperlink_id_map.tail();

    let start_offset = ByteOffset::from_usize(storage.content.end_offset());
    let mut entry_builder = storage.index.start_row();

    let mut offset = start_offset;
    let mut last_cell: isize = -1;

    for (idx, cell) in row.cells.iter().enumerate() {
        let idx = idx as isize;

        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            last_cell = idx;
            continue;
        }

        let cluster = row.extras.grapheme_at(idx as usize);
        let mut needs_processing =
            cluster.is_some() || cell.character != ' ' || !cell.flags.is_empty();

        let fg = cell.fg;
        if fg != fg_color {
            needs_processing = true;
            fg_color = fg;
            storage.fg_color_map.push_attribute_change(offset.., fg);
        }

        let style = BgAndStyle::from_cell(cell);
        if style != bg_and_style {
            needs_processing = true;
            bg_and_style = style;
            storage
                .bg_and_style_map
                .push_attribute_change(offset.., style);
        }

        let hl = row.extras.hyperlink_id_at(idx as usize);
        if hl != hyperlink_id {
            needs_processing = true;
            hyperlink_id = hl;
            storage.hyperlink_id_map.push_attribute_change(offset.., hl);
        }

        let grapheme = Grapheme::new_from_cell(cell, cluster);
        offset += grapheme.len();

        if needs_processing {
            for _ in last_cell..(idx - 1) {
                entry_builder.process_grapheme_info_unchecked(Grapheme::empty_cell().sizing_info());
                storage.content.push_grapheme(&Grapheme::empty_cell());
            }
            last_cell = idx;
            entry_builder.process_grapheme_info_unchecked(grapheme.sizing_info());
            storage.content.push_grapheme(&grapheme);
        }
    }

    if !row.wrapped {
        entry_builder.add_trailing_newline();
        storage.content.push_grapheme(&Grapheme::newline());
    }

    entry_builder.append_to_index(&mut storage.index);
}

// ── snapshot + comparison helpers ───────────────────────────────────────

/// Per-row index boundary, wrap flag, and grapheme run table.
#[derive(Debug, PartialEq)]
struct RowLayout {
    range: (usize, usize),
    wraps: bool,
    runs: Vec<(u16, u8, u16)>,
}

/// Everything `encode_row` writes: row count, per-row index layout, and the
/// full per-byte value streams of the three attribute maps.
#[derive(Debug, PartialEq)]
struct EncodingSnapshot {
    rows: usize,
    layout: Vec<RowLayout>,
    fg: Vec<CellColor>,
    bg_style: Vec<BgAndStyle>,
    links: Vec<Option<u32>>,
}

fn snapshot(storage: &FlatStorage) -> EncodingSnapshot {
    let end = storage.content.end_offset();
    let layout = (0..storage.len())
        .map(|i| {
            let range = storage
                .index
                .content_range_for_row(i)
                .expect("row in bounds");
            let runs = storage
                .index
                .grapheme_runs_for_row(i)
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
            RowLayout {
                range: (range.start.as_usize(), range.end.as_usize()),
                wraps: storage.row_wraps(i),
                runs,
            }
        })
        .collect();
    EncodingSnapshot {
        rows: storage.len(),
        layout,
        fg: storage
            .fg_color_map
            .iter_from(ByteOffset::zero())
            .take(end)
            .collect(),
        bg_style: storage
            .bg_and_style_map
            .iter_from(ByteOffset::zero())
            .take(end)
            .collect(),
        links: storage
            .hyperlink_id_map
            .iter_from(ByteOffset::zero())
            .take(end)
            .collect(),
    }
}

/// Encodes `rows` twice — once through the gated production `push`, once
/// through the ungated reference — and asserts the two storages are
/// observably identical, plus that the gated storage round-trips the input
/// rows through `get`.
fn assert_gate_equivalence(rows: &[Row], columns: usize, msg: &str) {
    let mut gated = FlatStorage::new(columns, usize::MAX);
    let mut ungated = FlatStorage::new(columns, usize::MAX);
    for row in rows {
        gated.push(row.clone());
        ungated_encode_row(&mut ungated, row);
    }

    assert_eq!(
        snapshot(&gated),
        snapshot(&ungated),
        "{msg}: gated encoding state diverges from the ungated reference"
    );

    let gated_rows: Vec<Row> = (0..gated.len())
        .map(|i| gated.get(i).expect("row in bounds"))
        .collect();
    assert_rows_equal(&gated_rows, rows, msg);
}

/// The row's content bytes, terminator included.
fn row_text(storage: &FlatStorage, row: usize) -> String {
    let range = storage
        .index
        .content_range_for_row(row)
        .expect("row in bounds");
    storage.content[range].to_string()
}

/// Per-byte hyperlink map values for content bytes `[start, start + count)`.
fn link_values(storage: &FlatStorage, start: usize, count: usize) -> Vec<Option<u32>> {
    storage
        .hyperlink_id_map
        .iter_from(ByteOffset::from_usize(start))
        .take(count)
        .collect()
}

// ── scenario row builders ───────────────────────────────────────────────

/// `· e 👩‍🔬 !` with the lead cells flagged per the VT print convention.
fn grapheme_row() -> Row {
    let mut row = Row::new(6);
    row.cells[1].character = 'e';
    row.cells[1].flags |= CellFlags::EXTRA;
    row.extras.set_grapheme(1, Arc::from("e\u{0301}"));
    row.cells[3].character = '👩';
    row.cells[3].width = CellWidth::Full;
    row.cells[3].flags |= CellFlags::EXTRA;
    row.cells[4].flags |= CellFlags::WIDE_SPACER;
    row.extras.set_grapheme(3, Arc::from("👩\u{200d}🔬"));
    row.cells[5].character = '!';
    row
}

/// `· a b · · ·` with cells 1–2 carrying OSC 8 id 7 (the two trailing
/// defaults force the in-row link reset byte).
fn hyperlink_row() -> Row {
    let mut row = Row::new(6);
    for col in 1..3 {
        row.cells[col].character = (b'a' + col as u8 - 1) as char;
        row.cells[col].flags |= CellFlags::HYPERLINK;
        row.extras.set_hyperlink(col, Some(7));
    }
    row
}

/// `a é · x · 中 ·` — plain ASCII, grapheme cluster, hyperlink, and a plain
/// wide glyph sharing one row (extras map non-empty and sparse).
fn mixed_row() -> Row {
    let mut row = Row::new(8);
    row.cells[0].character = 'a';
    row.cells[1].character = 'e';
    row.cells[1].flags |= CellFlags::EXTRA;
    row.extras.set_grapheme(1, Arc::from("e\u{0301}"));
    row.cells[3].character = 'x';
    row.cells[3].flags |= CellFlags::HYPERLINK;
    row.extras.set_hyperlink(3, Some(9));
    row.cells[5].character = '中';
    row.cells[5].width = CellWidth::Full;
    row.cells[6].flags |= CellFlags::WIDE_SPACER;
    row
}

// ── tests ───────────────────────────────────────────────────────────────

#[test]
fn grapheme_row_encodes_identically_with_and_without_gate() {
    let row = grapheme_row();
    assert_gate_equivalence(&[row], 6, "grapheme row");

    // Golden bytes: backfilled blanks at cols 0 and 2, whole-cluster bytes
    // for both clusters, '!' trailing, '\n' terminator.
    let mut gated = FlatStorage::new(6, usize::MAX);
    gated.push(grapheme_row());
    assert_eq!(row_text(&gated, 0), " e\u{0301} 👩\u{200d}🔬!\n");
}

#[test]
fn hyperlink_row_encodes_identically_with_and_without_gate() {
    let row = hyperlink_row();
    assert_gate_equivalence(&[row], 6, "hyperlink row");

    // Golden bytes + link spans: backfill blank, a, b, then the in-row reset
    // blank on the first trailing default (bytes 0..1 unlinked, 1..3 id 7,
    // 3.. reset).
    let mut gated = FlatStorage::new(6, usize::MAX);
    gated.push(hyperlink_row());
    assert_eq!(row_text(&gated, 0), " ab \n");
    assert_eq!(
        link_values(&gated, 0, 5),
        vec![None, Some(7), Some(7), None, None]
    );
}

#[test]
fn mixed_row_encodes_identically_with_and_without_gate() {
    let row = mixed_row();
    assert_gate_equivalence(&[row], 8, "mixed row");

    // Golden bytes: 'a'(0), 3-byte cluster (1..4), backfilled blanks for the
    // skipped defaults at 4 and 6, 'x'(5), wide 中 (7..10), terminator (10).
    // Link span is exactly byte 5 (id 9) — the backfilled blank at byte 4
    // must NOT inherit the link, and the blank default at cell 4 resets the
    // span from byte 6.
    let mut gated = FlatStorage::new(8, usize::MAX);
    gated.push(mixed_row());
    assert_eq!(row_text(&gated, 0), "ae\u{0301} x 中\n");
    assert_eq!(
        link_values(&gated, 0, 10),
        vec![
            None,
            None,
            None,
            None,
            None,
            Some(9),
            None,
            None,
            None,
            None
        ]
    );
}

#[test]
fn all_blank_rows_hit_the_gate_with_unchanged_encoding() {
    // Never-written blanks (extras empty, the seq fast-path shape): no bytes
    // except the terminator.
    let blank = Row::new(4);
    assert_gate_equivalence(&[blank], 4, "default blank row");

    // Written blanks (DIRTY set): every cell processes as one space byte,
    // still zero extras lookups under the gate.
    let mut written_blank = Row::new(4);
    for cell in &mut written_blank.cells {
        cell.flags |= CellFlags::DIRTY;
    }
    assert_gate_equivalence(&[written_blank], 4, "written blank row");

    let mut gated = FlatStorage::new(4, usize::MAX);
    gated.push(Row::new(4));
    assert_eq!(row_text(&gated, 0), "\n");
    let mut written_blank = Row::new(4);
    for cell in &mut written_blank.cells {
        cell.flags |= CellFlags::DIRTY;
    }
    gated.push(written_blank);
    assert_eq!(row_text(&gated, 1), "    \n");
}

#[test]
fn gate_is_per_row_while_attribute_state_still_crosses_rows() {
    // A fully-linked wrapped row leaves hyperlink Some(7) as the map tail;
    // the NEXT row's blank cells must still reset it exactly as the ungated
    // encoder does — the gate is per-row, the attribute state is not.
    let cols = 4;
    let mut linked = Row::new(cols);
    for col in 0..cols {
        linked.cells[col].character = (b'a' + col as u8) as char;
        linked.cells[col].flags |= CellFlags::HYPERLINK;
        linked.extras.set_hyperlink(col, Some(7));
    }
    linked.wrapped = true;

    let rows = vec![linked, Row::new(cols)];
    assert_gate_equivalence(&rows, cols, "linked row + blank continuation");

    let mut gated = FlatStorage::new(cols, usize::MAX);
    gated.extend(rows);
    assert_eq!(row_text(&gated, 0), "abcd");
    assert_eq!(row_text(&gated, 1), " \n");
    assert_eq!(
        link_values(&gated, 0, 6),
        vec![Some(7), Some(7), Some(7), Some(7), None, None]
    );
}
