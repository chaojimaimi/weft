use crate::{App, BlockId};
use tracing::{info, warn};

impl App {
    pub(super) fn run_annotation_action(&mut self, block_id: Option<BlockId>, action: &str) {
        self.run_annotation_action_with_export(block_id, action, None);
    }

    pub(super) fn run_annotation_action_with_export(
        &mut self,
        block_id: Option<BlockId>,
        action: &str,
        export_block_data: Option<weft_core::blocks::Block>,
    ) {
        match action {
            "toggle_bookmark" => {
                let Some(block_id) = block_id else { return };
                let Some(store) = self.sessions.annotation_store() else {
                    warn!("annotation store unavailable; bookmark not toggled");
                    return;
                };
                match store.toggle_bookmark(block_id) {
                    Ok(bookmarked) => {
                        if bookmarked {
                            self.bookmarked_blocks.insert(block_id);
                        } else {
                            self.bookmarked_blocks.remove(&block_id);
                        }
                        info!(?block_id, bookmarked, "bookmark toggled");
                        self.sync_bookmark_to_search_index(block_id);
                        self.request_redraw();
                    }
                    Err(error) => warn!(%error, "failed to toggle bookmark"),
                }
            }
            "add_note" => {
                let Some(block_id) = block_id else { return };
                let existing = self
                    .sessions
                    .annotation_store()
                    .and_then(|store| store.get(block_id).ok().flatten())
                    .and_then(|annotation| annotation.note);
                self.note_editor.open_for(block_id, existing.as_deref());
                self.request_redraw();
            }
            "export_block" => {
                let Some(block) = export_block_data else {
                    return;
                };
                let annotation = self
                    .sessions
                    .annotation_store()
                    .and_then(|store| store.get(block.id).ok().flatten());
                let markdown = weft_core::blocks::export::export_block_as_markdown(
                    &block,
                    annotation.as_ref(),
                );
                self.write_block_export(&block.command, &markdown);
            }
            _ => {}
        }
    }

    pub(crate) fn sync_bookmark_to_search_index(&self, block_id: BlockId) {
        let Some(index) = &self.search_index else {
            return;
        };
        let annotation = self
            .sessions
            .annotation_store()
            .and_then(|store| store.get(block_id).ok().flatten());
        let Some(annotation) = annotation else {
            if let Err(error) = index.delete(
                weft_core::search::SearchDocumentKind::Bookmark,
                &block_id.0.to_string(),
            ) {
                warn!(%error, "failed to delete bookmark from search index");
            }
            return;
        };
        let block = self
            .sessions
            .block_store()
            .and_then(|store| store.get(block_id).ok().flatten());
        let (command, cwd) = block
            .map(|block| (block.command.clone(), block.cwd.clone()))
            .unwrap_or_default();
        let updated_ms = annotation
            .updated_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        let document = weft_core::search::SearchDocument::from_bookmark(
            block_id.0,
            annotation.note.as_deref(),
            &command,
            &annotation.tags,
            cwd.as_deref(),
            updated_ms,
        );
        if let Err(error) = index.upsert(&document) {
            warn!(%error, "failed to upsert bookmark into search index");
        }
    }

    fn write_block_export(&self, command: &str, markdown: &str) {
        let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
            warn!("not on main thread; export cancelled");
            return;
        };
        if !crate::macos_alert::show_block_export_preview(mtm, markdown) {
            tracing::debug!("export cancelled from preview");
            return;
        }
        match crate::macos_file_dialog::pick_block_export_path(mtm, command) {
            Ok(Some(path)) => {
                if let Err(error) = std::fs::write(&path, markdown.as_bytes()) {
                    warn!(%error, ?path, "failed to write export file");
                } else {
                    info!(?path, bytes = markdown.len(), "block exported");
                }
            }
            Ok(None) => tracing::debug!("export cancelled by user"),
            Err(error) => warn!(%error, "save panel error"),
        }
    }
}
