//! Grid scroll-region + scrollback-navigation methods.
//!
//! v1.12.27a (P1-05): moved verbatim out of grid/mod.rs (line-budget
//! split; the flat/ submodule impl blocks are the precedent). Zero
//! behavior or visibility changes.

use super::{Grid, Row};

impl Grid {
    // ── Scrolling ────────────────────────────────────────────────

    /// Scroll the scroll region up by n lines.
    /// Lines scrolled off the top go into the scrollback buffer.
    ///
    /// For full-viewport scrolls (the common streaming case), records a
    /// `pending_scroll` delta so the renderer can shift its per-row vertex
    /// cache — an O(n) optimization over a full rebuild. For scroll-region
    /// scrolls (DECSTBM, used by TUI apps like `less`), the renderer's cache
    /// shift can't be used because it operates on the entire cache while the
    /// scroll only affected `[top..=bottom]`. In that case, all rows in the
    /// scroll region are marked dirty so the renderer rebuilds them.
    pub fn scroll_up(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_viewport = top == 0 && bottom == self.num_rows - 1;

        if n > bottom - top {
            // Push all rows in the scroll region into scrollback
            if top == 0 {
                for i in top..=bottom {
                    let old_row = std::mem::replace(&mut self.viewport[i], Row::new(self.num_cols));
                    self.push_history_row(old_row);
                }
            } else {
                for i in top..=bottom {
                    self.viewport[i].clear();
                }
            }
            if full_viewport {
                self.pending_scroll
                    .set(self.pending_scroll.get() + (bottom - top + 1) as i32);
            } else {
                // Scroll region: mark affected rows dirty for rebuild.
                for i in top..=bottom {
                    self.viewport[i].mark_dirty(self.num_cols - 1);
                }
            }
            return;
        }

        if full_viewport {
            // v1.0 perf: Full-viewport scroll using rotate_left.
            // For n=1 (the common streaming case): 1 Row alloc (was 2 with
            // drain+extend, was ~24 with the old shift loop).
            // rotate_left moves [0] to [n-1], shifts [1..] to [0..n-1].
            // We take the old [0] into scrollback first, insert a fresh
            // empty Row at [0], then rotate — the empty Row ends up at [n-1].
            for _ in 0..n {
                let old_top = std::mem::replace(&mut self.viewport[0], Row::new(self.num_cols));
                self.push_history_row(old_top);
                self.viewport.rotate_left(1);
            }
            self.pending_scroll
                .set(self.pending_scroll.get() + n as i32);
        } else {
            // Scroll region (or partial viewport): rotate in place, then
            // clear the exposed bottom rows. For top==0, push the old top
            // rows to scrollback before rotating.
            if top == 0 {
                for i in 0..n {
                    let old_top = std::mem::replace(&mut self.viewport[i], Row::new(self.num_cols));
                    self.push_history_row(old_top);
                }
            }
            self.viewport[top..=bottom].rotate_left(n);
            for i in (bottom + 1 - n)..=bottom {
                self.viewport[i].clear();
            }
            // Mark all rows in the scroll region dirty — the renderer's
            // per-row cache is position-relative and can't be shifted for a
            // partial-region scroll, so rebuild all affected rows.
            for i in top..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Scroll the scroll region down by n lines.
    ///
    /// Like [`scroll_up`](Self::scroll_up), only full-viewport scrolls use
    /// `pending_scroll` for the renderer cache shift. Scroll-region scrolls
    /// mark affected rows dirty instead.
    pub fn scroll_down(&mut self, n: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let full_viewport = top == 0 && bottom == self.num_rows - 1;

        if n > bottom - top {
            for i in top..=bottom {
                self.viewport[i].clear();
            }
            if full_viewport {
                self.pending_scroll
                    .set(self.pending_scroll.get() - (bottom - top + 1) as i32);
            } else {
                for i in top..=bottom {
                    self.viewport[i].mark_dirty(self.num_cols - 1);
                }
            }
            return;
        }

        // v1.0 perf: rotate_right + clear — zero allocations (was O(num_rows)).
        self.viewport[top..=bottom].rotate_right(n);
        for i in top..(top + n) {
            self.viewport[i].clear();
        }
        if full_viewport {
            self.pending_scroll
                .set(self.pending_scroll.get() - n as i32);
        } else {
            for i in top..=bottom {
                self.viewport[i].mark_dirty(self.num_cols - 1);
            }
        }
    }

    /// Set scroll region (CSI r). Parameters are 1-based.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.saturating_sub(1);
        let bottom = bottom.saturating_sub(1).min(self.num_rows - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            // Move cursor to home position
            self.cursor.row = 0;
            self.cursor.col = 0;
            self.cursor.wrap_pending = false;
        }
    }

    /// Reset scroll region to full viewport.
    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.num_rows - 1;
    }

    // ── Scrollback navigation ────────────────────────────────────

    /// Scroll viewport up (view older history).
    pub fn scroll_up_history(&mut self, lines: usize) {
        // Offset can never exceed the number of available history lines;
        // clamping to `scrollback.len()` keeps `cell()` indexing in bounds.
        let max = self.scrollback.len();
        self.set_scroll_offset((self.scroll_offset + lines).min(max));
    }

    /// Scroll viewport down (view newer content).
    pub fn scroll_down_history(&mut self, lines: usize) {
        self.set_scroll_offset(self.scroll_offset.saturating_sub(lines));
    }

    /// Scroll to the very top of history.
    pub fn scroll_to_top(&mut self) {
        self.set_scroll_offset(self.scrollback.len());
    }

    /// Scroll to the bottom (current output).
    pub fn scroll_to_bottom(&mut self) {
        self.set_scroll_offset(0);
    }

    /// Check if we're viewing history (scrolled up).
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Get the total number of scrollback lines.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Get scroll region boundaries (read-only).
    pub fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }
}
