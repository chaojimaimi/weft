use super::{CellFlags, Grid, Row};
use crate::blocks::{ForegroundSpan, StyledLine, StyledOutput, MAX_OUTPUT_BYTES};

const MAX_SNAPSHOT_COLOR_SPANS: usize = 4096;

impl Grid {
    /// Snapshot the rows produced since `scrollback_start`, followed by the
    /// live viewport. Primary-screen TUIs use this as their detached command
    /// transcript because their coordinate repaint stream is not linear text.
    pub fn document_text_from(&self, scrollback_start: u64) -> String {
        self.document_snapshot_from(scrollback_start).0
    }

    pub fn document_snapshot_from(&self, scrollback_start: u64) -> (String, StyledOutput) {
        let start = self.scrollback.index_since(scrollback_start);
        let scrollback = (start..self.scrollback.len()).filter_map(|i| self.scrollback.get(i));
        let viewport = self.viewport.iter().take(self.num_rows);
        let mut text = String::new();
        let mut lines = Vec::new();
        let mut span_count = 0_usize;
        let mut style_enabled = true;
        let mut started = false;
        let mut pending_empty = 0_usize;
        let mut line_index = 0_usize;
        for row in scrollback.chain(viewport) {
            let style_budget =
                style_enabled.then(|| MAX_SNAPSHOT_COLOR_SPANS.saturating_sub(span_count));
            let row = styled_row(
                row,
                self.num_cols,
                (MAX_OUTPUT_BYTES + 4).saturating_sub(text.len()),
                style_budget,
            );
            if row.text.is_empty() {
                if row.text_overflow {
                    mark_snapshot_truncated(&mut text);
                    lines.clear();
                    return (text, StyledOutput { lines });
                }
                pending_empty += usize::from(started);
                continue;
            }
            if started {
                line_index = line_index.saturating_add(1 + pending_empty);
                for _ in 0..=pending_empty {
                    if !push_snapshot_text(&mut text, "\n") {
                        lines.clear();
                        return (text, StyledOutput { lines });
                    }
                }
            } else {
                started = true;
            }
            pending_empty = 0;
            if !push_snapshot_text(&mut text, &row.text) {
                lines.clear();
                return (text, StyledOutput { lines });
            }
            if row.text_overflow {
                mark_snapshot_truncated(&mut text);
                lines.clear();
                return (text, StyledOutput { lines });
            }
            if row.style_overflow {
                style_enabled = false;
                lines.clear();
            } else if style_enabled && !row.foregrounds.is_empty() {
                span_count = span_count.saturating_add(row.foregrounds.len());
                lines.push(StyledLine {
                    line: line_index as u32,
                    foregrounds: row.foregrounds,
                });
            }
        }
        (text, StyledOutput { lines })
    }
}

struct SnapshotRow {
    text: String,
    foregrounds: Vec<ForegroundSpan>,
    text_overflow: bool,
    style_overflow: bool,
}

fn push_snapshot_text(text: &mut String, source: &str) -> bool {
    let remaining = (MAX_OUTPUT_BYTES + 4).saturating_sub(text.len());
    if source.len() <= remaining {
        text.push_str(source);
        return true;
    }
    let mut end = remaining.min(source.len());
    while end > 0 && !source.is_char_boundary(end) {
        end -= 1;
    }
    text.push_str(&source[..end]);
    false
}

fn mark_snapshot_truncated(text: &mut String) {
    if text.len() <= MAX_OUTPUT_BYTES {
        text.push(' ');
    }
}

fn styled_row(
    row: &Row,
    num_cols: usize,
    text_budget: usize,
    style_budget: Option<usize>,
) -> SnapshotRow {
    let last = row
        .cells
        .iter()
        .take(num_cols)
        .rposition(|cell| cell.character != ' ' && cell.character != '\0')
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut text = String::with_capacity(last.min(text_budget));
    let mut foregrounds: Vec<ForegroundSpan> = Vec::new();
    let mut char_index = 0_u32;
    let mut text_overflow = false;
    let mut style_overflow = false;
    for cell in row.cells.iter().take(last) {
        if !cell.flags.contains(CellFlags::WIDE_SPACER) {
            let character = if cell.character == '\0' {
                ' '
            } else {
                cell.character
            };
            if text.len() + character.len_utf8() > text_budget {
                text_overflow = true;
                break;
            }
            text.push(character);
            if style_budget.is_some()
                && !style_overflow
                && cell.fg != crate::grid::CellColor::Default
            {
                if let Some(span) = foregrounds.last_mut() {
                    if span.end == char_index && span.color == cell.fg {
                        span.end += 1;
                    } else if foregrounds.len() < style_budget.unwrap_or(0) {
                        foregrounds.push(ForegroundSpan {
                            start: char_index,
                            end: char_index + 1,
                            color: cell.fg,
                        });
                    } else {
                        style_overflow = true;
                    }
                } else if foregrounds.len() < style_budget.unwrap_or(0) {
                    foregrounds.push(ForegroundSpan {
                        start: char_index,
                        end: char_index + 1,
                        color: cell.fg,
                    });
                } else {
                    style_overflow = true;
                }
            }
            char_index += 1;
        }
    }
    SnapshotRow {
        text,
        foregrounds,
        text_overflow,
        style_overflow,
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
    fn document_snapshot_keeps_foreground_alignment_across_wide_cells() {
        let mut grid = Grid::with_scrollback(1, 8, 8);
        let mut styled = Row::new(8);
        styled.cells[0].character = 'A';
        styled.cells[0].fg = crate::grid::CellColor::Palette(1);
        styled.cells[1].character = '中';
        styled.cells[1].width = crate::grid::CellWidth::Full;
        styled.cells[1].fg = crate::grid::CellColor::Rgb(crate::grid::Color::rgb(2, 3, 4));
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
