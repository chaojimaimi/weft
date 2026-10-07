//! Tab-bar stage of `handle_mouse_press` (v1.12.27b P1-02), moved verbatim
//! from `mouse_press_controller.rs` — baseline :157-282.

use super::*;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the tab-bar level (baseline
    /// :157-282). Every in-bar path (traffic lights, arrows, close, tab,
    /// new tab, background drag/double-click) consumed the press; a press
    /// below the bar or without a renderer falls through.
    pub(crate) fn press_tab_bar(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
    ) -> PressOutcome {
        // v0.9 H1: Tab bar click handling — check before everything else so
        // tab clicks work even inside TUI apps that captured the mouse.
        // v1.2: always active (even single tab) since the bar is always drawn.
        if button == winit::event::MouseButton::Left {
            if let Some(renderer) = &self.renderer {
                let bar_h = renderer.tab_bar_height();
                if y as f32 <= bar_h {
                    // v1.1: clicks in the macOS traffic-light region (top-left)
                    // must pass through to the system (close/minimize/maximize).
                    if (x as f32) < renderer.traffic_lights_width() {
                        return PressOutcome::Consumed;
                    }
                    // Click is in the tab bar region. Resolve via the shared
                    // TabBar Scene (arrows → close → tab label → "+", in
                    // z-order). Falls through to the double-click handler when
                    // the click misses every target.
                    let xf = x as f32;
                    let yf = y as f32;
                    match self.tab_bar_target_at(xf, yf) {
                        Some(crate::tab_bar_component::TabBarTarget::ArrowLeft) => {
                            let cw = renderer.cell_width() as f32;
                            self.tab_bar.scroll_offset =
                                (self.tab_bar.scroll_offset - cw * 15.0).max(0.0);
                            self.clamp_tab_scroll();
                            self.request_redraw();
                            return PressOutcome::Consumed;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::ArrowRight) => {
                            let cw = renderer.cell_width() as f32;
                            self.tab_bar.scroll_offset += cw * 15.0;
                            self.clamp_tab_scroll();
                            self.request_redraw();
                            return PressOutcome::Consumed;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::Close(idx)) => {
                            // v1.11.13: deferred close — pressing × no longer
                            // closes immediately. The press records a drag
                            // candidate with `close_on_release`; a plain click
                            // (no movement past the threshold) closes on
                            // release, while a drag past the threshold
                            // reorders the tab instead.
                            self.tab_bar.hovered_tab = None;
                            self.interaction.tab_drag = Some(crate::app_state::TabBarDragState {
                                start_x: x,
                                start_y: y,
                                drag_index: idx,
                                grab_offset: 0.0,
                                insert_index: idx,
                                moved: false,
                                close_on_release: true,
                            });
                            self.request_redraw();
                            return PressOutcome::Consumed;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::Tab(hit_index)) => {
                            tracing::debug!(
                                "TAB_DRAG_DIAG: press hit tab={} at ({}, {}), bar_h={}, setting tab_drag",
                                hit_index, x, y, bar_h
                            );
                            if self.sessions.active_idx() != hit_index {
                                self.reset_ime_context("tab clicked");
                                self.sessions.switch_to(hit_index);
                                self.refresh_find_for_active_tab();
                            }
                            self.tab_bar.hovered_tab = None;
                            self.interaction.block_hovered = None;
                            self.interaction.block_selected = None;
                            self.interaction.block_action_hovered = None;
                            self.scroll_active_tab_into_view();
                            // v1.11: record press position for drag-to-reorder.
                            // The tab is already switched (above). If the user
                            // drags beyond the threshold, the ghost drag takes
                            // over; otherwise this is a plain click.
                            self.interaction.tab_drag = Some(crate::app_state::TabBarDragState {
                                start_x: x,
                                start_y: y,
                                drag_index: hit_index,
                                grab_offset: 0.0,
                                insert_index: hit_index,
                                moved: false,
                                close_on_release: false,
                            });
                            self.request_redraw();
                            return PressOutcome::Consumed;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::NewTab) => {
                            self.new_tab();
                            self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
                            return PressOutcome::Consumed;
                        }
                        None => {}
                    }
                    // v1.1: Click in the tab-bar background (not on any tab,
                    // not on the traffic lights). This is a draggable region
                    // (winit's native drag_window handles the drag). Detect a
                    // double-click here to toggle maximize, matching the macOS
                    // native titlebar double-click behavior.
                    tracing::debug!(
                        "TAB_DRAG_DIAG: no hit at ({}, {}), will drag_window",
                        xf,
                        yf
                    );
                    let now = std::time::Instant::now();
                    let is_double = self
                        .tab_bar
                        .last_titlebar_click
                        .map(|t| now.duration_since(t) < std::time::Duration::from_millis(500))
                        .unwrap_or(false);
                    if is_double {
                        if let Some(window) = &self.window {
                            let maximized = window.is_maximized();
                            window.set_maximized(!maximized);
                        }
                        self.tab_bar.last_titlebar_click = None;
                    } else {
                        self.tab_bar.last_titlebar_click = Some(now);
                        if let Some(window) = &self.window {
                            if let Err(err) = window.drag_window() {
                                tracing::warn!(?err, "native titlebar drag failed");
                            }
                        }
                    }
                    return PressOutcome::Consumed;
                }
            }
        }
        PressOutcome::NotHit
    }
}
