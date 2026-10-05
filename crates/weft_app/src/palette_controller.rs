//! Command palette controller — search refresh and key routing.

use super::*;

impl App {
    /// Refresh the palette search results from the workflow store + builtin commands.
    pub(super) fn refresh_palette_results(&mut self) {
        if !self.palette.runbook_entries.is_empty() {
            let query = self.palette.query.to_lowercase();
            self.palette.results = self
                .palette
                .runbook_entries
                .iter()
                .filter(|entry| {
                    query.is_empty()
                        || entry.command.to_lowercase().contains(&query)
                        || entry.description.to_lowercase().contains(&query)
                })
                .cloned()
                .map(PaletteEntry::Runbook)
                .collect();
            self.palette.selection = self
                .palette
                .selection
                .min(self.palette.results.len().saturating_sub(1));
            return;
        }
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
            BuiltinCmd::ImportConfig,
            BuiltinCmd::ExportConfig,
            BuiltinCmd::SaveWorkspace,
            BuiltinCmd::OpenWorkspace,
            BuiltinCmd::ImportRunbook,
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

        // v1.7.1: Submit FTS5 search query to the background worker.
        // Results arrive asynchronously and are merged in
        // `poll_palette_search_results` (called from the redraw path).
        if !self.palette.query.is_empty() {
            if let Some(worker) = &self.palette.search_worker {
                let cwd = self
                    .sessions
                    .active()
                    .and_then(|tab| tab.terminal.as_ref())
                    .and_then(|t| t.cwd().map(|s| s.to_string()));
                let gen = worker.submit(&self.palette.query, &[], cwd.as_deref(), 50);
                self.palette.search_generation = gen;
                self.palette.search_pending = true;
            }
        } else if let Some(worker) = &self.palette.search_worker {
            // Query cleared — cancel any in-flight search so stale results
            // don't merge into the now-empty results list.
            worker.invalidate();
            self.palette.search_generation = worker.current_generation();
            self.palette.search_pending = false;
        }
    }

    /// v1.7.1: Drain palette search worker results. Called from the redraw path.
    /// Merges background FTS5 search hits into the palette results list,
    /// prepending them so history results appear first.
    pub(super) fn poll_palette_search_results(&mut self) {
        if !self.palette.open {
            return;
        }
        let Some(worker) = &self.palette.search_worker else {
            return;
        };
        while let Some(result) = worker.try_recv_result() {
            // Staleness filter: only process results matching the current generation.
            if result.generation != self.palette.search_generation {
                continue;
            }
            self.palette.search_pending = false;
            if let Some(error) = result.error {
                warn!(%error, "palette search failed");
                continue;
            }
            // Remove old SearchHit entries first.
            self.palette
                .results
                .retain(|e| !matches!(e, PaletteEntry::SearchHit(_)));
            // Prepend new search hits (history results first).
            let mut new_entries: Vec<PaletteEntry> = result
                .hits
                .into_iter()
                .map(PaletteEntry::SearchHit)
                .collect();
            new_entries.append(&mut self.palette.results);
            self.palette.results = new_entries;
            // Clamp selection.
            if self.palette.selection >= self.palette.results.len() {
                self.palette.selection = 0;
            }
            self.request_redraw();
            break; // Only process one result per frame.
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
            // v1.8.1: AI command generation mode.
            PaletteSubMode::AiCommand { .. } => {
                return self.handle_palette_ai_key(key, mods, text);
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
                        // v1.8.1: 'a' enters AI command-generation mode.
                        'a' | 'A' => Some("ai"),
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
}
