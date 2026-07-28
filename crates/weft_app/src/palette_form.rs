//! Palette form sub-mode handlers extracted from palette_controller.

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
