use super::{CellFlags, Grid, Row};

impl Grid {
    /// Snapshot the rows produced since `scrollback_start`, followed by the
    /// live viewport. Primary-screen TUIs use this as their detached command
    /// transcript because their coordinate repaint stream is not linear text.
    pub fn document_text_from(&self, scrollback_start: u64) -> String {
        let start = self.scrollback.index_since(scrollback_start);
        let mut rows = Vec::with_capacity(self.scrollback.len() - start + self.num_rows);
        for index in start..self.scrollback.len() {
            if let Some(row) = self.scrollback.get(index) {
                rows.push(row_text(row, self.num_cols));
            }
        }
        rows.extend(
            self.viewport
                .iter()
                .take(self.num_rows)
                .map(|row| row_text(row, self.num_cols)),
        );
        let first = rows
            .iter()
            .position(|row| !row.is_empty())
            .unwrap_or(rows.len());
        let end = rows
            .iter()
            .rposition(|row| !row.is_empty())
            .map_or(first, |index| index + 1);
        rows[first..end].join("\n")
    }
}

fn row_text(row: &Row, num_cols: usize) -> String {
    let last = row
        .cells
        .iter()
        .take(num_cols)
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut text = String::with_capacity(last);
    for cell in row.cells.iter().take(last) {
        if !cell.flags.contains(CellFlags::WIDE_SPACER) {
            text.push(if cell.character == '\0' {
                ' '
            } else {
                cell.character
            });
        }
    }
    text
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
