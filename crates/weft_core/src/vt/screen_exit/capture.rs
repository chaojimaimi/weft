//! Primary-screen output capture — the data-plane takeover point and the
//! document snapshot primitive shared across the interrupt, freeze, snapshot
//! and settle paths.

use super::Terminal;
use crate::blocks::StyledOutput;

impl Terminal {
    pub(super) fn begin_primary_screen_output_capture(&mut self) {
        // v1.10.23 (FIX_OMP_CONTENT_LOSS): a new screen-owned session starts
        // with a clean preservation history. Nested 133 markers keep
        // `screen_document_start` set, so the accumulated frames survive
        // them; only a real boundary (settle → next command) resets it.
        let starting = self.block_tracker.screen_document_start().is_none();
        // v1.11.8 (PLAN_v1118 M-B): the data-plane takeover flip point.
        // v1.11.5's forensics lesson (flip points had no logs) closes here —
        // the first ownership of a command is the interesting boundary.
        if starting {
            tracing::info!(
                cursor_ops = self.capabilities.primary_screen_cursor_ops,
                phase = ?self.block_tracker.phase(),
                mode = ?self.tui_render_mode,
                "beginning primary-screen output capture (data plane takeover)"
            );
        } else {
            tracing::debug!(
                cursor_ops = self.capabilities.primary_screen_cursor_ops,
                phase = ?self.block_tracker.phase(),
                mode = ?self.tui_render_mode,
                "continuing primary-screen output capture (nested 133 marker)"
            );
        }
        if starting {
            self.capabilities.screen_history = crate::vt::capability::ScreenHistory::default();
        }
        self.block_tracker
            .begin_screen_owned_output(self.capabilities.primary_screen_document_candidate);
        // v1.10.25 (FIX_TUI_HISTORY_INCREMENTAL): first ownership — fold the
        // retained owned pre-capture rows into the screen prefix so the
        // composed transcript keeps document order.
        if starting && self.block_tracker.screen_document_start().is_some() {
            self.rebase_screen_prefix_at_capture_start();
        }
    }

    /// PLAN_B Phase 0 (docs/PLAN_B_phase0.md P0-1): the single snapshot façade
    /// every primary-screen path must route through — from Phase 1 (flat
    /// storage migration) onward this is the ONLY call site allowed to invoke
    /// the Grid snapshot fns, so the storage-source flip touches exactly one
    /// producer.
    ///
    /// Documented exception: `freeze.rs`'s scrollback-only frame-preservation
    /// path still calls the Grid directly because it needs the empty-viewport
    /// ownership-mask form this signature cannot express; that whole path
    /// migrates to the Phase 1 index mapping.
    pub fn primary_screen_document_snapshot(
        &self,
        document_start: u64,
    ) -> (String, StyledOutput, Option<usize>) {
        // v1.6.1: resolve hyperlink ids to URLs via the Terminal's registry
        // so captured Block output preserves OSC 8 links. The closure borrows
        // `&self.hyperlinks` immutably, which coexists with `&self.grid`.
        let url_resolver = |id: u32| -> Option<std::sync::Arc<str>> {
            self.hyperlinks.url(id).map(std::sync::Arc::<str>::from)
        };
        // PLAN_v11217 §3.5 (T4): the snapshot walk's text budget derives from
        // the tracker's configured cap (single source: `tracker.output_cap`;
        // the Grid receives it as the `text_cap` argument).
        let text_cap = self.block_tracker.output_cap();
        self.capabilities
            .primary_screen_ownership
            .viewport
            .as_ref()
            .map_or_else(
                || {
                    self.grid.document_snapshot_from_position_with_resolver(
                        document_start,
                        url_resolver,
                        text_cap,
                    )
                },
                |owned| {
                    // F2: tail-aligned read — the physical mask may carry up
                    // to TRIM_BATCH stale front entries (see ownership.rs).
                    let scrollback_mask = self
                        .capabilities
                        .primary_screen_ownership
                        .scrollback_mask_tail_aligned(self.grid.scrollback.len());
                    self.grid
                        .document_snapshot_from_position_with_ownership_masks_and_resolver(
                            document_start,
                            scrollback_mask,
                            owned,
                            url_resolver,
                            text_cap,
                        )
                },
            )
    }
}
