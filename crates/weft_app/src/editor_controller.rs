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
            let consumed = match key {
                Tab => {
                    self.editor_completion_next();
                    true
                }
                Enter => {
                    self.editor_completion_accept();
                    true
                }
                Up => {
                    self.editor_completion_prev();
                    true
                }
                Down => {
                    self.editor_completion_next();
                    true
                }
                // F4: unified keyboard protocol — PageUp/PageDown navigate
                // completion candidates, consistent with Palette/Find.
                PageUp => {
                    self.editor_completion_prev();
                    true
                }
                PageDown => {
                    self.editor_completion_next();
                    true
                }
                Escape => {
                    self.editor_completion_cancel();
                    true
                }
                _ => false,
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
        let ctx = CompleteCtx {
            cwd: &cwd,
            history: &history,
            path_bins: &path_bins,
        };
        let matches = complete(&prefix, &ctx, position);
        if matches.is_empty() {
            return;
        }
        let Some(t) = self.sessions.active_mut().terminal.as_mut() else {
            return;
        };
        let e = t.editor_mut();
        if matches.len() == 1 {
            // Single candidate: accept immediately (replace the word).
            e.start_completion(matches, ws, we);
            e.completion_accept();
        } else {
            e.start_completion(matches, ws, we);
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
            if let Some(pty) = &self.sessions.active_mut().pty {
                let _ = pty.write_sync(&bytes);
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
