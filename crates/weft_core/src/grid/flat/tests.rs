//! FlatStorage-level tests: Warp `mod_tests.rs` ports plus the weft-specific
//! suite required by PLAN_S3 T1 (CJK wide pairs across rebuild, RowExtras
//! cluster byte round-trips, wavy underline + SGR 58 through eviction,
//! hyperlink soft-wrap splits, position/clear/index_since semantics,
//! eviction ordering, never-re-zeroing offsets, and rebuild reference
//! equivalence at arbitrary widths).

use std::sync::Arc;

use crate::grid::cell::{Cell, CellColor, CellFlags, CellWidth, Color, UnderlineStyle};
use crate::grid::row::Row;

use super::testing::{assert_rows_equal, to_rows};
use super::FlatStorage;

/// Materializes every retained row into a Vec (0 = oldest).
fn collect_rows(storage: &FlatStorage) -> Vec<Row> {
    (0..storage.len())
        .map(|i| storage.get(i).expect("row index must be in bounds"))
        .collect()
}

/// A single row holding one repeated character, for position/eviction tests.
fn row_of(c: char, columns: usize) -> Row {
    let mut row = Row::new(columns);
    row.cells[0].character = c;
    row
}

// ── Warp mod_tests.rs ports ─────────────────────────────────────────────

#[test]
fn row_iteration() {
    // 1: hello w
    // 2: orld\n
    let storage = super::testing::from_content("hello world\n", 7);

    let row1 = storage.get(0).expect("first row must exist");
    assert_eq!(row1.cells[0].character, 'h');
    assert_eq!(row1.cells[6].character, 'w');

    let row2 = storage.get(1).expect("second row must exist");
    assert_eq!(row2.cells[0].character, 'o');
    assert_eq!(row2.cells[3].character, 'd');

    assert!(storage.get(2).is_none());
}

#[test]
fn row_with_double_width_char() {
    // 1: hi 😀
    // 2: hello\n
    let storage = super::testing::from_content("hi 😀 hello\n", 6);

    let row1 = storage.get(0).expect("first row must exist");
    assert_eq!(row1.cells[0].character, 'h');
    assert_eq!(row1.cells[3].character, '😀');
    assert_eq!(row1.cells[3].width, CellWidth::Full);
    assert!(row1.cells[4].flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(row1.cells[5].character, ' ');

    let row2 = storage.get(1).expect("second row must exist");
    assert_eq!(row2.cells[0].character, 'h');

    assert!(storage.get(2).is_none());
}

#[test]
fn push_rows_with_color() {
    let mut storage = FlatStorage::new(5, usize::MAX);

    let red_cell = Cell {
        character: 'r',
        fg: CellColor::Palette(1),
        ..Default::default()
    };

    let row = Row {
        cells: vec![
            Cell::default(),
            Cell::default(),
            red_cell,
            red_cell,
            Cell::default(),
        ],
        dirty_occ: 0,
        wrapped: false,
        extras: Default::default(),
    };
    storage.push(row.clone());

    let flat = storage.get(0).expect("row must exist");
    assert_rows_equal(&[flat], &[row], "colored row roundtrip");
}

#[test]
fn push_rows_with_color_and_multibyte_chars() {
    let mut storage = FlatStorage::new(5, usize::MAX);

    let red_cell = Cell {
        character: 'r',
        fg: CellColor::Palette(1),
        ..Default::default()
    };

    let multibyte_cell = Cell {
        character: '❤',
        ..Default::default()
    };

    let row = Row {
        cells: vec![
            multibyte_cell,
            multibyte_cell,
            red_cell,
            red_cell,
            multibyte_cell,
        ],
        dirty_occ: 0,
        wrapped: false,
        extras: Default::default(),
    };
    storage.push(row.clone());

    let flat = storage.get(0).expect("row must exist");
    assert_rows_equal(&[flat], &[row], "multibyte colored row roundtrip");
}

#[test]
fn row_roundtrip_and_resize() {
    let num_cols = 5;
    let s = "😀😃😄ag\na😁😆~!!\n😅sdf😂\n";
    let rows = to_rows(s, num_cols);

    let mut storage = FlatStorage::new(num_cols, usize::MAX);
    storage.extend(rows.clone());

    assert_rows_equal(
        &collect_rows(&storage),
        &rows,
        "roundtrip at original width",
    );

    // Same-width "resize" is a no-op that must not disturb anything.
    storage.set_columns(num_cols);
    assert_rows_equal(&collect_rows(&storage), &rows, "no-op resize");

    // Re-wrapping at a different width and back must reproduce the original
    // layout (rebuild is a pure re-segmentation of the same bytes).
    storage.set_columns(7);
    let wide = to_rows(s, 7);
    assert_rows_equal(&collect_rows(&storage), &wide, "rebuild at 7 columns");

    storage.set_columns(num_cols);
    assert_rows_equal(&collect_rows(&storage), &rows, "rebuild back to 5 columns");
}

#[test]
fn styling_change_within_trailing_empty_cells() {
    let num_cols = 5;
    let mut rows = to_rows("a\nb\n", num_cols);

    // Make the final cell in the first row bold: it is a default (blank)
    // cell whose only difference is styling, so it must still occupy a byte
    // to keep offsets column-aligned — and its style must not leak into the
    // next row.
    rows[0].cells[num_cols - 1].flags |= CellFlags::BOLD;

    let mut storage = FlatStorage::new(num_cols, usize::MAX);
    storage.extend(rows.clone());

    let flat_rows = collect_rows(&storage);
    assert_rows_equal(&flat_rows, &rows, "bold trailing blank roundtrip");

    assert!(!flat_rows[0].wrapped);
    assert!(flat_rows[0].cells[num_cols - 1]
        .flags
        .contains(CellFlags::BOLD));
    assert!(!flat_rows[1].cells[0].flags.contains(CellFlags::BOLD));
}

#[test]
fn clear_after_truncate_front() {
    let num_cols = 20;
    let rows = to_rows("abcd\n789\n1 overflow\n2 overflow\n", num_cols);

    let mut storage = FlatStorage::new(num_cols, 2);
    storage.extend(rows);

    // We pushed 4 rows, and the limit is 2, so 2 rows were truncated.
    assert_eq!(storage.len(), 2);
    assert_eq!(storage.num_truncated_rows(), 2);

    // The truncated rows are the ones we expect (oldest evicted first).
    assert_eq!(storage.get(0).expect("row").cells[0].character, '1');
    assert_eq!(storage.get(1).expect("row").cells[0].character, '2');

    // Clear: rows dropped, but the truncation counter is monotonic.
    storage.clear();
    assert_eq!(storage.len(), 0);
    assert_eq!(storage.num_truncated_rows(), 2);

    // New rows keep appending with the same eviction order.
    storage.extend(to_rows("abcd\n789\n1 overflow\n2 overflow\n", num_cols));
    assert_eq!(storage.len(), 2);
    assert_eq!(storage.num_truncated_rows(), 4);
    assert_eq!(storage.get(0).expect("row").cells[0].character, '1');
    assert_eq!(storage.get(1).expect("row").cells[0].character, '2');
}

#[test]
fn clear_after_truncate_front_then_resize_and_push_does_not_panic() {
    let old_cols = 20;
    let new_cols = 21;
    let rows = to_rows(&"abcdefghijklmnopqrst\n".repeat(100), old_cols);

    let mut storage = FlatStorage::new(old_cols, 1);
    storage.extend(rows);
    assert_eq!(storage.len(), 1);

    storage.clear();
    storage.set_columns(new_cols);

    storage.push_rows_from_string("new output\n");

    let row = storage.get(0).expect("row after clear + resize + push");
    assert_eq!(row.cells[0].character, 'n');
}

#[test]
fn wide_char_hyperlink_spacer_survives_roundtrip() {
    let mut storage = FlatStorage::new(5, usize::MAX);

    let mut wide_row = Row::new(5);
    wide_row.cells[0].character = '😀';
    wide_row.cells[0].width = CellWidth::Full;
    wide_row.cells[0].flags |= CellFlags::HYPERLINK;
    wide_row.cells[1].flags |= CellFlags::WIDE_SPACER | CellFlags::HYPERLINK;
    wide_row.extras.set_hyperlink(0, Some(3));
    wide_row.extras.set_hyperlink(1, Some(3));

    storage.push(wide_row);

    let flat = storage.get(0).expect("row must exist");
    assert_eq!(flat.cells[0].character, '😀');
    assert_eq!(flat.cells[0].width, CellWidth::Full);
    assert!(flat.cells[0].flags.contains(CellFlags::HYPERLINK));
    assert_eq!(flat.extras.hyperlink_id_at(0), Some(3));
    // Both halves of the wide glyph must stay hoverable after
    // rematerialization, not just the leading cell.
    assert!(flat.cells[1].flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(flat.extras.hyperlink_id_at(1), Some(3));
}

#[test]
fn blank_cells_before_hyperlink_are_not_clickable() {
    let mut storage = FlatStorage::new(5, usize::MAX);

    let mut linked_row = Row::new(4);
    linked_row.cells[2].character = 'a';
    linked_row.cells[2].flags |= CellFlags::HYPERLINK;
    linked_row.extras.set_hyperlink(2, Some(5));

    // Two blank cells precede the linked cell. They are backfilled when the
    // row is flattened, and must not inherit the hyperlink id.
    storage.push(linked_row);

    let flat = storage.get(0).expect("row must exist");
    assert_eq!(flat.extras.hyperlink_id_at(0), None);
    assert_eq!(flat.extras.hyperlink_id_at(1), None);
    assert_eq!(flat.cells[2].character, 'a');
    assert_eq!(flat.extras.hyperlink_id_at(2), Some(5));
    assert!(flat.cells[2].flags.contains(CellFlags::HYPERLINK));
}

// ── weft 专项 (PLAN_S3 T1) ──────────────────────────────────────────────

#[test]
fn rebuild_splits_cjk_wide_pair_across_lines() {
    // Original physical row (4 cols): a 中 b — exactly full.
    let rows = to_rows("a中b\n", 4);
    let mut storage = FlatStorage::new(4, usize::MAX);
    storage.extend(rows.clone());

    // Narrow to 2: 'a' wraps onto its own line (trailing blank cell),
    // '中' fills the next line entirely, 'b' follows.
    storage.set_columns(2);

    assert_eq!(storage.len(), 3, "a中b at width 2 = three display rows");
    let r0 = storage.get(0).expect("row 0");
    assert_eq!(r0.cells[0].character, 'a');
    assert_eq!(r0.cells[1].character, ' ', "the unfilled slot stays blank");
    assert!(r0.wrapped);

    let r1 = storage.get(1).expect("row 1");
    assert_eq!(r1.cells[0].character, '中');
    assert_eq!(r1.cells[0].width, CellWidth::Full);
    assert!(r1.cells[1].flags.contains(CellFlags::WIDE_SPACER));
    assert!(r1.wrapped);

    let r2 = storage.get(2).expect("row 2");
    assert_eq!(r2.cells[0].character, 'b');
    assert!(!r2.wrapped);

    // Widening back reproduces the original physical row.
    storage.set_columns(4);
    assert_rows_equal(&collect_rows(&storage), &rows, "CJK widen-back roundtrip");
}

#[test]
fn row_extras_cluster_bytes_roundtrip() {
    let mut storage = FlatStorage::new(6, usize::MAX);

    let mut row = Row::new(6);
    // Narrow multi-scalar cluster at col 1.
    row.cells[1].character = 'e';
    row.cells[1].flags |= CellFlags::EXTRA;
    row.extras.set_grapheme(1, Arc::from("e\u{0301}"));
    // Wide ZWJ cluster at col 3 (occupies 3+4).
    row.cells[3].character = '👩';
    row.cells[3].width = CellWidth::Full;
    row.cells[3].flags |= CellFlags::EXTRA;
    row.cells[4].flags |= CellFlags::WIDE_SPACER;
    row.extras.set_grapheme(3, Arc::from("👩\u{200d}🔬"));
    row.cells[5].character = '!';

    storage.push(row.clone());

    // Whole-cluster bytes must replace the base scalar's bytes on encode and
    // reconstruct the identical RowExtras on decode.
    let flat = storage.get(0).expect("row must exist");
    assert_rows_equal(&[flat], &[row], "cluster byte roundtrip");
}

#[test]
fn wavy_underline_and_sgr58_survive_push_evict_get() {
    // 评审 P1-3 专项: the underline slots (style + SGR 58 color) must
    // survive encode → retention eviction → materialize.
    let cols = 5;

    let mut decorated = Row::new(cols);
    decorated.cells[0].character = 'x';
    decorated.cells[0].flags |= CellFlags::BOLD;
    decorated.cells[0].underline_style = UnderlineStyle::Wavy;
    decorated.cells[0].underline_color = Some(CellColor::Palette(4));
    decorated.cells[2].character = 'y';
    decorated.cells[2].underline_style = UnderlineStyle::Dashed;
    decorated.cells[2].underline_color = Some(CellColor::Rgb(Color::rgb(10, 20, 30)));
    decorated.cells[2].flags |= CellFlags::ITALIC;

    let mut plain = Row::new(cols);
    plain.cells[1].character = 'p';

    let rows = vec![decorated.clone(), plain.clone(), decorated, plain];
    let mut storage = FlatStorage::new(cols, 2);
    storage.extend(rows.clone());

    assert_eq!(storage.num_truncated_rows(), 2);
    // 评审 P2 闭环: the underline slots must also survive an Index::rebuild —
    // attributes are keyed by byte offset, rebuild only re-splits rows, so a
    // narrow rewrap between eviction and read must not disturb them.
    storage.set_columns(3);
    let kept = collect_rows(&storage);
    // Narrowing 5→3 rewraps "x y " (the 4th byte is the attribute-reset blank
    // that keeps the Dashed run from bleeding into trailing defaults) into
    // "x y" + a ghost blank continuation — attribute changes need byte
    // positions, so an exactly-fitting row grows one continuation row
    // (Warp-faithful attribute-map mechanics). The P1-3 assertion is the
    // STYLE SLOTS surviving eviction + Index::rebuild, not row geometry.
    assert_eq!(kept.len(), 3, "decorated + ghost continuation + plain");
    let deco = &kept[0];
    assert_eq!(deco.cells[0].character, 'x');
    assert_eq!(
        (deco.cells[0].underline_style, deco.cells[0].underline_color),
        (UnderlineStyle::Wavy, Some(CellColor::Palette(4))),
        "wavy slot after rebuild"
    );
    assert_eq!(deco.cells[2].character, 'y');
    assert_eq!(
        (deco.cells[2].underline_style, deco.cells[2].underline_color),
        (
            UnderlineStyle::Dashed,
            Some(CellColor::Rgb(Color::rgb(10, 20, 30)))
        ),
        "dashed + SGR 58 slot after rebuild"
    );
    assert_eq!(deco.cells[0].flags & CellFlags::BOLD, CellFlags::BOLD);
    assert_eq!(deco.cells[2].flags & CellFlags::ITALIC, CellFlags::ITALIC);
    let clean =
        |c: &Cell| c.underline_style == UnderlineStyle::Single && c.underline_color.is_none();
    assert!(
        kept[1].cells.iter().all(clean),
        "ghost continuation is style-clean"
    );
    assert!(kept[2].cells.iter().all(clean), "plain row is style-clean");
    assert_eq!(kept[2].cells[1].character, 'p');
}

#[test]
fn hyperlink_survives_soft_wrap_split_and_rebuild() {
    // One logical line soft-wrapped into two physical rows, the content
    // cells linked — the OSC 8 span crosses a row boundary. r1's trailing
    // blank cells are unlinked, so the encode records the link reset there
    // (a reset forces one blank byte — that is how the boundary survives).
    // Content: "abcd" + "ef" + reset-blank + "\n".
    let cols = 4;
    let mut r0 = Row::new(cols);
    let mut r1 = Row::new(cols);
    for col in 0..cols {
        let cell = &mut r0.cells[col];
        cell.character = (b'a' + col as u8) as char;
        cell.flags |= CellFlags::HYPERLINK;
        r0.extras.set_hyperlink(col, Some(7));
    }
    for col in 0..2 {
        let cell = &mut r1.cells[col];
        cell.character = (b'e' + col as u8) as char;
        cell.flags |= CellFlags::HYPERLINK;
        r1.extras.set_hyperlink(col, Some(7));
    }
    r0.wrapped = true;

    let mut storage = FlatStorage::new(cols, usize::MAX);
    storage.extend([r0.clone(), r1.clone()]);

    // Identity re-segmentation round-trips both physical rows exactly.
    let materialized = collect_rows(&storage);
    assert_rows_equal(&materialized, &[r0, r1], "hyperlink identity rebuild");

    // Re-wrap at 3 columns: the byte stream splits as abc / def / blank —
    // both content fragments stay fully linked, and the trailing blank row
    // carries no link.
    storage.set_columns(3);

    let materialized = collect_rows(&storage);
    assert_eq!(materialized.len(), 3);
    assert!(materialized[0].wrapped);
    assert!(materialized[1].wrapped);
    assert!(!materialized[2].wrapped);
    for row in &materialized[0..2] {
        for col in 0..3 {
            assert_eq!(
                row.extras.hyperlink_id_at(col),
                Some(7),
                "fragmented hyperlink must stay clickable at ({col})"
            );
            assert!(row.cells[col].flags.contains(CellFlags::HYPERLINK));
        }
    }
    // The reset byte's row: a single blank, unlinked cell.
    assert_eq!(materialized[2].cells[0].character, ' ');
    assert_eq!(materialized[2].extras.hyperlink_id_at(0), None);
}

#[test]
fn position_clear_and_index_since_match_scrollback_semantics() {
    // Bit-for-bit transplant of scrollback.rs:96-108 behavior.
    let mut storage = FlatStorage::new(5, 3);
    for c in ['a', 'b', 'c', 'd', 'e'] {
        storage.push(row_of(c, 5));
    }
    assert_eq!(storage.position(), 5);
    assert_eq!(storage.len(), 3);

    // Oldest retained row is logical position 2.
    assert_eq!(storage.index_since(2), 0);
    assert_eq!(storage.index_since(4), 2);
    assert_eq!(storage.index_since(5), 3);
    assert_eq!(storage.index_since(6), 0, "future positions clamp to 0");

    // CSI 3J: rows dropped, anchors stay valid, position never rewinds.
    storage.clear();
    assert_eq!(storage.len(), 0);
    assert_eq!(storage.position(), 5, "clear must not reset position");
    assert_eq!(storage.index_since(0), 0);
    assert_eq!(storage.index_since(5), 0);

    storage.push(row_of('z', 5));
    assert_eq!(storage.position(), 6);
    assert_eq!(storage.index_since(5), 0);
    assert_eq!(storage.index_since(6), 1);
}

#[test]
fn zero_max_lines_makes_push_a_noop_like_scrollback() {
    // Scrollback::push returns before touching anything at max_lines == 0 —
    // position included. FlatStorage must match bit for bit.
    let mut storage = FlatStorage::new(5, 0);
    storage.extend(to_rows("abc\n", 5));
    assert_eq!(storage.len(), 0);
    assert_eq!(storage.position(), 0);
    assert_eq!(storage.num_truncated_rows(), 0);
}

#[test]
fn set_max_lines_shrink_keeps_newest_rows() {
    let mut storage = FlatStorage::new(5, usize::MAX);
    storage.push_rows_from_string("11111\n22222\n33333\n");
    assert_eq!(storage.position(), 3);

    // Shrink keeps the newest rows (contiguous suffix ending at position).
    storage.set_max_lines(1, 5);
    assert_eq!(storage.len(), 1);
    assert_eq!(storage.get(0).expect("row").cells[0].character, '3');
    assert_eq!(storage.position(), 3);

    // Growing keeps what's there.
    storage.set_max_lines(10, 5);
    assert_eq!(storage.max_lines(), 10);
    assert_eq!(storage.len(), 1);

    // Disabling drops the rest but keeps the anchor.
    storage.set_max_lines(0, 5);
    assert_eq!(storage.len(), 0);
    assert_eq!(storage.position(), 3);
}

#[test]
fn apply_max_rows_evicts_oldest_first_and_counts_monotonically() {
    let mut storage = FlatStorage::new(5, 2);
    storage.extend(to_rows("11111\n22222\n33333\n44444\n55555\n", 5));

    assert_eq!(storage.len(), 2);
    assert_eq!(storage.num_truncated_rows(), 3);
    assert_eq!(storage.get(0).expect("row").cells[0].character, '4');
    assert_eq!(storage.get(1).expect("row").cells[0].character, '5');

    // clear() does not reset the counter.
    storage.clear();
    assert_eq!(storage.num_truncated_rows(), 3);

    // Counter keeps growing across clear + re-push.
    storage.extend(to_rows("66666\n77777\n88888\n", 5));
    assert_eq!(storage.num_truncated_rows(), 4);
    assert_eq!(storage.get(0).expect("row").cells[0].character, '7');
    assert_eq!(storage.get(1).expect("row").cells[0].character, '8');
}

#[test]
fn content_offsets_survive_front_eviction_and_clear() {
    // D3: offset 永不归零 — eviction moves the tail pointer only, and every
    // later push continues from the same high-water mark.
    let mut storage = FlatStorage::new(5, usize::MAX);
    storage.push_rows_from_string("aaaaa\n");
    let first_end = storage.content.end_offset();
    assert_eq!(first_end, 6);

    storage.push_rows_from_string("bbbbb\n");
    let second_end = storage.content.end_offset();
    assert!(second_end > first_end);

    // Evict the oldest row: surviving offsets are unchanged.
    storage.truncate_rows_front(1);
    assert_eq!(storage.content.end_offset(), second_end);

    // New pushes continue forward from the high-water mark.
    storage.push_rows_from_string("ccccc\n");
    let before_clear = storage.content.end_offset();
    assert!(before_clear > second_end);

    // clear() trims the content tail back to the first surviving row
    // boundary (front-evicted offsets stay valid; the head pointer never
    // returns to zero).
    storage.clear();
    assert_eq!(storage.len(), 0);
    assert_eq!(
        storage.content.end_offset(),
        first_end,
        "tail trim lands on a row boundary, never zero"
    );

    storage.push_rows_from_string("ddddd\n");
    assert!(storage.content.end_offset() > first_end);

    // And materialization still works after all of it.
    assert_eq!(
        storage.get(0).expect("row").cells[0].character,
        'd',
        "content readable after eviction + clear + re-push"
    );
}

#[test]
fn materialize_all_drains_and_preserves_position_for_reencode() {
    // T2 resize bridge contract: rows come out owned, the storage drains,
    // and position/counters stay monotonic so anchors survive the reflow.
    let mut storage = FlatStorage::new(5, usize::MAX);
    storage.push_rows_from_string("aaaaa\nbbbbb\n");
    let position_before = storage.position();

    let rows = storage.materialize_all();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cells[0].character, 'a');
    assert_eq!(rows[1].cells[0].character, 'b');
    assert!(
        rows[0].cells[1].flags.contains(CellFlags::DIRTY),
        "written cells keep the write-marker"
    );

    assert_eq!(storage.len(), 0);
    assert_eq!(
        storage.position(),
        position_before,
        "drain must not rewind the anchor"
    );

    // The drained rows re-encode cleanly (what resize Phase 4 does).
    storage.extend(rows);
    assert_eq!(storage.len(), 2);
    assert_eq!(
        storage.get(1).expect("row").cells[0].character,
        'b',
        "re-encoded history reads back identically"
    );
}

#[test]
fn replace_row_reencodes_in_place_and_keeps_neighbors() {
    // T2 transitional get_mut adapter: an in-place edit must preserve every
    // other row exactly and keep offsets monotonic (never re-zeroed).
    let mut storage = FlatStorage::new(5, usize::MAX);
    storage.push_rows_from_string("11111\n22222\n33333\n");
    let end_before = storage.content.end_offset();

    let mut edited = storage.get(1).expect("row 1");
    edited.cells[0].character = 'X';
    edited.cells[0].flags |= CellFlags::BOLD;
    storage.replace_row(1, edited);

    assert_eq!(storage.len(), 3);
    let r0 = storage.get(0).expect("row 0");
    let r1 = storage.get(1).expect("row 1");
    let r2 = storage.get(2).expect("row 2");
    assert_eq!(r0.cells[0].character, '1', "predecessor untouched");
    assert_eq!(r1.cells[0].character, 'X');
    assert!(r1.cells[0].flags.contains(CellFlags::BOLD), "edit lands");
    assert_eq!(r2.cells[0].character, '3', "successor untouched");

    assert!(
        storage.content.end_offset() >= end_before,
        "offsets never rewind"
    );

    // The whole storage still round-trips through the attribute maps.
    let all: Vec<Row> = (0..storage.len())
        .map(|i| storage.get(i).expect("in bounds"))
        .collect();
    assert_eq!(all[2].cells[1].character, '3');
}

#[test]
fn rebuild_line_breaking_matches_reference_at_arbitrary_widths() {
    // Index::rebuild 折行等价: for every probed width, the re-segmented
    // index must materialize exactly what the independent print-path
    // reference layout produces.
    let s = "ab中cd😁e\nf😀gh\n\nij\n";
    let mut storage = FlatStorage::new(9, usize::MAX);
    storage.extend(to_rows(s, 9));

    for w in 2..=10usize {
        storage.set_columns(w);
        let expected = to_rows(s, w);
        let materialized = collect_rows(&storage);
        assert_rows_equal(&materialized, &expected, &format!("rebuild at width {w}"));
    }
}
