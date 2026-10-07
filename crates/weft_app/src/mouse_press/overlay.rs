//! Overlay hit stages of `handle_mouse_press` (v1.12.27b P1-02), moved
//! verbatim from `mouse_press_controller.rs` — baseline :508-594. Order
//! within the file is the original cascade order: smart select → hyperlink
//! → find buttons → popup border drag → palette.

use super::*;
use crate::geometry_controller::FindButtonAction;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the smart-select level (baseline
    /// :508-519).
    pub(crate) fn press_smart_select(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v1.10 Smart Select: Cmd+Shift+Click selects a semantic target;
        // Cmd+Option+Click explicitly opens a safe URL or reveals a path.
        if button == winit::event::MouseButton::Left
            && self.interaction.mods.state().super_key()
            && (self.interaction.mods.state().shift_key()
                || self.interaction.mods.state().alt_key())
            && self.config_state.config.editor.smart_select
        {
            let open = self.interaction.mods.state().alt_key();
            self.handle_smart_select_click(x, y, open);
            return PressOutcome::Consumed;
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the OSC 8 hyperlink level
    /// (baseline :521-534).
    pub(crate) fn press_hyperlink(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // OSC 8 hyperlink Cmd+Click: open the URL tagged on the clicked cell
        // via the registry's side-map. Bypasses normal selection / PTY mouse
        // reporting so Cmd+Click works even inside TUI apps that captured the
        // mouse (opencode, claude, vim) — same escape hatch as Shift+drag.
        if button == winit::event::MouseButton::Left && self.interaction.mods.state().super_key() {
            if let Some(url) = self.hyperlink_at_pixel(x, y) {
                // v1.6.1: surface open failures to the user via the status
                // hint mechanism (exit criterion: "外部 URL 打开失败有可见错误").
                if let Err(e) = open_url(&url) {
                    self.surface_config_error(&e.to_string());
                }
                return PressOutcome::Consumed;
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the find-popup buttons level
    /// (baseline :536-557).
    pub(crate) fn press_find_buttons(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // Find popup button clicks (regex / case / up / down). Bypasses the
        // normal selection / PTY mouse path so the buttons work even inside
        // TUI apps that captured the mouse — same rationale as Cmd+Click.
        if button == winit::event::MouseButton::Left && self.find.open {
            if let Some(action) = self.find_button_at(x as f32, y as f32) {
                match action {
                    FindButtonAction::ToggleRegex => {
                        self.find.regex_mode =
                            crate::find_controller::toggled_find_option(self.find.regex_mode);
                        self.arm_find_refresh();
                    }
                    FindButtonAction::ToggleCase => {
                        self.find.case_sensitive =
                            crate::find_controller::toggled_find_option(self.find.case_sensitive);
                        self.arm_find_refresh();
                    }
                    FindButtonAction::Next => self.find_cycle_next_prev(true),
                    FindButtonAction::Prev => self.find_cycle_next_prev(false),
                }
                return PressOutcome::Consumed;
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the popup-border-drag level
    /// (baseline :559-565).
    pub(crate) fn press_popup_border_drag(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // Check for popup border drag (completion or palette).
        if button == winit::event::MouseButton::Left {
            if let Some(drag) = self.check_popup_border_drag(x, y) {
                self.interaction.drag_state = Some(drag);
                return PressOutcome::Consumed;
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the palette level (baseline
    /// :567-594) — the plan's 倒数第四段. Palette is modal: any left press
    /// while it is open is consumed, even outside a row.
    pub(crate) fn press_palette(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v0.9: Command Palette mouse interaction — click inside the popup
        // (but not on the border drag zone) selects the entry; double-click
        // runs it immediately. Mirrors the history panel's click/double-click
        // pattern so the user doesn't have to press Enter.
        if button == winit::event::MouseButton::Left && self.palette.open {
            if let Some(clicked_idx) = self.palette_row_at(x, y) {
                let now = std::time::Instant::now();
                let is_double = self
                    .palette
                    .last_click
                    .map(|(t, row)| {
                        t.elapsed() < std::time::Duration::from_millis(400) && row == clicked_idx
                    })
                    .unwrap_or(false);
                self.palette.last_click = Some((now, clicked_idx));
                if is_double {
                    if let Some(entry) = self.palette.results.get(clicked_idx).cloned() {
                        self.activate_palette_entry(entry);
                    }
                } else {
                    self.palette.selection = clicked_idx;
                    self.request_redraw();
                }
            }
            // Palette is modal: a left click outside a row or resize handle is
            // still consumed and must never fall through to terminal selection.
            return PressOutcome::Consumed;
        }
        PressOutcome::NotHit
    }
}
