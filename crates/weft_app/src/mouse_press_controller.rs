//! Mouse press routing controller.
//!
//! v1.12.27b (P1-02): `handle_mouse_press` keeps only the cascade skeleton;
//! each cascade level moved verbatim into the `mouse_press/` stage modules
//! (`redraw/` precedent) and reports `PressOutcome::Consumed` where the
//! original had an inline `return` — so the tail `request_redraw` still
//! runs only when every level fell through.

use super::*;
use crate::mouse_press::PressOutcome;

impl App {
    pub(super) fn take_context_menu(&mut self, reason: &'static str) -> Option<ContextMenu> {
        self.interaction.context_menu.as_ref()?;
        self.reset_ime_context(reason);
        let menu = self.interaction.context_menu.take();
        self.clear_prev_focus_if_no_modal();
        menu
    }

    pub(super) fn handle_context_menu_key(&mut self, key: KeyCode, modifiers: Modifiers) -> bool {
        let Some(menu) = self.interaction.context_menu.as_ref() else {
            return false;
        };
        let action = crate::context_menu_component::context_menu_key_action(
            key,
            modifiers,
            menu.selection,
            CONTEXT_MENU_ITEMS.len(),
        );
        match action {
            crate::context_menu_component::ContextMenuKeyAction::Select(selection) => {
                if let Some(menu) = self.interaction.context_menu.as_mut() {
                    menu.selection = selection;
                }
                self.request_redraw();
            }
            crate::context_menu_component::ContextMenuKeyAction::Accept(selection) => {
                if let Some(menu) = self.take_context_menu("context menu accepted") {
                    let action = CONTEXT_MENU_ITEMS[selection].1;
                    self.run_context_action(menu.block_id, action);
                }
                self.request_redraw();
            }
            crate::context_menu_component::ContextMenuKeyAction::Cancel => {
                self.take_context_menu("context menu cancelled");
                self.request_redraw();
            }
            crate::context_menu_component::ContextMenuKeyAction::Consume => {}
        }
        true
    }

    /// v1.12.27b (P1-02): the cascade skeleton. Stage order is the original
    /// cascade order, 1:1 (baseline :52-806); every stage fn below is a
    /// verbatim move. `Consumed` ⇒ plain `return` (the original inline
    /// `return` — the tail below is skipped, as before); `NotHit` ⇒ fall to
    /// the next level. `selecting` / `block_view` (baseline :643-644) are
    /// the two cross-stage locals and are passed explicitly.
    pub(super) fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        match self.press_modal_route(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_session_route() {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_pane_divider(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_pane_focus(x, y) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        self.press_sync_mouse_protocols();
        match self.press_tab_bar(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_panel_sidebar_resize(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_panel_scrollbar(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_panel_rows(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_block_scrollbar(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_block_header_actions(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_collapse_chevron(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_smart_select(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_hyperlink(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_find_buttons(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_popup_border_drag(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_palette(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_prompt(x, y, button) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        // v1.12.27b (P1-02): baseline :643-644 — terminal classification
        // locals shared by the three tail stages below.
        let selecting = !self.mouse_reporting_active();
        let block_view = self.block_view_active();
        self.press_block_hover(y, button, block_view);
        match self.press_chrome_guard(x, y, block_view) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }
        match self.press_button_action(x, y, button, selecting, block_view) {
            PressOutcome::Consumed => return,
            PressOutcome::NotHit => {}
        }

        self.request_redraw();
    }
}
