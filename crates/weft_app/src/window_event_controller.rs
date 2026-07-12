//! Window event controller. The ApplicationHandler trait forwards here.

use super::*;

impl App {
    pub(super) fn dispatch_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("Window closed");
                // v1.0 H4: persist tab state so the session restores on
                // next launch. Goes through drain_effects so all persist
                // paths share the Effect::PersistTabs entry point.
                self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
                event_loop.exit();
            }
            WindowEvent::Resized(physical_size) => {
                // Grid/PTY tracks the renderer's visible terminal content
                // rectangle. The editor box is an overlay, but title/tab
                // chrome is outside that rectangle and must be subtracted.
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    // Responsive geometry depends on the new viewport, so
                    // update the renderer before asking for sidebar/chrome
                    // metrics or terminal rows/cols.
                    renderer.resize(window, physical_size);
                    // v0.9 W5: subtract sidebar width when the panel is open so
                    // the grid reflows beside the sidebar (mirrors grid_dims).
                    let chrome_left = if self.panel.open {
                        renderer.sidebar_push_width() as f64
                    } else {
                        0.0
                    };
                    let (new_rows, new_cols) =
                        dimensions_for_renderer(renderer, physical_size, chrome_left);

                    if new_cols > 0 && new_rows > 0 {
                        // Resize ALL tabs' grids immediately for smooth
                        // animation. The rewrap/dimension-only resize is fast
                        // (<1ms) so doing it on every intermediate event is
                        // fine. Background tabs also need resizing so their
                        // content wraps correctly when switched to.
                        for tab in &mut self.sessions.tabs {
                            if let Some(terminal) = &mut tab.terminal {
                                terminal.resize(new_rows, new_cols);
                            }
                            // Queue the PTY SIGWINCH for this tab.
                            tab.pending_pty_resize = Some((new_rows, new_cols));
                        }
                        info!(rows = new_rows, cols = new_cols, "all tabs resized (event)");
                        self.window_runtime.last_resize_instant = std::time::Instant::now();
                        // v1.2-fix: re-clamp tab scroll offset after resize.
                        // The window may have grown/shrunk, changing max_scroll.
                        // Without this, a stale scroll_offset can leave tabs
                        // culled (invisible) after resize.
                        self.clamp_tab_scroll();
                        self.scroll_active_tab_into_view();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Moving between Retina and non-Retina displays changes every
                // physical metric used by the renderer: glyph cells, padding,
                // title/tab chrome and sidebar width. Refresh those first,
                // then recompute Grid/PTY dimensions from the same geometry.
                // Winit follows this event with Resized on macOS; doing the
                // recompute here also covers a retained physical inner size.
                let padding = (
                    self.config_state.config.window.padding_x,
                    self.config_state.config.window.padding_y,
                );
                let changed = self
                    .renderer
                    .as_mut()
                    .is_some_and(|renderer| renderer.update_scale(scale_factor, padding));
                if changed {
                    self.recompute_layout();
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.handle_redraw_requested(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    if let PhysicalKey::Code(key_code) = event.physical_key {
                        // `event.text` already reflects Shift (and the keymap),
                        // e.g. Shift+A -> "A", Shift+1 -> "!". The editor uses it
                        // so typed commands keep their case / shifted symbols.
                        self.handle_key_event(
                            key_code,
                            self.interaction.mods,
                            event.text.as_deref(),
                        );
                    }
                }
            }
            WindowEvent::ModifiersChanged(new_mods) => {
                self.interaction.mods = new_mods;
            }
            WindowEvent::MouseInput { state, button, .. } => match state {
                winit::event::ElementState::Pressed => {
                    self.handle_mouse_press(
                        self.interaction.last_mouse_x,
                        self.interaction.last_mouse_y,
                        button,
                    );
                }
                winit::event::ElementState::Released => {
                    self.handle_mouse_release(
                        self.interaction.last_mouse_x,
                        self.interaction.last_mouse_y,
                        button,
                    );
                }
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.interaction.last_mouse_x = position.x;
                self.interaction.last_mouse_y = position.y;
                self.handle_mouse_move(position.x, position.y);
            }
            WindowEvent::CursorLeft { .. } => {
                if self.interaction.scrollbar_hovered && self.interaction.scrollbar_drag.is_none() {
                    self.interaction.scrollbar_hovered = false;
                    if let Some(window) = &self.window {
                        window.set_cursor(winit::window::CursorIcon::Default);
                    }
                    self.request_redraw();
                }
                if self.tab_bar.hovered_tab.is_some()
                    || self.tab_bar.plus_hovered
                    || self.tab_bar.arrow_left_hovered
                    || self.tab_bar.arrow_right_hovered
                {
                    self.tab_bar.clear_hover();
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_scroll(
                    delta,
                    self.interaction.last_mouse_x,
                    self.interaction.last_mouse_y,
                );
            }
            WindowEvent::Ime(ime_event) => self.handle_ime_event(ime_event),
            WindowEvent::Focused(focused) => {
                // Reset blink timer on focus change
                if focused {
                    self.window_runtime.cursor_blink_on = true;
                    self.window_runtime.cursor_blink_time = std::time::Instant::now();
                } else {
                    self.reset_ime_context("window focus lost");
                }
            }
            _ => {}
        }
        // v1.0 H4: check should_exit flag (set by close_tab on last tab).
        if self.should_exit {
            event_loop.exit();
        }
    }
}
