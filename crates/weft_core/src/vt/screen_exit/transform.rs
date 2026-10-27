use super::ownership::PrimaryScreenOwnership;
use super::Terminal;

impl Terminal {
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
                // F2 (PLAN_v1137): amortized front trim — drain in batches of
                // TRIM_BATCH instead of one row per LF (a 10KB memmove per
                // line at the scrollback cap). Consumers read through
                // `scrollback_mask_tail_aligned`, so the transient excess is
                // invisible to them.
                let mask = &mut self.capabilities.primary_screen_ownership.scrollback;
                let target = self.grid.scrollback.len();
                if mask.len() > target + PrimaryScreenOwnership::TRIM_BATCH {
                    let expired = mask.len() - target;
                    mask.drain(..expired);
                }
                while self.capabilities.primary_screen_ownership.scrollback.len()
                    < self.grid.scrollback.len()
                {
                    self.capabilities
                        .primary_screen_ownership
                        .scrollback
                        .insert(0, false);
                }
                // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): rows pushed out
                // during a screen-owned session were captured into the
                // in-flight block's screen prefix by
                // `capture_scrolled_out_screen_rows`; flip their ownership so
                // the snapshot walk skips them (their text lives in the
                // prefix — keeping them owned would duplicate them in the
                // composed block text). Same gates as the capture.
                if self.block_tracker.screen_document_start().is_some()
                    && self.capabilities.primary_screen_interrupt_capture.is_none()
                {
                    let mask = &mut self.capabilities.primary_screen_ownership.scrollback;
                    let from = mask.len().saturating_sub(pushed);
                    for entry in &mut mask[from..] {
                        *entry = false;
                    }
                }
            }
            transform_viewport_ownership(owned, top, bottom, count, down);
        }
    }
}

pub(super) fn transform_viewport_ownership(
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

pub(super) fn transform_document_start(
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
