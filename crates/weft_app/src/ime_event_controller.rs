//! macOS/winit IME event controller.

use super::*;

impl App {
    pub(super) fn handle_ime_event(&mut self, ime_event: winit::event::Ime) {
        match ime_event {
            winit::event::Ime::Enabled => {}
            winit::event::Ime::Preedit(text, _cursor) => {
                // Keyboard, preedit and commit must agree on the same
                // focus owner. Any overlay suppresses terminal preedit;
                // Settings intentionally has no text target yet.
                if self.overlay_input_owner().is_some() {
                    self.sessions.active_mut().ime_preedit.clear();
                } else {
                    self.sessions.active_mut().ime_preedit = text;
                }
            }
            winit::event::Ime::Commit(text) => {
                self.sessions.active_mut().ime_preedit.clear();
                if !text.is_empty() {
                    tracing::debug!(
                        tab = self.sessions.active_idx(),
                        len = text.len(),
                        "routing fresh IME commit"
                    );
                    match self.overlay_input_owner() {
                        Some(OverlayInputOwner::Palette) => {
                            self.palette.query.push_str(&text);
                            self.palette.selection = 0;
                            self.refresh_palette_results();
                            self.request_redraw();
                        }
                        Some(OverlayInputOwner::Settings) => {
                            // Settings currently has no free-text field.
                            // Consume the commit so it cannot leak into
                            // a covered prompt or passthrough PTY.
                            tracing::debug!(len = text.len(), "IME commit consumed by settings");
                            self.request_redraw();
                        }
                        Some(OverlayInputOwner::Find) => {
                            self.find.query.push_str(&text);
                            self.find.last_key = Some(std::time::Instant::now());
                            self.request_redraw();
                        }
                        Some(OverlayInputOwner::PanelSearch) => {
                            self.panel.query.push_str(&text);
                            self.clamp_panel_selection();
                            self.request_redraw();
                        }
                        None => {
                            let mode = self
                                .sessions
                                .active_mut()
                                .terminal
                                .as_ref()
                                .map(|t| t.effective_input_mode())
                                .unwrap_or(weft_core::input::InputMode::Passthrough);
                            if mode == weft_core::input::InputMode::Editor {
                                // Editor takeover: composed text goes into the box.
                                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                                    for c in text.chars() {
                                        t.editor_mut().buffer.insert_char(c);
                                    }
                                    // v0.9: IME input clears the editor
                                    // selection (typing replaces it).
                                    t.editor_mut().buffer.clear_selection();
                                }
                                self.interaction.prompt_dragging = false;
                                self.request_redraw();
                            } else {
                                // Passthrough: send committed text to the PTY.
                                //
                                // v1.0 fix: send the text as RAW BYTES, not
                                // via `encode_paste`. A typed/IME-committed
                                // character is keyboard INPUT, not a paste —
                                // wrapping it in bracketed-paste escapes
                                // (`\x1b[200~ … \x1b[201~`) corrupts
                                // alt-screen apps like `less`/`vim` which
                                // don't understand bracketed paste: the
                                // leading `\x1b[` is an unknown CSI to them,
                                // so a typed `/` (intended to start a
                                // search) put `less` into a confused state
                                // and the window appeared frozen until a
                                // resize forced a repaint. Real pastes
                                // (Cmd+V → `Effect::Paste`) still use
                                // `encode_paste` with bracketed wrapping.
                                // (bracketed_paste is a shell-prompt mode;
                                // it stays on inside alt-screen apps
                                // because `swap_alt` doesn't save/restore
                                // it, so we must not consult it here.)
                                let effects =
                                    effect::ime_commit_effects(self.sessions.active_idx(), &text);
                                self.drain_effects(effects);
                            }
                        }
                    }
                }
            }
            winit::event::Ime::Disabled => {
                for tab in self.sessions.tabs_mut() {
                    tab.ime_preedit.clear();
                }
            }
        }
    }
}
