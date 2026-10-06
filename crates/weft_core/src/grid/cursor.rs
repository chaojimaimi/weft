//! Cursor position and style.

/// Cursor position and state.
#[derive(Clone, Debug)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub wrap_pending: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            wrap_pending: false,
        }
    }
}

/// Cursor style (DECSCUSR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorStyle {
    /// Steady block █ (default)
    #[default]
    Block,
    /// Blinking block █
    BlinkingBlock,
    /// Blinking underline _
    BlinkingUnderline,
    /// Steady underline _
    Underline,
    /// Blinking bar |
    BlinkingBar,
    /// Steady bar |
    Bar,
}

impl CursorStyle {
    pub fn is_blinking(self) -> bool {
        matches!(
            self,
            Self::BlinkingBlock | Self::BlinkingUnderline | Self::BlinkingBar
        )
    }

    pub fn is_block(self) -> bool {
        matches!(self, Self::Block | Self::BlinkingBlock)
    }

    pub fn is_bar(self) -> bool {
        matches!(self, Self::Bar | Self::BlinkingBar)
    }

    pub fn is_underline(self) -> bool {
        matches!(self, Self::Underline | Self::BlinkingUnderline)
    }
}

// v1.12.27a (P1-05): the Grid cursor-movement / save-restore / tab-stop
// impl block, moved verbatim out of grid/mod.rs (line-budget split; the
// flat/ submodule impl blocks are the precedent). Only visibility edit:
// `cursor_in_scroll_region` is `pub(crate)` because the editing.rs
// partition (insert_blank_lines / delete_lines) still calls it.
use super::Grid;

impl Grid {
    // ── Cursor movement ──────────────────────────────────────────

    /// Move cursor up by `rows` lines, clamping at scroll_top or row 0.
    pub fn move_up(&mut self, rows: usize) {
        self.cursor.wrap_pending = false;
        let min_row = if self.cursor_in_scroll_region() {
            self.scroll_top
        } else {
            0
        };
        self.cursor.row = self.cursor.row.saturating_sub(rows).max(min_row);
    }

    /// Move cursor down by `rows` lines, clamping at scroll_bottom or last row.
    pub fn move_down(&mut self, rows: usize) {
        self.cursor.wrap_pending = false;
        let max_row = if self.cursor_in_scroll_region() {
            self.scroll_bottom
        } else {
            self.num_rows - 1
        };
        self.cursor.row = (self.cursor.row + rows).min(max_row);
    }

    /// Move cursor forward (right) by `cols` columns.
    pub fn move_forward(&mut self, cols: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = (self.cursor.col + cols).min(self.num_cols - 1);
    }

    /// Move cursor backward (left) by `cols` columns.
    pub fn move_backward(&mut self, cols: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(cols);
    }

    /// Move cursor to absolute row (0-based).
    pub fn set_cursor_row(&mut self, row: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.row = row.min(self.num_rows - 1);
    }

    /// Move cursor to absolute column (0-based).
    pub fn set_cursor_col(&mut self, col: usize) {
        self.cursor.wrap_pending = false;
        self.cursor.col = col.min(self.num_cols - 1);
    }

    /// Move cursor to the position given by CUP/HVP (1-based params).
    ///
    /// Under DECOM (origin mode), the row is relative to the scroll-region top
    /// and clamped to the region — full-screen TUI apps (e.g. `claude`, vim)
    /// set a scroll region + DECOM and expect CUP to be region-relative. We
    /// support only vertical margins (DECSTBM); columns stay absolute
    /// (no DECSLRM left/right margins).
    pub fn goto(&mut self, row: usize, col: usize, origin_mode: bool) {
        self.cursor.wrap_pending = false;
        let (origin_row, max_row) = if origin_mode {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.num_rows - 1)
        };
        let r = row.saturating_sub(1);
        self.cursor.row = (origin_row + r).min(max_row);
        self.cursor.col = col.saturating_sub(1).min(self.num_cols - 1);
    }

    /// Carriage return: move cursor to column 0.
    pub fn carriage_return(&mut self) {
        self.cursor.wrap_pending = false;
        self.cursor.col = 0;
    }

    /// Backspace: move cursor left one column (clamped at 0).
    pub fn backspace(&mut self) {
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
        } else {
            self.cursor.col = self.cursor.col.saturating_sub(1);
        }
    }

    // ── Cursor save/restore ──────────────────────────────────────

    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(self.cursor.clone());
    }

    pub fn restore_cursor(&mut self) {
        if let Some(saved) = &self.saved_cursor {
            self.cursor = saved.clone();
        }
    }

    /// Check if cursor is within the scroll region.
    pub(crate) fn cursor_in_scroll_region(&self) -> bool {
        self.cursor.row >= self.scroll_top && self.cursor.row <= self.scroll_bottom
    }

    // ── Tab stops ────────────────────────────────────────────────

    /// Set a tab stop at the current cursor column (ESC H / HTS).
    pub fn set_tabstop(&mut self) {
        if self.cursor.col < self.tabstops.len() {
            self.tabstops[self.cursor.col] = true;
        }
    }

    /// Clear tab stop at cursor column (CSI 0g).
    pub fn clear_tabstop(&mut self) {
        if self.cursor.col < self.tabstops.len() {
            self.tabstops[self.cursor.col] = false;
        }
    }

    /// Clear all tab stops (CSI 3g).
    pub fn clear_all_tabstops(&mut self) {
        // v1.11.16: `fill` supersedes the manual loop (clippy::manual_slice_fill
        // is a hard error under CI's `-D warnings`).
        self.tabstops.fill(false);
    }

    /// Advance cursor to next tab stop (CSI I / HT).
    pub fn advance_tab(&mut self, count: usize) {
        self.cursor.wrap_pending = false;
        for _ in 0..count {
            let mut next = self.cursor.col + 1;
            while next < self.num_cols && next < self.tabstops.len() {
                if self.tabstops[next] {
                    break;
                }
                next += 1;
            }
            self.cursor.col = next.min(self.num_cols - 1);
        }
    }

    /// Move cursor back to previous tab stop (CSI Z).
    pub fn back_tab(&mut self, count: usize) {
        self.cursor.wrap_pending = false;
        for _ in 0..count {
            if self.cursor.col == 0 {
                break;
            }
            let mut prev = self.cursor.col;
            loop {
                if prev == 0 {
                    break;
                }
                prev -= 1;
                if self.tabstops[prev] {
                    break;
                }
            }
            self.cursor.col = prev;
        }
    }
}
