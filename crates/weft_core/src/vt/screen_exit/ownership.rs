use crate::grid::{CellColor, Color, Grid, Row};

use super::Terminal;

#[derive(Default)]
pub(in crate::vt) struct PrimaryScreenOwnership {
    pub(in crate::vt) scrollback: Vec<bool>,
    pub(in crate::vt) viewport: Option<Vec<bool>>,
}

impl PrimaryScreenOwnership {
    pub(in crate::vt) fn resize_viewport(&mut self, rows: usize) {
        if let Some(viewport) = &mut self.viewport {
            viewport.resize(rows, false);
        }
    }

    pub(in crate::vt) fn retain_scrollback_suffix(&mut self, retained_rows: usize) {
        let remove = self.scrollback.len().saturating_sub(retained_rows);
        self.scrollback.drain(..remove);
        while self.scrollback.len() < retained_rows {
            self.scrollback.insert(0, false);
        }
    }

    /// Reflow a row ownership mask through the exact same Grid algorithm as
    /// the primary document. A shadow grid carries an opaque marker in every
    /// retained cell, so wrapping and logical-line merges transform ownership
    /// together with content instead of merely resizing the viewport suffix.
    fn reflowed(&self, grid: &Grid, rows: usize, cols: usize) -> Self {
        let retained_rows = grid.scrollback.len().saturating_add(grid.num_rows);
        let max_reflow_rows = retained_rows
            .saturating_mul(grid.num_cols.max(1))
            .saturating_div(cols.max(1))
            .saturating_add(retained_rows)
            .max(rows);
        let mut shadow = Grid::with_scrollback(grid.num_rows, grid.num_cols, max_reflow_rows);
        for index in 0..grid.scrollback.len() {
            if let Some(row) = grid.scrollback.get(index) {
                shadow.scrollback.push(marked_row(
                    &row,
                    self.scrollback.get(index).copied().unwrap_or(false),
                ));
            }
        }
        shadow.viewport = grid
            .viewport
            .iter()
            .enumerate()
            .map(|(index, row)| {
                marked_row(
                    row,
                    self.viewport
                        .as_deref()
                        .and_then(|owned| owned.get(index))
                        .copied()
                        .unwrap_or(false),
                )
            })
            .collect();
        shadow.cursor = grid.cursor.clone();
        shadow.resize(rows, cols);

        Self {
            scrollback: (0..shadow.scrollback.len())
                .map(|index| {
                    shadow
                        .scrollback
                        .get(index)
                        .is_some_and(|row| row_is_owned(&row))
                })
                .collect(),
            viewport: self
                .viewport
                .as_ref()
                .map(|_| shadow.viewport.iter().map(row_is_owned).collect::<Vec<_>>()),
        }
    }
}

const OWNED_MARKER: CellColor = CellColor::Rgb(Color {
    r: 11,
    g: 37,
    b: 73,
    a: 0,
});
const UNOWNED_MARKER: CellColor = CellColor::Rgb(Color {
    r: 73,
    g: 37,
    b: 11,
    a: 0,
});

fn marked_row(row: &Row, owned: bool) -> Row {
    let mut marked = row.clone();
    let marker = if owned { OWNED_MARKER } else { UNOWNED_MARKER };
    for cell in &mut marked.cells {
        cell.bg = marker;
    }
    marked
}

fn row_is_owned(row: &Row) -> bool {
    row.cells.iter().any(|cell| cell.bg == OWNED_MARKER)
}

impl Terminal {
    pub(in crate::vt) fn reflow_primary_screen_candidate(
        &mut self,
        rows: usize,
        cols: usize,
        hidden: bool,
    ) {
        let grid = if hidden {
            &mut self.alt_grid
        } else {
            &mut self.grid
        };
        let mut ownership = self
            .capabilities
            .primary_screen_ownership
            .reflowed(grid, rows, cols);
        self.capabilities.primary_screen_document_candidate = grid
            .resize_preserving_document_position(
                self.capabilities.primary_screen_document_candidate,
                rows,
                cols,
            );
        // The shadow grid deliberately has enough capacity to observe the
        // complete reflow, while the real grid keeps its configured
        // scrollback cap. Align both documents after the real resize drops
        // an overflowing oldest prefix.
        ownership.retain_scrollback_suffix(grid.scrollback.len());
        self.capabilities.primary_screen_ownership = ownership;
    }

    pub(in crate::vt) fn resize_visible_primary_screen_dims(&mut self, rows: usize, cols: usize) {
        self.grid.resize_dims(rows, cols);
        self.capabilities
            .primary_screen_ownership
            .resize_viewport(rows);
    }

    pub(in crate::vt) fn resize_hidden_primary_screen_dims(&mut self, rows: usize, cols: usize) {
        self.alt_grid.resize_dims(rows, cols);
        self.capabilities
            .primary_screen_ownership
            .resize_viewport(rows);
    }
}
