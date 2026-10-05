//! Palette entry activation and workflow execution extracted from palette_controller.

use super::*;

impl App {
    /// Activate a palette entry: workflow → enter form mode, builtin → execute.
    pub(super) fn activate_palette_entry(&mut self, entry: PaletteEntry) {
        // v1.12.23 audit batch 1: the palette can stay open after the last tab
        // exits (no path closes it on tab teardown), and every arm below
        // touches `sessions.active_mut()` — a bare index that panics with no
        // tabs. An activation without a session has nothing to act on.
        if self.sessions.is_empty() {
            tracing::debug!("palette activation ignored: no sessions");
            return;
        }
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
                    BuiltinCmd::ImportConfig => {
                        // v1.5.2: Show NSOpenPanel → import_config_document
                        // → apply. Errors are surfaced via `settings.error`
                        // (visible when Settings is open) and the status hint
                        // (visible when Settings is closed).
                        match self.import_config_interactive() {
                            Ok(()) => info!("palette import succeeded"),
                            Err(crate::macos_file_dialog::FilePanelError::NotMainThread) => {
                                warn!("import panel must run on the main thread");
                            }
                            Err(e) => {
                                warn!(error = %e, "palette import failed");
                            }
                        }
                        self.close_palette();
                    }
                    BuiltinCmd::ExportConfig => {
                        match self.export_config_interactive() {
                            Ok(()) => info!("palette export succeeded"),
                            Err(crate::macos_file_dialog::FilePanelError::NotMainThread) => {
                                warn!("export panel must run on the main thread");
                            }
                            Err(e) => {
                                warn!(error = %e, "palette export failed");
                            }
                        }
                        self.close_palette();
                    }
                    BuiltinCmd::SaveWorkspace => {
                        // v1.6.2: Show NSSavePanel → capture workspace → save.
                        // Errors are logged; cancel is silent.
                        let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
                            warn!("workspace save panel must run on the main thread");
                            self.close_palette();
                            self.request_redraw();
                            return;
                        };
                        match self.workspace_save_interactive(mtm) {
                            Ok(()) => info!("palette workspace save succeeded"),
                            Err(
                                crate::workspace_controller::WorkspaceInteractionError::Cancelled,
                            ) => {}
                            Err(e) => warn!(error = %e, "palette workspace save failed"),
                        }
                        self.close_palette();
                    }
                    BuiltinCmd::OpenWorkspace => {
                        // v1.6.2: Show NSOpenPanel → load workspace → restore.
                        let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
                            warn!("workspace open panel must run on the main thread");
                            self.close_palette();
                            self.request_redraw();
                            return;
                        };
                        match self.workspace_open_interactive(mtm) {
                            Ok(()) => info!("palette workspace open succeeded"),
                            Err(
                                crate::workspace_controller::WorkspaceInteractionError::Cancelled,
                            ) => {}
                            Err(e) => warn!(error = %e, "palette workspace open failed"),
                        }
                        self.close_palette();
                    }
                    BuiltinCmd::ImportRunbook => match self.import_runbook_interactive() {
                        Ok(()) => info!("runbook load scheduled"),
                        Err(crate::runbook_controller::RunbookInteractionError::Cancelled) => {
                            self.close_palette();
                        }
                        Err(e) => {
                            warn!(error = %e, "runbook import failed");
                            self.close_palette();
                        }
                    },
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
            PaletteEntry::SearchHit(hit) => {
                use weft_core::search::SearchDocumentKind;
                match hit.doc.kind {
                    SearchDocumentKind::Block | SearchDocumentKind::Bookmark => {
                        if let Ok(id) = hit.doc.stable_id.parse::<u64>() {
                            if self.navigate_to_block(weft_core::blocks::BlockId(id)) {
                                self.close_palette();
                            } else {
                                warn!(block_id = id, "search hit is not loaded in any pane");
                            }
                        }
                    }
                    SearchDocumentKind::Workflow => {
                        // v1.12.25 (3-B-2 P2-03): the old `.ok().flatten()`
                        // disguised a DB failure as "not found" — the command
                        // was silently not inserted (no log, no toast). Read
                        // the result WITHOUT the store borrow, then match so
                        // errors and absence are distinguishable and visible
                        // (same shape as execute_workflow's 3-B P1-05 fix).
                        let lookup = self
                            .palette
                            .store
                            .as_ref()
                            .map(|store| store.find_by_name(&hit.doc.title));
                        let command = match lookup {
                            Some(Ok(Some(wf))) => wf.steps.first().map(|step| step.command.clone()),
                            Some(Ok(None)) => {
                                warn!(name = %hit.doc.title, "workflow not found");
                                self.show_block_history_toast("workflow 不存在");
                                None
                            }
                            Some(Err(e)) => {
                                warn!(error = %e, name = %hit.doc.title, "failed to read workflow");
                                self.show_block_history_toast("读取 workflow 失败");
                                None
                            }
                            None => None, // no block store — unchanged silent no-op
                        };
                        if let Some(command) = command {
                            if let Some(terminal) = self
                                .sessions
                                .active_mut()
                                .and_then(|tab| tab.terminal.as_mut())
                            {
                                terminal.editor_mut().buffer.set_text(&command);
                                terminal.editor_mut().buffer.select_all();
                            }
                            self.close_palette();
                        }
                    }
                    SearchDocumentKind::Workspace => {
                        let path = std::path::PathBuf::from(&hit.doc.stable_id);
                        match weft_core::workspace::WorkspaceDocument::load(&path) {
                            Ok(workspace) => {
                                match self.restore_workspace_with_confirmation(&workspace) {
                                    Ok(Some(_)) => self.close_palette(),
                                    Ok(None) => {}
                                    Err(e) => {
                                        warn!(error = %e, path = %path.display(), "failed to restore workspace search hit")
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, path = %path.display(), "failed to load workspace search hit")
                            }
                        }
                    }
                }
                self.request_redraw();
            }
            PaletteEntry::Runbook(entry) => {
                if let Some(terminal) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.terminal.as_mut())
                {
                    terminal.editor_mut().buffer.set_text(&entry.command);
                    terminal.editor_mut().buffer.select_all();
                }
                self.close_palette();
                self.request_redraw();
            }
            // v1.8.1: Insert the AI-generated command into the editor.
            // No auto-execution — the user reviews and presses Enter.
            PaletteEntry::AiSuggestion { command, risk: _ } => {
                if let Some(terminal) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.terminal.as_mut())
                {
                    terminal.editor_mut().buffer.set_text(&command);
                    terminal.editor_mut().buffer.select_all();
                }
                self.close_palette();
                self.request_redraw();
            }
        }
    }

    /// Execute a workflow: render variables → submit commands to PTY.
    pub(super) fn execute_workflow(&mut self, form: WorkflowForm) {
        self.reset_ime_context("workflow submitted");
        // v1.12.23 audit batch 1: guard sits AFTER reset_ime_context — it is
        // empty-tabs safe (iterates tabs_mut + window-level discard) and must
        // still drop window-level native marked text — but BEFORE the bare
        // `active_mut()` below. No session means no editor and no PTY: nothing
        // to do.
        if self.sessions.is_empty() {
            tracing::debug!("workflow submit ignored: no sessions");
            return;
        }
        // v1.12.25 (audit 3-B, P1-01): type-forced no-op arm — the guard
        // above makes `None` unreachable here.
        if let Some(tab) = self.sessions.active_mut() {
            tab.arm_tui_scroll_window();
        }
        // v1.12.25 (audit 3-B, P1-05): the old `.ok().flatten()` disguised a
        // DB failure as "not found" — the error was silently swallowed (no
        // toast, no log). Read the result WITHOUT the store borrow, then
        // match so errors and absence are distinguishable and visible.
        let lookup = self
            .palette
            .store
            .as_ref()
            .map(|store| store.find_by_name(&form.workflow_name));
        let Some(wf) = (match lookup {
            Some(Ok(Some(wf))) => Some(wf),
            Some(Ok(None)) => {
                warn!(name = %form.workflow_name, "workflow not found");
                self.show_block_history_toast("workflow 不存在");
                None
            }
            Some(Err(e)) => {
                warn!(error = %e, name = %form.workflow_name, "failed to read workflow");
                self.show_block_history_toast("读取 workflow 失败");
                None
            }
            None => None, // no block store — unchanged silent no-op
        }) else {
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
                    if let Some(tab) = self.sessions.active_mut() {
                        if let Some(terminal) = &mut tab.terminal {
                            // Set the command text and submit via the editor path.
                            terminal.editor_mut().buffer.set_text(cmd);
                            let bytes = terminal.submit_command();
                            if !bytes.is_empty() && tab.write_user_input(&bytes).is_err() {
                                warn!("failed to write workflow command to PTY");
                            }
                        }
                    }
                }
                // Update use count.
                if let Some(store) = &self.palette.store {
                    let _ = store.bump_use_count(wf.id);
                }
            }
            Err(e) => {
                warn!(error = %e, "workflow render failed");
            }
        }
    }
}
