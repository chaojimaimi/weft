//! Main-thread execution for typed native accessibility actions.

use super::*;
use crate::accessibility_model::AccessibilityAction;

fn palette_theme_action(
    open: bool,
    palette: &crate::palette_state::PaletteState,
    name: &str,
    theme_cfg: &weft_core::config::ThemeConfig,
) -> Option<bool> {
    (open && palette.theme_picker_contains(name))
        .then(|| crate::palette_state::theme_is_dark(name, theme_cfg))
}

impl App {
    pub(super) fn perform_accessibility_action(&mut self, action: AccessibilityAction) {
        match action {
            AccessibilityAction::PressPoint { x, y } => {
                self.handle_mouse_press(x, y, winit::event::MouseButton::Left);
            }
            AccessibilityAction::NewTab => {
                self.new_tab();
                self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
            }
            AccessibilityAction::SwitchSession(session_id) => {
                let Some(index) = self.sessions.tab_index_by_session_id(session_id) else {
                    return;
                };
                // rust-reviewer P3: AX switching rides the same indicator
                // slide as click/next/prev — the slide starts after the
                // scroll settles (a local captures the pre-switch index;
                // no state field needed).
                let mut slide: Option<(usize, usize)> = None;
                if self.sessions.active_idx() != index {
                    self.reset_ime_context("accessibility tab action");
                    let prev_active = self.sessions.active_idx();
                    self.sessions.switch_to(index);
                    self.refresh_find_for_active_tab();
                    slide = Some((prev_active, index));
                }
                self.tab_bar.hovered_tab = None;
                self.interaction.block_hovered = None;
                self.interaction.block_selected = None;
                self.interaction.block_action_hovered = None;
                self.scroll_active_tab_into_view();
                if let Some((from, to)) = slide.take() {
                    self.begin_tab_indicator_slide(from, to);
                }
                self.request_redraw();
            }
            AccessibilityAction::ContextMenuItem { session_id, index } => {
                let Some(menu) = self.interaction.context_menu.as_ref() else {
                    return;
                };
                if menu.session_id != session_id
                    || !self
                        .sessions
                        .active()
                        .is_some_and(|tab| tab.session_id == session_id)
                    || index >= CONTEXT_MENU_ITEMS.len()
                {
                    return;
                }
                if let Some(menu) = self.take_context_menu("accessibility context menu action") {
                    self.run_context_action(menu.block_id, CONTEXT_MENU_ITEMS[index].1);
                    self.request_redraw();
                }
            }
            AccessibilityAction::PaletteEntry(key) => {
                // rust-reviewer P2-3: a Closing palette has already handed
                // input off — activating an entry mid-fade would run a
                // stale command (interactive gate, T1 contract).
                if !self.palette.interactive()
                    || !matches!(self.palette.submode, PaletteSubMode::Search)
                {
                    return;
                }
                let entry = self
                    .palette
                    .results
                    .iter()
                    .find(|entry| entry.accessibility_key() == key)
                    .cloned();
                if let Some(entry) = entry {
                    self.activate_palette_entry(entry);
                }
            }
            AccessibilityAction::PaletteTheme(name) => {
                // rust-reviewer P2-3: Closing 期不得应用主题——
                // palette_theme_action 通过后会真实 apply_theme_by_name。
                let Some(dark) = palette_theme_action(
                    self.palette.interactive(),
                    &self.palette,
                    &name,
                    &self.config_state.config.theme,
                ) else {
                    return;
                };
                self.apply_theme_by_name(&name, dark);
                self.close_palette();
                self.request_redraw();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::palette_theme_action;
    use crate::palette_state::{PaletteState, PaletteSubMode};

    #[test]
    fn palette_theme_action_requires_open_current_filtered_theme() {
        let cfg = weft_core::config::ThemeConfig::default();
        let mut palette = PaletteState::new();
        palette.open = true;
        palette.submode = PaletteSubMode::SelectTheme {
            buffer: "light".into(),
            themes: vec!["weft-light".into(), "weft-warm".into()],
        };
        assert_eq!(
            palette_theme_action(true, &palette, "weft-light", &cfg),
            Some(false)
        );
        assert_eq!(
            palette_theme_action(true, &palette, "weft-warm", &cfg),
            None
        );
        assert_eq!(
            palette_theme_action(false, &palette, "weft-light", &cfg),
            None
        );

        palette.submode = PaletteSubMode::Search;
        assert_eq!(
            palette_theme_action(true, &palette, "weft-light", &cfg),
            None
        );
    }
}
