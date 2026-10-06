//! macOS/winit IME event controller.

use super::*;

impl App {
    /// Cancel native marked text before keyboard ownership changes. macOS
    /// keeps this state on the window rather than on an individual Weft tab.
    pub(super) fn reset_ime_context(&mut self, reason: &'static str) {
        for action in event_replay::reset_ime_context_actions() {
            match action {
                event_replay::ImeContextResetAction::ClearAllPreedit => {
                    event_replay::clear_all_preedit(self.sessions.tabs_mut());
                }
                event_replay::ImeContextResetAction::DiscardNativeMarkedText => {
                    if let Some(window) = &self.window {
                        ime::discard_marked_text(window);
                    }
                }
            }
        }
        tracing::debug!(reason, "native IME context reset");
    }

    pub(super) fn handle_ime_event(&mut self, ime_event: winit::event::Ime) {
        let input = match ime_event {
            winit::event::Ime::Enabled => event_replay::ImeInput::Enabled,
            winit::event::Ime::Preedit(text, cursor) => {
                event_replay::ImeInput::Preedit { text, cursor }
            }
            winit::event::Ime::Commit(text) => event_replay::ImeInput::Commit(text),
            winit::event::Ime::Disabled => event_replay::ImeInput::Disabled,
        };
        // v1.12.24 (N-1): intercept BEFORE the router context is built — the
        // note editor is a focus-modal card, so composition routes to its own
        // inline preedit (never to the terminal preedit, which would drop
        // CJK into the PTY grid while ASCII direct keys land in the buffer —
        // the two-pipe split this fix removes).
        if self.overlay_input_owner() == Some(crate::input_router::OverlayInputOwner::NoteEditor) {
            self.handle_note_editor_ime(input);
            return;
        }
        let (active_tab, input_mode) = if self.sessions.is_empty() {
            (0, weft_core::input::InputMode::Passthrough)
        } else {
            (
                self.sessions.active_idx(),
                self.sessions
                    .active()
                    .and_then(|tab| tab.terminal.as_ref())
                    .map(|terminal| terminal.effective_input_mode())
                    .unwrap_or(weft_core::input::InputMode::Passthrough),
            )
        };
        let context = event_replay::ImeRouteContext {
            owner: self.overlay_input_owner(),
            input_mode,
            active_tab,
        };
        if let event_replay::ImeInput::Preedit { text, .. } = &input {
            // v1.10.26 probe: preedit events reaching the router (winit
            // delivered them) with the routing inputs. Info-level: fires
            // only while composing, and the ongoing symptom needs default-
            // level evidence.
            tracing::debug!(
                len = text.chars().count(),
                ?context.owner,
                ?context.input_mode,
                "IME_PREEDIT_ROUTE"
            );
        }
        for action in event_replay::route_ime_input(input, context) {
            match action {
                event_replay::ImeRoutingAction::ClearActivePreedit => {
                    // v1.8.4: when the Palette owns IME, clear the palette's
                    // preedit (not the tab's). v1.12.26 (P1-02/P1-03):
                    // Find/PanelSearch own preedit fields now, so a blanket
                    // clear sweeps them too — the Commit route sends
                    // ClearActivePreedit first (route side), and a stale
                    // composition would otherwise outlive the commit and
                    // keep painting over the committed text.
                    // v1.8.7: always discard native marked text for ANY overlay
                    // owner (Palette/Settings/Find/ContextMenu/PanelSearch), not
                    // just Palette. Without this, macOS IME intercepts Esc to
                    // dismiss residual marked text, preventing Settings/Find
                    // from closing on Esc (issue #3).
                    if context.owner == Some(crate::input_router::OverlayInputOwner::Palette) {
                        let had_preedit = !self.palette.ime_preedit.is_empty();
                        self.palette.ime_preedit.clear();
                        self.palette.ime_preedit_cursor = None;
                        if had_preedit {
                            if let Some(window) = &self.window {
                                crate::ime::discard_marked_text(window);
                            }
                        }
                    } else {
                        let mut had_overlay_preedit = false;
                        if !self.find.ime_preedit.is_empty() {
                            self.find.ime_preedit.clear();
                            self.find.ime_preedit_cursor = None;
                            had_overlay_preedit = true;
                        }
                        if !self.panel.ime_preedit.is_empty() {
                            self.panel.ime_preedit.clear();
                            self.panel.ime_preedit_cursor = None;
                            had_overlay_preedit = true;
                        }
                        if let Some(tab) = self.sessions.tab_mut(active_tab) {
                            let had_preedit = !tab.ime_preedit.is_empty();
                            tab.ime_preedit.clear();
                            tab.ime_preedit_cursor = None;
                            // v1.8.7: discard native marked text for non-Palette
                            // overlays too, so Esc isn't intercepted.
                            if (had_preedit || had_overlay_preedit) && context.owner.is_some() {
                                if let Some(window) = &self.window {
                                    crate::ime::discard_marked_text(window);
                                }
                            }
                        }
                    }
                }
                event_replay::ImeRoutingAction::SetActivePreedit { text, cursor } => {
                    if context.owner == Some(crate::input_router::OverlayInputOwner::Palette) {
                        self.palette.ime_preedit = text;
                        self.palette.ime_preedit_cursor = cursor;
                    } else if let Some(tab) = self.sessions.tab_mut(active_tab) {
                        tab.ime_preedit = text;
                        tab.ime_preedit_cursor = cursor;
                    }
                    // v1.10.26 root cause of the omp pinyin regression: the
                    // preedit string was updated but no redraw was scheduled,
                    // so nothing ever painted it. Grid-mode TUIs used to get
                    // away with this via the spinner's 80ms wake; v1.10.23
                    // gated that wake to the block view, leaving composition
                    // updates without a frame. IME events are low-frequency
                    // (only while composing), so an explicit request is cheap.
                    self.request_redraw();
                }
                // v1.12.26 (P1-02/P1-03): the target-carrying Set arms write
                // exactly the state the variant names — no owner re-guess.
                // Same explicit-redraw contract as SetActivePreedit above
                // (composition updates need a frame to paint).
                event_replay::ImeRoutingAction::SetActiveFindPreedit { text, cursor } => {
                    self.find.ime_preedit = text;
                    self.find.ime_preedit_cursor = cursor;
                    self.request_redraw();
                }
                event_replay::ImeRoutingAction::SetActivePanelPreedit { text, cursor } => {
                    self.panel.ime_preedit = text;
                    self.panel.ime_preedit_cursor = cursor;
                    self.request_redraw();
                }
                event_replay::ImeRoutingAction::ClearAllPreedit => {
                    event_replay::clear_all_preedit(self.sessions.tabs_mut());
                    // v1.8.4: also clear palette preedit on full reset.
                    self.palette.ime_preedit.clear();
                    self.palette.ime_preedit_cursor = None;
                    // v1.12.26 (P1-02/P1-03): find/panel join the full reset.
                    self.find.ime_preedit.clear();
                    self.find.ime_preedit_cursor = None;
                    self.panel.ime_preedit.clear();
                    self.panel.ime_preedit_cursor = None;
                    // Same contract as SetActivePreedit: a cleared preedit
                    // must leave the screen, which needs a frame.
                    self.request_redraw();
                }
                event_replay::ImeRoutingAction::Commit { target, text } => {
                    tracing::debug!(
                        tab = active_tab,
                        len = text.len(),
                        ?target,
                        "routing fresh IME commit"
                    );
                    match target {
                        // v1.12.24 (N-1): conservative sink — the note editor
                        // intercepts its IME before the router, so this arm
                        // only fires if routing ever changes under us.
                        event_replay::ImeCommitTarget::NoteEditorConsumed => {
                            tracing::debug!(len = text.len(), "IME commit consumed by note editor");
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::Palette => {
                            // v1.8.4: route commit to the correct buffer
                            // based on the active submode. AiCommand and
                            // other banner submodes have their own buffer;
                            // Search writes to the query.
                            match &mut self.palette.submode {
                                crate::palette_state::PaletteSubMode::AiCommand {
                                    buffer, ..
                                }
                                | crate::palette_state::PaletteSubMode::CreateWorkflow {
                                    buffer,
                                    ..
                                }
                                | crate::palette_state::PaletteSubMode::EditWorkflow {
                                    buffer,
                                    ..
                                }
                                | crate::palette_state::PaletteSubMode::SelectTheme {
                                    buffer,
                                    ..
                                } => {
                                    buffer.push_str(&text);
                                }
                                crate::palette_state::PaletteSubMode::Search => {
                                    self.palette.query.push_str(&text);
                                    self.palette.selection = 0;
                                    self.refresh_palette_results();
                                }
                                crate::palette_state::PaletteSubMode::ConfirmDelete { .. } => {}
                            }
                            self.palette.ime_preedit.clear();
                            self.palette.ime_preedit_cursor = None;
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::SettingsConsumed => {
                            tracing::debug!(len = text.len(), "IME commit consumed by settings");
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::ContextMenuConsumed => {
                            tracing::debug!(
                                len = text.len(),
                                "IME commit consumed by context menu"
                            );
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::Find => {
                            self.find.query.push_str(&text);
                            // v1.12.26 (P1-02): mirror the palette arm — the
                            // commit consumes the live composition.
                            self.find.ime_preedit.clear();
                            self.find.ime_preedit_cursor = None;
                            self.arm_find_refresh();
                        }
                        event_replay::ImeCommitTarget::PanelSearch => {
                            self.panel.query.push_str(&text);
                            // v1.12.26 (P1-03): same commit-consumes-preedit
                            // contract as the find bar.
                            self.panel.ime_preedit.clear();
                            self.panel.ime_preedit_cursor = None;
                            self.clamp_panel_scroll();
                            self.clamp_panel_selection();
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::Editor { tab } => {
                            if let Some(terminal) = self
                                .sessions
                                .tab_mut(tab)
                                .and_then(|session| session.terminal.as_mut())
                            {
                                for c in text.chars() {
                                    terminal.editor_mut().buffer.insert_char(c);
                                }
                                terminal.editor_mut().buffer.clear_selection();
                            }
                            self.interaction.prompt_dragging = false;
                            self.request_redraw();
                        }
                        event_replay::ImeCommitTarget::Pty { tab } => {
                            // Typed/IME text is raw keyboard input, not paste;
                            // bracketed-paste wrapping would corrupt less/vim.
                            // v1.10.4: log the committed text at info so CJK
                            // input drop/corruption during dogfood is observable
                            // from the default-level log without bumping
                            // RUST_LOG. Truncate to 40 chars to bound size.
                            tracing::info!(
                                tab,
                                bytes = text.len(),
                                preview = %text.chars().take(40).collect::<String>(),
                                "IME commit → PTY"
                            );
                            // v1.11.7 (PLAN_v1117 D-c): an IME commit forwarded
                            // to the PTY is user input — arm the interactive
                            // exemption exactly like a forwarded key, so CJK
                            // input into pi/claude gets the classic takeover
                            // instead of lingering in the block view.
                            if let Some(terminal) = self
                                .sessions
                                .tab_mut(tab)
                                .and_then(|session| session.terminal.as_mut())
                            {
                                terminal.note_interactive_stdin();
                            }
                            // v1.11.11 (M-B): Effects carry the stable session
                            // id; the commit target's index still routes the
                            // interactive-stdin note above.
                            if let Some(session) = self.sessions.tab(tab) {
                                self.drain_effects(effect::ime_commit_effects(
                                    session.session_id,
                                    &text,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    /// v1.12.24 (N-1): the open note editor's own IME pipeline. Preedit
    /// renders inline in the card (mirrors palette.ime_preedit); Commit
    /// inserts committed text at the caret via the SHARED insertion
    /// semantics of `handle_note_editor_key`'s printable arm (no fork);
    /// Enabled/Disabled drop stale composition state. Every branch redraws.
    fn handle_note_editor_ime(&mut self, input: event_replay::ImeInput) {
        match input {
            event_replay::ImeInput::Preedit { text, cursor } => {
                self.note_editor.ime_preedit = text;
                self.note_editor.ime_preedit_cursor = cursor;
            }
            event_replay::ImeInput::Commit(text) => {
                if !text.is_empty() {
                    self.insert_text_into_note_editor(&text);
                }
                self.note_editor.ime_preedit.clear();
                self.note_editor.ime_preedit_cursor = None;
            }
            event_replay::ImeInput::Enabled | event_replay::ImeInput::Disabled => {
                self.note_editor.ime_preedit.clear();
                self.note_editor.ime_preedit_cursor = None;
            }
        }
        self.request_redraw();
    }
}
