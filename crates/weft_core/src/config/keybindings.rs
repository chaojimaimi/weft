use std::collections::HashMap;

use crate::input::{KeyCode, Modifiers};

use super::action::Action;
use super::parsers::parse_binding;

/// Resolved keybinding table: physical key + modifiers → action.
#[derive(Clone, Debug)]
pub struct KeyBindings {
    pub map: HashMap<(KeyCode, Modifiers), Action>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        // Sensible defaults; user config merges onto (overrides) these.
        let pairs: &[(&str, Action)] = &[
            ("cmd+c", Action::Copy),
            ("cmd+v", Action::Paste),
            // v0.9 fix: Cmd+Shift+V also pastes (common terminal convention;
            // matches macOS "Paste and Match Style" habit).
            ("cmd+shift+v", Action::Paste),
            ("cmd+shift+comma", Action::ReloadConfig),
            ("shift+page_up", Action::ScrollPageUp),
            ("shift+page_down", Action::ScrollPageDown),
            ("cmd+up", Action::ScrollLineUp),
            ("cmd+down", Action::ScrollLineDown),
            ("cmd+home", Action::ScrollToTop),
            ("cmd+end", Action::ScrollToBottom),
            ("cmd+shift+b", Action::ToggleBlockPanel),
            ("cmd+p", Action::ToggleCommandPalette),
            ("cmd+equals", Action::ZoomIn),
            ("cmd+minus", Action::ZoomOut),
            ("cmd+0", Action::ZoomReset),
            ("cmd+f", Action::FindInGrid),
            ("cmd+shift+t", Action::ToggleTheme),
            // v0.9 H1: tab management shortcuts.
            ("cmd+t", Action::NewTab),
            // Close the focused pane when split, falling through to tab close
            // when it is the tab's last pane. Cmd+Ctrl+W always closes the tab.
            ("cmd+w", Action::ClosePane),
            ("cmd+ctrl+w", Action::CloseTab),
            ("cmd+shift+right_bracket", Action::NextTab),
            ("cmd+shift+left_bracket", Action::PrevTab),
            // v1.0 S1: Settings panel (macOS-standard Cmd+,).
            ("cmd+comma", Action::ToggleSettings),
            // v1.3: pane splits. Cmd+D splits the active pane into left/right;
            // Cmd+Shift+D splits into top/bottom. Matches tmux's prefix+% / +"|"
            // mental model but without the prefix, mirroring IDE defaults
            // (VSCode: Cmd+\, iTerm2: Cmd+D / Cmd+Shift+D).
            ("cmd+d", Action::SplitVertical),
            ("cmd+shift+d", Action::SplitHorizontal),
            // v1.3: pane focus cycling. Cmd+Option+] / [ mirrors the existing
            // Cmd+Shift+] / [ tab-switching shape, with Option as the "within
            // tab" modifier. These are the cyclic (declaration-order) bindings.
            ("cmd+alt+right_bracket", Action::FocusNextPane),
            ("cmd+alt+left_bracket", Action::FocusPrevPane),
            // v1.3.3: spatial direction focus. Cmd+Alt+Arrow now picks the
            // nearest pane in that direction (replacing the v1.3.0 cyclic
            // binding on the same keys — cyclic order is still on the bracket
            // keys above). Up / Down are new; Left / Right were previously
            // bound to FocusNextPane/FocusPrevPane.
            ("cmd+alt+up", Action::FocusPaneUp),
            ("cmd+alt+down", Action::FocusPaneDown),
            ("cmd+alt+right", Action::FocusPaneRight),
            ("cmd+alt+left", Action::FocusPaneLeft),
            // v1.3.3: zoom the active pane to fill the viewport. Cmd+Shift+Return
            // matches iTerm2's "toggle fullscreen on current pane" shortcut.
            // The same key toggles back to the prior layout.
            ("cmd+shift+return", Action::TogglePaneZoom),
        ];
        let mut map = HashMap::new();
        for (binding, action) in pairs {
            if let Some((k, m)) = parse_binding(binding) {
                map.insert((k, m), *action);
            }
        }
        Self { map }
    }
}

impl KeyBindings {
    /// Build from user overrides merged onto the defaults. Unparseable
    /// bindings are skipped (with a `warn!`).
    pub fn from_overrides(overrides: &HashMap<String, Action>) -> Self {
        let mut kb = Self::default();
        for (binding, action) in overrides {
            match parse_binding(binding) {
                Some((k, m)) => {
                    kb.map.insert((k, m), *action);
                }
                None => {
                    tracing::warn!(binding, "skipping unparseable keybinding");
                }
            }
        }
        kb
    }

    /// Look up the action for a key + modifier combo.
    pub fn lookup(&self, key: KeyCode, mods: Modifiers) -> Option<Action> {
        self.map.get(&(key, mods)).copied()
    }
}
