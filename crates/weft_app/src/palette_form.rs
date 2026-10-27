//! Palette form sub-mode handlers extracted from palette_controller.

use self::support::{palette_form_field_direction, workflow_save_failure_message};
use super::*;

impl App {
    // ── Settings panel (v1.0 S1, Cmd+,) ────────────────────────────────

    /// Handle a manage-verb palette action (create/edit/delete/export/ai),
    /// dispatched by the Option+chord layer (v1.13.18 P1) or AI/creation
    /// flows. Store verbs re-check the selected entry's type below.
    pub(super) fn handle_palette_action(&mut self, action: &str) -> bool {
        match action {
            "create" => {
                self.palette.submode = PaletteSubMode::CreateWorkflow {
                    step: CreateStep::Name,
                    buffer: String::new(),
                    name: String::new(),
                    command: String::new(),
                };
                self.request_redraw();
                true
            }
            "edit" => {
                if let Some(PaletteEntry::Workflow(wf)) =
                    self.palette.results.get(self.palette.selection).cloned()
                {
                    self.palette.submode = PaletteSubMode::EditWorkflow {
                        id: wf.id,
                        name: wf.name.clone(),
                        buffer: wf
                            .steps
                            .first()
                            .map(|s| s.command.clone())
                            .unwrap_or_default(),
                    };
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
            "delete" => {
                if let Some(PaletteEntry::Workflow(wf)) =
                    self.palette.results.get(self.palette.selection).cloned()
                {
                    self.palette.submode = PaletteSubMode::ConfirmDelete {
                        id: wf.id,
                        name: wf.name.clone(),
                    };
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
            "export" => {
                if let Some(store) = &self.palette.store {
                    if let Some(PaletteEntry::Workflow(wf)) =
                        self.palette.results.get(self.palette.selection)
                    {
                        match store.export_yaml(wf.id) {
                            Ok(yaml) => {
                                info!(workflow = %wf.name, "workflow YAML exported to log");
                                tracing::debug!(yaml = %yaml, "exported workflow YAML");
                            }
                            Err(e) => warn!(error = %e, "failed to export workflow YAML"),
                        }
                    }
                }
                // Export doesn't change mode — stay in search.
                false
            }
            // v1.8.1: Enter AI command-generation mode. Only available when
            // the local Ollama backend is configured.
            "ai" => {
                if self.ai_state.is_configured() {
                    self.palette.submode = PaletteSubMode::AiCommand {
                        buffer: String::new(),
                        pending_id: None,
                        last_query: String::new(),
                        error: None,
                    };
                    self.palette.results.clear();
                    self.palette.selection = 0;
                    self.request_redraw();
                    true
                } else {
                    tracing::debug!("AI not configured; ignoring 'ai' palette shortcut");
                    false
                }
            }
            _ => false,
        }
    }

    /// Handle keys in CreateWorkflow sub-mode (guided step-by-step entry).
    pub(super) fn handle_palette_create_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        let PaletteSubMode::CreateWorkflow {
            step,
            buffer,
            name,
            command,
        } = &mut self.palette.submode
        else {
            return false;
        };

        match key {
            KeyCode::Escape => {
                self.palette.submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                match step {
                    CreateStep::Name => {
                        if buffer.is_empty() {
                            return true; // ignore empty name
                        }
                        *name = std::mem::take(buffer);
                        *step = CreateStep::Command;
                        self.request_redraw();
                        true
                    }
                    CreateStep::Command => {
                        if buffer.is_empty() {
                            return true;
                        }

                        // Create the workflow in the store.
                        let wf = weft_core::workflow::Workflow {
                            id: 0,
                            name: name.clone(),
                            description: "User-created workflow".into(),
                            steps: vec![weft_core::workflow::WorkflowStep {
                                command: buffer.clone(),
                            }],
                            variables: vec![],
                            source: weft_core::workflow::WorkflowSource::Manual,
                            use_count: 0,
                            last_used_ms: 0,
                        };
                        // v1.13.3 (C1+C2): read the insert result WITHOUT the
                        // store borrow, then match (v1.12.25 3-B-2 P2-03
                        // pattern) — every toast call below happens after the
                        // store borrow has ended.
                        let outcome = self.palette.store.as_ref().map(|store| store.insert(&wf));
                        match outcome {
                            Some(Ok(id)) => {
                                // v1.13.2 (WP-F): the field mutations were deferred here
                                // from before the insert attempt — a failed save must not
                                // consume the buffer or flip the step to Done.
                                *command = std::mem::take(buffer);
                                *step = CreateStep::Done;
                                let mut indexed = wf.clone();
                                indexed.id = id;
                                if let Some(index) = &self.search_index {
                                    if let Err(e) = index.upsert(
                                        &weft_core::search::SearchDocument::from_workflow(&indexed),
                                    ) {
                                        warn!(error = %e, "failed to index new workflow");
                                    }
                                }
                                info!(name = %wf.name, "workflow created");

                                // Return to search mode and refresh.
                                self.palette.submode = PaletteSubMode::Search;
                                self.palette.query.clear();
                                self.refresh_palette_results();
                                self.request_redraw();
                                true
                            }
                            Some(Err(e)) => {
                                warn!(error = %e, "failed to save new workflow");
                                // v1.13.3 (C1): a failed save used to bounce nowhere
                                // with only a warn log. Now: classified toast copy
                                // (duplicate name vs transient), and the CreateWorkflow
                                // form stays open — buffer/step were never mutated
                                // (v1.13.2 WP-F semantics unchanged), so Enter can
                                // retry after an edit and Esc abandons explicitly.
                                self.show_block_history_toast(workflow_save_failure_message(&e));
                                true
                            }
                            None => {
                                // v1.13.3 (C2): a missing store used to fall through
                                // to Search — a silent fake success. Keep the form
                                // open and say so (replaces the v1.13.2 known residue).
                                warn!("no workflow store; keeping the create form open");
                                self.show_block_history_toast("数据库不可用，未保存");
                                true
                            }
                        }
                    }
                    CreateStep::Done => true,
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                self.request_redraw();
                true
            }
            _ if crate::paint::command_surface::resolve_command_surface_key(key, mods)
                != crate::paint::command_surface::CommandSurfaceKeyAction::Unhandled =>
            {
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    buffer.push(c);
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Handle keys in EditWorkflow sub-mode.
    pub(super) fn handle_palette_edit_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        let (id, name) = match &self.palette.submode {
            PaletteSubMode::EditWorkflow { id, name, .. } => (*id, name.clone()),
            _ => return false,
        };
        let _ = id; // the lookup is by name; id kept for future direct addressing

        match key {
            KeyCode::Escape => {
                self.palette.submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                let new_command = match &self.palette.submode {
                    PaletteSubMode::EditWorkflow { buffer, .. } => buffer.clone(),
                    _ => return false,
                };

                // v1.12.25 (3-B-2 P2-03): the old `.unwrap_or(None)` disguised
                // a DB failure as "not found" while the tail reset the submode
                // anyway — a silent fake success (the user believed the edit
                // was saved while the buffer was thrown away). Read the result
                // WITHOUT the store borrow, then match: on Err/Ok(None) the
                // failure surfaces on the block-history toast channel
                // (execute_workflow 3-B P1-05 precedent) and the EditWorkflow
                // submode + buffer are KEPT so the user can retry or abandon
                // explicitly via Esc.
                let lookup = self
                    .palette
                    .store
                    .as_ref()
                    .map(|store| store.find_by_name(&name));
                match lookup {
                    Some(Ok(Some(mut wf))) => {
                        // Update the workflow in the store.
                        if let Some(step) = wf.steps.first_mut() {
                            step.command = new_command.clone();
                        } else {
                            // No steps — append the new command as the first step.
                            wf.steps.push(weft_core::workflow::WorkflowStep {
                                command: new_command.clone(),
                            });
                        }
                        // v1.13.3 (E1): same result-first pattern for the update —
                        // a failed save used to warn and still bounce back to
                        // Search (fake success). Store borrow ends before any
                        // `self.` feedback call below.
                        let outcome = self.palette.store.as_ref().map(|store| store.update(&wf));
                        match outcome {
                            Some(Ok(())) => {
                                if let Some(index) = &self.search_index {
                                    if let Err(e) = index.upsert(
                                        &weft_core::search::SearchDocument::from_workflow(&wf),
                                    ) {
                                        warn!(error = %e, "failed to reindex workflow");
                                    }
                                }
                                info!(name = %name, "workflow updated");

                                self.palette.submode = PaletteSubMode::Search;
                                self.palette.query.clear();
                                self.refresh_palette_results();
                                self.request_redraw();
                                true
                            }
                            Some(Err(e)) => {
                                warn!(error = %e, "failed to update workflow");
                                // Edit never renames, so a UNIQUE hit is unreachable
                                // on this path (see workflow_save_failure_message's
                                // precision argument) — the generic copy is correct.
                                self.show_block_history_toast("保存失败，请重试");
                                // Keep the EditWorkflow submode + buffer (retry / manual Esc).
                                self.request_redraw();
                                true
                            }
                            None => {
                                // v1.13.3 (E2, rust-reviewer P1): DEFENSIVE-ONLY —
                                // unreachable today: the lookup above gates this
                                // update block and returns its own None arm first
                                // whenever store is absent (palette.store is set
                                // once at startup). Kept as a guard, not a path.
                                warn!(
                                    "no workflow store at update time; keeping the edit form open"
                                );
                                self.show_block_history_toast("数据库不可用，未保存");
                                // Keep the EditWorkflow submode + buffer (retry / manual Esc).
                                self.request_redraw();
                                true
                            }
                        }
                    }
                    Some(Ok(None)) => {
                        warn!(name = %name, "workflow to edit not found");
                        self.show_block_history_toast("workflow 不存在");
                        // Keep the EditWorkflow submode + buffer (retry / manual Esc).
                        self.request_redraw();
                        true
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, name = %name, "failed to read workflow");
                        self.show_block_history_toast("读取 workflow 失败");
                        // Keep the EditWorkflow submode + buffer (retry / manual Esc).
                        self.request_redraw();
                        true
                    }
                    None => {
                        // v1.13.3 (E2, rust-reviewer P1): the REACHABLE store=None
                        // path for Edit — the lookup above already returned None,
                        // so store.update was never reached. Used to silently
                        // bounce back to Search ("unchanged behavior") — a fake
                        // success. Keep the form open and say so instead.
                        warn!("no workflow store; keeping the edit form open");
                        self.show_block_history_toast("数据库不可用，未保存");
                        // Keep the EditWorkflow submode + buffer (retry / manual Esc).
                        self.request_redraw();
                        true
                    }
                }
            }
            KeyCode::Backspace => {
                if let PaletteSubMode::EditWorkflow { buffer, .. } = &mut self.palette.submode {
                    buffer.pop();
                }
                self.request_redraw();
                true
            }
            _ if crate::paint::command_surface::resolve_command_surface_key(key, mods)
                != crate::paint::command_surface::CommandSurfaceKeyAction::Unhandled =>
            {
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    if let PaletteSubMode::EditWorkflow { buffer, .. } = &mut self.palette.submode {
                        buffer.push(c);
                    }
                    self.request_redraw();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Handle keys in ConfirmDelete sub-mode.
    pub(super) fn handle_palette_delete_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Escape => {
                self.palette.submode = PaletteSubMode::Search;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                let (id, name) = match &self.palette.submode {
                    PaletteSubMode::ConfirmDelete { id, name } => (*id, name.clone()),
                    _ => return false,
                };
                // v1.13.3 (D1+D2): result-first pattern — a failed delete used
                // to be indistinguishable from success on the UI (both bounced
                // back to Search silently). Store borrow ends before any
                // `self.` feedback call below. Every arm returns to Search:
                // refresh_palette_results self-heals result visibility, and a
                // delete has no input to preserve, so the confirm state is NOT
                // kept (unlike Create/Edit which keep their forms).
                let outcome = self.palette.store.as_ref().map(|store| store.delete(id));
                match outcome {
                    Some(Ok(())) => {
                        if let Some(index) = &self.search_index {
                            if let Err(e) = index.delete(
                                weft_core::search::SearchDocumentKind::Workflow,
                                &id.to_string(),
                            ) {
                                warn!(error = %e, "failed to remove workflow from search index");
                            }
                        }
                        info!(name = %name, "workflow deleted");
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "failed to delete workflow");
                        self.show_block_history_toast("删除失败，请重试");
                    }
                    None => {
                        warn!("no workflow store; returning to search");
                        self.show_block_history_toast("数据库不可用，未保存");
                    }
                }
                self.palette.submode = PaletteSubMode::Search;
                self.palette.query.clear();
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
            _ => true, // consume all other keys in confirm mode
        }
    }

    /// Handle keys while in the workflow variable form sub-mode.
    pub(super) fn handle_palette_form_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        if let Some(forward) = palette_form_field_direction(key, mods) {
            if let Some(form) = &mut self.palette.form {
                form.current_field = crate::input_router::cycle_list_selection(
                    form.current_field,
                    form.var_names.len(),
                    forward,
                );
            }
            self.request_redraw();
            return true;
        }
        match key {
            KeyCode::Escape => {
                // Return to search mode (keep palette open).
                self.palette.form = None;
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // Execute the workflow with the filled variables.
                let form = self.palette.form.take();
                if let Some(form) = form {
                    self.execute_workflow(form);
                }
                self.close_palette();
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                if let Some(form) = &mut self.palette.form {
                    if form.current_field < form.var_values.len() {
                        form.var_values[form.current_field].pop();
                    }
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c != '\0' && !c.is_control() {
                    if let Some(form) = &mut self.palette.form {
                        if form.current_field < form.var_values.len() {
                            form.var_values[form.current_field].push(c);
                        }
                    }
                    self.request_redraw();
                    return true;
                }
                false
            }
        }
    }

    // ── v1.8.1: AI command-generation sub-mode ────────────────────────

    /// Handle keys while in `PaletteSubMode::AiCommand`.
    ///
    /// - **Enter**: If a query is typed and no request is pending, spawn
    ///   `ai_state.spawn_command_gen`. If results exist, activate the
    ///   selected suggestion (insert into editor).
    /// - **Escape**: Cancel any pending request and return to Search mode.
    /// - **Backspace**: Delete from the buffer.
    /// - **Char**: Append to the buffer.
    /// - **Up/Down**: Navigate results (when present).
    pub(super) fn handle_palette_ai_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        use crate::paint::command_surface::CommandSurfaceKeyAction;

        // Cancel on Escape.
        if key == KeyCode::Escape {
            // v1.8.9: Two-stage Esc — if AI results are showing, the first
            // Esc clears them and restores the input buffer so the user can
            // edit/retry; a second Esc closes the palette. If there are no
            // results (or the request is still pending), Esc closes
            // immediately. This mirrors the UX of Warp/other AI palettes
            // and avoids an accidental Esc discarding a useful result.
            let has_results = !self.palette.results.is_empty();
            let is_pending = matches!(
                &self.palette.submode,
                PaletteSubMode::AiCommand { pending_id, .. } if pending_id.is_some()
            );
            if has_results && !is_pending {
                // First Esc: clear results + error, restore the last query.
                if let PaletteSubMode::AiCommand {
                    buffer,
                    last_query,
                    error,
                    ..
                } = &mut self.palette.submode
                {
                    if buffer.is_empty() && !last_query.is_empty() {
                        *buffer = std::mem::take(last_query);
                    }
                    *error = None;
                }
                self.palette.results.clear();
                self.palette.selection = 0;
                self.ai_state.cancel_all();
                self.request_redraw();
                return true;
            }
            // No results (or pending): close the palette.
            self.ai_state.cancel_all();
            self.close_palette();
            self.request_redraw();
            return true;
        }

        // Navigate results with Up/Down when results exist.
        let has_results = !self.palette.results.is_empty();
        if has_results {
            let protocol = crate::paint::command_surface::resolve_command_surface_key(key, mods);
            match protocol {
                CommandSurfaceKeyAction::MoveUp | CommandSurfaceKeyAction::PageUp => {
                    if self.palette.selection > 0 {
                        self.palette.selection -= 1;
                    }
                    self.request_redraw();
                    return true;
                }
                CommandSurfaceKeyAction::MoveDown | CommandSurfaceKeyAction::PageDown => {
                    if self.palette.selection + 1 < self.palette.results.len() {
                        self.palette.selection += 1;
                    }
                    self.request_redraw();
                    return true;
                }
                CommandSurfaceKeyAction::Accept => {
                    // Activate the selected AI suggestion.
                    if let Some(entry) = self.palette.results.get(self.palette.selection).cloned() {
                        self.activate_palette_entry(entry);
                    }
                    return true;
                }
                _ => {}
            }
        }

        match key {
            KeyCode::Enter => {
                // Submit the query to the AI backend.
                let PaletteSubMode::AiCommand {
                    buffer,
                    pending_id,
                    last_query,
                    error,
                } = &mut self.palette.submode
                else {
                    return false;
                };
                if pending_id.is_some() {
                    // Already waiting — ignore.
                    return true;
                }
                if buffer.trim().is_empty() {
                    return true;
                }
                // v1.8.7: Save the query before taking the buffer so we can
                // restore it if generation fails. Clear any prior error.
                let query = std::mem::take(buffer);
                *last_query = query.clone();
                *error = None;
                let cwd = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.with_terminal(|t| t.cwd().map(str::to_owned)))
                    .flatten()
                    .unwrap_or_default();
                // Gather recent history (most-recent first, capped).
                let recent_history: Vec<String> = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.with_terminal(|t| t.editor().history().to_vec()))
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .take(crate::ai::MAX_HISTORY_ENTRIES)
                    .collect();
                let prompt = crate::ai::CommandGenPrompt {
                    user_query: query,
                    cwd,
                    recent_history,
                };
                if let Some(id) = self.ai_state.spawn_command_gen(prompt) {
                    *pending_id = Some(id);
                    // Clear old results while waiting.
                    self.palette.results.clear();
                    self.palette.selection = 0;
                    self.request_redraw();
                }
                true
            }
            KeyCode::Backspace => {
                if let PaletteSubMode::AiCommand {
                    buffer, pending_id, ..
                } = &mut self.palette.submode
                {
                    if pending_id.is_none() {
                        buffer.pop();
                        self.request_redraw();
                    }
                }
                true
            }
            KeyCode::Char(c) => {
                let PaletteSubMode::AiCommand {
                    buffer, pending_id, ..
                } = &mut self.palette.submode
                else {
                    return false;
                };
                if pending_id.is_some() {
                    return true;
                }
                let ch = resolve_text_char(text, c, mods.contains(Modifiers::SHIFT));
                if ch != '\0' && !ch.is_control() {
                    buffer.push(ch);
                    self.request_redraw();
                }
                true
            }
            _ => true, // consume all other keys in AI mode
        }
    }
}

mod support;

#[cfg(test)]
#[path = "palette_form/tests.rs"]
mod tests;
