//! Palette form sub-mode handlers extracted from palette_controller.

use super::*;
use weft_core::workflow::WorkflowStoreError;

pub(crate) fn palette_form_field_direction(key: KeyCode, mods: Modifiers) -> Option<bool> {
    use crate::paint::command_surface::CommandSurfaceKeyAction;
    match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
        CommandSurfaceKeyAction::MoveUp | CommandSurfaceKeyAction::PageUp => Some(false),
        CommandSurfaceKeyAction::MoveDown | CommandSurfaceKeyAction::PageDown => Some(true),
        CommandSurfaceKeyAction::CycleFocus => Some(!mods.contains(Modifiers::SHIFT)),
        _ => None,
    }
}

/// v1.13.3: classify a workflow-store save failure for the toast copy.
/// `name` is the table's only UNIQUE column (weft_core workflow.rs SCHEMA),
/// so a constraint violation means "duplicate name" — the one deterministic
/// failure an Enter-retry can never fix. Everything else is transient from
/// the user's point of view and gets the generic retry copy.
pub(crate) fn workflow_save_failure_message(err: &WorkflowStoreError) -> &'static str {
    use rusqlite::ffi::ErrorCode;
    // 判定精度论证（v1.13.3 r2）：workflows 表中 `name` 是唯一 UNIQUE 列；其余
    // 约束仅 NOT NULL —— insert 恒供全列（Create 表单拒空名 + serde 恒非 NULL），
    // NOT NULL 不可达；update SET name 在 Edit 不改名前提下不触发 UNIQUE。故
    // 主码 ConstraintViolation 在实践上等价于"重名"。
    // 耦合注明：若未来升级为 extended_code == SQLITE_CONSTRAINT_UNIQUE 精确判定，
    // 下方单测的构造器须同步换结构体字面量 `Error { code, extended_code }`——
    // libsqlite3-sys 0.28 只有 `new(c_int)` 一个构造器且它把 extended_code 设为
    // result_code 本身（无 new_extended；字段 pub 可字面量构造）——见测试内钉死注释。
    if matches!(
        err,
        WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
            if e.code == ErrorCode::ConstraintViolation
    ) {
        "名称已存在，请换一个"
    } else {
        "保存失败，请重试"
    }
}

impl App {
    // ── Settings panel (v1.0 S1, Cmd+,) ────────────────────────────────

    /// Handle a single-key palette action (create/edit/delete/export).
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
                    .and_then(|tab| tab.terminal.as_ref())
                    .and_then(|t| t.cwd().map(|s| s.to_string()))
                    .unwrap_or_default();
                // Gather recent history (most-recent first, capped).
                let recent_history: Vec<String> = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.terminal.as_ref())
                    .map(|t| t.editor().history().to_vec())
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

#[cfg(test)]
mod tests {
    use super::palette_form_field_direction;
    use super::workflow_save_failure_message;
    use weft_core::input::{KeyCode, Modifiers};
    use weft_core::workflow::WorkflowStoreError;

    #[test]
    fn palette_form_owns_tab_arrows_and_page_navigation() {
        assert_eq!(
            palette_form_field_direction(KeyCode::Tab, Modifiers::empty()),
            Some(true)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::Tab, Modifiers::SHIFT),
            Some(false)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::Up, Modifiers::empty()),
            Some(false)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::Down, Modifiers::empty()),
            Some(true)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::PageUp, Modifiers::empty()),
            Some(false)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::PageDown, Modifiers::empty()),
            Some(true)
        );
        assert_eq!(
            palette_form_field_direction(KeyCode::Char('x'), Modifiers::empty()),
            None
        );
    }

    #[test]
    fn save_failure_message_maps_constraint_violation_to_duplicate_name() {
        // 构造器耦合钉死（v1.13.3）：`ffi::Error::new(c_int)` 是 libsqlite3-sys 0.28
        // 唯一构造器，它把 extended_code 设为 result_code 本身（无 new_extended；
        // 字段 pub）。若 `workflow_save_failure_message` 升级为 extended_code ==
        // SQLITE_CONSTRAINT_UNIQUE 精确判定，本测试必须同步换结构体字面量
        // `Error { code, extended_code }` 构造——此处失败即提醒两处一起改。
        let err = WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            None,
        ));
        assert_eq!(workflow_save_failure_message(&err), "名称已存在，请换一个");
    }

    #[test]
    fn save_failure_message_maps_io_to_generic_copy() {
        let err = WorkflowStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "db not writable",
        ));
        assert_eq!(workflow_save_failure_message(&err), "保存失败，请重试");
    }

    #[test]
    fn save_failure_message_maps_non_constraint_sqlite_to_generic_copy() {
        let err = WorkflowStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            None,
        ));
        assert_eq!(workflow_save_failure_message(&err), "保存失败，请重试");
    }
}
