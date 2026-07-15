use super::{CellFlags, Grid};

impl Grid {
    /// Extract text from the row currently displayed after applying the
    /// scrollback offset. Accessibility and other viewport consumers must use
    /// this instead of [`row_text`](Self::row_text), which intentionally reads
    /// only the live buffer for shell-marker snapshots.
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
            out.push(if cell.character == '\0' {
                ' '
            } else {
                cell.character
            });
        }
        out
    }
}
