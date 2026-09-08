//! Prompt editor and completion controller extracted from the application shell.

use super::*;

impl App {
    /// Returns true if consumed. Ctrl chords that aren't editor ops fall through
    /// (returns false) so Ctrl+C etc. still reach the PTY.
    pub(super) fn handle_editor_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        text: Option<&str>,
    ) -> bool {
        use weft_core::input::{KeyCode::*, Modifiers};
        let shift = mods.contains(Modifiers::SHIFT);

        // F2 P1-3: Cmd+Enter always submits the editor command (regardless of
        // submit_on_ctrl_enter config). "Submit and keep editor" = the editor
        // is cleared and stays in Editor mode until the shell's preexec marker
        // transitions to Passthrough. Handled before the SUPER early return so
        // it doesn't fall through to app-level shortcuts.
        if key == Enter
            && editor_enter_submits(self.config_state.config.editor.submit_on_ctrl_enter, mods)
            && mods.contains(Modifiers::SUPER)
        {
            self.editor_submit();
            return true;
        }

        // v0.9 fix: Cmd (SUPER) chords are app-level shortcuts (copy/paste/
        // tab/panel…), not editor input. If a Cmd chord reaches here it
        // means no keybinding matched — drop it instead of inserting the
        // character into the editor (e.g. Cmd+Shift+V was inserting 'V').
        if mods.contains(Modifiers::SUPER) {
            return false;
        }

        // v0.9: any non-Cmd editor key clears the mouse-drag selection so
        // typing replaces the selection. Cmd+C is handled above (returns
        // false) so it won't clear the selection — copy still works.
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            if t.editor().buffer.has_selection() {
                t.editor_mut().buffer.clear_selection();
                self.request_redraw();
            }
        }
        self.interaction.prompt_dragging = false;

        // Ctrl editor ops (Ctrl+C / other Ctrl chords fall through to the PTY).
        if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::ALT) && key != Enter {
            let consumed = if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                let e = t.editor_mut();
                match key {
                    Char('a') => {
                        e.buffer.move_line_home();
                        true
                    }
                    Char('e') => {
                        e.buffer.move_line_end();
                        true
                    }
                    Char('w') => {
                        e.buffer.delete_word_back();
                        true
                    }
                    Char('u') => {
                        e.buffer.clear_line();
                        true
                    }
                    Char('k') => {
                        e.buffer.delete_to_end();
                        true
                    }
                    Char('r') => {
                        if e.is_searching() {
                            e.search_next();
                        } else {
                            e.search_start();
                        }
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };
            return consumed;
        }

        // Ctrl+R search mode intercepts printable/backspace/enter/esc/arrows.
        let searching = self
            .sessions
            .active_mut()
            .terminal
            .as_ref()
            .map(|t| t.editor().is_searching())
            .unwrap_or(false);
        if searching {
            if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                let e = t.editor_mut();
                match key {
                    Char(c) => {
                        e.search_input(resolve_text_char(text, c, shift));
                        return true;
                    }
                    Backspace => {
                        e.search_backspace();
                        return true;
                    }
                    Enter => {
                        e.search_accept();
                        return true;
                    }
                    Escape => {
                        e.search_cancel();
                        return true;
                    }
                    Up => {
                        e.search_prev();
                        return true;
                    }
                    Down => {
                        e.search_next();
                        return true;
                    }
                    _ => {}
                }
            }
            return false;
        }

        // Tab-completion mode: Tab cycles, Enter accepts (no submit), Up/Down
        // navigate, Esc cancels. Any other key cancels and falls through to
        // normal editing (so typing/deleting ends the session).
        let completing = self
            .sessions
            .active_mut()
            .terminal
            .as_ref()
            .map(|t| t.editor().is_completing())
            .unwrap_or(false);
        if completing {
            use crate::paint::command_surface::CommandSurfaceKeyAction;
            let consumed =
                match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
                    CommandSurfaceKeyAction::CycleFocus => {
                        if shift {
                            self.editor_completion_prev();
                        } else {
                            self.editor_completion_next();
                        }
                        true
                    }
                    CommandSurfaceKeyAction::Accept => {
                        self.editor_completion_accept();
                        true
                    }
                    CommandSurfaceKeyAction::MoveUp => {
                        self.editor_completion_prev();
                        true
                    }
                    CommandSurfaceKeyAction::MoveDown => {
                        self.editor_completion_next();
                        true
                    }
                    CommandSurfaceKeyAction::PageUp => {
                        self.editor_completion_page(false);
                        true
                    }
                    CommandSurfaceKeyAction::PageDown => {
                        self.editor_completion_page(true);
                        true
                    }
                    CommandSurfaceKeyAction::Cancel => {
                        self.editor_completion_cancel();
                        true
                    }
                    CommandSurfaceKeyAction::Unhandled => false,
                };
            if consumed {
                self.request_redraw();
                return true;
            }
            self.editor_completion_cancel();
        }

        match key {
            Tab => {
                self.editor_start_completion();
                true
            }
            Enter => {
                // submit_on_ctrl_enter: Ctrl+Enter submits, plain Enter newlines
                // (Warp default). Otherwise plain Enter submits, Shift+Enter
                // newlines.
                let do_submit = editor_enter_submits(
                    self.config_state.config.editor.submit_on_ctrl_enter,
                    mods,
                );
                if do_submit {
                    self.editor_submit();
                } else if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.split_newline();
                }
                true
            }
            Char(c) => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut()
                        .buffer
                        .insert_char(resolve_text_char(text, c, shift));
                }
                true
            }
            Backspace => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.delete_backspace();
                }
                true
            }
            Delete => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.delete_forward();
                }
                true
            }
            Left => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.move_left();
                }
                true
            }
            Right => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.move_right();
                }
                true
            }
            Home => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.move_line_home();
                }
                true
            }
            End => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.move_line_end();
                }
                true
            }
            Up => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    let e = t.editor_mut();
                    if e.buffer.cursor.0 == 0 {
                        e.history_prev();
                    } else {
                        e.buffer.cursor.0 -= 1;
                        let len = e.buffer.lines[e.buffer.cursor.0].chars().count();
                        e.buffer.cursor.1 = e.buffer.cursor.1.min(len);
                    }
                }
                true
            }
            Down => {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    let e = t.editor_mut();
                    let last = e.buffer.line_count() - 1;
                    if e.buffer.cursor.0 == last {
                        e.history_next();
                    } else {
                        e.buffer.cursor.0 += 1;
                        let len = e.buffer.lines[e.buffer.cursor.0].chars().count();
                        e.buffer.cursor.1 = e.buffer.cursor.1.min(len);
                    }
                }
                true
            }
            Escape => true, // swallow stray Esc in editor mode
            _ => false,
        }
    }

    // ── Tab completion (drives Editor's completion state machine) ───────────

    pub(super) fn editor_start_completion(&mut self) {
        // Gather context under an immutable borrow, then mutate the editor.
        let pane_session_id = self.sessions.active().pane_session_id;
        let (line_owned, col, cwd, history) = match self.sessions.active_mut().terminal.as_ref() {
            Some(t) => {
                let line_idx = t.editor().buffer.cursor.0;
                let col = t.editor().buffer.cursor.1;
                let line = t.editor().buffer.lines.get(line_idx).cloned();
                let cwd = t.cwd().unwrap_or("").to_string();
                let history = t.editor().history().to_vec();
                (line, col, cwd, history)
            }
            None => return,
        };
        let Some(line_str) = line_owned.as_deref() else {
            return;
        };

        // Determine the word range and prefix. Normally this is the token left
        // of the cursor. But when the cursor sits on whitespace after a command
        // (e.g. `cd |`), word_at returns None — in that case, if we're at an
        // argument position, treat it as an empty-prefix path completion so
        // Tab lists all files/dirs in the cwd (matching Warp's behavior).
        let (ws, we, prefix, is_cmd_pos): (usize, usize, String, bool) =
            match word_at(line_str, col) {
                Some((ws, we)) => {
                    let prefix: String = line_str.chars().skip(ws).take(we - ws).collect();
                    let is_cmd = is_command_position(line_str, ws);
                    (ws, we, prefix, is_cmd)
                }
                None => {
                    // Cursor on whitespace. Check if there's a command token
                    // before the cursor (making this an argument position).
                    // If so, start an empty-prefix path completion.
                    let is_cmd = is_command_position(line_str, col);
                    if is_cmd {
                        return; // blank line or after operator — nothing to complete
                    }
                    (col, col, String::new(), false)
                }
            };

        let position = if is_cmd_pos {
            CompletePosition::Command
        } else {
            CompletePosition::Argument
        };

        // Skip empty prefix at command position (nothing to match).
        if prefix.is_empty() && is_cmd_pos {
            return;
        }

        let path_bins: Vec<String> = if is_cmd_pos {
            self.config_state.path_bins.clone()
        } else {
            Vec::new()
        };
        let workflows = self
            .palette
            .store
            .as_ref()
            .and_then(|store| store.list().ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|workflow| {
                workflow
                    .steps
                    .first()
                    .map(|step| (workflow.name, step.command.clone()))
            })
            .collect();
        let workspace_commands = self
            .capture_workspace("active".to_string())
            .into_iter()
            .flat_map(|workspace| workspace.tabs)
            .flat_map(|tab| {
                tab.panes
                    .collect_panes()
                    .into_iter()
                    .map(|(_, draft)| draft.to_string())
                    .collect::<Vec<_>>()
            })
            .filter(|draft| !draft.trim().is_empty())
            .collect();
        self.completion_worker
            .submit(crate::completion_worker::CompletionSubmit {
                pane_session_id,
                line: line_str.to_string(),
                cursor_col: col,
                word_start: ws,
                word_end: we,
                prefix,
                cwd,
                history,
                path_bins,
                workflows,
                workspace_commands,
                position,
            });
    }

    pub(super) fn poll_completion_results(&mut self) {
        // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2): stale worker
        // results can arrive after the last tab exited; the loop body derefs
        // sessions.active() unconditionally. (The Wake arm now skips the call
        // on empty tabs — this entry guard is the second, independent layer.)
        if self.sessions.tabs().is_empty() {
            return;
        }
        while let Some(result) = self.completion_worker.try_recv() {
            if result.generation != self.completion_worker.current_generation()
                || self.sessions.active().pane_session_id != result.pane_session_id
            {
                continue;
            }
            let Some(terminal) = self.sessions.active_mut().terminal.as_mut() else {
                continue;
            };
            let editor = terminal.editor_mut();
            let line_idx = editor.buffer.cursor.0;
            let current_line = editor.buffer.lines.get(line_idx).map(String::as_str);
            if current_line != Some(result.line.as_str())
                || editor.buffer.cursor.1 != result.cursor_col
                || result.matches.is_empty()
            {
                continue;
            }
            editor.start_completion(result.matches, result.word_start, result.word_end);
            if editor
                .completion_view()
                .is_some_and(|(matches, _)| matches.len() == 1)
            {
                editor.completion_accept();
            }
        }
    }

    /// v1.8: Drain completed AI request results from the background tokio
    /// tasks. Routes `CommandGen` results to the palette as
    /// `PaletteEntry::AiSuggestion` entries; `Diagnose` results are dropped
    /// here (v1.8.2 will route them to the block view).
    pub(super) fn poll_ai_results(&mut self) {
        let events = self.ai_state.poll();
        if events.is_empty() {
            return;
        }

        // v1.8.7: Discard any residual native marked text when AI results
        // arrive. The user typed a query (possibly via CJK IME) and the
        // composition was committed, but macOS may still hold marked-text
        // state that intercepts the next Esc keypress, preventing the
        // palette from closing (issue #2: "再次按Esc时Palette窗口并没有关闭").
        if let Some(window) = &self.window {
            crate::ime::discard_marked_text(window);
        }

        for event in events {
            match event {
                crate::ai::AiResultEvent::CommandGen { id, command } => {
                    // Only accept if the palette is in AiCommand mode and
                    // the id matches the pending request.
                    if let PaletteSubMode::AiCommand {
                        pending_id,
                        last_query,
                        error,
                        ..
                    } = &mut self.palette.submode
                    {
                        if *pending_id == Some(id) {
                            *pending_id = None;
                            // v1.8.7: Clear the saved query + error on success.
                            last_query.clear();
                            *error = None;
                            let risk = crate::ai::classify_command_risk(&command);
                            // Clear old AI suggestions and prepend the new one.
                            self.palette
                                .results
                                .retain(|e| !matches!(e, PaletteEntry::AiSuggestion { .. }));
                            self.palette
                                .results
                                .insert(0, PaletteEntry::AiSuggestion { command, risk });
                            self.palette.selection = 0;
                            self.request_redraw();
                        }
                    }
                }
                crate::ai::AiResultEvent::Error { id, message } => {
                    // Show error in the palette if it's the pending request.
                    if let PaletteSubMode::AiCommand {
                        pending_id,
                        buffer,
                        last_query,
                        error,
                    } = &mut self.palette.submode
                    {
                        if *pending_id == Some(id) {
                            *pending_id = None;
                            tracing::warn!(error = %message, "AI command generation failed");
                            // v1.8.7: Restore the user's query so they can
                            // edit/retry, and surface the error in the banner.
                            if buffer.is_empty() && !last_query.is_empty() {
                                *buffer = std::mem::take(last_query);
                            }
                            *error = Some(message.clone());
                            self.palette
                                .results
                                .retain(|e| !matches!(e, PaletteEntry::AiSuggestion { .. }));
                            self.palette.selection = 0;
                            self.request_redraw();
                        }
                    }
                    // v1.8.2: Also check block diagnose state.
                    let mut matched_block = None;
                    for (&block_id, state) in &self.block_diagnose_state {
                        if state.pending_id == Some(id) {
                            matched_block = Some(block_id);
                            break;
                        }
                    }
                    if let Some(block_id) = matched_block {
                        if let Some(state) = self.block_diagnose_state.get_mut(&block_id) {
                            state.pending_id = None;
                            state.result = Some(Err(message));
                        }
                        self.request_redraw();
                    }
                }
                crate::ai::AiResultEvent::Cancelled { id } => {
                    // Clear the pending marker if this was our request.
                    if let PaletteSubMode::AiCommand { pending_id, .. } = &mut self.palette.submode
                    {
                        if *pending_id == Some(id) {
                            *pending_id = None;
                            self.request_redraw();
                        }
                    }
                    // v1.8.2: Also clear block diagnose pending state.
                    let mut matched_block = None;
                    for (&block_id, state) in &self.block_diagnose_state {
                        if state.pending_id == Some(id) {
                            matched_block = Some(block_id);
                            break;
                        }
                    }
                    if let Some(block_id) = matched_block {
                        if let Some(state) = self.block_diagnose_state.get_mut(&block_id) {
                            state.pending_id = None;
                            // Keep existing result if any; just clear the
                            // "thinking" indicator. If there was no prior
                            // result, remove the entry so the panel disappears.
                            if state.result.is_none() {
                                self.block_diagnose_state.remove(&block_id);
                            }
                        }
                        self.request_redraw();
                    }
                }
                crate::ai::AiResultEvent::Diagnose { id, explanation } => {
                    // v1.8.2: Route to the block that issued the request.
                    // Find the block_id whose pending_id matches.
                    let mut matched_block = None;
                    for (&block_id, state) in &self.block_diagnose_state {
                        if state.pending_id == Some(id) {
                            matched_block = Some(block_id);
                            break;
                        }
                    }
                    if let Some(block_id) = matched_block {
                        if let Some(state) = self.block_diagnose_state.get_mut(&block_id) {
                            state.pending_id = None;
                            state.result = Some(Ok(explanation));
                        }
                        self.request_redraw();
                    }
                }
                crate::ai::AiResultEvent::ModelsRefreshed { id, result } => {
                    // v1.8.3: Route to the Settings LocalAi tab. Only accept
                    // if the id matches the in-flight models request.
                    if self.ai_models_request_id == Some(id) {
                        self.ai_models_request_id = None;
                        match result {
                            Ok(models) => {
                                let n = models.len();
                                self.ai_models = models;
                                self.ai_connection_status =
                                    crate::app_state::AiConnectionStatus::Ok(n);
                            }
                            Err(msg) => {
                                self.ai_connection_status =
                                    crate::app_state::AiConnectionStatus::Failed(msg);
                            }
                        }
                        self.request_redraw();
                    }
                }
            }
        }
    }

    pub(super) fn editor_completion_next(&mut self) {
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.editor_mut().completion_next();
        }
    }

    pub(super) fn editor_completion_prev(&mut self) {
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.editor_mut().completion_prev();
        }
    }

    fn editor_completion_page(&mut self, forward: bool) {
        let Some((selected, target)) = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.editor().completion_view())
            .map(|(matches, selected)| {
                let target = crate::paint::command_surface::apply_page_selection(
                    selected,
                    matches.len(),
                    self.interaction.popup_max_rows,
                    forward,
                );
                (selected, target)
            })
        else {
            return;
        };
        if target >= selected {
            for _ in selected..target {
                self.editor_completion_next();
            }
        } else {
            for _ in target..selected {
                self.editor_completion_prev();
            }
        }
    }

    pub(super) fn editor_completion_accept(&mut self) {
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.editor_mut().completion_accept();
        }
    }

    pub(super) fn editor_completion_cancel(&mut self) {
        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
            t.editor_mut().completion_cancel();
        }
    }

    /// Submit the editor's command: write PTY bytes (and any terminal query
    /// response) and locally block the editor through the Enter→preexec window.
    pub(super) fn editor_submit(&mut self) {
        // The editor and the launched command share one native NSView IME
        // context. Drop any uncommitted editor composition before the PTY/TUI
        // becomes the input owner, otherwise its first key can commit stale
        // text into less/vim.
        self.reset_ime_context("editor command submitted");
        self.sessions.active_mut().arm_tui_scroll_window();
        let bytes = self
            .sessions
            .active_mut()
            .terminal
            .as_mut()
            .map(|t| t.submit_command())
            .unwrap_or_default();
        if !bytes.is_empty() {
            if let Err(error) = self.sessions.active_mut().write_user_input(&bytes) {
                warn!(%error, "failed to submit editor command to PTY");
            }
        }
        let resp = self
            .sessions
            .active_mut()
            .terminal
            .as_mut()
            .map(|t| t.take_response())
            .unwrap_or_default();
        if !resp.is_empty() {
            if let Some(pty) = &self.sessions.active_mut().pty {
                let _ = pty.write_sync(&resp);
            }
        }
        // Snap the block view to the bottom so the user sees the new
        // command's output. Without this, a fast command (e.g. `echo hi`)
        // finishes before the next redraw's `had_output && phase ==
        // CommandExecuting` check fires — the phase is already back to
        // AtPrompt by then, so the existing snap logic never triggers and
        // the view stays scrolled up on history. Snapping here, at submit
        // time, guarantees the user sees the result regardless of how fast
        // the command completes.
        self.sessions.active_mut().snap_to_bottom();
        self.request_redraw();
    }
}

fn editor_enter_submits(submit_on_ctrl_enter: bool, mods: Modifiers) -> bool {
    if mods.contains(Modifiers::SUPER) {
        return true;
    }
    if submit_on_ctrl_enter {
        mods.contains(Modifiers::CONTROL)
    } else {
        !mods.contains(Modifiers::SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use super::editor_enter_submits;
    use weft_core::input::Modifiers;

    #[test]
    fn enter_policy_matches_config_and_command_override() {
        assert!(editor_enter_submits(false, Modifiers::empty()));
        assert!(!editor_enter_submits(false, Modifiers::SHIFT));
        assert!(!editor_enter_submits(true, Modifiers::empty()));
        assert!(editor_enter_submits(true, Modifiers::CONTROL));
        assert!(editor_enter_submits(true, Modifiers::SUPER));
    }
}
