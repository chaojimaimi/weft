//! Action dispatch extracted from `main.rs`.
//!
//! `execute_action` is the single fan-out point for `Action` values resolved
//! from keybindings. It handles global/session gating, then dispatches each
//! action variant to its handler (copy/paste/scroll/tabs/modals/theme/zoom).

use crate::effect::Effect;
use weft_core::config::Action;
use weft_core::pane_layout::{FocusDirection, SplitDirection};

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
                let effects = self.close_active_tab_with_confirmation();
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
            // v1.3 Batch 3: pane split / focus / close. The split tree and
            // per-pane PTY/Terminal state are wired up; the multi-pane
            // renderer (Batch 5) will make splits visible. Until then,
            // splits "work" (panes exist, focus cycles, PTYs run) but only
            // the active pane is drawn — the inactive panes keep their
            // terminals updated in the background via pump_pty /
            // process_messages on the active tab's Deref path.
            Action::SplitHorizontal | Action::SplitVertical => {
                let direction = match action {
                    Action::SplitHorizontal => SplitDirection::Horizontal,
                    Action::SplitVertical => SplitDirection::Vertical,
                    // Unreachable: the match arm only enters for these two.
                    _ => return true,
                };
                let scrollback = self.config_state.config.scrollback.lines;
                // v1.3.4: Pull the current content rect + cell size so the
                // new pane can be forked at the correct post-split
                // (rows, cols) — otherwise the shell prompt lands at the
                // bottom of a full-viewport grid and the renderer clips it.
                let geo = if let Some(layout) = self.terminal_layout() {
                    crate::tab::PaneSplitGeometry {
                        content_rect: [
                            layout.content.left as f32,
                            layout.content.top as f32,
                            layout.content.right as f32,
                            layout.content.bottom as f32,
                        ],
                        cell_w: layout.cell_width as f32,
                        cell_h: layout.cell_height as f32,
                    }
                } else {
                    // Renderer not ready yet — pass zeros so split_active_pane
                    // falls back to the active pane's current size. The next
                    // recompute_layout() corrects it.
                    crate::tab::PaneSplitGeometry::default()
                };
                let tab = self.sessions.active_mut();
                let old_active = tab.active_pane_id();
                match tab.split_active_pane(direction, 0.5, scrollback, &self.proxy, geo) {
                    Ok(id) => {
                        tracing::info!(?id, ?direction, ?old_active, "pane split");
                        let tab2 = self.sessions.active();
                        tracing::info!(
                            panes = ?tab2.split_tree().panes(),
                            active = ?tab2.active_pane_id(),
                            "split tree state after split"
                        );
                        // Apply theme palette to the new pane's terminal so
                        // it matches the window's renderer theme (same as
                        // new_tab does). The atlas is shared per-window.
                        // Nested `if let` keeps the disjoint-field borrows
                        // (`self.sessions` mut, `self.renderer` imm) visible
                        // to the borrow checker.
                        if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                            if let Some(r) = self.renderer.as_ref() {
                                t.set_palette(r.theme().palette);
                            }
                        }
                        // v1.3.4: New pane is already forked at the right
                        // size. recompute_layout() still runs to queue the
                        // matching SIGWINCH + resize the original pane.
                        self.recompute_layout();
                        self.request_redraw();
                    }
                    Err(e) => {
                        tracing::warn!(?e, ?action, "pane split failed");
                    }
                }
                true
            }
            Action::FocusNextPane => {
                if let Some(id) = self.sessions.active_mut().focus_next_pane() {
                    tracing::info!(?id, "focus next pane");
                    self.refresh_find_for_active_tab();
                    self.request_redraw();
                }
                true
            }
            Action::FocusPrevPane => {
                if let Some(id) = self.sessions.active_mut().focus_prev_pane() {
                    tracing::info!(?id, "focus prev pane");
                    self.refresh_find_for_active_tab();
                    self.request_redraw();
                }
                true
            }
            Action::ClosePane => {
                if close_pane_disposition(self.sessions.active().pane_count())
                    == ClosePaneDisposition::CloseTab
                {
                    // Preserve the final pane's Terminal until close_tab()
                    // settles and persists any pending block output.
                    let effects = self.close_active_tab_with_confirmation();
                    self.drain_effects(effects);
                    return true;
                }
                if !self.confirm_active_pane_close() {
                    return true;
                }
                match self.sessions.active_mut().close_active_pane() {
                    Ok(false) => {
                        tracing::info!("pane closed, tab still has panes");
                        self.recompute_layout();
                        self.refresh_find_for_active_tab();
                        self.request_redraw();
                    }
                    Ok(true) => tracing::error!("multi-pane close unexpectedly emptied tab"),
                    Err(e) => {
                        tracing::warn!(?e, "close pane failed");
                    }
                }
                true
            }
            Action::TogglePaneZoom => {
                let was_zoomed = self.sessions.active().is_zoomed();
                let zoomed = self.sessions.active_mut().toggle_pane_zoom();
                if was_zoomed == zoomed.is_some() {
                    // No state change (single-pane tree or empty) — skip
                    // the layout work, just consume the key.
                    return true;
                }
                if let Some(id) = zoomed {
                    tracing::info!(?id, "pane zoomed in");
                } else {
                    tracing::info!("pane zoom toggled off");
                }
                // Layout must be recomputed so the grid/PTY sizing reflects
                // the new single-pane (or restored multi-pane) geometry.
                self.recompute_layout();
                self.refresh_find_for_active_tab();
                self.request_redraw();
                true
            }
            Action::FocusPaneUp => self.focus_direction_pane(FocusDirection::Up),
            Action::FocusPaneDown => self.focus_direction_pane(FocusDirection::Down),
            Action::FocusPaneLeft => self.focus_direction_pane(FocusDirection::Left),
            Action::FocusPaneRight => self.focus_direction_pane(FocusDirection::Right),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosePaneDisposition {
    ClosePane,
    CloseTab,
}

fn close_pane_disposition(pane_count: usize) -> ClosePaneDisposition {
    if pane_count <= 1 {
        ClosePaneDisposition::CloseTab
    } else {
        ClosePaneDisposition::ClosePane
    }
}

impl crate::App {
    /// v1.3.3: Move focus to the nearest pane in `dir`, based on the
    /// current content rect. Falls back to a no-op redraw when the layout
    /// can't be computed (e.g. window not yet ready) — better to silently
    /// ignore the key than to panic on an early key event.
    fn focus_direction_pane(&mut self, dir: FocusDirection) -> bool {
        let Some(layout) = self.terminal_layout() else {
            return true; // Consume the key even if we can't act on it yet.
        };
        let content_rect: weft_core::pane_layout::Rect = [
            layout.content.left as f32,
            layout.content.top as f32,
            layout.content.right as f32,
            layout.content.bottom as f32,
        ];
        if let Some(id) = self
            .sessions
            .active_mut()
            .focus_direction_pane(dir, content_rect)
        {
            tracing::info!(?id, ?dir, "focus direction pane");
            self.refresh_find_for_active_tab();
        }
        self.request_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{close_pane_disposition, ClosePaneDisposition};

    #[test]
    fn contextual_close_preserves_last_pane_for_tab_finalization() {
        assert_eq!(close_pane_disposition(1), ClosePaneDisposition::CloseTab);
        assert_eq!(close_pane_disposition(2), ClosePaneDisposition::ClosePane);
    }
}
