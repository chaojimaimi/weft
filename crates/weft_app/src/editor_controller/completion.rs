//! Editor tab-completion family, moved verbatim out of
//! `editor_controller.rs` (v1.15.2 governance cart, zero-rewrite).
//! Child module of the controller (mouse_controller/ precedent).

use super::*;

impl App {
    // ── Tab completion (drives Editor's completion state machine) ───────────

    pub(super) fn editor_start_completion(&mut self) {
        // Gather context under an immutable borrow, then mutate the editor.
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — nothing to
        // complete.
        let Some(tab) = self.sessions.active_mut() else {
            return;
        };
        let pane_session_id = tab.pane_session_id;
        // T10 P1: the context tuple is owned, so it can escape the guard
        // scope; the `None` (no terminal) early return is preserved.
        let context = tab.with_terminal(|t| {
            let line_idx = t.editor().buffer.cursor.0;
            let col = t.editor().buffer.cursor.1;
            let line = t.editor().buffer.lines.get(line_idx).cloned();
            let cwd = t.cwd().unwrap_or("").to_string();
            let history = t.editor().history().to_vec();
            (line, col, cwd, history)
        });
        let (line_owned, col, cwd, history) = match context {
            Some(context) => context,
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

    pub(crate) fn poll_completion_results(&mut self) {
        // v1.11.15 (FIX B, PLAN_v11115_EXIT_RACE_MOUSE_LEAK §2): stale worker
        // results can arrive after the last tab exited; the loop body derefs
        // sessions.active() unconditionally. (The Wake arm now skips the call
        // on empty tabs — this entry guard is the second, independent layer.)
        if self.sessions.tabs().is_empty() {
            return;
        }
        while let Some(result) = self.completion_worker.try_recv() {
            if result.generation != self.completion_worker.current_generation()
                || self
                    .sessions
                    .active()
                    .map_or(true, |tab| tab.pane_session_id != result.pane_session_id)
            {
                continue;
            }
            let Some(mut terminal) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            else {
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

    pub(super) fn editor_completion_next(&mut self) {
        if let Some(mut t) = self
            .sessions
            .active_mut()
            .and_then(|tab| tab.lock_terminal())
        {
            t.editor_mut().completion_next();
        }
    }

    pub(super) fn editor_completion_prev(&mut self) {
        if let Some(mut t) = self
            .sessions
            .active_mut()
            .and_then(|tab| tab.lock_terminal())
        {
            t.editor_mut().completion_prev();
        }
    }

    pub(super) fn editor_completion_page(&mut self, forward: bool) {
        // T10 P1: `completion_view` lends the terminal's match list, so the
        // page-selection math runs inside the guard scope and only the owned
        // (selected, target) pair escapes.
        let Some((selected, target)) = self.sessions.active().and_then(|tab| {
            tab.with_terminal(|terminal| {
                terminal
                    .editor()
                    .completion_view()
                    .map(|(matches, selected)| {
                        let target = crate::paint::command_surface::apply_page_selection(
                            selected,
                            matches.len(),
                            self.interaction.popup_max_rows,
                            forward,
                        );
                        (selected, target)
                    })
            })
            .flatten()
        }) else {
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
        if let Some(mut t) = self
            .sessions
            .active_mut()
            .and_then(|tab| tab.lock_terminal())
        {
            t.editor_mut().completion_accept();
        }
    }

    pub(super) fn editor_completion_cancel(&mut self) {
        if let Some(mut t) = self
            .sessions
            .active_mut()
            .and_then(|tab| tab.lock_terminal())
        {
            t.editor_mut().completion_cancel();
        }
    }
}
