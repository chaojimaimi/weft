//! Effect dispatch + clipboard/paste helpers extracted from `main.rs`.
//!
//! `drain_effects` is the single fan-out point for `Effect` values produced
//! across the app (PTY writes, interrupts, resizes, clipboard, persistence,
//! tab lifecycle). `copy_selection` and `apply_paste*` live here because they
//! are the two main producers/consumers of clipboard effects.

use crate::effect::Effect;
use crate::{clipboard_copy, clipboard_paste, warn};
use weft_core::input::encode_paste;

impl crate::App {
    /// Fan out `effects` to their side effects. Each effect is dispatched
    /// exactly once; a failed dispatch logs but does not abort the remaining
    /// effects in the batch.
    pub(crate) fn drain_effects(&mut self, effects: impl IntoIterator<Item = Effect>) {
        for effect in effects {
            match effect {
                Effect::WritePty { tab, bytes } => {
                    if let Some(session) = self.sessions.tab_mut(tab) {
                        let session_id = session.session_id;
                        let input_seq = session.input_seq();
                        let (screen_owner, settle_state, history_snapshot_due) = session
                            .terminal
                            .as_ref()
                            .map(|t| (t.screen_owner(), t.settle_state(), t.history_snapshot_due()))
                            .unwrap_or((
                                weft_core::vt::ScreenOwner::Shell,
                                weft_core::vt::SettleState::Idle,
                                false,
                            ));
                        tracing::debug!(
                            session_id,
                            input_seq,
                            tab,
                            bytes_len = bytes.len(),
                            %screen_owner,
                            %settle_state,
                            history_snapshot_due,
                            delivery = "pty-write",
                            "effect dispatched",
                        );
                        if let Err(error) = session.write_user_input(&bytes) {
                            warn!(%error, tab, "failed to apply PTY write effect");
                        }
                    }
                }
                Effect::InterruptPty { tab } => {
                    if let Some(session) = self.sessions.tab_mut(tab) {
                        let session_id = session.session_id;
                        let input_seq = session.input_seq();
                        let (screen_owner, settle_state, history_snapshot_due) = session
                            .terminal
                            .as_ref()
                            .map(|t| (t.screen_owner(), t.settle_state(), t.history_snapshot_due()))
                            .unwrap_or((
                                weft_core::vt::ScreenOwner::Shell,
                                weft_core::vt::SettleState::Idle,
                                false,
                            ));
                        tracing::debug!(
                            session_id,
                            input_seq,
                            tab,
                            %screen_owner,
                            %settle_state,
                            history_snapshot_due,
                            delivery = "pty-etx",
                            "interrupt effect dispatched",
                        );
                        let delivered = session.interrupt_pty();
                        if !delivered {
                            warn!(
                                tab,
                                "interrupt delivery failed; preserving PTY output and phase"
                            );
                        }
                    }
                }
                Effect::ResizePty {
                    tab,
                    pane_id,
                    rows,
                    cols,
                } => {
                    self.apply_pty_resize_effect(tab, pane_id, rows, cols);
                }
                Effect::CopyClipboard { text } => clipboard_copy(&text),
                Effect::PersistTabs => self.save_all_tabs(),
                Effect::PersistBlocks { blocks } => self.persist_blocks(&blocks),
                Effect::Paste { tab } => self.apply_paste(tab),
                Effect::Exit => self.should_exit = true,
                Effect::TabClosed {
                    removed_idx,
                    new_active,
                    is_last,
                } => {
                    // close_tab already applied the synchronous mutations;
                    // retain this effect as the post-close extension point.
                    tracing::info!(removed_idx, new_active, is_last, "tab closed effect");
                }
                Effect::TabSwitched { new_idx, prev_idx } => {
                    // Synchronous mutation (sessions.next/prev, IME reset,
                    // find refresh, tab-bar scroll) already ran in
                    // `next_tab`/`prev_tab`. Extension point for future
                    // post-switch consumers.
                    tracing::info!(new_idx, prev_idx, "tab switched effect");
                }
                Effect::RequestRedraw => self.request_redraw(),
            }
        }
    }

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

    /// Read the system clipboard (synchronous — NSPasteboard has AppKit main
    /// thread affinity) and apply the text to `tab`. Editor mode inserts into
    /// the prompt buffer; Passthrough forwards to the PTY with optional
    /// bracketed-paste wrapping. `Effect::Paste` is the system-clipboard
    /// entry point; find-bar Cmd+V can pass already-read text directly.
    pub(crate) fn apply_paste(&mut self, tab: usize) {
        let Some(text) = clipboard_paste() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.apply_paste_text(tab, &text);
    }

    /// Apply already-read `text` to `tab` according to its input mode. Split
    /// out so callers that already hold the clipboard text (e.g. the find bar
    /// Cmd+V path) can skip the NSPasteboard round-trip.
    pub(crate) fn apply_paste_text(&mut self, tab: usize, text: &str) {
        let mode = self
            .sessions
            .tab(tab)
            .and_then(|t| t.terminal.as_ref())
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);

        if mode == weft_core::input::InputMode::Editor {
            // Preserve pasted newlines explicitly because insert_char rejects
            // controls; drop CR to normalize external CRLF text.
            if let Some(t) = self
                .sessions
                .tab_mut(tab)
                .and_then(|tab| tab.terminal.as_mut())
            {
                let buf = &mut t.editor_mut().buffer;
                for c in text.chars() {
                    if c == '\n' {
                        buf.split_newline();
                    } else if c != '\r' {
                        buf.insert_char(c);
                    }
                }
            }
            self.request_redraw();
        } else {
            // Passthrough: forward to the PTY.
            let bracketed = self
                .sessions
                .tab(tab)
                .and_then(|t| t.terminal.as_ref())
                .map(|t| t.bracketed_paste)
                .unwrap_or(false);
            let bytes = encode_paste(text, bracketed);
            if let Some(session) = self.sessions.tab_mut(tab) {
                if let Err(e) = session.write_user_input(&bytes) {
                    warn!(error = %e, tab, "failed to paste to PTY");
                }
            }
        }
    }

    /// Copy selection to system clipboard.
    ///
    /// Dispatches on the active view: block view copies from the
    /// content-anchored `BlockViewSelection` read through the CURRENT
    /// document source (what the user sees — no stale snapshot), grid view
    /// copies from the terminal Grid. This split fixes the "复制错位" bug
    /// where a grid-coordinate copy landed on the wrong line because the
    /// block view's pitch/scroll/layout don't map 1:1 to grid rows.
    pub(crate) fn copy_selection(&mut self) {
        let text = {
            let tab = self.sessions.active();
            let Some(terminal) = tab.terminal.as_ref() else {
                return;
            };
            // Editor drag-selection takes priority over block/grid selection.
            terminal
                .editor()
                .buffer
                .selected_text()
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    if terminal.show_block_view() {
                        let source = crate::selection::SelectionDocSource::new(
                            terminal.block_tracker().session_blocks(),
                            terminal.block_tracker().in_flight().map(|live| live.output),
                        );
                        tab.selection_handler.block_view_text(&source)
                    } else {
                        tab.selection_handler.selected_text(terminal.grid())
                    }
                })
        };
        self.drain_effects(crate::effect::copy_clipboard_effects(text));
    }
}
