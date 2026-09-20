//! Primary-screen scroll and row manipulation — the TUI-safe scroll
//! discipline (no GPU blit under a repainting owner) and runtime scrollback
//! limits.

use super::Terminal;

impl Terminal {
    /// Scroll the grid and keep screen-document and viewport-relative side
    /// state synchronized with the same row rotation.
    pub(in crate::vt) fn scroll_grid_up(&mut self, count: usize) {
        self.scroll_grid_rows(count, false);
    }

    pub(in crate::vt) fn scroll_grid_down(&mut self, count: usize) {
        self.scroll_grid_rows(count, true);
    }

    /// v1.10.4: Whether the current viewport is owned by a TUI that repaints
    /// after scrolling (alt-screen apps, or primary-screen TUIs with >= 2
    /// cursor-addressing ops — the openclaw/Claude-Code pattern). Such apps
    /// must NOT use the GPU scroll-blit fast path (which assumes scrolled
    /// rows keep their content): they overwrite scrolled rows with their
    /// redraw, so blitting stale content under the redraw produced the
    /// "content squeezed together / overlapping" corruption. The print path
    /// also uses this to keep cursor-follow viewport scrolls alive across
    /// TUI repaints.
    pub(super) fn tui_owned_scroll(&self) -> bool {
        self.capabilities.alt_active || self.capabilities.primary_screen_cursor_ops >= 2
    }

    fn scroll_grid_rows(&mut self, count: usize, down: bool) {
        let origin_before = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        if down {
            self.grid.scroll_down(count);
        } else {
            self.grid.scroll_up(count);
        }
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): capture rows pushed out of
        // the viewport into the screen prefix before the ownership transform
        // rotates the viewport mask (a no-op for down-scrolls — pushed is 0).
        self.capture_scrolled_out_screen_rows(origin_before);
        self.transform_primary_screen_rows(
            origin_before,
            self.grid.scrollback.position(),
            top,
            bottom,
            count,
            down,
        );
        if self.tui_owned_scroll() {
            self.grid.discard_scroll_and_dirty_all();
        }
        if !self.hyperlinks.cell_map_is_empty() {
            self.hyperlinks.clear_cell_map();
        }
    }

    /// Apply a runtime scrollback limit to the primary grid and its ownership
    /// mask as one transaction. The primary grid is hidden in `alt_grid`
    /// while an alternate-screen application is active.
    pub fn set_scrollback_max_lines(&mut self, max_lines: usize) {
        let primary = if self.capabilities.alt_active {
            &mut self.alt_grid
        } else {
            &mut self.grid
        };
        primary.set_scrollback_max_lines(max_lines);
        self.capabilities
            .primary_screen_ownership
            .retain_scrollback_suffix(primary.scrollback.len());
    }

    pub(in crate::vt) fn index_primary_screen(&mut self) -> bool {
        let origin = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        let scrolled = self.grid.index();
        if scrolled {
            // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): capture the row pushed
            // out of the viewport into the screen prefix before the ownership
            // transform rotates the viewport mask.
            self.capture_scrolled_out_screen_rows(origin);
            self.transform_primary_screen_rows(
                origin,
                self.grid.scrollback.position(),
                top,
                bottom,
                1,
                false,
            );
            // v1.10.4: LF overflow is THE dominant scroll path for
            // primary-screen TUIs (content streaming past the bottom row).
            // `grid.index()` records a pending_scroll delta; for TUI-owned
            // viewports we discard it so the renderer rebuilds instead of
            // GPU-blitting stale content under the app's redraw.
            if self.tui_owned_scroll() {
                self.grid.discard_scroll_and_dirty_all();
            }
        }
        scrolled
    }

    pub(in crate::vt) fn reverse_index_primary_screen(&mut self) -> bool {
        let origin = self.grid.scrollback.position();
        let (top, bottom) = self.grid.scroll_region();
        let scrolled = self.grid.reverse_index();
        if scrolled {
            self.transform_primary_screen_rows(
                origin,
                self.grid.scrollback.position(),
                top,
                bottom,
                1,
                true,
            );
            if self.tui_owned_scroll() {
                self.grid.discard_scroll_and_dirty_all();
            }
        }
        scrolled
    }

    pub(in crate::vt) fn insert_primary_screen_lines(&mut self, count: usize) {
        let origin = self.grid.scrollback.position();
        let row = self.grid.cursor.row;
        let (top, bottom) = self.grid.scroll_region();
        self.grid.insert_blank_lines(count);
        if (top..=bottom).contains(&row) {
            self.transform_primary_screen_rows(origin, origin, row, bottom, count, true);
        }
    }

    pub(in crate::vt) fn delete_primary_screen_lines(&mut self, count: usize) {
        let origin = self.grid.scrollback.position();
        let row = self.grid.cursor.row;
        let (top, bottom) = self.grid.scroll_region();
        self.grid.delete_lines(count);
        if (top..=bottom).contains(&row) {
            self.transform_primary_screen_rows(origin, origin, row, bottom, count, false);
        }
    }
}
