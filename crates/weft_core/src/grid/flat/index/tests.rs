//! Tests for the flat row index (ported from Warp `index_tests.rs` plus
//! weft-specific guards: Entry size budget and the RLE fast-path equivalence
//! property).

use std::num::NonZeroU16;

use super::super::testing::from_content;

use super::{ByteOffset, GraphemeInfo, GraphemeRun, GraphemeSizing, Index, Point};

fn ascii_grapheme_info() -> GraphemeInfo {
    GraphemeInfo {
        cell_width: 1,
        utf8_bytes: NonZeroU16::new(1).unwrap(),
    }
}

fn wide_grapheme_info() -> GraphemeInfo {
    GraphemeInfo {
        cell_width: 2,
        utf8_bytes: NonZeroU16::new(4).unwrap(),
    }
}

/// PLAN_S3 硬约束: index entries back every display row of scrollback; 24B
/// keeps 2-3 of them per cache line. Warp pins this with the
/// static_assertions crate (not a weft dependency) — same contract as a test.
#[test]
fn entry_stays_within_cache_line_budget() {
    assert_eq!(std::mem::size_of::<super::Entry>(), 24);
}

#[test]
fn index_with_empty_string() {
    // 1: \n
    let storage = from_content("\n", 5);
    assert_eq!(storage.index.rows.len(), 1);
}

#[test]
fn index_with_consistent_one_byte_length_and_cell_width() {
    // 1: abcde
    // 2: fgh\n
    let storage = from_content("abcdefgh\n", 5);
    assert_eq!(storage.index.rows.len(), 2);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(5).unwrap(),
            info: ascii_grapheme_info(),
        })
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(5)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(3).unwrap(),
            info: ascii_grapheme_info(),
        })
    );
}

#[test]
fn index_with_consistent_two_cell_width_and_four_byte_length() {
    // 1: 😀😃😄😁
    // 2: 😆😅😂\n
    let storage = from_content("😀😃😄😁😆😅😂\n", 8);
    assert_eq!(storage.index.rows.len(), 2);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(4).unwrap(),
            info: wide_grapheme_info(),
        })
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(16)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(3).unwrap(),
            info: wide_grapheme_info(),
        })
    );
}

#[test]
fn index_with_grapheme_overflowing_end_of_row() {
    // 1: 😀😃
    // 2: 😄\n
    let storage = from_content("😀😃😄\n", 5);
    assert_eq!(storage.index.rows.len(), 2);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(2).unwrap(),
            info: wide_grapheme_info(),
        })
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(8)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(1).unwrap(),
            info: wide_grapheme_info(),
        })
    );
}

#[test]
fn index_with_grapheme_overflowing_nonfull_row_marks_leading_spacer() {
    // weft push encoding wraps wide chars whole (print-path semantics), so
    // the leading-spacer slot decision only surfaces when Index::rebuild
    // re-segments wrapped content. "abcd😀\n" at 5 cols pushes as
    // "abcd"(wrapped) + "😀\n"; rebuilding at the same width must decide —
    // again — that the wide char leaves a spacer slot at the end of row 0.
    let mut storage = from_content("abcd😀\n", 5);
    assert_eq!(storage.index.rows.len(), 2);
    assert!(!storage.index.rows[0].ends_with_leading_wide_char_spacer);

    let rebuilt = Index::rebuild(&storage.index, 5);
    storage.index = rebuilt;

    assert!(storage.index.rows[0].ends_with_leading_wide_char_spacer);
    assert!(!storage.index.rows[0].has_trailing_newline);
    assert!(storage.index.rows[1].has_trailing_newline);
    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(4)
    );

    // The spacer slot materializes as a blank trailing cell.
    let r0 = storage.get(0).expect("row 0");
    assert_eq!(r0.cells[4].character, ' ');
    assert!(r0.wrapped);
}

#[test]
fn index_with_inconsistent_cell_widths() {
    // 1: 😀a😃
    // 2: 😄\n
    let storage = from_content("😀a😃😄\n", 5);
    assert_eq!(storage.index.rows.len(), 2);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::NonUniform
    );
    let grapheme_runs = storage
        .index
        .grapheme_sizing
        .get(&ByteOffset::zero())
        .expect("index should have grapheme run info");
    assert_eq!(grapheme_runs.len(), 3);
    assert_eq!(
        grapheme_runs[0],
        GraphemeRun {
            count: NonZeroU16::new(1).unwrap(),
            info: wide_grapheme_info(),
        }
    );
    assert_eq!(
        grapheme_runs[1],
        GraphemeRun {
            count: NonZeroU16::new(1).unwrap(),
            info: ascii_grapheme_info(),
        }
    );
    assert_eq!(
        grapheme_runs[2],
        GraphemeRun {
            count: NonZeroU16::new(1).unwrap(),
            info: wide_grapheme_info(),
        }
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(9)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(1).unwrap(),
            info: wide_grapheme_info(),
        })
    );
}

#[test]
fn index_with_newlines() {
    // 1: abc\n
    // 2: defgh
    let storage = from_content("abc\ndefgh", 5);
    assert_eq!(storage.index.rows.len(), 2);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(3).unwrap(),
            info: ascii_grapheme_info(),
        })
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(4)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(5).unwrap(),
            info: ascii_grapheme_info(),
        })
    );
}

#[test]
fn index_with_repeated_newlines() {
    // 1: abc\n
    // 2: \n
    // 3: defgh
    let storage = from_content("abc\n\ndefgh", 5);
    assert_eq!(storage.index.rows.len(), 3);

    assert_eq!(storage.index.rows[0].content_offset, ByteOffset::zero());
    assert_eq!(
        storage.index.rows[0].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(3).unwrap(),
            info: ascii_grapheme_info(),
        })
    );

    assert_eq!(
        storage.index.rows[1].content_offset,
        ByteOffset::from_usize(4)
    );
    assert_eq!(
        storage.index.rows[1].grapheme_sizing,
        GraphemeSizing::EmptyRow
    );

    assert_eq!(
        storage.index.rows[2].content_offset,
        ByteOffset::from_usize(5)
    );
    assert_eq!(
        storage.index.rows[2].grapheme_sizing,
        GraphemeSizing::Uniform(GraphemeRun {
            count: NonZeroU16::new(5).unwrap(),
            info: ascii_grapheme_info(),
        })
    );
}

#[test]
fn index_with_exactly_full_row() {
    // 1: abc
    let storage = from_content("abc", 3);
    assert_eq!(storage.index.rows.len(), 1);
    assert_eq!(storage.index.content_len, 3);
}

#[test]
fn index_with_full_row_and_newline() {
    // The newline shouldn't start a new row; it only decides whether the
    // row soft- or hard-wraps.
    //
    // 1: abc\n
    let storage = from_content("abc\n", 3);
    assert_eq!(storage.index.rows.len(), 1);
    assert_eq!(storage.index.content_len, 4);

    // 1: abc
    // 2: d\n
    let storage = from_content("abcd\n", 3);
    assert_eq!(storage.index.rows.len(), 2);
    assert_eq!(storage.index.content_len, 5);
}

#[test]
fn push_extra_row_onto_index() {
    // 1: abc\n
    let mut storage = from_content("abc\n", 5);
    assert_eq!(storage.index.rows.len(), 1);

    // 1: abc\n
    // 2: def\n
    storage.push_rows_from_string("def\n");
    assert_eq!(storage.index.rows.len(), 2);
}

#[test]
fn push_extra_row_onto_index_with_softwrapped_first_line() {
    // 1: abcde
    let mut storage = from_content("abcde", 5);
    assert_eq!(storage.index.rows.len(), 1);

    // 1: abcde
    // 2: 123\n
    storage.push_rows_from_string("123\n");
    assert_eq!(storage.index.rows.len(), 2);
}

mod offset_point_conversion {
    use super::*;

    #[test]
    fn normal_cell() {
        // 1: 😀😃
        // 2: 😄\n
        // 3: a😄\n
        let storage = from_content("😀😃😄\na😄\n", 5);

        let original_point = Point { row: 2, col: 0 };

        // T5: content_offset_at_point died with the marker dance — the
        // cursor anchors walk runs row-locally. Feed the hand-computed
        // offset and check the to_point mapping instead.
        let offset = ByteOffset::from_usize(13);

        let point = storage
            .index
            .content_offset_to_point(offset)
            .expect("should be able to convert offset back to point");
        assert_eq!(point, original_point);
    }

    #[test]
    fn wide_char() {
        // 1: 😀😃
        // 2: 😄\n
        // 3: a😄\n
        let storage = from_content("😀😃😄\na😄\n", 5);

        let original_point = Point { row: 0, col: 2 };

        // T5: content_offset_at_point died with the marker dance — the
        // cursor anchors walk runs row-locally. Feed the hand-computed
        // offset and check the to_point mapping instead.
        let offset = ByteOffset::from_usize(4);

        let point = storage
            .index
            .content_offset_to_point(offset)
            .expect("should be able to convert offset back to point");
        assert_eq!(point, original_point);
    }

    #[test]
    fn nonuniform_row() {
        // 1: 😀😃
        // 2: 😄\n
        // 3: a😄\n
        let storage = from_content("😀😃😄\na😄\n", 5);

        let original_point = Point { row: 2, col: 1 };

        // T5: content_offset_at_point died with the marker dance — the
        // cursor anchors walk runs row-locally. Feed the hand-computed
        // offset and check the to_point mapping instead.
        let offset = ByteOffset::from_usize(14);

        let point = storage
            .index
            .content_offset_to_point(offset)
            .expect("should be able to convert offset back to point");
        assert_eq!(point, original_point);
    }
}

/// RLE 快路径等价性（R3 止损的 T1 必做项）: bulk run consumption must produce
/// byte-identical index state to the grapheme-by-grapheme slow path, at every
/// probed width.
#[test]
fn rebuild_rle_fast_path_matches_grapheme_by_grapheme_path() {
    let storage = from_content("😀a😃😄\nb中cdef\n\nghijklm\nopq\n", 8);
    let old = &storage.index;

    for columns in [2usize, 3, 5, 8, 11, 20] {
        let fast = Index::rebuild(old, columns);
        let slow = Index::rebuild_without_rle_fast_path(old, columns);

        assert_eq!(
            fast.rows, slow.rows,
            "fast vs slow entries diverged at width {columns}"
        );
        assert_eq!(fast.content_len, slow.content_len, "width {columns}");
        assert_eq!(
            fast.grapheme_sizing, slow.grapheme_sizing,
            "fast vs slow non-uniform runs diverged at width {columns}"
        );
    }
}
