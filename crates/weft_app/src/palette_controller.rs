//! Command palette controller extracted from the application shell.

use super::*;

pub(crate) fn palette_form_field_direction(key: KeyCode, mods: Modifiers) -> Option<bool> {
    use crate::paint::command_surface::CommandSurfaceKeyAction;
    match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
        CommandSurfaceKeyAction::MoveUp | CommandSurfaceKeyAction::PageUp => Some(false),
        CommandSurfaceKeyAction::MoveDown | CommandSurfaceKeyAction::PageDown => Some(true),
        CommandSurfaceKeyAction::CycleFocus => Some(!mods.contains(Modifiers::SHIFT)),
        _ => None,
    }
}

impl App {
    /// Refresh the palette search results from the workflow store + builtin commands.
    pub(super) fn refresh_palette_results(&mut self) {
        let mut results = Vec::new();

        // Workflows from the store.
        if let Some(store) = &self.palette.store {
            let workflows = if self.palette.query.is_empty() {
                store.list().unwrap_or_default()
            } else {
                store.search(&self.palette.query, 50).unwrap_or_default()
            };
            for wf in workflows {
                results.push(PaletteEntry::Workflow(wf));
            }
        }

        // Builtin commands (filtered by query if non-empty).
        let builtins = [
            BuiltinCmd::ToggleTheme,
            BuiltinCmd::SelectTheme,
            BuiltinCmd::ToggleBlockPanel,
            BuiltinCmd::ReloadConfig,
        ];
        for b in &builtins {
            let label = b.label();
            if self.palette.query.is_empty()
                || label
                    .to_lowercase()
                    .contains(&self.palette.query.to_lowercase())
            {
                results.push(PaletteEntry::Builtin(*b));
            }
        }

        // v1.5.1: Profile entries. The "Switch Profile: <name>" labels are
        // filtered by the query like any other entry. "Base" is always
        // present (switching to base clears `active_profile`). The
        // currently-active profile is marked so the renderer can show a
        // checkmark and `activate_palette_entry` can no-op on re-selection.
        //
        // Profiles come from `config_state.source()` (raw, not effective)
        // so the names match what's in the TOML file — important for the
        // subsequent `switch_profile(name)` call.
        let active = self.active_profile_name().map(str::to_owned);
        let q = self.palette.query.to_lowercase();
        let profile_filter = |label: &str| q.is_empty() || label.to_lowercase().contains(&q);
        // "Base" entry — switching to base means `active_profile = None`.
        // Label uses the "Switch Profile: " prefix so typing "profile" or
        // "switch" surfaces all entries.
        let base_label = "Switch Profile: Base";
        if profile_filter(base_label) {
            results.push(PaletteEntry::Profile {
                name: "Base".to_string(),
                active: active.is_none(),
            });
        }
        for name in self.profile_names_sorted() {
            let label = format!("Switch Profile: {name}");
            if !profile_filter(&label) {
                continue;
            }
            results.push(PaletteEntry::Profile {
                name: name.clone(),
                active: active.as_deref() == Some(name.as_str()),
            });
        }

        self.palette.results = results;
        // Clamp selection.
        if self.palette.selection >= self.palette.results.len() {
            self.palette.selection = 0;
        }
    }

    /// Handle a key while the Command Palette is open. Returns true if consumed.
    pub(super) fn handle_palette_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        // Let modifier chords fall through (so cmd+p can toggle closed).
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }

        // If we're in form mode (filling workflow variables), route differently.
        if self.palette.form.is_some() {
            return self.handle_palette_form_key(key, mods, text);
        }

        // Route to sub-mode handler if not in Search.
        match &self.palette.submode {
            PaletteSubMode::CreateWorkflow { .. } => {
                return self.handle_palette_create_key(key, mods, text);
            }
            PaletteSubMode::EditWorkflow { .. } => {
                return self.handle_palette_edit_key(key, mods, text);
            }
            PaletteSubMode::ConfirmDelete { .. } => {
                return self.handle_palette_delete_key(key);
            }
            PaletteSubMode::SelectTheme { .. } => {
                return self.handle_palette_select_theme_key(key, mods, text);
            }
            PaletteSubMode::Search => {}
        }

        use crate::paint::command_surface::CommandSurfaceKeyAction;
        let protocol = crate::paint::command_surface::resolve_command_surface_key(key, mods);
        match protocol {
            CommandSurfaceKeyAction::Cancel => {
                self.close_palette();
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::MoveUp => {
                if self.palette.selection > 0 {
                    self.palette.selection -= 1;
                }
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::MoveDown => {
                if self.palette.selection + 1 < self.palette.results.len() {
                    self.palette.selection += 1;
                }
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::PageUp => {
                self.palette.selection = crate::paint::command_surface::apply_page_selection(
                    self.palette.selection,
                    self.palette.results.len(),
                    self.interaction.popup_max_rows,
                    false,
                );
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::PageDown => {
                self.palette.selection = crate::paint::command_surface::apply_page_selection(
                    self.palette.selection,
                    self.palette.results.len(),
                    self.interaction.popup_max_rows,
                    true,
                );
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::CycleFocus => {
                self.palette.selection = crate::input_router::cycle_list_selection(
                    self.palette.selection,
                    self.palette.results.len(),
                    !mods.contains(Modifiers::SHIFT),
                );
                self.request_redraw();
                return true;
            }
            CommandSurfaceKeyAction::Accept => {
                if let Some(entry) = self.palette.results.get(self.palette.selection).cloned() {
                    self.activate_palette_entry(entry);
                }
                return true;
            }
            CommandSurfaceKeyAction::Unhandled => {}
        }

        match key {
            KeyCode::Backspace => {
                // If query is empty and we were typing '>', clear it.
                if self.palette.query.is_empty() {
                    self.palette.submode = PaletteSubMode::Search;
                } else {
                    self.palette.query.pop();
                    self.palette.selection = 0;
                    self.refresh_palette_results();
                }
                self.request_redraw();
                true
            }
            _ => {
                let c = resolve_text_char(text, '\0', false);
                if c == '\0' || c.is_control() {
                    return false;
                }

                // Check for action shortcuts when a workflow is selected and
                // query is empty (single-char commands).
                if self.palette.query.is_empty() {
                    let action = match c {
                        '>' => Some("create"),
                        'e' | 'E' => Some("edit"),
                        'd' | 'D' => Some("delete"),
                        'x' | 'X' => Some("export"),
                        _ => None,
                    };
                    if let Some(act) = action {
                        return self.handle_palette_action(act);
                    }
                }

                self.palette.query.push(c);
                self.palette.selection = 0;
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
        }
    }

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
                        *command = std::mem::take(buffer);
                        *step = CreateStep::Done;

                        // Create the workflow in the store.
                        let wf = weft_core::workflow::Workflow {
                            id: 0,
                            name: name.clone(),
                            description: "User-created workflow".into(),
                            steps: vec![weft_core::workflow::WorkflowStep {
                                command: command.clone(),
                            }],
                            variables: vec![],
                            source: weft_core::workflow::WorkflowSource::Manual,
                            use_count: 0,
                            last_used_ms: 0,
                        };
                        if let Some(store) = &self.palette.store {
                            if let Err(e) = store.insert(&wf) {
                                warn!(error = %e, "failed to save new workflow");
                            } else {
                                info!(name = %wf.name, "workflow created");
                            }
                        }

                        // Return to search mode and refresh.
                        self.palette.submode = PaletteSubMode::Search;
                        self.palette.query.clear();
                        self.refresh_palette_results();
                        self.request_redraw();
                        true
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

                // Update the workflow in the store.
                if let Some(store) = &self.palette.store {
                    if let Some(mut wf) = store.find_by_name(&name).unwrap_or(None) {
                        if let Some(step) = wf.steps.first_mut() {
                            step.command = new_command.clone();
                        } else {
                            // No steps — append the new command as the first step.
                            wf.steps.push(weft_core::workflow::WorkflowStep {
                                command: new_command.clone(),
                            });
                        }
                        if let Err(e) = store.update(&wf) {
                            warn!(error = %e, "failed to update workflow");
                        } else {
                            info!(name = %name, "workflow updated");
                        }
                    }
                }
                let _ = id; // id already used via find_by_name
                self.palette.submode = PaletteSubMode::Search;
                self.palette.query.clear();
                self.refresh_palette_results();
                self.request_redraw();
                true
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
                if let Some(store) = &self.palette.store {
                    if let Err(e) = store.delete(id) {
                        warn!(error = %e, "failed to delete workflow");
                    } else {
                        info!(name = %name, "workflow deleted");
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

    /// Activate a palette entry: workflow → enter form mode, builtin → execute.
    pub(super) fn activate_palette_entry(&mut self, entry: PaletteEntry) {
        match entry {
            PaletteEntry::Workflow(wf) => {
                let var_names = wf.all_var_names();
                if var_names.is_empty() {
                    // No variables — execute immediately (skip the form).
                    let form = WorkflowForm {
                        workflow_id: wf.id,
                        workflow_name: wf.name.clone(),
                        workflow_description: wf.description.clone(),
                        var_names: Vec::new(),
                        var_values: Vec::new(),
                        current_field: 0,
                    };
                    self.execute_workflow(form);
                    self.close_palette();
                    self.request_redraw();
                } else {
                    // Has variables — enter form-fill mode.
                    let var_count = var_names.len();
                    self.palette.form = Some(WorkflowForm {
                        workflow_id: wf.id,
                        workflow_name: wf.name.clone(),
                        workflow_description: wf.description.clone(),
                        var_names,
                        var_values: vec![String::new(); var_count],
                        current_field: 0,
                    });
                    self.request_redraw();
                }
            }
            PaletteEntry::Builtin(cmd) => {
                match cmd {
                    BuiltinCmd::ToggleTheme => {
                        self.toggle_theme();
                        self.close_palette();
                    }
                    BuiltinCmd::SelectTheme => {
                        // v0.9 W2+: enter theme picker sub-mode instead of
                        // closing the palette. List built-in themes + custom
                        // theme files from ~/.config/weft/themes/.
                        let themes = self.available_theme_names();
                        self.palette.submode = PaletteSubMode::SelectTheme {
                            buffer: String::new(),
                            themes,
                        };
                        self.palette.query.clear();
                        self.palette.selection = 0;
                        self.refresh_theme_picker_results();
                        // NOTE: do NOT close the palette — user must pick.
                    }
                    BuiltinCmd::ToggleBlockPanel => {
                        self.execute_action(Action::ToggleBlockPanel);
                        self.close_palette();
                    }
                    BuiltinCmd::ReloadConfig => {
                        self.execute_action(Action::ReloadConfig);
                        self.close_palette();
                    }
                }
                self.request_redraw();
            }
            PaletteEntry::Profile { name, active } => {
                // v1.5.1: Switch to the selected profile. No-op if it's
                // already active (the user can still re-select via Enter
                // to confirm, but we skip the transaction to avoid a
                // pointless save). The "Base" sentinel name maps to
                // `active_profile = None`.
                if active {
                    self.close_palette();
                    self.request_redraw();
                    return;
                }
                let target = if name == "Base" {
                    None
                } else {
                    Some(name.as_str())
                };
                match self.switch_profile(target) {
                    Ok(()) => {
                        info!(profile = %name, "palette switched profile");
                        self.close_palette();
                    }
                    Err(e) => {
                        warn!(error = %e, "palette profile switch failed");
                        // Keep the palette open so the user sees the failure
                        // (the next refresh keeps the same results list).
                    }
                }
                self.request_redraw();
            }
        }
    }

    /// Execute a workflow: render variables → submit commands to PTY.
    pub(super) fn execute_workflow(&mut self, form: WorkflowForm) {
        self.reset_ime_context("workflow submitted");
        self.sessions.active_mut().arm_tui_scroll_window();
        let Some(store) = &self.palette.store else {
            return;
        };
        let wf = store.find_by_name(&form.workflow_name).ok().flatten();
        let Some(wf) = wf else {
            return;
        };

        // Build variable values map.
        let mut values = std::collections::HashMap::new();
        for (name, val) in form.var_names.iter().zip(form.var_values.iter()) {
            values.insert(name.clone(), val.clone());
        }

        match wf.render(&values) {
            Ok(commands) => {
                for cmd in &commands {
                    let tab = self.sessions.active_mut();
                    if let Some(terminal) = &mut tab.terminal {
                        // Set the command text and submit via the editor path.
                        terminal.editor_mut().buffer.set_text(cmd);
                        let bytes = terminal.submit_command();
                        if !bytes.is_empty() && tab.write_user_input(&bytes).is_err() {
                            warn!("failed to write workflow command to PTY");
                        }
                    }
                }
                // Update use count.
                let _ = store.bump_use_count(wf.id);
            }
            Err(e) => {
                warn!(error = %e, "workflow render failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::palette_form_field_direction;
    use weft_core::input::{KeyCode, Modifiers};

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
}
