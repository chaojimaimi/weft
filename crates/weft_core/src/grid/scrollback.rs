//! Scrollback buffer (ring buffer).
//!
//! Stores rows that have scrolled off the top of the viewport.

use super::{resize_row_cells, row::Row};

pub struct Scrollback {
    /// Ring buffer of rows.
    buffer: Vec<Row>,
    /// Maximum number of lines (configurable).
    pub(super) max_lines: usize,
    /// Head pointer (next write position).
    head: usize,
    /// Current number of occupied lines.
    len: usize,
}

impl Scrollback {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(max_lines),
            max_lines,
            head: 0,
            len: 0,
        }
    }

    /// Push a row into the scrollback buffer.
    pub fn push(&mut self, row: Row) {
        if self.max_lines == 0 {
            return;
        }
        if self.len < self.max_lines {
            self.buffer.push(row);
            self.len += 1;
            self.head = self.len % self.max_lines;
        } else {
            self.buffer[self.head] = row;
            self.head = (self.head + 1) % self.max_lines;
        }
    }

    /// v1.0 perf: Push multiple rows (avoids repeated method call overhead
    /// in the drain+extend scroll path).
    pub fn extend<I: IntoIterator<Item = Row>>(&mut self, iter: I) {
        for row in iter {
            self.push(row);
        }
    }

    /// Pop the most recent row from scrollback (LIFO for scroll-up undo).
    pub fn pop(&mut self) -> Option<Row> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        if self.len < self.buffer.len() {
            // Still within the Vec, just pop
            self.head = self.len % self.max_lines;
            self.buffer.pop()
        } else {
            // Ring buffer wrap-around case
            let idx = if self.head == 0 {
                self.max_lines - 1
            } else {
                self.head - 1
            };
            self.head = idx;
            // Swap out the row
            let cols = self.buffer[idx].cells.len();
            Some(std::mem::replace(&mut self.buffer[idx], Row::new(cols)))
        }
    }

    /// Number of lines in scrollback.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get a row from scrollback by index (0 = oldest, len-1 = newest).
    pub fn get(&self, index: usize) -> Option<&Row> {
        if index >= self.len {
            return None;
        }
        let start = if self.len < self.max_lines {
            0
        } else {
            self.head
        };
        let actual = (start + index) % self.max_lines.min(self.buffer.len());
        self.buffer.get(actual)
    }

    /// Keep historical rows compatible with dimension-only viewport rendering.
    pub(super) fn resize_cols(&mut self, new_cols: usize) {
        for row in &mut self.buffer {
            resize_row_cells(&mut row.cells, new_cols);
            row.repair_wide_pairs();
        }
    }

    /// Resize the scrollback buffer.
    pub fn resize(&mut self, new_max: usize, cols: usize) {
        if new_max == self.max_lines {
            return;
        }
        if new_max == 0 {
            self.buffer.clear();
            self.len = 0;
            self.head = 0;
            self.max_lines = 0;
            return;
        }

        // Collect rows in order (oldest first)
        let mut rows: Vec<Row> = Vec::with_capacity(new_max);
        for i in 0..self.len.min(new_max) {
            if let Some(row) = self.get(i) {
                rows.push(row.clone());
            }
        }
        // Pad with empty rows if needed
        while rows.len() < new_max.min(self.len) {
            rows.push(Row::new(cols));
        }

        self.buffer = rows;
        self.len = self.buffer.len();
        self.head = self.len % new_max;
        self.max_lines = new_max;
    }

    /// Update maximum lines (without discarding data if growing).
    pub fn set_max_lines(&mut self, max_lines: usize, cols: usize) {
        self.resize(max_lines, cols);
    }
}
