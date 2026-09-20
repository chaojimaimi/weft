//! Row index over the flat content stream (ported from Warp `index.rs`).
//!
//! Maps display row → content byte range. Entries are small (a size test
//! pins 24B) and grapheme sizing is run-length encoded, so plain ASCII rows
//! cost one stack-resident entry and nothing else. [`Index::rebuild`]
//! (rebuild.rs) re-wraps rows at a new column count without copying a single
//! content byte — that is the entire reflow story of PLAN_S3 §二 D4.
//!
//! Content offsets are absolute into the ever-growing stream (see
//! `super::content`): front eviction changes the tail pointer only, so
//! every offset-keyed structure stays stable.

/// pub(crate) so `EntryBuilder`'s effective visibility matches
/// `Index::start_row`'s (flat subtree) — avoids the `private_interfaces`
/// lint for the return type.
pub(crate) mod rebuild;

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU16;
use std::ops::Range;

use thiserror::Error;

use super::content::ByteOffset;

/// Absolute (row, col) address inside the flat index. weft has no global
/// grid Point type, so flat defines the minimal shape it needs (Warp:
/// `model::Point`).
#[allow(dead_code)] // T5: resize protocol (D4 cursor-offset mapping) is the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Point {
    pub row: usize,
    pub col: usize,
}

/// Per-display-row index entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Entry {
    /// Content offset at which this row's bytes begin.
    content_offset: ByteOffset,
    /// Grapheme sizing for this row (see [`GraphemeSizing`]).
    grapheme_sizing: GraphemeSizing,
    /// Whether the row's backing content includes a trailing newline —
    /// `false` marks a soft-wrapped continuation (weft: `Row.wrapped`).
    pub has_trailing_newline: bool,
    /// Whether the row ends with a leading wide-char spacer (the next row
    /// starts with a wide char that didn't fit here). Only ever set by
    /// [`Index::rebuild`]: weft's push encoding wraps wide chars whole at
    /// print time, so pushed rows never contain one. Materialization renders
    /// the slot as a plain blank cell, which is exactly the visual result.
    pub ends_with_leading_wide_char_spacer: bool,
}

/// Run-length-encoded grapheme sizing information.
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) enum GraphemeSizing {
    /// All graphemes in the row share one sizing.
    Uniform(GraphemeRun),
    /// Mixed sizing; details live in [`Index::grapheme_sizing`], keyed by the
    /// row's content offset so front eviction never invalidates them.
    NonUniform,
    /// The row contains no graphemes.
    EmptyRow,
}

/// A run of consecutive graphemes with identical sizing.
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct GraphemeRun {
    count: NonZeroU16,
    info: GraphemeInfo,
}

impl GraphemeRun {
    fn cols(&self) -> usize {
        self.count.get() as usize * self.info.cell_width as usize
    }
}

/// Sizing metadata for a single grapheme.
#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct GraphemeInfo {
    /// Terminal columns this grapheme occupies (1 or 2 in practice).
    pub cell_width: u8,
    /// UTF-8 byte length.
    pub utf8_bytes: NonZeroU16,
}

type GraphemeRuns = Vec<GraphemeRun>;

/// The row index itself. The only flat-storage structure not keyed by
/// content offset (rows are a VecDeque, front-evictable by construction).
pub(crate) struct Index {
    rows: VecDeque<Entry>,
    columns: usize,
    /// Absolute offset of the end of the indexed content.
    content_len: usize,
    /// Non-uniform grapheme run details, keyed by row start offset.
    grapheme_sizing: BTreeMap<ByteOffset, GraphemeRuns>,
}

impl Index {
    /// Creates an empty index for the given column count.
    /// `initial_capacity` pre-sizes the row deque.
    pub(crate) fn new(columns: usize, initial_capacity: Option<usize>) -> Self {
        Self {
            rows: VecDeque::with_capacity(initial_capacity.unwrap_or_default()),
            columns,
            content_len: 0,
            grapheme_sizing: Default::default(),
        }
    }

    /// Truncates the index tail to `new_len` rows, returning the new content
    /// length for the other flat structures to trim to.
    pub(crate) fn truncate(&mut self, new_len: usize) -> ByteOffset {
        let Some(new_content_len) = self.content_offset_for_row(new_len) else {
            // Truncating past the end: nothing to do.
            return ByteOffset::from_usize(self.content_len);
        };

        self.rows.truncate(new_len);
        // Drop grapheme sizing metadata for the truncated rows.
        let _ = self.grapheme_sizing.split_off(&new_content_len);

        self.content_len = new_content_len.as_usize();

        new_content_len
    }

    /// Removes the first `count` rows, returning the new start offset for
    /// the remaining content.
    pub(crate) fn truncate_front(&mut self, count: usize) -> ByteOffset {
        let new_start_offset = match self.content_offset_for_row(count) {
            Some(offset) => offset,
            None => {
                if count > self.rows.len() {
                    tracing::warn!(
                        rows = self.rows.len(),
                        truncate_count = count,
                        "attempted to truncate more rows than exist in flat storage"
                    );
                }
                ByteOffset::from_usize(self.content_len)
            }
        };

        for _ in 0..count {
            self.rows.pop_front();
        }
        self.grapheme_sizing = self.grapheme_sizing.split_off(&new_start_offset);

        new_start_offset
    }

    /// Number of indexed rows.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Content byte offset for a (row, col) point.
    ///
    /// Errors when the point is out of bounds, or past the content cells of
    /// a row (e.g. a non-zero column in an empty row).
    #[allow(dead_code)] // T5: resize protocol (D4 step 3/5) is the caller.
    pub(crate) fn content_offset_at_point(
        &self,
        point: Point,
    ) -> Result<ByteOffset, ContentOffsetToPointError> {
        let entry =
            self.rows
                .get(point.row)
                .ok_or_else(|| ContentOffsetToPointError::RowOutOfBounds {
                    row: point.row,
                    max_row: self.rows.len().saturating_sub(1),
                })?;

        let runs = match &entry.grapheme_sizing {
            GraphemeSizing::Uniform(grapheme_run) => std::slice::from_ref(grapheme_run),
            GraphemeSizing::NonUniform => self
                .grapheme_sizing
                .get(&entry.content_offset)
                .ok_or(ContentOffsetToPointError::MissingGraphemeSizing {
                    content_offset: entry.content_offset,
                })?
                .as_slice(),
            GraphemeSizing::EmptyRow => {
                if point.col == 0 {
                    return Ok(entry.content_offset);
                }
                return Err(ContentOffsetToPointError::NonZeroColumnInEmptyRow {
                    row: point.row,
                    col: point.col,
                });
            }
        };

        let mut offset = entry.content_offset;
        let mut cols_remaining = point.col;

        for run in runs {
            if cols_remaining == 0 {
                break;
            }

            let cols_from_run = run.cols().min(cols_remaining);
            let graphemes_from_run = cols_from_run / run.info.cell_width as usize;

            offset += graphemes_from_run * run.info.utf8_bytes.get() as usize;
            cols_remaining -= cols_from_run;
        }

        if cols_remaining == 0 {
            return Ok(offset);
        }

        // The requested column exceeded the content-ful cells of this row.
        Err(ContentOffsetToPointError::ColumnExceedsContent {
            row: point.row,
            col: point.col,
        })
    }

    /// (row, col) point for a content byte offset.
    #[allow(dead_code)] // T5: resize protocol (D4 step 5) is the caller.
    pub(crate) fn content_offset_to_point(
        &self,
        offset: ByteOffset,
    ) -> Result<Point, PointFromContentOffsetError> {
        let partition = self
            .rows
            .partition_point(|entry| entry.content_offset <= offset);
        let row = match partition.checked_sub(1) {
            Some(r) => r,
            None => {
                let first_row_offset = self
                    .rows
                    .front()
                    .map(|e| e.content_offset)
                    .unwrap_or_default();
                return Err(PointFromContentOffsetError::OffsetBeforeFirstRow {
                    offset,
                    first_row_offset,
                });
            }
        };

        let entry = self
            .rows
            .get(row)
            .ok_or(PointFromContentOffsetError::RowOutOfBounds { row })?;

        let runs = match &entry.grapheme_sizing {
            GraphemeSizing::Uniform(grapheme_run) => std::slice::from_ref(grapheme_run),
            GraphemeSizing::NonUniform => self
                .grapheme_sizing
                .get(&entry.content_offset)
                .ok_or(PointFromContentOffsetError::MissingGraphemeSizing {
                    content_offset: entry.content_offset,
                })?
                .as_slice(),
            GraphemeSizing::EmptyRow => {
                // The only valid content offset for an empty row is its start.
                assert_eq!(offset, entry.content_offset);
                return Ok(Point { row, col: 0 });
            }
        };

        let mut column = 0;
        let mut remaining_offset = offset - entry.content_offset;

        for run in runs {
            let graphemes_in_run = run.cols() / run.info.cell_width as usize;
            let content_in_run =
                ByteOffset::from_usize(graphemes_in_run * run.info.utf8_bytes.get() as usize);

            let remaining_offset_in_run = remaining_offset.min(content_in_run);
            let remaining_graphemes_in_run =
                remaining_offset_in_run.as_usize() / run.info.utf8_bytes.get() as usize;
            let remaining_cells_in_run = remaining_graphemes_in_run * run.info.cell_width as usize;

            column += remaining_cells_in_run;
            remaining_offset -= remaining_offset_in_run;

            if remaining_offset == ByteOffset::zero() {
                return Ok(Point { row, col: column });
            }
        }

        Err(PointFromContentOffsetError::OffsetDoesNotMapToCellInRow { row, offset })
    }

    /// The content byte range backing one display row.
    pub(crate) fn content_range_for_row(&self, row: usize) -> Option<Range<ByteOffset>> {
        let start = self.content_offset_for_row(row)?;
        let end = self
            .content_offset_for_row(row + 1)
            .unwrap_or_else(|| ByteOffset::from_usize(self.content_len));
        Some(start..end)
    }

    fn content_offset_for_row(&self, row: usize) -> Option<ByteOffset> {
        Some(self.rows.get(row)?.content_offset)
    }

    pub(crate) fn get_entry(&self, row: usize) -> Option<&Entry> {
        self.rows.get(row)
    }

    /// Grapheme runs for one row (None when the row is out of bounds).
    fn grapheme_runs_for_row(&self, row_idx: usize) -> Option<&[GraphemeRun]> {
        let entry = self.get_entry(row_idx)?;

        let runs = match &entry.grapheme_sizing {
            GraphemeSizing::Uniform(grapheme_run) => std::slice::from_ref(grapheme_run),
            GraphemeSizing::NonUniform => {
                self.grapheme_sizing.get(&entry.content_offset)?.as_slice()
            }
            GraphemeSizing::EmptyRow => &[],
        };

        Some(runs)
    }

    /// Per-grapheme sizing for one row (None when out of bounds).
    pub(crate) fn grapheme_infos_for_row(
        &self,
        row_idx: usize,
    ) -> Option<impl Iterator<Item = GraphemeInfo> + '_> {
        let runs = self.grapheme_runs_for_row(row_idx)?;

        Some(runs.iter().flat_map(|run| {
            // MSRV 1.75: `std::iter::repeat_n` (1.82) is not available.
            std::iter::repeat(run.info).take(run.count.get() as usize)
        }))
    }
}

/// Errors from [`Index::content_offset_at_point`].
#[allow(dead_code)] // T5: resize protocol (D4) is the caller.
#[derive(Debug, Error)]
pub(crate) enum ContentOffsetToPointError {
    #[error("point row {row} is outside the bounds of the index (max: {max_row})")]
    RowOutOfBounds { row: usize, max_row: usize },
    #[error(
        "missing grapheme sizing data for non-uniform row at content offset {content_offset:?}"
    )]
    MissingGraphemeSizing { content_offset: ByteOffset },
    #[error("point column {col} is not 0 for empty row {row}")]
    NonZeroColumnInEmptyRow { row: usize, col: usize },
    #[error("point column {col} exceeds the number of content cells in row {row}")]
    ColumnExceedsContent { row: usize, col: usize },
}

/// Errors from [`Index::content_offset_to_point`].
#[allow(dead_code)] // T5: resize protocol (D4) is the caller.
#[derive(Debug, Error)]
pub(crate) enum PointFromContentOffsetError {
    #[error(
        "offset {offset:?} is before the start of the first row (first row starts at {first_row_offset:?})"
    )]
    OffsetBeforeFirstRow {
        offset: ByteOffset,
        first_row_offset: ByteOffset,
    },
    #[error("computed row index {row} is out of bounds")]
    RowOutOfBounds { row: usize },
    #[error(
        "missing grapheme sizing data for non-uniform row at content offset {content_offset:?}"
    )]
    MissingGraphemeSizing { content_offset: ByteOffset },
    #[error("content offset {offset:?} does not map to a cell in row {row}")]
    OffsetDoesNotMapToCellInRow { row: usize, offset: ByteOffset },
}

#[cfg(test)]
mod tests;
