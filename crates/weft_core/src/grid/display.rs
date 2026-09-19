use super::{CellFlags, Grid};

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
                return self
                    .scrollback
                    .get(global)
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
        let last = (0..self.num_cols)
            .rposition(|col| {
                let cell = self.cell(row, col);
                cell.character != ' ' && cell.character != '\0'
            })
            .map(|col| col + 1)
            .unwrap_or(0);
        let mut out = String::with_capacity(last);
        for col in 0..last {
            let cell = self.cell(row, col);
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            if cell.flags.contains(CellFlags::EXTRA) {
                if let Some(cluster) = self.grapheme_at(row, col) {
                    out.push_str(cluster);
                    continue;
                }
            }
            out.push(if cell.character == '\0' {
                ' '
            } else {
                cell.character
            });
        }
        out
    }
}
