//! Document-boundary freeze and scroll-driven ownership transforms for the
//! primary screen.

use super::Terminal;

impl Terminal {
    pub(super) fn discard_superseded_primary_screen_frame(&mut self) {
        self.clear_primary_screen_scrollback();
        let origin = self.grid.scrollback.position();
        self.capabilities.primary_screen_document_candidate = origin;
        if self.block_tracker.screen_document_start().is_some() {
            self.block_tracker.set_screen_document_start(origin);
        }
        tracing::debug!(origin, "discarded superseded atomic primary-screen frame");
    }

    /// Freeze the shell/TUI boundary at OSC 133;B, before the launched
    /// program can paint text that happens to equal its command name.
    pub(in crate::vt) fn freeze_primary_screen_document_candidate(&mut self) {
        let viewport_start = (0..self.grid.num_rows)
            .rev()
            .find(|&row| !self.grid.row_text(row).trim().is_empty())
            .map_or(0, |row| row.saturating_add(1));
        self.capabilities.primary_screen_document_candidate = self
            .grid
            .scrollback
            .position()
            .saturating_add(viewport_start as u64);
        self.capabilities.primary_screen_ownership.scrollback =
            vec![false; self.grid.scrollback.len()];
        self.capabilities.primary_screen_ownership.viewport = Some(vec![false; self.grid.num_rows]);
        tracing::debug!(
            viewport_start,
            document_start = self.capabilities.primary_screen_document_candidate,
            "primary-screen document boundary"
        );
    }

    pub(in crate::vt) fn include_primary_screen_viewport_row(&mut self, row: usize) {
        if self.capabilities.alt_active {
            return;
        }
        if let Some(touched) = &mut self.capabilities.primary_screen_ownership.viewport {
            if let Some(owned) = touched.get_mut(row) {
                *owned = true;
            }
        }
        let position = self.grid.scrollback.position().saturating_add(row as u64);
        if self.block_tracker.phase() == crate::blocks::ShellPhase::CommandExecuting {
            self.capabilities.primary_screen_document_candidate = self
                .capabilities
                .primary_screen_document_candidate
                .min(position);
        }
        self.block_tracker
            .include_screen_document_position(position);
    }

    pub(super) fn transform_primary_screen_rows(
        &mut self,
        origin_before: u64,
        origin_after: u64,
        top: usize,
        bottom: usize,
        count: usize,
        down: bool,
    ) {
        if self.capabilities.alt_active {
            return;
        }
        let transform = |start| {
            transform_document_start(start, origin_before, origin_after, top, bottom, count, down)
        };
        if self.block_tracker.phase() == crate::blocks::ShellPhase::CommandExecuting {
            self.capabilities.primary_screen_document_candidate =
                transform(self.capabilities.primary_screen_document_candidate);
        }
        if let Some(start) = self.block_tracker.screen_document_start() {
            self.block_tracker
                .set_screen_document_start(transform(start));
        }
        if let Some(owned) = &mut self.capabilities.primary_screen_ownership.viewport {
            let pushed = origin_after.saturating_sub(origin_before) as usize;
            if !down && top == 0 && pushed > 0 {
                self.capabilities
                    .primary_screen_ownership
                    .scrollback
                    .extend(owned.iter().take(pushed.min(owned.len())).copied());
                if self.capabilities.primary_screen_ownership.scrollback.len()
                    > self.grid.scrollback.len()
                {
                    let expired = self
                        .capabilities
                        .primary_screen_ownership
                        .scrollback
                        .len()
                        .saturating_sub(self.grid.scrollback.len());
                    self.capabilities
                        .primary_screen_ownership
                        .scrollback
                        .drain(..expired);
                }
                while self.capabilities.primary_screen_ownership.scrollback.len()
                    < self.grid.scrollback.len()
                {
                    self.capabilities
                        .primary_screen_ownership
                        .scrollback
                        .insert(0, false);
                }
            }
            transform_viewport_ownership(owned, top, bottom, count, down);
        }
    }

    pub(in crate::vt) fn clear_primary_screen_scrollback(&mut self) {
        self.grid.clear_scrollback();
        self.capabilities
            .primary_screen_ownership
            .scrollback
            .clear();
    }
}

fn transform_viewport_ownership(
    owned: &mut [bool],
    top: usize,
    bottom: usize,
    count: usize,
    down: bool,
) {
    let Some(region) = owned.get_mut(top..=bottom) else {
        return;
    };
    let count = count.min(region.len());
    if count == 0 {
        return;
    }
    if down {
        region.rotate_right(count);
        region[..count].fill(false);
    } else {
        region.rotate_left(count);
        let clear_from = region.len() - count;
        region[clear_from..].fill(false);
    }
}

fn transform_document_start(
    start: u64,
    origin_before: u64,
    origin_after: u64,
    top: usize,
    bottom: usize,
    count: usize,
    down: bool,
) -> u64 {
    if start < origin_before || top > bottom {
        return start;
    }
    let row = start.saturating_sub(origin_before) as usize;
    let count = count.min(bottom - top + 1);
    if down {
        if (top..=bottom).contains(&row) {
            origin_after.saturating_add(row.saturating_add(count).min(bottom + 1) as u64)
        } else {
            start.saturating_add(origin_after.saturating_sub(origin_before))
        }
    } else if top == 0 {
        if row <= bottom + 1 {
            start
        } else {
            start.saturating_add(origin_after.saturating_sub(origin_before))
        }
    } else if (top + 1..=bottom).contains(&row) {
        origin_after.saturating_add(row.saturating_sub(count).max(top) as u64)
    } else {
        start.saturating_add(origin_after.saturating_sub(origin_before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_document_start_tracks_viewport_row_rotations() {
        assert_eq!(transform_document_start(3, 0, 1, 0, 5, 1, false), 3);
        assert_eq!(transform_document_start(3, 0, 0, 0, 5, 1, true), 4);
        assert_eq!(transform_document_start(3, 0, 0, 1, 5, 1, false), 2);
        assert_eq!(transform_document_start(3, 0, 0, 1, 5, 1, true), 4);
    }

    #[test]
    fn viewport_ownership_follows_scrolls_and_insert_delete_lines() {
        let mut owned = vec![true, false, true, false, true];
        transform_viewport_ownership(&mut owned, 0, 4, 1, false);
        assert_eq!(owned, [false, true, false, true, false]);

        transform_viewport_ownership(&mut owned, 1, 4, 2, true);
        assert_eq!(owned, [false, false, false, true, false]);

        transform_viewport_ownership(&mut owned, 1, 4, 1, false);
        assert_eq!(owned, [false, false, true, false, false]);
    }
}
