//! M6-a rewrite-watermark method family, moved verbatim out of
//! `output_capture.rs` (v1.13.9 backlog clear zero-behavior file-budget
//! split; `impl OutputCapture` cross-file block per the cursor.rs
//! precedent).

use std::sync::atomic::{AtomicUsize, Ordering};

use super::OutputCapture;

impl OutputCapture {
    // ── M6-a rewrite watermark (PLAN_M6 §A-1) ────────────────────────────

    /// Record the earliest byte offset an operation touched. `fetch_min`
    /// keeps the low-water mark; `usize::MAX` means "pure append so far".
    pub(super) fn note_min_write_offset(&self, offset: usize) {
        self.min_write_offset.fetch_min(offset, Ordering::Relaxed);
    }

    /// Read and reset the watermark (take semantics — the live layout
    /// cache's sync consumes it exactly once, after which the capture is
    /// "pure append" again until the next in-place op). Production
    /// consumers go through `InFlightBlock::take_min_write_offset` (which
    /// swaps the shared handle); this direct twin serves the unit tests.
    #[cfg(test)]
    pub(crate) fn take_min_write_offset(&self) -> usize {
        self.min_write_offset.swap(usize::MAX, Ordering::Relaxed)
    }

    /// Shared handle for `InFlightBlock` — lets the renderer's two sync call
    /// sites take the watermark through an immutable borrow of the tracker.
    pub(crate) fn min_write_offset_handle(&self) -> &AtomicUsize {
        &self.min_write_offset
    }

    /// Current watermark value without consuming (test observability).
    #[cfg(test)]
    pub(crate) fn min_write_offset_value(&self) -> usize {
        self.min_write_offset.load(Ordering::Relaxed)
    }
}
