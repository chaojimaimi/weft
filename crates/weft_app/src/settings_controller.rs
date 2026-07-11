//! Settings overlay controller extracted from the application shell.

use super::*;

impl App {
    /// v1.0 S1: Handle a key while the Settings panel is open. Returns
    /// true if consumed. Modal — captures all non-modifier-chord keys so
    /// the panel owns keyboard input while visible.
    ///
    /// Key map:
    /// - `Esc` → close without saving (discard draft)
    /// - `Tab` → cycle to next tab
    /// - `↑` / `↓` → navigate selection within the active tab
    /// - `Enter` → apply selected row (e.g. pick a theme); marks the draft
    ///   dirty; does NOT close the panel
    /// - `Cmd+Enter` → save draft to disk & close
    /// - other chords with Cmd/Ctrl/Alt fall through to keybindings
    pub(super) fn handle_settings_key(
        &mut self,
        key: KeyCode,
        mods: Modifiers,
        _text: Option<&str>,
    ) -> bool {
        use crate::overlay::SettingsTab;

        // Cmd+Enter: save draft to disk & close.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Enter {
            self.save_settings_draft(true);
            self.request_redraw();
            return true;
        }

        // Let other modifier chords fall through (so Cmd+, can toggle
        // closed, Cmd+Q still quits, etc.).
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }

        match key {
            KeyCode::Escape => {
                // Close without saving (discard draft).
                self.settings.open = false;
                self.settings.error = None;
                self.request_redraw();
                true
            }
            KeyCode::Tab => {
                // Cycle to the next tab (wraps around).
                let tabs = SettingsTab::ALL;
                let idx = tabs
                    .iter()
                    .position(|t| *t == self.settings.tab)
                    .unwrap_or(0);
                self.settings.tab = tabs[(idx + 1) % tabs.len()];
                self.settings.selection = 0;
                self.settings.scroll_offset = 0;
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                // v1.0 fix: wrap-around selection so users can cycle through
                // all rows with arrow keys alone (no End/Home needed).
                let max = self.settings_tab_row_count();
                if max > 0 {
                    self.settings.selection = (self.settings.selection + max - 1) % max;
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                // v1.0 fix: wrap-around selection (Down at bottom → top).
                let max = self.settings_tab_row_count();
                if max > 0 {
                    self.settings.selection = (self.settings.selection + 1) % max;
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // Apply the selected row in the active tab to the draft.
                self.apply_settings_selection();
                self.request_redraw();
                true
            }
            // v1.0 S1-c: ←/→ nudges the value of the selected row in the
            // Font / Window tabs (no-op in Appearance/Keybindings which are
            // pick-list tabs). The step sizes and clamps match what the
            // renderer's footer hint advertises ("←→ adjust").
            KeyCode::Left | KeyCode::Right => {
                let delta = if key == KeyCode::Left { -1 } else { 1 };
                self.adjust_settings_value(delta);
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// v1.0 S2: Persist the working draft to disk and reload the live config
    /// so theme/font/padding changes take effect immediately. When `close`
    /// is true the panel is dismissed; the Apply button passes false so the
    /// user can keep editing. Failures (e.g. unset `HOME`, read-only config
    /// dir) are surfaced via [`settings_error`] instead of just being logged.
    pub(super) fn save_settings_draft(&mut self, close: bool) {
        if self.settings.dirty {
            if let Err(e) = self.settings.draft.save() {
                tracing::warn!(error = ?e, "failed to save settings draft");
                self.settings.error = Some(e.to_string());
                // Don't close on failure — the user needs to see the error.
                return;
            }
            // Apply the new config immediately so theme/font changes
            // take effect without a restart. reload_config() reads the
            // freshly-saved file and calls apply_config(), which
            // rebuilds the renderer theme + atlas.
            self.reload_config();
            self.settings.dirty = false;
            self.settings.error = None;
        }
        if close {
            self.settings.open = false;
            self.settings.error = None;
        }
    }

    /// v1.0 S1-c: Adjust the value of the currently-selected row in the
    /// active tab by `delta` (±1). Font/Window rows map to numeric fields;
    /// the Font family row cycles through a fixed list of common macOS
    /// monospace families. The Logo tab cycles the Dock icon variant.
    /// No-op for Appearance/Keybindings (pick-list tabs).
    pub(super) fn adjust_settings_value(&mut self, delta: i32) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Font => match self.settings.selection {
                0 => {
                    // Family: cycle Menlo → Monaco → SF Mono → System → Menlo.
                    const FAMILIES: &[&str] = &["Menlo", "Monaco", "SF Mono", "System"];
                    let cur = FAMILIES
                        .iter()
                        .position(|f| *f == self.settings.draft.font.family)
                        .unwrap_or(0);
                    let next = (cur as i32 + delta).rem_euclid(FAMILIES.len() as i32) as usize;
                    self.settings.draft.font.family = FAMILIES[next].to_string();
                    self.settings.dirty = true;
                }
                1 => {
                    // Size: ±0.5 pt, clamped to [8.0, 24.0].
                    self.settings.draft.font.size =
                        (self.settings.draft.font.size + delta as f32 * 0.5).clamp(8.0, 24.0);
                    self.settings.dirty = true;
                }
                2 => {
                    // Line height: ±0.05, clamped to [1.0, 1.5].
                    self.settings.draft.font.line_height = (self.settings.draft.font.line_height
                        + delta as f32 * 0.05)
                        .clamp(1.0, 1.5);
                    self.settings.dirty = true;
                }
                _ => {}
            },
            SettingsTab::Window => match self.settings.selection {
                0 => {
                    // Opacity: ±0.05, clamped to [0.5, 1.0].
                    self.settings.draft.window.opacity =
                        (self.settings.draft.window.opacity + delta as f32 * 0.05).clamp(0.5, 1.0);
                    self.settings.dirty = true;
                }
                1 => {
                    // Padding X: ±1 cell, clamped to [0, 20].
                    self.settings.draft.window.padding_x =
                        ((self.settings.draft.window.padding_x as i32 + delta).max(0) as u32)
                            .min(20);
                    self.settings.dirty = true;
                }
                2 => {
                    // Padding Y: ±1 cell, clamped to [0, 20].
                    self.settings.draft.window.padding_y =
                        ((self.settings.draft.window.padding_y as i32 + delta).max(0) as u32)
                            .min(20);
                    self.settings.dirty = true;
                }
                3 => {
                    // Scrollback: ±1000 lines, clamped to [1000, 100000].
                    self.settings.draft.scrollback.lines =
                        ((self.settings.draft.scrollback.lines as i32 + delta * 1000).max(1000)
                            as usize)
                            .min(100000);
                    self.settings.dirty = true;
                }
                _ => {}
            },
            SettingsTab::Logo => {
                if self.settings.selection == 0 {
                    // Variant: cycle Cool → Warm → Light → Transparent → Cool.
                    let variants = weft_core::config::LogoVariant::ALL;
                    let cur = variants
                        .iter()
                        .position(|v| *v == self.settings.draft.logo.variant)
                        .unwrap_or(0);
                    let next = (cur as i32 + delta).rem_euclid(variants.len() as i32) as usize;
                    self.settings.draft.logo.variant = variants[next];
                    self.settings.dirty = true;
                }
            }
            SettingsTab::Appearance | SettingsTab::Keybindings => {
                // Pick-list tabs — ←/→ has no meaning here.
            }
        }
    }

    /// v1.0 S1: Number of selectable rows in the active Settings tab.
    pub(super) fn settings_tab_row_count(&self) -> usize {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => self.settings_theme_views().len(),
            // v1.0 S1-d: must match the rows rendered by build_settings_vertices
            // (Family / Size / Line height). Out-of-sync values let ↑↓ walk
            // past the rendered rows.
            SettingsTab::Font => 3,
            SettingsTab::Keybindings => self.settings_keybinding_views().len(),
            // v1.0 S1-d: Opacity / Padding X / Padding Y / Scrollback — match
            // the four rows rendered by build_settings_vertices.
            SettingsTab::Window => 4,
            // v1.0 Logo: single row (Variant) — ←/→ cycles the value.
            SettingsTab::Logo => 1,
        }
    }

    /// v1.0 S1: Apply the currently-selected row in the active tab to the
    /// draft. In v1.0 only the Appearance tab is interactive (theme
    /// selection); the other tabs are read-only display.
    pub(super) fn apply_settings_selection(&mut self) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                if let Some(view) = self
                    .settings_theme_views()
                    .get(self.settings.selection)
                    .copied()
                {
                    if self.settings.draft.theme.name != view.name {
                        self.settings.draft.theme.name = view.name.to_string();
                        // Disable follow_system so the user's explicit theme
                        // choice takes precedence over the system appearance.
                        // Otherwise apply_config would ignore `name` and pick
                        // the theme from system light/dark mode.
                        self.settings.draft.theme.follow_system = false;
                        // v1.0: remember the user's preferred dark theme so
                        // Cmd+Shift+T can toggle back to it. If the selected
                        // theme is dark (name doesn't contain "light"), update
                        // preferred_dark_theme.
                        if !view.name.contains("light") {
                            self.config_state.preferred_dark_theme = view.name.to_string();
                        }
                        self.settings.dirty = true;
                    }
                }
            }
            SettingsTab::Font
            | SettingsTab::Keybindings
            | SettingsTab::Window
            | SettingsTab::Logo => {
                // Read-only from Enter — ←/→ handles adjustments instead.
            }
        }
    }

    /// v1.0 S1: The list of built-in themes for the Appearance tab. The
    /// order matches `Theme::resolve_named`'s match arms so the list
    /// stays in sync with the resolver.
    pub(super) fn settings_theme_views(&self) -> Vec<crate::overlay::SettingsThemeView> {
        use crate::overlay::SettingsThemeView;
        vec![
            SettingsThemeView {
                name: "weft-warm",
                label: "Weft Warm (default)",
            },
            SettingsThemeView {
                name: "weft-light",
                label: "Weft Light",
            },
            SettingsThemeView {
                name: "warp",
                label: "Warp Dark",
            },
            SettingsThemeView {
                name: "dracula",
                label: "Dracula",
            },
            SettingsThemeView {
                name: "solarized-dark",
                label: "Solarized Dark",
            },
            SettingsThemeView {
                name: "gruvbox-dark",
                label: "Gruvbox Dark",
            },
            SettingsThemeView {
                name: "nord",
                label: "Nord",
            },
            SettingsThemeView {
                name: "tokyo-night",
                label: "Tokyo Night",
            },
            SettingsThemeView {
                name: "catppuccin",
                label: "Catppuccin Mocha",
            },
            SettingsThemeView {
                name: "one-dark",
                label: "One Dark",
            },
            SettingsThemeView {
                name: "monokai-pro",
                label: "Monokai Pro",
            },
        ]
    }

    /// v1.0 S1: The list of keybinding rows for the Keybindings tab
    /// (read-only display). Built from the resolved keybinding table.
    pub(super) fn settings_keybinding_views(&self) -> Vec<crate::overlay::SettingsKeybindingView> {
        use crate::overlay::SettingsKeybindingView;
        // Reverse-lookup: action → chord. The keybindings map is
        // (KeyCode, Modifiers) → Action, so we iterate and collect.
        let mut views = Vec::new();
        for (chord, action) in &self.config_state.keybindings.map {
            let label = match action {
                weft_core::config::Action::Copy => "Copy",
                weft_core::config::Action::Paste => "Paste",
                weft_core::config::Action::ReloadConfig => "Reload Config",
                weft_core::config::Action::ScrollPageUp => "Scroll Page Up",
                weft_core::config::Action::ScrollPageDown => "Scroll Page Down",
                weft_core::config::Action::ScrollLineUp => "Scroll Line Up",
                weft_core::config::Action::ScrollLineDown => "Scroll Line Down",
                weft_core::config::Action::ScrollToTop => "Scroll To Top",
                weft_core::config::Action::ScrollToBottom => "Scroll To Bottom",
                weft_core::config::Action::ToggleBlockPanel => "Toggle Block Panel",
                weft_core::config::Action::ToggleCommandPalette => "Command Palette",
                weft_core::config::Action::ZoomIn => "Zoom In",
                weft_core::config::Action::ZoomOut => "Zoom Out",
                weft_core::config::Action::ZoomReset => "Zoom Reset",
                weft_core::config::Action::FindInGrid => "Find In Grid",
                weft_core::config::Action::ToggleTheme => "Toggle Theme",
                weft_core::config::Action::NewTab => "New Tab",
                weft_core::config::Action::CloseTab => "Close Tab",
                weft_core::config::Action::NextTab => "Next Tab",
                weft_core::config::Action::PrevTab => "Previous Tab",
                weft_core::config::Action::ToggleSettings => "Settings",
            };
            views.push(SettingsKeybindingView {
                action: label.to_string(),
                binding: chord_label(chord.0, chord.1),
            });
        }
        // Sort by action label for stable display.
        views.sort_by(|a, b| a.action.cmp(&b.action));
        views
    }
}
