//! Main-thread execution for typed native accessibility actions.

use super::*;
use crate::accessibility_model::AccessibilityAction;

fn palette_theme_action(
    open: bool,
    palette: &crate::palette_state::PaletteState,
    name: &str,
) -> Option<bool> {
    (open && palette.theme_picker_contains(name))
        .then(|| crate::palette_state::theme_name_is_dark(name))
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
                if self.sessions.active_idx() != index {
                    self.reset_ime_context("accessibility tab action");
                    self.sessions.switch_to(index);
                    self.refresh_find_for_active_tab();
                }
                self.tab_bar.hovered_tab = None;
                self.interaction.block_hovered = None;
                self.interaction.block_selected = None;
                self.interaction.block_action_hovered = None;
                self.scroll_active_tab_into_view();
                self.request_redraw();
            }
            AccessibilityAction::ContextMenuItem { session_id, index } => {
                let Some(menu) = self.interaction.context_menu.as_ref() else {
                    return;
                };
                if menu.session_id != session_id
                    || self.sessions.active().session_id != session_id
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
                if !self.palette.open || !matches!(self.palette.submode, PaletteSubMode::Search) {
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
                let Some(dark) = palette_theme_action(self.palette.open, &self.palette, &name)
                else {
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
        let mut palette = PaletteState::new();
        palette.open = true;
        palette.submode = PaletteSubMode::SelectTheme {
            buffer: "light".into(),
            themes: vec!["weft-light".into(), "weft-warm".into()],
        };
        assert_eq!(
            palette_theme_action(true, &palette, "weft-light"),
            Some(false)
        );
        assert_eq!(palette_theme_action(true, &palette, "weft-warm"), None);
        assert_eq!(palette_theme_action(false, &palette, "weft-light"), None);

        palette.submode = PaletteSubMode::Search;
        assert_eq!(palette_theme_action(true, &palette, "weft-light"), None);
    }
}
