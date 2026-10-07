//! History-panel stages of `handle_mouse_press` (v1.12.27b P1-02), moved
//! verbatim from `mouse_press_controller.rs` — baseline :284-385. Order
//! within the file is the original cascade order.

use super::*;
use crate::effect::Effect;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the sidebar-resize level
    /// (baseline :284-307).
    pub(crate) fn press_panel_sidebar_resize(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v0.9 W2: history panel click → select row + scroll terminal to block.
        // Handled before PTY mouse reporting so panel clicks work even inside
        // TUI apps that captured the mouse.
        // F3-3: sidebar resize handle is checked first — a click on the right
        // edge starts a width drag instead of selecting a panel row.
        if button == winit::event::MouseButton::Left && self.panel.open {
            let xf = x as f32;
            let yf = y as f32;
            if self.sidebar_resize_hit(xf, yf, 4.0) {
                let start_width = self
                    .renderer
                    .as_ref()
                    .map(|r| r.sidebar_width() / r.scale() as f32)
                    .unwrap_or(240.0);
                self.interaction.sidebar_drag = Some(crate::app_state::SidebarDragState {
                    start_x: x,
                    start_width,
                });
                if let Some(window) = &self.window {
                    window.set_cursor(winit::window::CursorIcon::EwResize);
                }
                return PressOutcome::Consumed;
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the panel-scrollbar level
    /// (baseline :308-329).
    pub(crate) fn press_panel_scrollbar(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        if button == winit::event::MouseButton::Left && self.panel.open {
            let xf = x as f32;
            let yf = y as f32;
            if let Some(layout) = self.active_panel_scrollbar_layout() {
                if crate::panel_scrollbar::contains(layout.hit, xf, yf) {
                    let thumb_height = layout.thumb[3] - layout.thumb[1];
                    let grab_offset = crate::panel_scrollbar::thumb_grab_offset(&layout, xf, yf)
                        .unwrap_or(thumb_height / 2.0);
                    self.panel.scroll_offset =
                        crate::panel_scrollbar::scroll_offset_for_pointer(&layout, yf, grab_offset);
                    self.clamp_panel_scroll();
                    self.clamp_panel_selection();
                    self.interaction.panel_scrollbar_drag =
                        Some(crate::panel_scrollbar::PanelScrollbarDragState {
                            layout,
                            grab_offset,
                        });
                    self.request_redraw();
                    return PressOutcome::Consumed;
                }
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the panel-rows level (baseline
    /// :330-385). Row/SearchField/LoadOlder hits consumed the press; the
    /// miss path (None) keeps its unfocus side effects and falls through.
    pub(crate) fn press_panel_rows(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        if button == winit::event::MouseButton::Left && self.panel.open {
            let xf = x as f32;
            let yf = y as f32;
            match self.panel_target_at(xf, yf) {
                Some(crate::panel_component::PanelTarget::Row(clicked)) => {
                    // Click on a history row: select it AND focus the
                    // panel so Up/Down keys navigate the list (Warp-style).
                    // Single click only selects + scrolls + highlights
                    // the block; double-click (or Enter) sends the command
                    // to the prompt editor.
                    self.panel.search_focused = true;
                    let now = std::time::Instant::now();
                    let is_double = self
                        .panel
                        .last_click
                        .map(|(t, row)| {
                            t.elapsed() < std::time::Duration::from_millis(400) && row == clicked
                        })
                        .unwrap_or(false);
                    self.panel.last_click = Some((now, clicked));
                    self.panel.selection = clicked;
                    self.clamp_panel_selection();
                    self.scroll_to_panel_selection();
                    if is_double {
                        self.send_panel_selection_to_input();
                    }
                    return PressOutcome::Consumed;
                }
                Some(crate::panel_component::PanelTarget::SearchField) => {
                    // Click in the search input field: focus it so keyboard
                    // input goes to panel_query (bug 6 fix).
                    self.panel.search_focused = true;
                    self.request_redraw();
                    return PressOutcome::Consumed;
                }
                Some(crate::panel_component::PanelTarget::LoadOlder) => {
                    // v1.11.2 X4 (PLAN_v1112 §1.3): footer「加载更早」— page
                    // older history out of SQLite into the panel.
                    self.drain_effects([Effect::LoadOlderBlocks]);
                    return PressOutcome::Consumed;
                }
                None => {
                    // Click outside the panel (or below the last row):
                    // unfocus search (but keep panel open).
                    if self.panel.search_focused {
                        self.panel.search_focused = false;
                        // v1.12.26 (review P1): unfocus must also drop any
                        // active IME composition — a lingering preedit kept a
                        // phantom string in the box and left native marked
                        // text routing to the terminal owner.
                        self.reset_ime_context("panel search unfocused");
                        self.request_redraw();
                    }
                }
            }
        }
        PressOutcome::NotHit
    }
}
