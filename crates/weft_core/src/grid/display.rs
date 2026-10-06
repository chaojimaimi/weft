use super::row::Row;
use super::{Cell, CellFlags, CellWidth, Grid, RowExtras};

/// v1.12.27a (P1-01): one yielded cell of the shared row walk. `ch` is the
/// RAW cell char — consumers map it themselves (display/snapshot render
/// `\0` as a space; selection passes it through unchanged; zero-behavior
/// red line). `cluster` is `Some` only when the cell carries
/// `CellFlags::EXTRA` and a `RowExtras` grapheme entry exists.
pub(crate) struct WalkedCell<'a> {
    pub col: usize,
    pub ch: char,
    pub cluster: Option<&'a str>,
    pub width: u8,
}

/// v1.12.27a (P1-01): the ONE cell-walking rule (skip `WIDE_SPACER`;
/// `EXTRA` cells contribute their full cluster) shared by text extraction
/// and tokenization — previously five inline copies (display, selection,
/// find ×2, snapshot ×2) drifted once already (audit core P1-1). Yields
/// columns `[0, min(limit, cells.len()))` in order; `limit` is the
/// caller's own extent (trailing-blank trim, row width, or `cells.len()`).
pub(crate) fn walk_cells<'a>(
    cells: &'a [Cell],
    extras: &'a RowExtras,
    limit: usize,
) -> impl Iterator<Item = WalkedCell<'a>> {
    let width = cells.len().min(limit);
    (0..width).filter_map(move |col| {
        let cell = &cells[col];
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            return None;
        }
        let cluster = if cell.flags.contains(CellFlags::EXTRA) {
            extras.grapheme_at(col)
        } else {
            None
        };
        let width = if cell.width == CellWidth::Full { 2 } else { 1 };
        Some(WalkedCell {
            col,
            ch: cell.character,
            cluster,
            width,
        })
    })
}

/// Text of one physical row, shared by [`Grid::row_text`] (live viewport,
/// shell-marker snapshots) and [`Grid::displayed_row_text`] (scroll-aware
/// a11y): skip `WIDE_SPACER`; `EXTRA` cells contribute their full
/// multi-scalar cluster; `\0` renders as a space; trailing blanks trimmed.
pub(crate) fn row_display_text(row: &Row, cols: usize) -> String {
    let width = cols.min(row.cells.len());
    let last = row.cells[..width]
        .iter()
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map_or(0, |index| index + 1);
    let mut out = String::with_capacity(last);
    // v1.12.27a (P1-01): the walk is the shared [`walk_cells`] rule; this
    // consumer maps `\0` → space (the display/snapshot 口径).
    for item in walk_cells(&row.cells, &row.extras, last) {
        if let Some(cluster) = item.cluster {
            out.push_str(cluster);
        } else {
            out.push(if item.ch == '\0' { ' ' } else { item.ch });
        }
    }
    out
}

impl Grid {
    /// Whether the displayed row continues onto the following physical row.
    /// Honors scrollback offset exactly like [`Grid::cell`].
    pub fn displayed_row_wrapped(&self, row: usize) -> bool {
        if row >= self.num_rows {
            return false;
        }
        let sb_len = self.scrollback.len();
        let offset = self.scroll_offset.min(sb_len);
        if offset > 0 {
            let global = sb_len - offset + row;
            if global < sb_len {
                // D1: history rows come from the materialized window; the
                // window index equals `row` (see `Grid::cell`).
                return self
                    .history_window_row(row)
                    .is_some_and(|history| history.wrapped);
            }
            return self
                .viewport
                .get(global - sb_len)
                .is_some_and(|visible| visible.wrapped);
        }
        self.viewport
            .get(row)
            .is_some_and(|visible| visible.wrapped)
    }

    /// Extract text from the row currently displayed after applying the
    /// scrollback offset. Accessibility and other viewport consumers must use
    /// this instead of [`row_text`](Self::row_text), which intentionally reads
    /// only the live buffer for shell-marker snapshots.
    /// PLAN_B Phase 0: retained as the scroll-aware twin of `row_text` (the
    /// live-viewport semantics live on that method's doc).
    ///
    /// v1.6.0: cells tagged with `CellFlags::EXTRA` contribute their full
    /// multi-scalar grapheme cluster via [`Grid::grapheme_at`], so accessibility
    /// reads the same decomposed string the renderer paints.
    pub fn displayed_row_text(&self, row: usize) -> String {
        if row >= self.num_rows {
            return String::new();
        }
        // Resolve the source row with exactly [`Grid::cell`]'s scroll math,
        // then build the text from it in one pass (T3 D5-3 — the per-column
        // cell()/grapheme_at() replay is gone).
        let sb_len = self.scrollback.len();
        let offset = self.scroll_offset.min(sb_len);
        if offset > 0 {
            if row < offset {
                if let Some(history_row) = self.history_window_row(row) {
                    return row_display_text(history_row, self.num_cols);
                }
                return String::new();
            }
            if let Some(visible) = self.viewport.get(row - offset) {
                return row_display_text(visible, self.num_cols);
            }
            return String::new();
        }
        if let Some(visible) = self.viewport.get(row) {
            return row_display_text(visible, self.num_cols);
        }
        String::new()
    }
}
