#[path = "snapshot_row.rs"]
pub(crate) mod snapshot_row; // v1.11.3: gate budget; pub(crate) so the grid walker-consistency test can pin `styled_row` to the other Row-level text paths (v1.12.27a P1-01)

use self::snapshot_row::{mark_snapshot_truncated, push_snapshot_text, styled_row};
use super::{Grid, Row};
use crate::blocks::{StyledLine, StyledOutput};
use std::sync::Arc;

const MAX_SNAPSHOT_COLOR_SPANS: usize = 4096;
/// v1.6.1: Maximum link spans per captured row. Prevents a malicious TUI
/// from spamming tiny links that bloat the snapshot. 256 is generous — a
/// typical line has 0-2 links.
pub(crate) const MAX_SNAPSHOT_LINK_SPANS: usize = 256;

/// v1.10.20: snapshot text budget, derived from the configured block output
/// cap (PLAN_v11217 §3.5, T4 C-class). The former `SNAPSHOT_TEXT_BUDGET`
/// global constant is gone: the cap argument is passed in per call (single
/// source: `BlockTracker::output_cap`), and the +4 slack is the historical
/// headroom the walk abort logic keeps for the truncation marker byte.
/// Shared with `grid/snapshot_line_map.rs` (single bound for both walks).
pub(crate) const fn snapshot_text_budget(text_cap: usize) -> usize {
    text_cap.saturating_add(4)
}

/// v1.6.1: Convert a resolved URL `Arc<str>` to the `String` that `LinkSpan`
/// stores. Centralized so the conversion is consistent across all call sites.
pub(crate) fn url_to_string(url: Arc<str>) -> String {
    url.to_string()
}

// T2: flat history materializes owned rows (no `&Row` to lend), so retained
// rows are handed out by value. All uses are read-only cold paths.
impl Grid {
    /// Reflow once while mapping an absolute row boundary to the rebuilt
    /// document.
    ///
    /// T5 (D4): the boundary maps through the resize's row map — the anchor
    /// row's content occupies a contiguous post-rebuild row range, and the
    /// boundary lands on its first row. This replaces the marker-cell trick
    /// (write marker bg -> resize -> hunt marker -> restore): no cell
    /// mutation, no `replace_row` re-encode, and the anchor can no longer
    /// drift when the marked column wraps differently.
    pub(crate) fn resize_preserving_document_position(
        &mut self,
        document_start: u64,
        new_rows: usize,
        new_cols: usize,
    ) -> (u64, crate::grid::GridRowMap) {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return (document_start, crate::grid::GridRowMap::identity());
        }
        let retained_start = self
            .scrollback
            .position()
            .saturating_sub(self.scrollback.len() as u64);
        // The anchor names a document row: its index within the unified
        // (scrollback + viewport) row sequence, clamped into range.
        let old_row_index = document_start.saturating_sub(retained_start) as usize;

        let map = self.resize_impl(new_rows, new_cols);

        let (first_new_row, _) = map
            .old_row_new_range
            .get(old_row_index)
            .copied()
            .unwrap_or((0, 0));
        let retained_start_post = self
            .scrollback
            .position()
            .saturating_sub(self.scrollback.len() as u64);
        let mapped = retained_start_post + first_new_row as u64;
        // The map covers rows post-rebuild; `apply_max_rows` may have
        // evicted an oldest prefix — the boundary clamps into the surviving
        // document instead of pointing past its start.
        (mapped.max(retained_start_post), map)
    }

    /// Snapshot the rows produced since `scrollback_start`, followed by the
    /// live viewport. Primary-screen TUIs use this as their detached command
    /// transcript because their coordinate repaint stream is not linear text.
    #[doc(hidden)]
    pub fn document_text_from(&self, scrollback_start: u64, text_cap: usize) -> String {
        self.document_snapshot_from(scrollback_start, text_cap).0
    }

    #[doc(hidden)]
    pub fn document_snapshot_from(
        &self,
        scrollback_start: u64,
        text_cap: usize,
    ) -> (String, StyledOutput) {
        self.document_snapshot_from_indices(
            self.scrollback.index_since(scrollback_start),
            0,
            None,
            None,
            text_cap,
        )
    }

    #[doc(hidden)]
    pub fn document_snapshot_from_position(
        &self,
        document_start: u64,
        text_cap: usize,
    ) -> (String, StyledOutput) {
        let viewport_origin = self.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.scrollback.index_since(document_start), 0)
        } else {
            (
                self.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        self.document_snapshot_from_indices(scrollback_start, viewport_start, None, None, text_cap)
    }

    /// v1.6.1: Same as [`document_snapshot_from_position`](Self::document_snapshot_from_position)
    /// but resolves hyperlink ids to URLs via `url_resolver`. Used by the Block
    /// capture path which has access to the Terminal's `HyperlinkRegistry`.
    pub fn document_snapshot_from_position_with_resolver<F>(
        &self,
        document_start: u64,
        url_resolver: F,
        text_cap: usize,
    ) -> (String, StyledOutput, Option<usize>)
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
        let viewport_origin = self.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.scrollback.index_since(document_start), 0)
        } else {
            (
                self.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        self.document_snapshot_with_url_resolver(
            scrollback_start,
            viewport_start,
            None,
            None,
            url_resolver,
            text_cap,
        )
    }

    /// Snapshot a primary-screen document while omitting retained rows that
    /// the current application has never owned. Filtering is deliberately
    /// non-destructive: the live Grid keeps shell context needed by terminal
    /// emulation, while detached history sees only application output.
    pub(crate) fn document_snapshot_from_position_with_ownership_masks_and_resolver<F>(
        &self,
        document_start: u64,
        scrollback_owned: &[bool],
        viewport_owned: &[bool],
        url_resolver: F,
        text_cap: usize,
    ) -> (String, StyledOutput, Option<usize>)
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
        let viewport_origin = self.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.scrollback.index_since(document_start), 0)
        } else {
            (
                self.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        self.document_snapshot_with_url_resolver(
            scrollback_start,
            viewport_start,
            Some(scrollback_owned),
            Some(viewport_owned),
            url_resolver,
            text_cap,
        )
    }

    fn document_snapshot_from_indices(
        &self,
        scrollback_start: usize,
        viewport_start: usize,
        scrollback_owned: Option<&[bool]>,
        viewport_owned: Option<&[bool]>,
        text_cap: usize,
    ) -> (String, StyledOutput) {
        // v1.6.1: resolve hyperlink ids to URLs via the terminal's registry.
        // The closure captures `&self` (immutable) so it can be called per row.
        let url_resolver = |id: u32| -> Option<Arc<str>> {
            // Grid doesn't own a HyperlinkRegistry — the Terminal does. We
            // pass the resolver in from the caller (document_snapshot_with_url_resolver)
            // or fall back to None here (no links captured).
            let _ = id;
            None
        };
        let (text, styled, _) = self.document_snapshot_with_url_resolver(
            scrollback_start,
            viewport_start,
            scrollback_owned,
            viewport_owned,
            url_resolver,
            text_cap,
        );
        (text, styled)
    }

    /// v1.6.1: Snapshot variant that resolves hyperlink ids to URLs via the
    /// provided closure. The closure receives a `u32` hyperlink id (from
    /// `RowExtras.hyperlink_id`) and returns the URL string if known.
    /// Used by the Block capture path which has access to the Terminal's
    /// `HyperlinkRegistry`.
    pub(crate) fn document_snapshot_with_url_resolver<F>(
        &self,
        scrollback_start: usize,
        viewport_start: usize,
        scrollback_owned: Option<&[bool]>,
        viewport_owned: Option<&[bool]>,
        url_resolver: F,
        text_cap: usize,
    ) -> (String, StyledOutput, Option<usize>)
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
        // PLAN_v11217 §3.5 (T4): the walk budget derives from the configured
        // cap — no global constant. The +4 slack is historical headroom.
        let text_budget = snapshot_text_budget(text_cap);
        let cursor_row = self.cursor.row;
        let scrollback = (scrollback_start..self.scrollback.len()).filter_map(|index| {
            scrollback_owned
                .map_or(true, |owned| owned.get(index).copied().unwrap_or(false))
                .then(|| self.scrollback.get(index))
                .flatten()
        });
        // v1.10.6: pair each viewport row with its grid index so we can
        // detect the cursor row during snapshot construction. The index
        // is needed because empty rows are skipped, breaking the 1:1
        // correspondence between grid rows and snapshot lines.
        let viewport_indexed: Vec<(usize, &Row)> = self
            .viewport
            .iter()
            .take(self.num_rows)
            .enumerate()
            .skip(viewport_start.min(self.num_rows))
            .filter(|(index, _)| {
                viewport_owned.map_or(true, |owned| owned.get(*index).copied().unwrap_or(false))
            })
            .collect();
        // v1.10.7: tag each row with whether it is the cursor's grid row.
        // The scrollback chain never carries the cursor (it always sits in
        // the viewport), so scrollback rows are never tagged. The viewport
        // rows keep their absolute grid index (the `enumerate` ran BEFORE the
        // `.skip(viewport_start)`), so the cursor is matched by index, not by
        // chain position. The v1.10.6 tracker counted every yielded row
        // (scrollback first), which shifted the match by the number of
        // scrollback rows between `document_start` and the viewport origin —
        // the caret/preedit then landed N rows above the real input row.
        let cursor_row_excluded = !viewport_owned.map_or(true, |owned| {
            owned.get(cursor_row).copied().unwrap_or(false)
        });
        let scrollback_tagged = scrollback.map(|row| (false, row));
        let viewport_tagged = viewport_indexed
            .into_iter()
            .map(|(index, row)| (index == cursor_row, row.clone()));
        let mut text = String::new();
        let mut lines = Vec::new();
        let mut span_count = 0_usize;
        let style_enabled = true;
        let mut started = false;
        let mut pending_empty = 0_usize;
        let mut line_index = 0_usize;
        let mut cursor_snapshot_line: Option<usize> = None;
        let mut pending_empty_cursor_line: Option<usize> = None;
        // Walk the grid rows in document order.
        for (on_cursor_index, row) in scrollback_tagged.chain(viewport_tagged) {
            let on_cursor_row = on_cursor_index && !cursor_row_excluded;
            let style_budget =
                style_enabled.then(|| MAX_SNAPSHOT_COLOR_SPANS.saturating_sub(span_count));
            let row = styled_row(
                &row,
                self.num_cols,
                text_budget.saturating_sub(text.len()),
                style_budget,
                &url_resolver,
            );
            if row.text.is_empty() {
                if row.text_overflow {
                    mark_snapshot_truncated(&mut text, text_cap);
                    lines.clear();
                    return (text, StyledOutput { lines }, cursor_snapshot_line);
                }
                // Commit this candidate only if a later non-empty row
                // materializes the pending newlines in the snapshot text.
                if on_cursor_row && started {
                    pending_empty_cursor_line = Some(line_index.saturating_add(1 + pending_empty));
                }
                pending_empty += usize::from(started);
                continue;
            }
            if started {
                line_index = line_index.saturating_add(1 + pending_empty);
                for _ in 0..=pending_empty {
                    if !push_snapshot_text(&mut text, "\n", text_budget) {
                        lines.clear();
                        return (text, StyledOutput { lines }, cursor_snapshot_line);
                    }
                }
                if let Some(cursor_line) = pending_empty_cursor_line.take() {
                    cursor_snapshot_line = Some(cursor_line);
                }
            } else {
                started = true;
            }
            // The cursor lands on this non-empty row → snapshot line_index.
            if on_cursor_row {
                cursor_snapshot_line = Some(line_index);
            }
            pending_empty = 0;
            if !push_snapshot_text(&mut text, &row.text, text_budget) {
                lines.clear();
                return (text, StyledOutput { lines }, cursor_snapshot_line);
            }
            if row.text_overflow {
                mark_snapshot_truncated(&mut text, text_cap);
                lines.clear();
                return (text, StyledOutput { lines }, cursor_snapshot_line);
            }
            if row.style_overflow {
                // v1.13.10 FIX (audit P1): this used to set style_enabled=false
                // AND clear `lines`, discarding every previously accumulated
                // row's styles (all-or-nothing). Degrade per-row instead — the
                // overflowing row simply isn't pushed below; its text and all
                // other rows' styles survive. Overflow means THIS row's span
                // count exceeded the *remaining* budget (push_color_span
                // returns false at used >= budget); span_count is deliberately
                // not advanced for it (else-if below), so a later row that
                // fits in what's left is still pushed — sparse `lines` are
                // expected, and StyledOutput::line's binary search handles
                // missing line indices. (`style_enabled` now stays true for
                // the whole document; the flag remains only as the
                // budget-plumbing switch into styled_row.)
            } else if style_enabled
                && (!row.foregrounds.is_empty()
                    || !row.backgrounds.is_empty()
                    || !row.links.is_empty()
                    || !row.attributes.is_empty()
                    || !row.underline_colors.is_empty())
            {
                span_count = span_count
                    .saturating_add(row.foregrounds.len())
                    .saturating_add(row.backgrounds.len());
                lines.push(StyledLine {
                    line: line_index as u32,
                    foregrounds: row.foregrounds,
                    backgrounds: row.backgrounds,
                    links: row.links,
                    attributes: row.attributes,
                    underline_colors: row.underline_colors,
                });
            }
        }
        (text, StyledOutput { lines }, cursor_snapshot_line)
    }

    /// v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): extract the retained scrollback
    /// rows `[start, len)` as (text, optional styled line) pairs — the raw
    /// material for the incremental scroll-out prefix capture. Uses the same
    /// per-row extraction as the document snapshot (`styled_row`), so prefix
    /// text and styles match the snapshot semantics; ownership filtering and
    /// empty-row skipping are applied by the caller's pure function
    /// (`blocks::screen_capture::extract_owned_pushed_rows`).
    pub(crate) fn snapshot_rows_from_scrollback<F>(
        &self,
        start: usize,
        url_resolver: F,
        text_cap: usize,
    ) -> Vec<(String, Option<StyledLine>)>
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
        (start..self.scrollback.len())
            .filter_map(|index| {
                let row = self.scrollback.get(index)?;
                // PLAN_v11217 §3.5 (T4): the per-row budget derives from the
                // configured cap (single source: the caller's tracker field).
                let snap = styled_row(
                    &row,
                    self.num_cols,
                    snapshot_text_budget(text_cap),
                    Some(MAX_SNAPSHOT_COLOR_SPANS),
                    &url_resolver,
                );
                let styled = if snap.style_overflow
                    || (snap.foregrounds.is_empty()
                        && snap.backgrounds.is_empty()
                        && snap.attributes.is_empty()
                        && snap.underline_colors.is_empty())
                {
                    None
                } else {
                    Some(StyledLine {
                        line: 0,
                        foregrounds: snap.foregrounds,
                        backgrounds: snap.backgrounds,
                        links: snap.links,
                        attributes: snap.attributes,
                        underline_colors: snap.underline_colors,
                    })
                };
                Some((snap.text, styled))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
