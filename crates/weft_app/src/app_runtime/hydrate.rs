//! Startup history-hydration family, moved verbatim out of
//! `app_runtime.rs` (v1.13.9 backlog clear zero-behavior file-budget
//! split). `hydrate_persisted_history` is `pub(super)` so the sibling
//! test modules (retention_tests / tests) keep calling it directly;
//! `hydrate_tabs_from_history_store` is `pub(crate)` — the exact
//! effective visibility of the pre-split `pub(super)` (app_runtime is a
//! crate-root module), so recovery_controller's call site is unchanged.

use super::*;

/// v1.7.6 → FIX ② (2026-08-19): Hydrate a terminal with persisted history.
///
/// The editor's ↑-key recall gets ONLY this tab's commands (filtered by
/// `tab_block_ids`), matching Warp's per-tab session-scoped history. The block
/// tracker similarly gets ONLY this tab's blocks. See FIX_EDITOR_HISTORY_PER_TAB.md
/// for Warp comparison and rationale.
///
/// Before the fix, editor ↑-key recall incorrectly used GLOBAL history (all tabs
/// mixed together), while the block tracker was correctly per-tab — this mismatch
/// meant "Cmd+Shift+B panel is per-tab, but ↑ arrows cross-tab".
pub(super) fn hydrate_persisted_history(
    terminal: &mut Terminal,
    global_newest_first: &[weft_core::blocks::Block],
    tab_block_ids: &[u64],
    block_id_allocator: std::sync::Arc<weft_core::block_id_sequence::BlockIdPool>,
) {
    // Editor ↑-key recall: per-tab (this tab's own blocks), consistent with
    // the block tracker / Cmd+Shift+B panel below and Warp's session-scoped
    // up-arrow history. v1.7.6's global hydration mixed all tabs' commands
    // into every tab's ↑ recall (user-visible cross-tab mixing).
    let commands = global_newest_first
        .iter()
        .rev()
        .filter(|b| tab_block_ids.contains(&b.id.0))
        .map(|block| strip_prompt_prefix(&block.command))
        .filter(|command| !command.trim().is_empty())
        .collect();
    terminal.editor_mut().load_history(commands);

    // Block tracker: only this tab's blocks (per-tab isolation).
    // SQLite returns newest→oldest; BlockTracker expects chronological.
    let tab_blocks: Vec<_> = global_newest_first
        .iter()
        .rev()
        .filter(|b| tab_block_ids.contains(&b.id.0))
        .cloned()
        .collect();
    terminal.block_tracker_mut().load_blocks(tab_blocks);
    terminal
        .block_tracker_mut()
        .use_shared_id_allocator(block_id_allocator);
}

impl App {
    /// v1.10.23: Hydrate per-tab history from the SQLite block store.
    ///
    /// Extracted from `resumed()` so BOTH startup paths run it exactly
    /// once, only after the tab topology is final:
    ///
    /// - Synchronous path (no recovery / clean shutdown): right after
    ///   `restore_tab_snapshots()` in `resumed()`.
    /// - Event-driven recovery path: inside `apply_recovery_choice`,
    ///   after the chosen restore path rebuilt the tabs.
    ///
    /// v1.7.6: Per-tab isolation — each tab's block tracker receives ONLY
    /// the blocks it produced last session (from its TabSnapshot's
    /// `block_ids`). The editor's ↑-key recall still gets the full global
    /// history. This prevents all tabs from showing the same mixed history.
    pub(crate) fn hydrate_tabs_from_history_store(&mut self) {
        let Some(block_id_allocator) = self
            .sessions
            .block_store()
            .map(BlockStore::block_id_allocator)
        else {
            return;
        };
        let persisted_history = match self.sessions.block_store() {
            Some(store) => match store.recent(1000) {
                Ok(history) => history,
                Err(e) => {
                    warn!(error = %e, "failed to load block history");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        for tab in self.sessions.tabs_mut() {
            // v1.12.28 (P1-02 ⑤): per-leaf hydration — every pane reads its
            // OWN attached snapshot (the split-tree restore attaches
            // per-leaf), REPLACING the old Deref pass that reached the
            // active pane only; the active pane is covered by this walk
            // exactly once, so no double hydration.
            for pane in tab.panes_mut() {
                let pane_block_ids: Vec<u64> = pane
                    .restored_snapshot
                    .as_ref()
                    .map(|snap| snap.block_ids.clone())
                    .unwrap_or_default();
                pane.with_terminal(|terminal| {
                    hydrate_persisted_history(
                        terminal,
                        &persisted_history,
                        &pane_block_ids,
                        block_id_allocator.clone(),
                    );
                });
            }
        }
    }
}
