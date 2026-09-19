#[path = "snapshot_row.rs"]
mod snapshot_row; // v1.11.3: gate budget

use self::snapshot_row::{mark_snapshot_truncated, push_snapshot_text, styled_row};
use super::{CellColor, CellFlags, Color, Grid, Row};
use crate::blocks::{StyledLine, StyledOutput, MAX_OUTPUT_BYTES};
use std::sync::Arc;

const MAX_SNAPSHOT_COLOR_SPANS: usize = 4096;
/// v1.6.1: Maximum link spans per captured row. Prevents a malicious TUI
/// from spamming tiny links that bloat the snapshot. 256 is generous — a
/// typical line has 0-2 links.
pub(crate) const MAX_SNAPSHOT_LINK_SPANS: usize = 256;

/// v1.10.20: snapshot text budget — shared with `grid/snapshot_line_map.rs` (single bound for both walks).
pub(crate) const SNAPSHOT_TEXT_BUDGET: usize = MAX_OUTPUT_BYTES + 4;

/// v1.6.1: Convert a resolved URL `Arc<str>` to the `String` that `LinkSpan`
/// stores. Centralized so the conversion is consistent across all call sites.
pub(crate) fn url_to_string(url: Arc<str>) -> String {
    url.to_string()
}

fn retained_row(grid: &Grid, index: usize) -> Option<&Row> {
    if index < grid.scrollback.len() {
        grid.scrollback.get(index)
    } else {
        grid.viewport
            .get(index.saturating_sub(grid.scrollback.len()))
    }
}

fn row_content_end(row: &Row) -> usize {
    row.cells
        .iter()
        .rposition(|cell| cell.character != ' ' || !cell.flags.is_empty())
        .map(|index| index + 1)
        .unwrap_or(0)
}

impl Grid {
    /// Reflow once while mapping an absolute row boundary to the rebuilt
    /// document. A temporary cell marker preserves a boundary that points at
    /// a blank row or into a wrapped logical line.
    pub(crate) fn resize_preserving_document_position(
        &mut self,
        document_start: u64,
        new_rows: usize,
        new_cols: usize,
    ) -> u64 {
        if new_rows == self.num_rows && new_cols == self.num_cols {
            return document_start;
        }
        let retained_start = self
            .scrollback
            .position()
            .saturating_sub(self.scrollback.len() as u64);
        let retained_index = document_start.saturating_sub(retained_start) as usize;
        let scrollback_len = self.scrollback.len();
        let retained_len = scrollback_len.saturating_add(self.num_rows);
        let direct_column = retained_row(self, retained_index).and_then(|row| {
            let end = row_content_end(row);
            row.cells[..end]
                .iter()
                .position(|cell| !cell.flags.contains(CellFlags::WIDE_SPACER))
        });
        let (marker_row, marker_column, after_line) = if let Some(column) = direct_column {
            (retained_index, column, false)
        } else {
            let previous = (0..retained_index.min(retained_len))
                .rev()
                .find_map(|index| {
                    let row = retained_row(self, index)?;
                    let end = row_content_end(row);
                    row.cells[..end]
                        .iter()
                        .rposition(|cell| !cell.flags.contains(CellFlags::WIDE_SPACER))
                        .map(|column| (index, column))
                });
            let Some((row, column)) = previous else {
                self.resize(new_rows, new_cols);
                return 0;
            };
            (row, column, true)
        };
        let row = if marker_row < scrollback_len {
            self.scrollback.get_mut(marker_row)
        } else {
            self.viewport
                .get_mut(marker_row.saturating_sub(scrollback_len))
        };
        let Some(cell) = row.and_then(|row| row.cells.get_mut(marker_column)) else {
            self.resize(new_rows, new_cols);
            return document_start;
        };
        const MARKER: CellColor = CellColor::Rgb(Color {
            r: 17,
            g: 29,
            b: 43,
            a: 0,
        });
        let original_bg = cell.bg;
        cell.bg = MARKER;
        self.resize(new_rows, new_cols);

        let marker = (0..self.scrollback.len().saturating_add(self.num_rows)).find_map(|index| {
            retained_row(self, index)
                .and_then(|row| row.cells.iter().position(|cell| cell.bg == MARKER))
                .map(|column| (index, column))
        });
        let Some((marker_row, marker_column)) = marker else {
            return document_start;
        };
        if marker_row < self.scrollback.len() {
            self.scrollback.get_mut(marker_row).unwrap().cells[marker_column].bg = original_bg;
        } else {
            self.viewport[marker_row - self.scrollback.len()].cells[marker_column].bg = original_bg;
        }
        let mut boundary_row = marker_row;
        if after_line {
            loop {
                let wrapped = retained_row(self, boundary_row).is_some_and(|row| row.wrapped);
                boundary_row = boundary_row.saturating_add(1);
                if !wrapped {
                    break;
                }
            }
        }
        self.scrollback
            .position()
            .saturating_sub(self.scrollback.len() as u64)
            .saturating_add(boundary_row as u64)
    }

    /// Snapshot the rows produced since `scrollback_start`, followed by the
    /// live viewport. Primary-screen TUIs use this as their detached command
    /// transcript because their coordinate repaint stream is not linear text.
    #[doc(hidden)]
    pub fn document_text_from(&self, scrollback_start: u64) -> String {
        self.document_snapshot_from(scrollback_start).0
    }

    #[doc(hidden)]
    pub fn document_snapshot_from(&self, scrollback_start: u64) -> (String, StyledOutput) {
        self.document_snapshot_from_indices(
            self.scrollback.index_since(scrollback_start),
            0,
            None,
            None,
        )
    }

    #[doc(hidden)]
    pub fn document_snapshot_from_position(&self, document_start: u64) -> (String, StyledOutput) {
        let viewport_origin = self.scrollback.position();
        let (scrollback_start, viewport_start) = if document_start <= viewport_origin {
            (self.scrollback.index_since(document_start), 0)
        } else {
            (
                self.scrollback.len(),
                document_start.saturating_sub(viewport_origin) as usize,
            )
        };
        self.document_snapshot_from_indices(scrollback_start, viewport_start, None, None)
    }

    /// v1.6.1: Same as [`document_snapshot_from_position`](Self::document_snapshot_from_position)
    /// but resolves hyperlink ids to URLs via `url_resolver`. Used by the Block
    /// capture path which has access to the Terminal's `HyperlinkRegistry`.
    pub fn document_snapshot_from_position_with_resolver<F>(
        &self,
        document_start: u64,
        url_resolver: F,
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
        )
    }

    /// Snapshot a primary-screen document while omitting retained rows that
    /// the current application has never owned. Filtering is deliberately
    /// non-destructive: the live Grid keeps shell context needed by terminal
    /// emulation, while detached history sees only application output.
    #[allow(dead_code)] // used in tests + kept for callers that don't need link capture
    pub(crate) fn document_snapshot_from_position_with_ownership_masks(
        &self,
        document_start: u64,
        scrollback_owned: &[bool],
        viewport_owned: &[bool],
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
        self.document_snapshot_from_indices(
            scrollback_start,
            viewport_start,
            Some(scrollback_owned),
            Some(viewport_owned),
        )
    }

    /// v1.6.1: Same as [`document_snapshot_from_position_with_ownership_masks`]
    /// but resolves hyperlink ids to URLs via `url_resolver`.
    pub(crate) fn document_snapshot_from_position_with_ownership_masks_and_resolver<F>(
        &self,
        document_start: u64,
        scrollback_owned: &[bool],
        viewport_owned: &[bool],
        url_resolver: F,
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
        )
    }

    fn document_snapshot_from_indices(
        &self,
        scrollback_start: usize,
        viewport_start: usize,
        scrollback_owned: Option<&[bool]>,
        viewport_owned: Option<&[bool]>,
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
    ) -> (String, StyledOutput, Option<usize>)
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
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
            .map(|(index, row)| (index == cursor_row, row));
        let mut text = String::new();
        let mut lines = Vec::new();
        let mut span_count = 0_usize;
        let mut style_enabled = true;
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
                row,
                self.num_cols,
                SNAPSHOT_TEXT_BUDGET.saturating_sub(text.len()),
                style_budget,
                &url_resolver,
            );
            if row.text.is_empty() {
                if row.text_overflow {
                    mark_snapshot_truncated(&mut text);
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
                    if !push_snapshot_text(&mut text, "\n") {
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
            if !push_snapshot_text(&mut text, &row.text) {
                lines.clear();
                return (text, StyledOutput { lines }, cursor_snapshot_line);
            }
            if row.text_overflow {
                mark_snapshot_truncated(&mut text);
                lines.clear();
                return (text, StyledOutput { lines }, cursor_snapshot_line);
            }
            if row.style_overflow {
                style_enabled = false;
                lines.clear();
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
    ) -> Vec<(String, Option<StyledLine>)>
    where
        F: Fn(u32) -> Option<Arc<str>>,
    {
        (start..self.scrollback.len())
            .filter_map(|index| {
                let row = self.scrollback.get(index)?;
                let snap = styled_row(
                    row,
                    self.num_cols,
                    MAX_OUTPUT_BYTES,
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
mod tests {
    use super::*;

    fn row(text: &str, cols: usize) -> Row {
        let mut row = Row::new(cols);
        for (index, ch) in text.chars().enumerate() {
            row.cells[index].character = ch;
        }
        row
    }

    #[test]
    fn document_keeps_command_scrollback_and_final_viewport_only() {
        let mut grid = Grid::with_scrollback(4, 40, 20);
        grid.scrollback.push(row("older shell history", 40));
        let command_start = grid.scrollback.position();
        grid.scrollback.push(row("complete answer line 1", 40));
        grid.viewport[0] = row("complete answer line 2", 40);
        grid.viewport[2] = row("Press Ctrl-C again to exit", 40);
        grid.viewport[3] = row("claude --resume session-id", 40);

        assert_eq!(
            grid.document_text_from(command_start),
            "complete answer line 1\ncomplete answer line 2\n\nPress Ctrl-C again to exit\nclaude --resume session-id"
        );
    }

    /// v1.10.7 regression: the cursor snapshot line must be exact even when
    /// the document starts in the SCROLLBACK (the shell prompt row scrolled
    /// above the viewport origin). The v1.10.6 tracker decremented across the
    /// whole scrollback→viewport chain, shifting the match by the number of
    /// scrollback rows between `document_start` and the viewport origin — the
    /// BlockView caret/preedit then landed N rows above the real input row.
    #[test]
    fn cursor_snapshot_line_counts_viewport_rows_only() {
        // 6 rows: 2 scrollback + 4 viewport. The document starts at the
        // scrollback row right after the (excluded) shell prompt row, so
        // ONE scrollback row sits between document_start and the viewport.
        let mut grid = Grid::with_scrollback(4, 40, 2);
        grid.scrollback.push(row("shell prompt row", 40));
        let document_start = grid.scrollback.position();
        grid.scrollback.push(row("banner of the app", 40));
        grid.viewport[0] = row("app header line", 40);
        grid.viewport[1] = row("app content line", 40);
        grid.viewport[2] = row("input prompt: ", 40);
        grid.viewport[3] = row("", 40);
        // Cursor sits on viewport row 2 ("input prompt: ") — the row the
        // user is typing at. In the document it is line index 3
        // (banner, header, content, input) — NOT 3 - 1 scrollback row.
        grid.cursor.row = 2;
        grid.cursor.col = 14;

        let (text, _styled, cursor_line) =
            grid.document_snapshot_from_position_with_resolver(document_start, |_| None);
        assert_eq!(
            text,
            "banner of the app\napp header line\napp content line\ninput prompt:",
        );
        assert_eq!(
            cursor_line,
            Some(3),
            "cursor on viewport row 2 must map to document line 3, not shifted by scrollback rows"
        );
    }

    /// The cursor row must still be tracked when the document starts exactly
    /// at the viewport origin (no scrollback rows between).
    #[test]
    fn cursor_snapshot_line_tracks_cursor_at_viewport_origin_document() {
        let mut grid = Grid::with_scrollback(4, 40, 20);
        grid.viewport[0] = row("first document row", 40);
        grid.viewport[1] = row("", 40);
        grid.viewport[2] = row("input row here", 40);
        grid.cursor.row = 2;
        let document_start = grid.scrollback.position();

        let (text, _styled, cursor_line) =
            grid.document_snapshot_from_position_with_resolver(document_start, |_| None);
        assert_eq!(text, "first document row\n\ninput row here");
        assert_eq!(cursor_line, Some(2));
    }

    #[test]
    fn document_clamps_a_cleared_scrollback_baseline() {
        let mut grid = Grid::with_scrollback(2, 20, 20);
        grid.viewport[0] = row("final screen", 20);
        assert_eq!(grid.document_text_from(99), "final screen");
    }

    #[test]
    fn document_uses_a_logical_baseline_after_ring_buffer_is_full() {
        let mut grid = Grid::with_scrollback(1, 24, 2);
        grid.scrollback.push(row("old 1", 24));
        grid.scrollback.push(row("old 2", 24));
        let command_start = grid.scrollback.position();
        grid.scrollback.push(row("new answer 1", 24));
        grid.scrollback.push(row("new answer 2", 24));
        grid.viewport[0] = row("resume tail", 24);

        assert_eq!(
            grid.document_text_from(command_start),
            "new answer 1\nnew answer 2\nresume tail"
        );
    }

    #[test]
    fn document_baseline_survives_clear_and_more_than_baseline_pushes() {
        let mut grid = Grid::with_scrollback(1, 24, 20);
        for index in 0..5 {
            grid.scrollback.push(row(&format!("old {index}"), 24));
        }
        let command_start = grid.scrollback.position();
        grid.clear_scrollback();
        for index in 0..7 {
            grid.scrollback.push(row(&format!("new {index}"), 24));
        }
        grid.viewport[0] = row("resume", 24);

        assert_eq!(
            grid.document_text_from(command_start),
            "new 0\nnew 1\nnew 2\nnew 3\nnew 4\nnew 5\nnew 6\nresume"
        );
    }

    #[test]
    fn absolute_viewport_boundary_survives_scroll_ring_overflow_and_csi3j_clear() {
        let mut grid = Grid::with_scrollback(4, 24, 2);
        grid.viewport[0] = row("old shell", 24);
        grid.viewport[1] = row("command", 24);
        grid.viewport[2] = row("answer one", 24);
        grid.viewport[3] = row("answer two", 24);
        let document_start = grid.scrollback.position() + 2;

        grid.scroll_up(3);
        assert_eq!(
            grid.document_snapshot_from_position(document_start).0,
            "answer one\nanswer two"
        );
        grid.clear_scrollback();
        assert_eq!(
            grid.document_snapshot_from_position(document_start).0,
            "answer two"
        );
        for index in 0..4 {
            grid.scrollback.push(row(&format!("new {index}"), 24));
        }
        assert_eq!(
            grid.document_snapshot_from_position(document_start).0,
            "new 2\nnew 3\nanswer two"
        );
    }

    #[test]
    fn owned_viewport_snapshot_omits_stale_rows_without_mutating_grid() {
        let mut grid = Grid::with_scrollback(5, 24, 16);
        grid.viewport[0] = row("Claude banner", 24);
        grid.viewport[1] = row("stale shell table", 24);
        grid.viewport[2] = row("restored answer", 24);
        grid.viewport[3] = row("stale shell footer", 24);
        grid.viewport[4] = row("prompt", 24);
        let owned = [true, false, true, false, true];

        let snapshot = grid
            .document_snapshot_from_position_with_ownership_masks(0, &[], &owned)
            .0;

        assert_eq!(snapshot, "Claude banner\nrestored answer\nprompt");
        assert_eq!(grid.row_text(1), "stale shell table");
        assert_eq!(grid.row_text(3), "stale shell footer");
    }

    #[test]
    fn owned_snapshot_filters_scrollback_and_viewport_with_aligned_masks() {
        let mut grid = Grid::with_scrollback(3, 24, 16);
        grid.scrollback.push(row("old shell context", 24));
        grid.scrollback.push(row("owned answer page", 24));
        grid.viewport[0] = row("owned answer tail", 24);
        grid.viewport[1] = row("stale shell footer", 24);
        grid.viewport[2] = row("owned prompt", 24);

        let snapshot = grid
            .document_snapshot_from_position_with_ownership_masks(
                0,
                &[false, true],
                &[true, false, true],
            )
            .0;

        assert_eq!(
            snapshot,
            "owned answer page\nowned answer tail\nowned prompt"
        );
    }

    #[test]
    fn reflow_maps_a_blank_boundary_after_wrapped_shell_rows() {
        let mut grid = Grid::with_scrollback(5, 12, 16);
        grid.viewport[0] = row("old shell", 12);
        grid.viewport[0].wrapped = true;
        grid.viewport[1] = row("history", 12);
        grid.viewport[2] = row("claude", 12);
        grid.viewport[4] = row("startup", 12);
        grid.cursor.row = 4;
        let document_start = grid.scrollback.position() + 3;

        let mapped = grid.resize_preserving_document_position(document_start, 6, 6);
        let snapshot = grid.document_snapshot_from_position(mapped).0;

        assert!(!snapshot.contains("old shell"));
        assert!(!snapshot.contains("history"));
        assert!(!snapshot.contains("claude"));
        assert_eq!(snapshot.replace('\n', ""), "startup");
    }

    #[test]
    fn document_snapshot_keeps_foreground_alignment_across_wide_cells() {
        let mut grid = Grid::with_scrollback(1, 8, 8);
        let mut styled = Row::new(8);
        styled.cells[0].character = 'A';
        styled.cells[0].fg = crate::grid::CellColor::Palette(1);
        styled.cells[1].character = '中';
        styled.cells[1].width = crate::grid::CellWidth::Full;
        styled.cells[1].fg = crate::grid::CellColor::Rgb(crate::grid::Color::rgb(2, 3, 4));
        styled.cells[1].bg = crate::grid::CellColor::Palette(7);
        styled.cells[2].flags.insert(CellFlags::WIDE_SPACER);
        styled.cells[3].character = 'B';
        styled.cells[3].fg = crate::grid::CellColor::Palette(5);
        grid.viewport[0] = styled;

        let (text, snapshot) = grid.document_snapshot_from(0);
        assert_eq!(text, "A中B");
        let line = snapshot.line(0).expect("colored line");
        assert_eq!(
            line.foreground_at(0),
            Some(crate::grid::CellColor::Palette(1))
        );
        assert_eq!(
            line.foreground_at(1),
            Some(crate::grid::CellColor::Rgb(crate::grid::Color::rgb(
                2, 3, 4
            )))
        );
        assert_eq!(
            line.foreground_at(2),
            Some(crate::grid::CellColor::Palette(5))
        );
        assert_eq!(line.background_at(0), None);
        assert_eq!(
            line.background_at(1),
            Some(crate::grid::CellColor::Palette(7))
        );
        assert_eq!(line.background_at(2), None);
    }

    #[test]
    fn document_snapshot_streams_text_into_the_capture_limit() {
        let cols = 1_100;
        let mut grid = Grid::with_scrollback(1, cols, 1_100);
        let full = "x".repeat(cols);
        for _ in 0..1_100 {
            grid.scrollback.push(row(&full, cols));
        }

        let (text, styled) = grid.document_snapshot_from(0);
        assert!(text.len() > MAX_OUTPUT_BYTES);
        assert!(text.len() <= MAX_OUTPUT_BYTES + 4);
        assert!(styled.lines.is_empty(), "truncated text cannot keep styles");
    }

    #[test]
    fn document_snapshot_discards_styles_after_the_rle_span_limit() {
        let cols = MAX_SNAPSHOT_COLOR_SPANS + 2;
        let mut grid = Grid::with_scrollback(1, cols, 1);
        let mut styled = Row::new(cols);
        for (index, cell) in styled.cells.iter_mut().enumerate() {
            cell.character = 'x';
            cell.fg = crate::grid::CellColor::Palette((index % 2 + 1) as u8);
        }
        grid.viewport[0] = styled;

        let (text, styled) = grid.document_snapshot_from(0);
        assert_eq!(text.len(), cols);
        assert!(styled.lines.is_empty());
    }

    #[test]
    fn shrinking_scrollback_keeps_newest_command_rows() {
        let mut grid = Grid::with_scrollback(1, 24, 6);
        grid.scrollback.push(row("old", 24));
        let command_start = grid.scrollback.position();
        for index in 0..5 {
            grid.scrollback.push(row(&format!("answer {index}"), 24));
        }
        grid.scrollback.resize(3, 24);

        assert_eq!(
            grid.document_text_from(command_start),
            "answer 2\nanswer 3\nanswer 4"
        );
    }
}
