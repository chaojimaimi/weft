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
    /// Logical insertion position; advances beyond ring capacity.
    position: u64,
}

impl Scrollback {
    pub fn new(max_lines: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(max_lines),
            max_lines,
            head: 0,
            len: 0,
            position: 0,
        }
    }

    /// Push a row into the scrollback buffer.
    pub fn push(&mut self, row: Row) {
        if self.max_lines == 0 {
            return;
        }
        self.position = self.position.saturating_add(1);
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

    /// Number of lines in scrollback.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clear retained rows without resetting the logical insertion position.
    /// Screen-capture baselines remain valid across CSI 3J.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.head = 0;
        self.len = 0;
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    /// Index of the oldest retained row inserted at or after `position`.
    pub fn index_since(&self, position: u64) -> usize {
        if position > self.position {
            return 0; // The buffer was cleared and recreated.
        }
        let oldest = self.position.saturating_sub(self.len as u64);
        position.max(oldest).saturating_sub(oldest) as usize
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
    pub fn resize(&mut self, new_max: usize, _cols: usize) {
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

        // Keep the newest rows when shrinking so logical positions remain a
        // contiguous suffix ending at `position`.
        let mut rows: Vec<Row> = Vec::with_capacity(new_max);
        let keep = self.len.min(new_max);
        let start = self.len.saturating_sub(keep);
        for i in start..self.len {
            if let Some(row) = self.get(i) {
                rows.push(row.clone());
            }
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
