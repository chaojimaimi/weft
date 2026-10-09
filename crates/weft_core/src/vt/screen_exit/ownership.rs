//! Primary-screen row ownership mask (v1.10.19+) and its resize migration.
//!
//! T5 (D4/PLAN_B): the mask migrates through a resize via the protocol's
//! old→new row map — O(rows) index arithmetic — instead of the deleted
//! shadow-grid full clone (a whole `Grid` of marker-painted rows reflowed
//! per resize). A post-resize row is owned iff any pre-resize row whose
//! content it carries was owned; that matches the shadow's `row_is_owned`
//! any-marker reduction, because wrapped fragments carry their source rows'
//! bytes and empty rows carry no bytes at all.

use super::Terminal;

#[derive(Clone, Default)]
pub(in crate::vt) struct PrimaryScreenOwnership {
    pub(in crate::vt) scrollback: Vec<bool>,
    pub(in crate::vt) viewport: Option<Vec<bool>>,
}

impl PrimaryScreenOwnership {
    /// v1.13.7 F2 (PLAN_v1137): the scroll handler lets the physical mask
    /// run up to [`Self::TRIM_BATCH`] entries longer than the scrollback
    /// before draining (amortized front trim — draining 1 row per LF was
    /// a 10KB memmove per line at the scrollback cap). Consumers therefore
    /// read the TAIL-ALIGNED slice: the last `scrollback_len` entries are
    /// exactly the logical mask for the current scrollback rows.
    pub(in crate::vt) const TRIM_BATCH: usize = 256;

    pub(in crate::vt) fn scrollback_mask_tail_aligned(&self, scrollback_len: usize) -> &[bool] {
        let excess = self.scrollback.len().saturating_sub(scrollback_len);
        &self.scrollback[excess..]
    }

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

    /// Reduces the mask through [`crate::grid::GridRowMap`]: every old
    /// document row (scrollback rows first, then viewport rows) paints its
    /// ownership onto the post-rebuild rows its content occupies.
    ///
    /// `viewport_had_mask = false` means "no ownership evidence yet" — the
    /// capture path must stay unfiltered for viewport rows, so the absence
    /// carries through (the shadow mapped `Option::as_ref` the same way).
    pub(in crate::vt) fn from_row_map(
        map: &crate::grid::GridRowMap,
        pre_flags: &[bool],
        viewport_had_mask: bool,
    ) -> Self {
        let total = map.old_row_new_range.last().map_or(0, |(_, last)| last + 1);
        let mut owned = vec![false; total];
        for ((first, last), &flag) in map.old_row_new_range.iter().zip(pre_flags) {
            if flag {
                for owned_flag in &mut owned[*first..=*last] {
                    *owned_flag = true;
                }
            }
        }
        let flat = map.flat_rows.min(owned.len());
        let viewport = viewport_had_mask.then(|| {
            let mut mask = owned
                .get(flat..flat + map.popped)
                .map(<[bool]>::to_vec)
                .unwrap_or_default();
            // Blank padding rows the pop could not fill are unowned.
            mask.resize(map.viewport_rows, false);
            mask
        });
        Self {
            scrollback: owned[..flat].to_vec(),
            viewport,
        }
    }
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
        // Capture the pre-resize flags in unified document order (scrollback
        // rows, then viewport rows) — the same order the row map keys on.
        let viewport_had_mask = self
            .capabilities
            .primary_screen_ownership
            .viewport
            .is_some();
        // F2: tail-aligned clone — the physical mask may carry up to
        // TRIM_BATCH stale front entries; pre_flags must key on the logical
        // scrollback rows (see scrollback_mask_tail_aligned).
        let mut pre_flags = self
            .capabilities
            .primary_screen_ownership
            .scrollback_mask_tail_aligned(grid.scrollback.len())
            .to_vec();
        match &self.capabilities.primary_screen_ownership.viewport {
            Some(viewport_owned) => pre_flags.extend_from_slice(viewport_owned),
            None => pre_flags.resize(grid.scrollback.len() + grid.num_rows, false),
        }
        let (new_candidate, row_map) = grid.resize_preserving_document_position(
            self.capabilities.primary_screen_document_candidate,
            rows,
            cols,
        );
        self.capabilities.primary_screen_document_candidate = new_candidate;
        // T5 review P2: an identity map (dimensions unchanged) carries no
        // rows to reduce — returning an empty mask here would wipe every
        // scrap of ownership evidence on a PTY resize that ended up a no-op.
        let mut ownership = if row_map.old_row_new_range.is_empty() {
            self.capabilities.primary_screen_ownership.clone()
        } else {
            PrimaryScreenOwnership::from_row_map(&row_map, &pre_flags, viewport_had_mask)
        };
        // The protocol's `apply_max_rows` may have evicted an oldest prefix;
        // align the mask with the surviving document.
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
