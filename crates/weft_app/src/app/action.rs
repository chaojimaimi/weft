//! Action dispatch extracted from `main.rs`.
//!
//! `execute_action` is the single fan-out point for `Action` values resolved
//! from keybindings. It handles global/session gating, then dispatches each
//! action variant to its handler (copy/paste/scroll/tabs/modals/theme/zoom).

use crate::effect::Effect;
use weft_core::config::Action;

impl crate::App {
    /// Dispatch a weft action resolved from a keybinding. Returns true if the
    /// key was consumed (must not be forwarded to the PTY).
    pub(crate) fn execute_action(&mut self, action: Action) -> bool {
        if crate::input_router::route_global_action(self.interaction.context_menu.is_some())
            == crate::input_router::GlobalActionOverlayRoute::DismissContextMenu
        {
            self.take_context_menu("global action dispatched");
        }
        if crate::input_router::route_session_action(!self.sessions.is_empty(), action)
            == crate::input_router::SessionInputRoute::Consume
        {
            return true;
        }
        match action {
            Action::Copy => {
                self.copy_selection();
                true
            }
            Action::Paste => {
                self.drain_effects(vec![Effect::Paste {
                    tab: self.sessions.active_idx(),
                }]);
                true
            }
            Action::ReloadConfig => {
                self.reload_config();
                true
            }
            Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollLineUp
            | Action::ScrollLineDown
            | Action::ScrollToTop
            | Action::ScrollToBottom => {
                self.scroll_action(action);
                true
            }
            Action::ToggleBlockPanel => {
                if self.panel.open {
                    self.panel.close();
                } else {
                    self.panel.open = true;
                    // Fresh search/selection each time the panel opens.
                    self.panel.query.clear();
                    self.panel.clear_transient_selection();
                    self.panel.search_focused = false;
                }
                // v0.9 W5: resize grid for sidebar so the terminal content
                // reflows beside the panel instead of being covered by it.
                self.recompute_layout();
                self.request_redraw();
                true
            }
            Action::ToggleCommandPalette => {
                self.reset_ime_context("command palette toggled");
                if self.palette.open {
                    self.palette.close();
                    self.clear_prev_focus_if_no_modal();
                } else {
                    // v0.9 fix: opening the palette closes the find bar (and
                    // vice versa) so only one modal owns keyboard input at a
                    // time. Without this, Cmd+F then Cmd+P leaves both
                    // popups open and keystrokes go to the wrong one.
                    // v1.0 S1: also close the Settings panel.
                    self.close_find();
                    self.close_settings();
                    // F4: save the current focus so it can be restored when
                    // the palette closes.
                    self.save_focus_for_modal(crate::scene::FocusId::PaletteQuery);
                    self.palette.open_search();
                    self.refresh_palette_results();
                }
                self.request_redraw();
                true
            }
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                self.zoom_action(action);
                true
            }
            Action::FindInGrid => {
                self.reset_ime_context("find toggled");
                if self.find.open {
                    self.find.close();
                    self.clear_prev_focus_if_no_modal();
                } else {
                    // v0.9 fix: opening find closes the palette (see above).
                    // v1.0 S1: also close the Settings panel.
                    self.close_palette();
                    self.close_settings();
                    // F4: save the current focus so it can be restored when
                    // the find bar closes.
                    self.save_focus_for_modal(crate::scene::FocusId::FindQuery);
                    self.find.reset_query();
                    self.find.open = true;
                }
                self.request_redraw();
                true
            }
            Action::ToggleTheme => {
                self.toggle_theme();
                true
            }
            Action::NewTab => {
                self.new_tab();
                self.drain_effects(vec![Effect::PersistTabs]);
                true
            }
            Action::CloseTab => {
                let effects = self.close_tab();
                self.drain_effects(effects);
                true
            }
            Action::NextTab => {
                let effects = self.next_tab();
                self.drain_effects(effects);
                true
            }
            Action::PrevTab => {
                let effects = self.prev_tab();
                self.drain_effects(effects);
                true
            }
            Action::ToggleSettings => {
                if self.settings.open {
                    self.close_settings();
                } else {
                    self.reset_ime_context("settings opened");
                    // Mutual exclusion: close other modals.
                    self.close_palette();
                    self.close_find();
                    self.save_focus_for_modal(crate::scene::FocusId::Settings);
                    self.open_settings();
                }
                self.request_redraw();
                true
            }
        }
    }
}
