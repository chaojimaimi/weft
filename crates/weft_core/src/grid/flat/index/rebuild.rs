//! `Index::rebuild` — re-wrap the row index at a new column count (ported
//! from Warp `index.rs`'s `EntryBuilder`). This is the reflow core of
//! PLAN_S3 §二 D4: the content byte stream is untouched; only row
//! segmentation is recomputed, so a width change costs O(graphemes) of index
//! work and zero content copies.
//!
//! The RLE fast path (process_ascii_run) is the T1-mandatory mitigation for
//! Warp's own TODO ("processing each grapheme individually is a clearly poor
//! choice for ASCII"): a uniform width-1, 1-byte run is bulk-consumed into
//! per-row slices instead of looping one grapheme at a time.

use std::mem;
use std::num::NonZeroU16;

use super::super::content::ByteOffset;
use super::super::grapheme::Grapheme;
use super::{Entry, GraphemeInfo, GraphemeRun, GraphemeSizing, Index};

impl Index {
    /// Starts a fresh [`EntryBuilder`] for the append (push-encoding) path.
    pub(crate) fn start_row(&mut self) -> EntryBuilder {
        EntryBuilder::new()
    }

    /// Rebuilds the index to wrap lines at a different number of columns.
    /// Content bytes and attribute maps are untouched.
    pub(crate) fn rebuild(old_index: &Index, columns: usize) -> Self {
        Self::rebuild_impl(old_index, columns, true)
    }

    #[cfg(test)]
    pub(crate) fn rebuild_without_rle_fast_path(old_index: &Index, columns: usize) -> Self {
        Self::rebuild_impl(old_index, columns, false)
    }

    fn rebuild_impl(old_index: &Index, columns: usize, allow_rle_fast_path: bool) -> Self {
        let mut index = Self::new(columns, Some(old_index.len()));
        // Start content where the surviving content starts (front eviction
        // must not rewind the offset space).
        index.content_len = old_index
            .rows
            .front()
            .map(|entry| entry.content_offset.as_usize())
            .unwrap_or(old_index.content_len);

        let mut entry_builder = EntryBuilder::new();
        let fast_path = allow_rle_fast_path && columns > 0;

        // Walk the old rows grapheme by grapheme (runs in bulk on the fast
        // path), re-emitting newlines for rows that hard-wrap.
        for row_idx in 0..old_index.len() {
            if let Some(runs) = old_index.grapheme_runs_for_row(row_idx) {
                for run in runs {
                    let ascii_uniform = run.info.cell_width == 1 && run.info.utf8_bytes.get() == 1;
                    if fast_path && ascii_uniform {
                        entry_builder.process_ascii_run(run.info, run.count, &mut index);
                    } else {
                        for _ in 0..run.count.get() {
                            entry_builder.process_grapheme_info(run.info, &mut index);
                        }
                    }
                }
            }
            if old_index
                .get_entry(row_idx)
                .expect("row should have an entry")
                .has_trailing_newline
            {
                entry_builder.process_grapheme(&Grapheme::newline(), &mut index);
            }
        }

        // Final entry — unless reflowing produced only trailing empty space,
        // which must not materialize as a phantom row.
        entry_builder.append_to_index_if_nonempty(&mut index);

        if index.content_len > old_index.content_len {
            tracing::error!("index rebuild produced more content than the source index");
        }

        index
    }
}

/// Accumulates one row's [`Entry`] while graphemes stream through.
#[derive(Default)]
pub(crate) struct EntryBuilder {
    num_cells: usize,
    incr_content_offset: ByteOffset,
    has_trailing_newline: bool,
    ends_with_leading_wide_char_spacer: bool,
    grapheme_runs: Vec<GraphemeRun>,
    #[cfg(debug_assertions)]
    was_processed: bool,
}

impl EntryBuilder {
    fn new() -> Self {
        Default::default()
    }

    /// Processes the next grapheme of a row. Newlines terminate the current
    /// entry (with a trailing newline) and start the next one.
    pub(crate) fn process_grapheme(&mut self, grapheme: &Grapheme, index: &mut Index) {
        if grapheme.starts_new_row() {
            self.add_trailing_newline();
            mem::take(self).append_to_index(index);
            return;
        }

        self.process_grapheme_info(grapheme.sizing_info(), index);
    }

    /// Processes the next grapheme, cutting the row when this grapheme no
    /// longer fits.
    fn process_grapheme_info(&mut self, info: GraphemeInfo, index: &mut Index) {
        let grapheme_len = info.utf8_bytes.get() as usize;
        debug_assert!(
            grapheme_len > 0,
            "should not process an empty string as a grapheme"
        );

        if info.cell_width == 0 {
            tracing::warn!("encountered unexpected grapheme with a computed cell width of zero");
            return;
        }
        debug_assert!(
            info.cell_width <= 2,
            "graphemes should not be more than two cells wide, but encountered one with width {}",
            info.cell_width
        );

        // Not enough room: cut the row here; the current grapheme starts the
        // next one.
        if self.num_cells + info.cell_width as usize > index.columns {
            // A wide char in a non-full row leaves a one-cell gap at the end
            // of this row — remember it (the wide char itself stays whole).
            if info.cell_width > 1 && self.num_cells != index.columns {
                self.add_leading_wide_char_spacer();
            }
            mem::take(self).append_to_index(index);
            debug_assert_eq!(self.incr_content_offset, ByteOffset::zero());
        }

        self.num_cells += info.cell_width as usize;

        self.process_grapheme_info_unchecked(info);
    }

    /// RLE fast path: bulk-consume `count` uniform width-1, 1-byte graphemes.
    ///
    /// Equivalent to looping `process_grapheme_info` because width-1
    /// graphemes can never trigger the leading-wide-char-spacer cut; rows
    /// that fill exactly stay *pending* (not flushed) — the flush happens on
    /// the next grapheme or newline, exactly like the slow path.
    fn process_ascii_run(&mut self, info: GraphemeInfo, count: NonZeroU16, index: &mut Index) {
        debug_assert!(
            info.cell_width == 1 && info.utf8_bytes.get() == 1,
            "fast path requires uniform single-byte graphemes"
        );

        let mut remaining = count.get() as usize;
        while remaining > 0 {
            if self.num_cells == index.columns {
                // Row exactly full: flush and continue in a fresh row (the
                // slow path cuts lazily at the same point).
                mem::take(self).append_to_index(index);
            }
            let take = remaining.min(index.columns - self.num_cells);
            self.num_cells += take;
            self.incr_content_offset += take;
            match self.grapheme_runs.last_mut() {
                Some(last) if last.info == info => {
                    let new_count = last.count.get() + take as u16;
                    last.count = NonZeroU16::new(new_count)
                        .expect("should not have more than 2^16 cells in a single row");
                }
                _ => {
                    self.grapheme_runs.push(GraphemeRun {
                        count: NonZeroU16::new(take as u16)
                            .expect("row width must fit in a u16 run"),
                        info,
                    });
                }
            }
            remaining -= take;
        }
    }

    /// Processes the next grapheme without row-fit checks. Only valid when
    /// the row cannot overflow (push encoding: rows come pre-laid-out).
    pub(crate) fn process_grapheme_info_unchecked(&mut self, info: GraphemeInfo) {
        let grapheme_len = info.utf8_bytes.get() as usize;

        self.incr_content_offset += grapheme_len;

        // Merge into the trailing run when sizing matches (RLE).
        match self.grapheme_runs.last_mut() {
            Some(last_run) if last_run.info == info => {
                last_run.count = last_run
                    .count
                    .checked_add(1)
                    .expect("should not have more than 2^16 graphemes in a single row");
            }
            _ => {
                self.grapheme_runs.push(GraphemeRun {
                    count: NonZeroU16::new(1).expect("1 != 0"),
                    info,
                });
            }
        }
    }

    /// Marks the row as hard-terminated (accounts for the '\n' byte).
    pub(crate) fn add_trailing_newline(&mut self) {
        self.incr_content_offset += '\n'.len_utf8();
        self.has_trailing_newline = true;
    }

    /// Marks the row as ending with a leading wide-char spacer.
    pub(crate) fn add_leading_wide_char_spacer(&mut self) {
        self.ends_with_leading_wide_char_spacer = true;
    }

    /// Appends the entry to `index` unless nothing was accumulated.
    pub(crate) fn append_to_index_if_nonempty(mut self, index: &mut Index) {
        #[cfg(debug_assertions)]
        {
            self.was_processed = true;
        }

        if !self.is_empty() {
            self.append_to_index(index);
        }
    }

    /// Builds the [`Entry`] and appends it to `index`.
    pub(crate) fn append_to_index(mut self, index: &mut Index) {
        let content_offset = ByteOffset::from_usize(index.content_len);

        let grapheme_sizing = if self.grapheme_runs.len() == 1 {
            GraphemeSizing::Uniform(
                self.grapheme_runs
                    .pop()
                    .expect("checked grapheme_runs.len() == 1 above"),
            )
        } else if self.grapheme_runs.is_empty() {
            GraphemeSizing::EmptyRow
        } else {
            index
                .grapheme_sizing
                .insert(content_offset, mem::take(&mut self.grapheme_runs));
            GraphemeSizing::NonUniform
        };

        index.content_len += self.incr_content_offset.as_usize();
        index.rows.push_back(Entry {
            content_offset,
            grapheme_sizing,
            has_trailing_newline: self.has_trailing_newline,
            ends_with_leading_wide_char_spacer: self.ends_with_leading_wide_char_spacer,
        });

        #[cfg(debug_assertions)]
        {
            self.was_processed = true;
        }
    }

    fn is_empty(&self) -> bool {
        self.incr_content_offset == ByteOffset::zero()
            && !self.has_trailing_newline
            && !self.ends_with_leading_wide_char_spacer
            && self.grapheme_runs.is_empty()
    }
}

impl Drop for EntryBuilder {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        debug_assert!(
            self.was_processed,
            "EntryBuilder must be processed before it is dropped"
        );
    }
}
