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
        let (active_tab, input_mode) = if self.sessions.is_empty() {
            (0, weft_core::input::InputMode::Passthrough)
        } else {
            (
                self.sessions.active_idx(),
                self.sessions
                    .active()
                    .terminal
                    .as_ref()
                    .map(|terminal| terminal.effective_input_mode())
                    .unwrap_or(weft_core::input::InputMode::Passthrough),
            )
        };
        let context = event_replay::ImeRouteContext {
            owner: self.overlay_input_owner(),
            input_mode,
            active_tab,
        };
        for action in event_replay::route_ime_input(input, context) {
            match action {
                event_replay::ImeRoutingAction::ClearActivePreedit => {
                    if let Some(tab) = self.sessions.tab_mut(active_tab) {
                        tab.ime_preedit.clear();
                        tab.ime_preedit_cursor = None;
                    }
                }
                event_replay::ImeRoutingAction::SetActivePreedit { text, cursor } => {
                    if let Some(tab) = self.sessions.tab_mut(active_tab) {
                        tab.ime_preedit = text;
                        tab.ime_preedit_cursor = cursor;
                    }
                }
                event_replay::ImeRoutingAction::ClearAllPreedit => {
                    event_replay::clear_all_preedit(self.sessions.tabs_mut());
                }
                event_replay::ImeRoutingAction::Commit { target, text } => {
                    tracing::debug!(
                        tab = active_tab,
                        len = text.len(),
                        ?target,
                        "routing fresh IME commit"
                    );
                    match target {
                        event_replay::ImeCommitTarget::Palette => {
                            self.palette.query.push_str(&text);
                            self.palette.selection = 0;
                            self.refresh_palette_results();
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
                            self.arm_find_refresh();
                        }
                        event_replay::ImeCommitTarget::PanelSearch => {
                            self.panel.query.push_str(&text);
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
                            self.drain_effects(effect::ime_commit_effects(tab, &text));
                        }
                    }
                }
            }
        }
    }
}
