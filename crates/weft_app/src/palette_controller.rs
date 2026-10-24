//! Command palette controller — search refresh and key routing.

use super::*;

/// v1.13.18 P1: workflow-management verbs relocated from the old bare-key
/// empty-query shortcuts (e/d/x/a/>) to Option+letter chords, so a bare
/// letter always types into the search query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaletteManageAction {
    Create,
    Ai,
    Edit,
    Delete,
    Export,
}

impl PaletteManageAction {
    /// The verb string [`App::handle_palette_action`] dispatches on.
    fn verb(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Ai => "ai",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Export => "export",
        }
    }
}

/// Resolve an Option+letter chord into a manage verb. Exact pure-ALT only:
/// Shift+Option types third-layer variant characters (评审二轮 P2 — the
/// user's intent can never be a manage chord), and SUPER/CONTROL belong to
/// global keybindings (Cmd+D = SplitVertical must keep working). Keys are
/// the physical-letter mapping of `event_replay::map_winit_key`
/// (`KeyCode::Char('d')` etc.), so the predicate is independent of the
/// macOS variant text ("∂") winit reports for Option+letter.
pub(crate) fn palette_alt_chord_action(
    key: KeyCode,
    mods: Modifiers,
) -> Option<PaletteManageAction> {
    if mods != Modifiers::ALT {
        return None;
    }
    match key {
        KeyCode::Char('n') => Some(PaletteManageAction::Create),
        KeyCode::Char('a') => Some(PaletteManageAction::Ai),
        KeyCode::Char('e') => Some(PaletteManageAction::Edit),
        KeyCode::Char('d') => Some(PaletteManageAction::Delete),
        KeyCode::Char('x') => Some(PaletteManageAction::Export),
        _ => None,
    }
}

/// Type gate for the manage verbs: Edit/Delete/Export operate on workflow
/// store rows and require the selected entry to BE a Workflow (aligned with
/// the internal gates in `handle_palette_action`); Create/Ai are
/// selection-independent. `None` (nothing selected) only allows Create/Ai.
pub(crate) fn manage_action_allowed(
    action: PaletteManageAction,
    selected: Option<&PaletteEntry>,
) -> bool {
    match action {
        PaletteManageAction::Create | PaletteManageAction::Ai => true,
        PaletteManageAction::Edit | PaletteManageAction::Delete | PaletteManageAction::Export => {
            matches!(selected, Some(PaletteEntry::Workflow(_)))
        }
    }
}

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
                    .and_then(|tab| tab.with_terminal(|t| t.cwd().map(str::to_owned)))
                    .flatten();
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

    /// v1.13.18 P1: the Option+chord layer of the palette modal. Branch
    /// order is correctness-critical (评审二轮 P1): on macOS Option+letter
    /// reports a variant character in `text` ("∂"), so a swallow guard
    /// consulted FIRST would eat every manage verb — the chord judgment
    /// must run before it.
    ///
    /// 1. Manage-verb chords (Search submode only, no variable form):
    ///    gate via [`manage_action_allowed`] then dispatch. Consumed
    ///    unconditionally on a gated hit — export and unconfigured-AI
    ///    return false from `handle_palette_action`, but the chord is still
    ///    consumed so its variant character never lands in the query.
    /// 2. Swallow guard: any remaining Option+printable variant character
    ///    (Option+digits/punctuation, Shift+Option layers) is consumed —
    ///    also in form/submodes (评审 P2b: a mis-press must not dump "∂"
    ///    into a form buffer or flip modes).
    /// 3. Everything else with Option (arrows: text=None; control text
    ///    like Option+Enter) falls through exactly like the old wholesale
    ///    ALT early-return did.
    ///
    /// `None` = not an Option chord, continue normal routing.
    fn handle_palette_alt_chord(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> Option<bool> {
        if !mods.contains(Modifiers::ALT) {
            return None;
        }
        if self.palette.form.is_none() && matches!(self.palette.submode, PaletteSubMode::Search) {
            if let Some(action) = palette_alt_chord_action(key, mods) {
                let selected = self.palette.results.get(self.palette.selection);
                if manage_action_allowed(action, selected) {
                    let _ = self.handle_palette_action(action.verb());
                    return Some(true);
                }
            }
        }
        let c = resolve_text_char(text, '\0', false);
        if c != '\0' && !c.is_control() {
            return Some(true);
        }
        Some(false)
    }

    /// Handle a key while the Command Palette is open. Returns true if consumed.
    pub(super) fn handle_palette_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        // Let cmd/ctrl chords fall through (so cmd+p can toggle closed and
        // Cmd+D still splits the pane). v1.13.18 P1: ALT is NO longer a
        // wholesale fall-through — it is handled explicitly below.
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL) {
            return false;
        }
        if let Some(consumed) = self.handle_palette_alt_chord(key, mods, text) {
            return consumed;
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

                // v1.13.18 P1: the old empty-query single-char verb block
                // (e/d/x/a/>) is gone — bare characters ALWAYS search, and
                // the manage verbs moved to the Option+chord layer above.

                self.palette.query.push(c);
                self.palette.selection = 0;
                self.refresh_palette_results();
                self.request_redraw();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow_entry() -> PaletteEntry {
        PaletteEntry::Workflow(weft_core::workflow::Workflow {
            id: 1,
            name: "test-workflow".into(),
            description: String::new(),
            steps: vec![weft_core::workflow::WorkflowStep {
                command: "echo hi".into(),
            }],
            variables: vec![],
            source: weft_core::workflow::WorkflowSource::Manual,
            use_count: 0,
            last_used_ms: 0,
        })
    }

    // ── v1.13.18 P1: palette_alt_chord_action chord table ────────────────

    #[test]
    fn altchord_maps_exact_option_letters_to_manage_verbs() {
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::ALT),
            Some(PaletteManageAction::Delete)
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('e'), Modifiers::ALT),
            Some(PaletteManageAction::Edit)
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('x'), Modifiers::ALT),
            Some(PaletteManageAction::Export)
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('n'), Modifiers::ALT),
            Some(PaletteManageAction::Create)
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('a'), Modifiers::ALT),
            Some(PaletteManageAction::Ai)
        );
    }

    #[test]
    fn altchord_rejects_shift_option_and_cmd_layers() {
        // Shift+Option is a third-layer variant-character chord (评审二轮 P2).
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::ALT | Modifiers::SHIFT),
            None
        );
        // Cmd+D must stay free for the global SplitVertical binding.
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::SUPER),
            None
        );
        // cmd+alt layers (cmd+alt+arrows/brackets territory) never manage.
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::SUPER | Modifiers::ALT),
            None
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::CONTROL | Modifiers::ALT),
            None
        );
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('d'), Modifiers::empty()),
            None
        );
    }

    #[test]
    fn altchord_ignores_unbound_letters_and_arrows() {
        // Not a manage verb → falls through to the swallow guard.
        assert_eq!(
            palette_alt_chord_action(KeyCode::Char('z'), Modifiers::ALT),
            None
        );
        // Direction keys carry no text and never manage.
        assert_eq!(palette_alt_chord_action(KeyCode::Up, Modifiers::ALT), None);
        assert_eq!(
            palette_alt_chord_action(KeyCode::Down, Modifiers::ALT),
            None
        );
    }

    // ── v1.13.18 P1: manage_action_allowed type gate ─────────────────────

    #[test]
    fn manage_gate_requires_workflow_selection_for_store_verbs() {
        assert!(manage_action_allowed(
            PaletteManageAction::Delete,
            Some(&workflow_entry())
        ));
        assert!(manage_action_allowed(
            PaletteManageAction::Edit,
            Some(&workflow_entry())
        ));
        assert!(manage_action_allowed(
            PaletteManageAction::Export,
            Some(&workflow_entry())
        ));
        // Builtin (e.g. Toggle Theme) must NOT be deletable via the chord.
        assert!(!manage_action_allowed(
            PaletteManageAction::Delete,
            Some(&PaletteEntry::Builtin(BuiltinCmd::ToggleTheme))
        ));
        // Nothing selected → no store verb.
        assert!(!manage_action_allowed(PaletteManageAction::Delete, None));
    }

    #[test]
    fn manage_gate_always_allows_create_and_ai() {
        assert!(manage_action_allowed(PaletteManageAction::Create, None));
        assert!(manage_action_allowed(
            PaletteManageAction::Create,
            Some(&PaletteEntry::Builtin(BuiltinCmd::ToggleTheme))
        ));
        assert!(manage_action_allowed(
            PaletteManageAction::Ai,
            Some(&workflow_entry())
        ));
    }

    // ── v1.13.18 P1: order pin (评审二轮 P1) ─────────────────────────────
    // The wiring in `handle_palette_alt_chord` MUST judge the chord before
    // the swallow guard: on macOS Option+letter's `text` is a variant
    // character ("∂"), which the swallow guard would consume. This test
    // pins the premise: the SAME Option+D press both resolves a chord verb
    // AND resolves to a printable variant text — so a swallow-first order
    // would have eaten all five verbs.

    #[test]
    fn option_chord_judgment_precedes_the_swallow_guard() {
        let chord = palette_alt_chord_action(KeyCode::Char('d'), Modifiers::ALT);
        assert_eq!(chord, Some(PaletteManageAction::Delete));
        // macOS winit reports Option+D text as the variant "∂".
        let variant = resolve_text_char(Some("∂"), '\0', false);
        assert_ne!(variant, '\0');
        assert!(!variant.is_control());
        // Both predicates fire for one press → chord judgment must run first.
    }
}
