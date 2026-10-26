//! Block-persist effect bodies (PersistBlocks / LoadOlderBlocks arms),
//! moved verbatim out of `effect_dispatch.rs` (v1.13.9 backlog clear
//! zero-behavior file-budget split). `drain_effects` and its for-loop
//! dispatch sequence stay in the parent — only these method bodies moved
//! (plan P2 invariant: effects iteration order and shared-state mutation
//! sequence unchanged).

use crate::warn;
use tracing::info;

/// v1.15.0 (T3): the load-older branch outcome, lifted out of
/// `apply_load_older_blocks` so the five-branch footer decision is a pure,
/// truth-table testable mapping ([`load_older_footer_decision`]). The state
/// writes themselves (`Tab::footer_feedback` /
/// `tracker.footer_exhausted`) stay in the effect body.
pub(crate) enum LoadOlderOutcome {
    /// The store returned `loaded` older blocks (prepended by the caller).
    Loaded(usize),
    /// The store answered empty — the exhausted ruling burns.
    StoreEmpty,
    /// No in-memory blocks at all: a stale click on an unrendered footer.
    /// The hint shows, but the ruling must NOT burn (new commands produce
    /// blocks and the footer must stay usable).
    StaleClick,
    /// The store read failed.
    Failed,
}

/// v1.15.0 (T3): branch → footer-state decision. `Some((message,
/// exhausted))` = show `message` for one TTL and (for `exhausted == true`)
/// set the tracker's persistent dim; `None` = stay silent. The ONLY
/// `exhausted == true` arm is [`LoadOlderOutcome::StoreEmpty`] — the store
/// itself ruled there is nothing older (retention evictions reset that
/// flag, see `enforce_retention`).
pub(crate) fn load_older_footer_decision(outcome: LoadOlderOutcome) -> Option<(String, bool)> {
    use crate::panel_component::{
        FOOTER_FEEDBACK_ERROR, FOOTER_FEEDBACK_EXHAUSTED, FOOTER_FEEDBACK_LOADED_PREFIX,
        FOOTER_FEEDBACK_LOADED_SUFFIX,
    };
    match outcome {
        LoadOlderOutcome::Loaded(loaded) => Some((
            format!("{FOOTER_FEEDBACK_LOADED_PREFIX}{loaded}{FOOTER_FEEDBACK_LOADED_SUFFIX}"),
            false,
        )),
        LoadOlderOutcome::StoreEmpty => Some((FOOTER_FEEDBACK_EXHAUSTED.to_string(), true)),
        LoadOlderOutcome::StaleClick => Some((FOOTER_FEEDBACK_EXHAUSTED.to_string(), false)),
        LoadOlderOutcome::Failed => Some((FOOTER_FEEDBACK_ERROR.to_string(), false)),
    }
}

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
    /// preserved), and reports the outcome IN PLACE on the footer itself
    /// (v1.15.0 T3: `Tab::footer_feedback` — the remote toast is gone for
    /// this action). WHAT each branch writes is decided by the pure
    /// [`load_older_footer_decision`] table; this body only performs the
    /// state writes. The store-missing branch stays silent (unchanged).
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
        let decision = match oldest_started_ms {
            None => {
                // No blocks at all: the footer button is not even rendered,
                // so reaching here means a stale click.
                load_older_footer_decision(LoadOlderOutcome::StaleClick)
            }
            Some(oldest_started_ms) => {
                let Some(store) = self.sessions.block_store() else {
                    // Store missing → silent (pre-T3 behavior preserved).
                    return;
                };
                match store.older_than(oldest_started_ms, LOAD_OLDER_PAGE) {
                    Ok(blocks) if blocks.is_empty() => {
                        load_older_footer_decision(LoadOlderOutcome::StoreEmpty)
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
                        load_older_footer_decision(LoadOlderOutcome::Loaded(loaded))
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to load older block history");
                        load_older_footer_decision(LoadOlderOutcome::Failed)
                    }
                }
            }
        };
        let Some((message, exhausted)) = decision else {
            return;
        };
        if let Some(tab) = self.sessions.active_mut() {
            tab.footer_feedback = Some((message, std::time::Instant::now()));
            if exhausted {
                // The persistent dim rides the tracker that owns the blocks
                // the ruling is about (multi-pane tabs stay per-pane).
                if let Some(mut t) = tab.lock_terminal() {
                    t.block_tracker_mut().burn_footer_exhausted();
                }
            }
        }
        self.request_redraw();
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

#[cfg(test)]
mod tests {
    use super::{load_older_footer_decision, LoadOlderOutcome};

    /// v1.15.0 (T3): the five-branch footer decision table (the store-
    /// missing branch never reaches the decision — silence is the caller's
    /// early return, asserted here as the None-shape contract of the
    /// remaining four).
    #[test]
    fn load_older_footer_decision_truth_table() {
        use crate::panel_component::{FOOTER_FEEDBACK_ERROR, FOOTER_FEEDBACK_EXHAUSTED};
        // Loaded: count message, no exhausted burn.
        assert_eq!(
            load_older_footer_decision(LoadOlderOutcome::Loaded(7)),
            Some(("已加载 7 条".to_string(), false))
        );
        // StoreEmpty: the hint AND the exhausted burn (the only burn arm).
        assert_eq!(
            load_older_footer_decision(LoadOlderOutcome::StoreEmpty),
            Some((FOOTER_FEEDBACK_EXHAUSTED.to_string(), true))
        );
        // Stale click: the same hint, but NO burn.
        assert_eq!(
            load_older_footer_decision(LoadOlderOutcome::StaleClick),
            Some((FOOTER_FEEDBACK_EXHAUSTED.to_string(), false))
        );
        // Failed: the error message, NO burn.
        assert_eq!(
            load_older_footer_decision(LoadOlderOutcome::Failed),
            Some((FOOTER_FEEDBACK_ERROR.to_string(), false))
        );
        // The exhausted message must be exactly the shared constant (both
        // hint arms share it — no third literal to drift).
        assert_eq!(FOOTER_FEEDBACK_EXHAUSTED, "没有更早的历史");
        assert_eq!(FOOTER_FEEDBACK_ERROR, "读取历史失败");
    }
}
