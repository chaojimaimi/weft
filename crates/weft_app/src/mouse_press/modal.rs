//! Modal routing + pre-focus stages of `handle_mouse_press` (v1.12.27b
//! P1-02), moved verbatim from `mouse_press_controller.rs` — baseline
//! :52-156. Order within the file is the original cascade order.

use super::*;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the modal-routing level (baseline
    /// :52-76). Settings/context-menu/palette-consume arms consumed the
    /// press; PaletteLeft and Terminal fall through.
    pub(crate) fn press_modal_route(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        match crate::input_router::route_modal_mouse(
            self.palette.open,
            self.settings.open,
            self.interaction.context_menu.is_some(),
            button,
        ) {
            crate::input_router::ModalMouseRoute::PaletteLeft => {}
            crate::input_router::ModalMouseRoute::SettingsLeft => {
                self.handle_settings_mouse_press(x as f32, y as f32);
                return PressOutcome::Consumed;
            }
            crate::input_router::ModalMouseRoute::ContextMenuLeft => {
                if let Some(menu) = self.take_context_menu("context menu closed by click") {
                    self.execute_context_menu(&menu, x as f32, y as f32);
                }
                return PressOutcome::Consumed;
            }
            crate::input_router::ModalMouseRoute::DismissContextMenu => {
                self.take_context_menu("context menu dismissed by mouse");
                self.request_redraw();
                return PressOutcome::Consumed;
            }
            crate::input_router::ModalMouseRoute::Consume => return PressOutcome::Consumed,
            crate::input_router::ModalMouseRoute::Terminal => {}
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the session-routing level
    /// (baseline :78-82) — a Consume route swallows the press.
    pub(crate) fn press_session_route(&mut self) -> PressOutcome {
        if crate::input_router::route_session_input(!self.sessions.is_empty())
            == crate::input_router::SessionInputRoute::Consume
        {
            return PressOutcome::Consumed;
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the pane-divider grab level
    /// (baseline :84-108).
    pub(crate) fn press_pane_divider(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v1.3.2: check for pane-divider grab before focusing a pane.
        // If the click lands on a divider strip (±4px), start a resize drag
        // instead of focusing/selecting. Mirrors the sidebar resize pattern.
        if button == winit::event::MouseButton::Left {
            if let Some(divider) = self.pane_divider_hit_test(x as f32, y as f32) {
                self.interaction.pane_divider_drag = Some(crate::app_state::PaneDividerDragState {
                    axis: divider.axis,
                    first: divider.first,
                    second: divider.second,
                    bounds: divider.bounds,
                });
                if let Some(window) = &self.window {
                    let icon = match divider.axis {
                        crate::paint::pane_dividers::DividerAxis::Vertical => {
                            winit::window::CursorIcon::EwResize
                        }
                        crate::paint::pane_dividers::DividerAxis::Horizontal => {
                            winit::window::CursorIcon::NsResize
                        }
                    };
                    window.set_cursor(icon);
                }
                return PressOutcome::Consumed;
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the pane-focus level (baseline
    /// :110-137). The empty-tabs transient (:119) and the focus-change
    /// redraw path (:134) consumed the press; a no-op focus falls through.
    pub(crate) fn press_pane_focus(&mut self, x: f64, y: f64) -> PressOutcome {
        // v1.3 Batch 6: focus the pane under the cursor on click. This
        // switches the active pane BEFORE the rest of the mouse handling
        // (which all goes through `active_mut()`), so clicks/selections/
        // PTY mouse events route to the clicked pane. For single-pane tabs
        // `pane_at_pixel` always returns the one pane id — no-op switch.
        if let Some(pane_id) = self.pane_at_pixel(x, y) {
            // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — no pane to
            // focus, ignore the press.
            let Some(tab) = self.sessions.active_mut() else {
                return PressOutcome::Consumed;
            };
            if tab.active_pane_id() != pane_id {
                if let Err(e) = tab.set_active_pane(pane_id) {
                    tracing::warn!(error = ?e, "failed to focus pane under cursor");
                } else {
                    // Hit regions and block rows still describe the previously
                    // active pane. Redraw before accepting a content action.
                    self.refresh_find_for_active_tab();
                    if let Some(renderer) = &mut self.renderer {
                        renderer.force_full_grid_redraw();
                    }
                    self.window_runtime.cursor_blink_on = true;
                    self.window_runtime.cursor_blink_time = std::time::Instant::now();
                    self.request_redraw();
                    return PressOutcome::Consumed;
                }
            }
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the mouse-protocol sync level
    /// (baseline :139-156) — no hit-testing, always falls through.
    pub(crate) fn press_sync_mouse_protocols(&mut self) {
        // v1.0 fix: sync InputHandler.mouse_protocol + sgr_mouse from the
        // Terminal's VT-parsed values before any mouse-event encoding. Without
        // this the handler's copy stays `Off` (its setters are test-only) and
        // `encode_mouse` returns None — mouse-aware apps (vim `set mouse=a`,
        // tmux, htop) never receive clicks/drags. sgr_mouse selects the report
        // format (SGR-1006 vs legacy) — sending the wrong format corrupts the
        // app (vim `~@k`). Mirrors the scroll-path sync.
        let modes = self
            .sessions
            .active()
            .and_then(|tab| tab.terminal.as_ref())
            .map(|t| (t.mouse_protocol(), t.sgr_mouse()));
        if let Some((mp, sgr)) = modes {
            if let Some(tab) = self.sessions.active_mut() {
                tab.input_handler.mouse_protocol = mp;
                tab.input_handler.sgr_mouse = sgr;
            }
        }
    }
}
