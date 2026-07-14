//! Settings overlay controller extracted from the application shell.
//!
//! F5: Updated for split sidebar layout with 6 categories (Appearance,
//! Terminal, Input, Keybindings, Window, Advanced). Logo and Font merged
//! into Appearance. Adds keybinding conflict detection, field-level
//! validation, and narrow-window drill-down navigation.

use super::*;
use std::collections::{HashMap, HashSet};

impl App {
    pub(super) fn handle_settings_mouse_press(&mut self, x: f32, y: f32) {
        use crate::settings_component::SettingsTarget;

        match self.settings_target_at(x, y) {
            Some(SettingsTarget::SidebarCategory(tab)) => {
                if self.settings.tab != tab {
                    self.settings.tab = tab;
                    self.settings.selection = 0;
                    self.settings.scroll_offset = 0;
                    if self.settings_is_narrow() {
                        self.settings.drill_down = true;
                    }
                }
                self.request_redraw();
            }
            Some(SettingsTarget::Theme(i)) => {
                self.settings.selection = i;
                self.apply_settings_selection();
                self.request_redraw();
            }
            Some(SettingsTarget::CloseButton) => {
                self.close_settings();
                self.request_redraw();
            }
            Some(SettingsTarget::SaveButton) => {
                self.save_settings_draft(true);
                self.request_redraw();
            }
            Some(SettingsTarget::ApplyButton) => {
                self.save_settings_draft(false);
                self.request_redraw();
            }
            None if !self.point_inside_settings_box(x, y) => {
                self.close_settings();
                self.request_redraw();
            }
            None => {}
        }
    }

    /// F5: Handle a key while the Settings panel is open. Returns true if
    /// consumed. Modal — captures all non-modifier-chord keys so the panel
    /// owns keyboard input while visible.
    ///
    /// Navigation model:
    /// - **Wide mode** (split layout): Tab cycles sidebar categories; ↑↓
    ///   navigates within the content form; Enter applies; ←/→ adjusts.
    /// - **Narrow mode** (drill-down): When showing sidebar, ↑↓ navigates
    ///   categories and Enter drills into one. When showing content, ↑↓
    ///   navigates the form and Esc returns to the sidebar.
    /// - `Esc` closes the panel (from sidebar in narrow mode, or from wide
    ///   mode). In narrow content mode, Esc first returns to the sidebar.
    /// - `Cmd+Enter` saves draft to disk & closes.
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

        let is_narrow = self.settings_is_narrow();

        match key {
            KeyCode::Escape => {
                if is_narrow && self.settings.drill_down {
                    // Narrow content mode: Esc returns to sidebar.
                    self.settings.drill_down = false;
                    self.settings.selection = 0;
                    self.request_redraw();
                    true
                } else {
                    // Wide mode or narrow sidebar mode: close without saving.
                    self.close_settings();
                    self.request_redraw();
                    true
                }
            }
            KeyCode::Tab => {
                // Cycle to the next sidebar category (wraps around).
                let tabs = SettingsTab::ALL;
                let idx = tabs
                    .iter()
                    .position(|t| *t == self.settings.tab)
                    .unwrap_or(0);
                self.settings.tab = tabs[(idx + 1) % tabs.len()];
                self.settings.selection = 0;
                self.settings.scroll_offset = 0;
                if is_narrow {
                    // Tab always drills into the content view.
                    self.settings.drill_down = true;
                }
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                if is_narrow && !self.settings.drill_down {
                    // Narrow sidebar mode: navigate categories.
                    let max = SettingsTab::ALL.len();
                    if max > 0 {
                        let idx = SettingsTab::ALL
                            .iter()
                            .position(|t| *t == self.settings.tab)
                            .unwrap_or(0);
                        let new_idx = (idx + max - 1) % max;
                        self.settings.tab = SettingsTab::ALL[new_idx];
                    }
                } else {
                    // Content mode: navigate rows with wrap-around.
                    let max = self.settings_tab_row_count();
                    if max > 0 {
                        self.settings.selection = (self.settings.selection + max - 1) % max;
                    }
                }
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                if is_narrow && !self.settings.drill_down {
                    // Narrow sidebar mode: navigate categories.
                    let max = SettingsTab::ALL.len();
                    if max > 0 {
                        let idx = SettingsTab::ALL
                            .iter()
                            .position(|t| *t == self.settings.tab)
                            .unwrap_or(0);
                        let new_idx = (idx + 1) % max;
                        self.settings.tab = SettingsTab::ALL[new_idx];
                    }
                } else {
                    // Content mode: navigate rows with wrap-around.
                    let max = self.settings_tab_row_count();
                    if max > 0 {
                        self.settings.selection = (self.settings.selection + 1) % max;
                    }
                }
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                if is_narrow && !self.settings.drill_down {
                    // Narrow sidebar mode: Enter drills into the selected category.
                    self.settings.drill_down = true;
                    self.settings.selection = 0;
                    self.request_redraw();
                    true
                } else {
                    // Content mode: apply the selected row.
                    self.apply_settings_selection();
                    self.request_redraw();
                    true
                }
            }
            // ←/→ nudges the value of the selected row (no-op for pick-list
            // categories like Appearance themes and Keybindings).
            KeyCode::Left | KeyCode::Right => {
                let delta = if key == KeyCode::Left { -1 } else { 1 };
                self.adjust_settings_value(delta);
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// F5: Detect whether the viewport is narrow (<640pt logical). The
    /// settings panel switches to single-column drill-down mode.
    pub(super) fn settings_is_narrow(&self) -> bool {
        let Some(renderer) = &self.renderer else {
            return false;
        };
        let (vp_w, _) = renderer.viewport();
        let scale = renderer.scale() as f32;
        if scale <= 0.0 {
            return false;
        }
        let logical_w = vp_w / scale;
        logical_w < crate::layout::SETTINGS_NARROW_THRESHOLD
    }

    /// v1.0 S2: Persist the working draft to disk and reload the live config
    /// so theme/font/padding changes take effect immediately. When `close`
    /// is true the panel is dismissed; the Apply button passes false so the
    /// user can keep editing. Failures (e.g. unset `HOME`, read-only config
    /// dir) are surfaced via [`settings_error`] instead of just being logged.
    ///
    /// F5: Runs field-level validation before saving. If validation fails,
    /// `field_errors` is populated and the save is aborted.
    pub(super) fn save_settings_draft(&mut self, close: bool) {
        // F5: validate before saving.
        self.settings.field_errors = validate_settings(&self.settings.draft);
        if !self.settings.field_errors.is_empty() {
            self.settings.error = Some(format!(
                "{} field(s) need attention",
                self.settings.field_errors.len()
            ));
            return;
        }

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
            self.close_settings();
        }
    }

    /// F5: Adjust the value of the currently-selected row in the active
    /// category by `delta` (±1). Numeric rows map to config fields; the
    /// Font family / Logo variant rows cycle through fixed lists.
    pub(super) fn adjust_settings_value(&mut self, delta: i32) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                let theme_count = self.settings_theme_views().len();
                let row = self.settings.selection;
                // Rows 0..theme_count are the theme pick-list (←/→ no-op).
                if row < theme_count {
                    return;
                }
                let r = row - theme_count;
                match r {
                    0 => {
                        // Logo Variant: cycle Cool → Warm → Light → Transparent.
                        let variants = weft_core::config::LogoVariant::ALL;
                        let cur = variants
                            .iter()
                            .position(|v| *v == self.settings.draft.logo.variant)
                            .unwrap_or(0);
                        let next = (cur as i32 + delta).rem_euclid(variants.len() as i32) as usize;
                        self.settings.draft.logo.variant = variants[next];
                        self.settings.dirty = true;
                    }
                    1 => {
                        // Font Family: cycle Menlo → Monaco → SF Mono → System.
                        const FAMILIES: &[&str] = &["Menlo", "Monaco", "SF Mono", "System"];
                        let cur = FAMILIES
                            .iter()
                            .position(|f| *f == self.settings.draft.font.family)
                            .unwrap_or(0);
                        let next = (cur as i32 + delta).rem_euclid(FAMILIES.len() as i32) as usize;
                        self.settings.draft.font.family = FAMILIES[next].to_string();
                        self.settings.dirty = true;
                    }
                    2 => {
                        // Font Size: ±0.5 pt, clamped to [8.0, 24.0].
                        self.settings.draft.font.size =
                            (self.settings.draft.font.size + delta as f32 * 0.5).clamp(8.0, 24.0);
                        self.settings.dirty = true;
                    }
                    3 => {
                        // Line height: ±0.05, clamped to [1.0, 1.5].
                        self.settings.draft.font.line_height =
                            (self.settings.draft.font.line_height + delta as f32 * 0.05)
                                .clamp(1.0, 1.5);
                        self.settings.dirty = true;
                    }
                    4 => {
                        // Window opacity: ±0.05, clamped to [0.5, 1.0].
                        self.settings.draft.window.opacity = (self.settings.draft.window.opacity
                            + delta as f32 * 0.05)
                            .clamp(0.5, 1.0);
                        self.settings.dirty = true;
                    }
                    _ => {}
                }
            }
            SettingsTab::Terminal => match self.settings.selection {
                0 => {
                    // Scrollback: ±1000 lines, clamped to [1000, 100000].
                    self.settings.draft.scrollback.lines =
                        ((self.settings.draft.scrollback.lines as i32 + delta * 1000).max(1000)
                            as usize)
                            .min(100000);
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
                _ => {}
            },
            SettingsTab::Input => {
                if self.settings.selection == 0 {
                    // Submit on Ctrl+Enter: toggle.
                    self.settings.draft.editor.submit_on_ctrl_enter =
                        !self.settings.draft.editor.submit_on_ctrl_enter;
                    self.settings.dirty = true;
                }
            }
            SettingsTab::Keybindings => {
                // Read-only list — ←/→ has no meaning here.
            }
            SettingsTab::Window => match self.settings.selection {
                0 => {
                    // Window width: ±20 px, clamped to [400, 4000].
                    self.settings.draft.window.width =
                        ((self.settings.draft.window.width as i32 + delta * 20).max(400) as u32)
                            .min(4000);
                    self.settings.dirty = true;
                }
                1 => {
                    // Window height: ±20 px, clamped to [300, 4000].
                    self.settings.draft.window.height =
                        ((self.settings.draft.window.height as i32 + delta * 20).max(300) as u32)
                            .min(4000);
                    self.settings.dirty = true;
                }
                2 => {
                    // Sidebar width: ±10pt, clamped to [240, 360]. None → start at 280.
                    let cur = self.settings.draft.window.sidebar_width.unwrap_or(280.0);
                    let next = (cur + delta as f32 * 10.0).clamp(
                        weft_core::config::SIDEBAR_MIN_WIDTH,
                        weft_core::config::SIDEBAR_MAX_WIDTH,
                    );
                    self.settings.draft.window.sidebar_width = Some(next);
                    self.settings.dirty = true;
                }
                _ => {}
            },
            SettingsTab::Advanced => {
                // Placeholder rows — no real config backing yet.
            }
        }
    }

    /// F5: Number of selectable rows in the active Settings category.
    pub(super) fn settings_tab_row_count(&self) -> usize {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                // Theme list + Logo Variant + Font Family + Font Size + Line Height + Opacity.
                self.settings_theme_views().len() + 5
            }
            SettingsTab::Terminal => 3, // Scrollback + Padding X + Padding Y.
            SettingsTab::Input => 1,    // Submit on Ctrl+Enter.
            SettingsTab::Keybindings => self.settings_keybinding_views().len(),
            SettingsTab::Window => 3,   // Width + Height + Sidebar Width.
            SettingsTab::Advanced => 2, // Debug Logging + Experimental (placeholders).
        }
    }

    /// F5: Apply the currently-selected row in the active category to the
    /// draft. Only the Appearance theme list is interactive via Enter (theme
    /// selection); all other rows use ←/→ for adjustments.
    pub(super) fn apply_settings_selection(&mut self) {
        use crate::overlay::SettingsTab;
        match self.settings.tab {
            SettingsTab::Appearance => {
                let theme_count = self.settings_theme_views().len();
                if self.settings.selection < theme_count {
                    if let Some(view) = self
                        .settings_theme_views()
                        .get(self.settings.selection)
                        .copied()
                    {
                        if self.settings.draft.theme.name != view.name {
                            self.settings.draft.theme.name = view.name.to_string();
                            self.settings.draft.theme.follow_system = false;
                            if !view.name.contains("light") {
                                self.config_state.preferred_dark_theme = view.name.to_string();
                            }
                            self.settings.dirty = true;
                        }
                    }
                }
                // Non-theme rows: Enter is a no-op (←/→ handles adjustments).
            }
            SettingsTab::Terminal
            | SettingsTab::Input
            | SettingsTab::Keybindings
            | SettingsTab::Window
            | SettingsTab::Advanced => {
                // ←/→ handles adjustments; Enter is a no-op.
            }
        }
    }

    /// v1.0 S1: The list of built-in themes for the Appearance category. The
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

    /// F5: The list of keybinding rows for the Keybindings category. Each
    /// row carries a `conflict` flag set by [`detect_keybinding_conflicts`].
    pub(super) fn settings_keybinding_views(&self) -> Vec<crate::overlay::SettingsKeybindingView> {
        use crate::overlay::SettingsKeybindingView;
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
                conflict: false,
            });
        }
        // F5: detect conflicts and mark conflicting rows.
        let conflicts = detect_keybinding_conflicts(&views);
        for v in &mut views {
            if conflicts.contains(v.binding.as_str()) {
                v.conflict = true;
            }
        }
        // Sort by action label for stable display.
        views.sort_by(|a, b| a.action.cmp(&b.action));
        views
    }
}

// ── F5: Pure helper functions ─────────────────────────────────────────

/// F5: Detect keybinding conflicts — chords bound to more than one action.
/// Returns the set of conflicting chord strings. A chord is a "conflict"
/// when two different actions share the same chord string.
///
/// This is a pure function with no side effects, making it easy to unit-test.
/// In normal operation the keybindings map deduplicates by (KeyCode, Modifiers),
/// so conflicts only arise from UI editing or config aliasing. The function
/// is infrastructure-ready for future keybinding editing.
pub(crate) fn detect_keybinding_conflicts(
    views: &[crate::overlay::SettingsKeybindingView],
) -> HashSet<String> {
    let mut chord_actions: HashMap<&str, HashSet<&str>> = HashMap::new();
    for v in views {
        chord_actions
            .entry(v.binding.as_str())
            .or_default()
            .insert(v.action.as_str());
    }
    chord_actions
        .into_iter()
        .filter(|(_, actions)| actions.len() > 1)
        .map(|(chord, _)| chord.to_string())
        .collect()
}

/// F5: Validate the settings draft before saving. Returns a list of
/// (field_label, error_message) pairs. Empty vec = all valid.
///
/// This is a pure function with no side effects, making it easy to unit-test.
pub(crate) fn validate_settings(config: &weft_core::config::Config) -> Vec<(String, String)> {
    let mut errors = Vec::new();
    if config.font.size < 8.0 || config.font.size > 24.0 {
        errors.push((
            "Font Size".to_string(),
            format!("{} is out of range [8.0, 24.0]", config.font.size),
        ));
    }
    if config.font.line_height < 1.0 || config.font.line_height > 1.5 {
        errors.push((
            "Line Height".to_string(),
            format!("{} is out of range [1.0, 1.5]", config.font.line_height),
        ));
    }
    if config.window.opacity < 0.5 || config.window.opacity > 1.0 {
        errors.push((
            "Window Opacity".to_string(),
            format!("{} is out of range [0.5, 1.0]", config.window.opacity),
        ));
    }
    if config.scrollback.lines < 1000 || config.scrollback.lines > 100_000 {
        errors.push((
            "Scrollback".to_string(),
            format!("{} is out of range [1000, 100000]", config.scrollback.lines),
        ));
    }
    if config.window.width < 400 {
        errors.push((
            "Window Width".to_string(),
            format!("{} is below minimum 400", config.window.width),
        ));
    }
    if config.window.height < 300 {
        errors.push((
            "Window Height".to_string(),
            format!("{} is below minimum 300", config.window.height),
        ));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::SettingsKeybindingView;

    fn kb(action: &str, binding: &str) -> SettingsKeybindingView {
        SettingsKeybindingView {
            action: action.to_string(),
            binding: binding.to_string(),
            conflict: false,
        }
    }

    #[test]
    fn detect_conflicts_empty_list() {
        let views: Vec<SettingsKeybindingView> = vec![];
        let conflicts = detect_keybinding_conflicts(&views);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn detect_conflicts_no_duplicates() {
        let views = vec![
            kb("Copy", "cmd+c"),
            kb("Paste", "cmd+v"),
            kb("Settings", "cmd+comma"),
        ];
        let conflicts = detect_keybinding_conflicts(&views);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn detect_conflicts_same_chord_different_actions() {
        // "cmd+x" bound to both Copy and Cut — that's a conflict.
        let views = vec![
            kb("Copy", "cmd+c"),
            kb("Cut", "cmd+x"),
            kb("Paste", "cmd+x"),
        ];
        let conflicts = detect_keybinding_conflicts(&views);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts.contains("cmd+x"));
    }

    #[test]
    fn detect_conflicts_same_action_different_chords_is_not_conflict() {
        // Paste bound to both cmd+v and cmd+shift+v — NOT a conflict
        // (same action, different chords is fine).
        let views = vec![kb("Paste", "cmd+v"), kb("Paste", "cmd+shift+v")];
        let conflicts = detect_keybinding_conflicts(&views);
        assert!(conflicts.is_empty());
    }

    #[test]
    fn detect_conflicts_multiple_conflicting_chords() {
        let views = vec![
            kb("Copy", "cmd+c"),
            kb("Cut", "cmd+c"),
            kb("Paste", "cmd+v"),
            kb("Close", "cmd+w"),
            kb("New Tab", "cmd+w"),
        ];
        let conflicts = detect_keybinding_conflicts(&views);
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts.contains("cmd+c"));
        assert!(conflicts.contains("cmd+w"));
    }

    #[test]
    fn validate_default_config_is_clean() {
        let config = weft_core::config::Config::default();
        let errors = validate_settings(&config);
        assert!(errors.is_empty(), "default config should have no errors");
    }

    #[test]
    fn validate_font_size_out_of_range() {
        let mut config = weft_core::config::Config::default();
        config.font.size = 30.0;
        let errors = validate_settings(&config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "Font Size");
        assert!(errors[0].1.contains("out of range"));
    }

    #[test]
    fn validate_opacity_out_of_range() {
        let mut config = weft_core::config::Config::default();
        config.window.opacity = 0.1;
        let errors = validate_settings(&config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "Window Opacity");
    }

    #[test]
    fn validate_scrollback_out_of_range() {
        let mut config = weft_core::config::Config::default();
        config.scrollback.lines = 100;
        let errors = validate_settings(&config);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "Scrollback");
    }

    #[test]
    fn validate_multiple_errors_collected() {
        let mut config = weft_core::config::Config::default();
        config.font.size = 1.0;
        config.window.opacity = 0.0;
        config.scrollback.lines = 10;
        let errors = validate_settings(&config);
        assert_eq!(errors.len(), 3);
        let labels: Vec<&str> = errors.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"Font Size"));
        assert!(labels.contains(&"Window Opacity"));
        assert!(labels.contains(&"Scrollback"));
    }

    #[test]
    fn validate_window_dimensions_minimum() {
        let mut config = weft_core::config::Config::default();
        config.window.width = 200;
        config.window.height = 100;
        let errors = validate_settings(&config);
        assert_eq!(errors.len(), 2);
        let labels: Vec<&str> = errors.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"Window Width"));
        assert!(labels.contains(&"Window Height"));
    }
}
