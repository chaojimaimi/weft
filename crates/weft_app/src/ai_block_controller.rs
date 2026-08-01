//! v1.8.2: Block diagnose AI controller.
//!
//! Bridges the block view and `AiState::spawn_diagnose`. When the user
//! triggers a diagnose (via the header button or `Action::DiagnoseBlock`),
//! the block's command/output/exit_code/cwd are pulled from the terminal's
//! `BlockTracker` and passed to the AI backend. Results are routed back
//! through `poll_ai_results` → `AiResultEvent::Diagnose` →
//! `block_diagnose_state`.

use crate::ai::DiagnosePrompt;
use crate::app_state::BlockDiagnoseState;
use weft_core::blocks::BlockId;

impl crate::App {
    /// v1.8.2: Diagnose the block currently under the mouse cursor (or the
    /// selected block if no hover). Pulls the block's data from the active
    /// terminal's `BlockTracker`, constructs a `DiagnosePrompt`, and spawns
    /// an AI request. No-op when AI is not configured, the block doesn't
    /// exist, or the block hasn't failed (exit_code is None or 0).
    pub(crate) fn spawn_block_diagnose_from_hover(&mut self) {
        if !self.ai_state.is_configured() {
            tracing::debug!("AI not configured; DiagnoseBlock ignored");
            return;
        }

        // Prefer the hovered block; fall back to the selected block.
        let target = self
            .interaction
            .block_hovered
            .or(self.interaction.block_selected);

        let Some(block_id) = target else {
            tracing::debug!("no block hovered/selected; DiagnoseBlock ignored");
            return;
        };

        self.spawn_block_diagnose(block_id);
    }

    /// v1.8.2: Spawn a diagnose request for a specific block by id.
    /// Looks up the block in the active terminal's session blocks, builds
    /// a `DiagnosePrompt`, and calls `AiState::spawn_diagnose`. Stores the
    /// pending request id in `block_diagnose_state` so `poll_ai_results`
    /// can route the result back.
    pub(crate) fn spawn_block_diagnose(&mut self, block_id: BlockId) {
        if !self.ai_state.is_configured() {
            return;
        }

        // Find the block in the active terminal's session blocks.
        let Some(block) = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .and_then(|t| {
                t.block_tracker()
                    .session_blocks()
                    .iter()
                    .find(|b| b.id == block_id)
            })
            .cloned()
        else {
            tracing::warn!(?block_id, "block not found for diagnose");
            return;
        };

        // Only diagnose failed blocks (exit_code is Some and non-zero).
        let Some(exit_code) = block.exit_code else {
            tracing::debug!(?block_id, "block has no exit code; skipping diagnose");
            return;
        };
        if exit_code == 0 {
            tracing::debug!(?block_id, "block succeeded (exit 0); skipping diagnose");
            return;
        }

        // If there's already a pending request for this block, cancel it
        // before spawning a new one.
        if let Some(state) = self.block_diagnose_state.get(&block_id) {
            if let Some(id) = state.pending_id {
                self.ai_state.cancel(id);
            }
        }

        let prompt = DiagnosePrompt {
            command: block.command.clone(),
            output: block.output.to_string(),
            exit_code,
            cwd: block.cwd.clone().unwrap_or_default(),
        };

        if let Some(id) = self.ai_state.spawn_diagnose(prompt) {
            self.block_diagnose_state.insert(
                block_id,
                BlockDiagnoseState {
                    pending_id: Some(id),
                    result: None,
                },
            );
            self.request_redraw();
        }
    }

    /// v1.8.2: Close the diagnose panel for a block and cancel any pending
    /// request. Called when the user clicks the panel's close button.
    pub(crate) fn close_block_diagnose(&mut self, block_id: BlockId) {
        if let Some(state) = self.block_diagnose_state.remove(&block_id) {
            if let Some(id) = state.pending_id {
                self.ai_state.cancel(id);
            }
            self.request_redraw();
        }
    }

    /// v1.8.2: Clear all diagnose state (e.g. when switching tabs or closing
    /// the terminal). Cancels all pending requests.
    #[allow(dead_code)]
    pub(crate) fn clear_all_block_diagnose(&mut self) {
        if self.block_diagnose_state.is_empty() {
            return;
        }
        // Cancel only the diagnose-related pending requests. We can't easily
        // distinguish diagnose ids from command-gen ids in AiState's flat
        // cancellation map, so we cancel all — the palette's AiCommand mode
        // is closed when switching tabs anyway.
        self.ai_state.cancel_all();
        self.block_diagnose_state.clear();
    }
}
