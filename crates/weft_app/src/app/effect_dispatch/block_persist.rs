//! Block-persist effect bodies (PersistBlocks / LoadOlderBlocks arms),
//! moved verbatim out of `effect_dispatch.rs` (v1.13.9 backlog clear
//! zero-behavior file-budget split). `drain_effects` and its for-loop
//! dispatch sequence stay in the parent — only these method bodies moved
//! (plan P2 invariant: effects iteration order and shared-state mutation
//! sequence unchanged).

use crate::warn;
use tracing::info;

impl crate::App {
    /// Persist a batch of drained command blocks to the BlockStore. Best-effort:
    /// each failure is logged but does not abort the remaining inserts. Extracted
    /// from `process_messages` so the same logic serves the `PersistBlocks` effect.
    pub(crate) fn persist_blocks(&self, blocks: &[weft_core::blocks::Block]) {
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        for block in blocks {
            if let Err(e) = store.insert(block) {
                warn!(error = %e, "failed to persist block");
                continue;
            }
            if let Some(index) = &self.search_index {
                let started_ms = block
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                let document = weft_core::search::SearchDocument::from_block(
                    block.id.0,
                    &block.command,
                    block.output.as_ref(),
                    block.cwd.as_deref(),
                    started_ms,
                );
                if let Err(e) = index.upsert(&document) {
                    warn!(error = %e, block_id = block.id.0, "failed to index persisted block");
                }
            }
        }
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.3): the panel footer's「加载更早」action.
    /// Pages one batch of pre-retention history out of SQLite via keyset
    /// pagination, prepends it to the active tab's tracker (time order
    /// preserved), and reports the outcome through the toast channel.
    /// Blocks already in memory are unaffected; an exhausted DB answers with
    /// 「没有更早的历史」.
    pub(crate) fn apply_load_older_blocks(&mut self) {
        const LOAD_OLDER_PAGE: usize = 200;

        let oldest_started_ms = self
            .sessions
            .active()
            .and_then(|tab| {
                tab.with_terminal(|t| t.block_tracker().blocks().first().map(|b| b.started_at))
            })
            .flatten()
            .map(|started| {
                started
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            });
        let Some(oldest_started_ms) = oldest_started_ms else {
            // No blocks at all: the footer button is not even rendered, so
            // reaching here means a stale click — answer with the empty hint.
            self.show_block_history_toast("没有更早的历史");
            return;
        };
        let Some(store) = self.sessions.block_store() else {
            return;
        };
        match store.older_than(oldest_started_ms, LOAD_OLDER_PAGE) {
            Ok(blocks) if blocks.is_empty() => {
                self.show_block_history_toast("没有更早的历史");
            }
            Ok(blocks) => {
                let loaded = blocks.len();
                if let Some(mut t) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.lock_terminal())
                {
                    t.block_tracker_mut().load_older_to_front(blocks);
                }
                info!(loaded, "loaded older block history from store");
                self.show_block_history_toast(&format!("已加载 {loaded} 条"));
                self.request_redraw();
            }
            Err(e) => {
                warn!(error = %e, "failed to load older block history");
            }
        }
    }

    /// v1.11.2 X4: reuse the paste-toast surface for generic feedback text.
    /// Same 3s TTL and 1 Hz expiry tick apply.
    /// v1.12.25 (audit 3-B, P1-05): `pub(crate)` so the palette workflow
    /// arms can surface DB read failures on the same toast channel.
    pub(crate) fn show_block_history_toast(&mut self, message: &str) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_paste_toast(Some((message.to_string(), std::time::Instant::now())));
        }
        self.request_redraw();
    }
}
