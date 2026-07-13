//! Mouse movement, drag, context-menu, and scroll controller.

use super::*;

impl App {
    /// Handle mouse release.
    pub(super) fn handle_mouse_release(
        &mut self,
        _x: f64,
        _y: f64,
        button: winit::event::MouseButton,
    ) {
        // End popup border drag if active.
        if button == winit::event::MouseButton::Left
            && self.interaction.scrollbar_drag.take().is_some()
        {
            let hovered = self.active_scrollbar_layout().is_some_and(|layout| {
                crate::scrollbar_component::contains(layout.hit, _x as f32, _y as f32)
            });
            self.interaction.scrollbar_hovered = hovered;
            if let Some(window) = &self.window {
                let icon = if hovered {
                    winit::window::CursorIcon::NsResize
                } else {
                    winit::window::CursorIcon::Default
                };
                window.set_cursor(icon);
            }
            self.request_redraw();
            return;
        }
        if button == winit::event::MouseButton::Left && self.interaction.drag_state.is_some() {
            self.interaction.drag_state = None;
            return;
        }

        // v0.9: end editor drag-selection (the selection itself stays so
        // Cmd+C can copy it).
        if button == winit::event::MouseButton::Left && self.interaction.prompt_dragging {
            self.interaction.prompt_dragging = false;
            // A click without drag (anchor == cursor) leaves an empty
            // selection — clear it so the caret shows normally.
            if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                if !t.editor().buffer.has_selection() {
                    // has_selection returns false when anchor==cursor, so
                    // explicitly clear the anchor to drop the empty selection.
                    t.editor_mut().buffer.clear_selection();
                }
            }
            self.request_redraw();
        }

        let pos = self.pixel_to_grid(_x, _y);
        self.sessions.active_mut().selection_handler.end();

        let btn = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return,
        };
        self.send_mouse_event(btn, MouseAction::Release, pos);
    }

    /// Handle mouse movement.
    pub(super) fn handle_mouse_move(&mut self, x: f64, y: f64) {
        if let Some(drag) = self.interaction.scrollbar_drag {
            if !self.interaction.scrollbar_hovered {
                self.interaction.scrollbar_hovered = true;
                if let Some(window) = &self.window {
                    window.set_cursor(winit::window::CursorIcon::NsResize);
                }
            }
            let offset = crate::scrollbar_component::scroll_offset_for_pointer(
                &drag.layout,
                y as f32,
                drag.grab_offset,
            );
            self.sessions.active_mut().set_block_scroll(offset);
            self.request_redraw();
            return;
        }
        // v1.0 fix: sync mouse_protocol + sgr_mouse (see handle_mouse_press)
        // so move-event encoding (ButtonEvent/AnyEvent drag reporting) reflects
        // the app's actual mouse mode and report format.
        let modes = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .map(|t| (t.mouse_protocol, t.sgr_mouse));
        if let Some((mp, sgr)) = modes {
            self.sessions.active_mut().input_handler.mouse_protocol = mp;
            self.sessions.active_mut().input_handler.sgr_mouse = sgr;
        }
        // F2 P0-3: when mouse reporting is active (vim/less/htop), use Arrow
        // so the TUI app controls the pointer. Otherwise use Text for normal
        // terminal input, or NsResize when hovering the scrollbar.
        let mouse_reporting_active = modes
            .map(|(mp, _)| mp != MouseProtocol::Off)
            .unwrap_or(false);
        // Update popup drag if active (clone to avoid borrow conflict).
        if let Some(drag) = self.interaction.drag_state.clone() {
            self.update_popup_drag(x, y, &drag);
            return;
        }

        let scrollbar_hovered = self.active_scrollbar_layout().is_some_and(|layout| {
            crate::scrollbar_component::contains(layout.hit, x as f32, y as f32)
        });
        let scrollbar_changed = scrollbar_hovered != self.interaction.scrollbar_hovered;
        if scrollbar_changed {
            self.interaction.scrollbar_hovered = scrollbar_hovered;
        }
        // F2 P0-3: set cursor based on mouse reporting + scrollbar hover.
        // Default == arrow cursor; used when a TUI app (vim/less/htop) has
        // enabled mouse reporting so it owns the pointer.
        let in_terminal_content = self.terminal_layout().is_some_and(|layout| {
            x >= layout.content.left
                && x <= layout.content.right
                && y >= layout.content.top
                && y <= layout.content.bottom
        });
        let over_panel = self.panel.open
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| x < r.sidebar_width() as f64);
        let terminal_cursor_allowed = in_terminal_content
            && !over_panel
            && !self.settings.open
            && !self.palette.open
            && self.interaction.context_menu.is_none();
        if let Some(window) = &self.window {
            let icon = if mouse_reporting_active {
                winit::window::CursorIcon::Default
            } else if scrollbar_hovered {
                winit::window::CursorIcon::NsResize
            } else if terminal_cursor_allowed {
                winit::window::CursorIcon::Text
            } else {
                winit::window::CursorIcon::Default
            };
            window.set_cursor(icon);
        }
        if scrollbar_changed {
            self.request_redraw();
        }

        // v0.9 W1+: tab bar hover detection — show close "×" on the hovered
        // tab (Warp-style) and highlight the "+" / scroll arrows.
        // v1.2: always active (even single tab) since the bar is always drawn.
        // Hit testing goes through the shared TabBar Scene (no renderer state).
        {
            use crate::tab_bar_component::TabBarTarget;
            let (new_hover, new_plus_hover, new_la_hover, new_ra_hover) =
                match self.tab_bar_target_at(x as f32, y as f32) {
                    Some(TabBarTarget::Tab(idx)) => (Some(idx), false, false, false),
                    Some(TabBarTarget::Close(idx)) => (Some(idx), false, false, false),
                    Some(TabBarTarget::NewTab) => (None, true, false, false),
                    Some(TabBarTarget::ArrowLeft) => (None, false, true, false),
                    Some(TabBarTarget::ArrowRight) => (None, false, false, true),
                    None => (None, false, false, false),
                };
            let changed = new_hover != self.tab_bar.hovered_tab
                || new_plus_hover != self.tab_bar.plus_hovered
                || new_la_hover != self.tab_bar.arrow_left_hovered
                || new_ra_hover != self.tab_bar.arrow_right_hovered;
            self.tab_bar.hovered_tab = new_hover;
            self.tab_bar.plus_hovered = new_plus_hover;
            self.tab_bar.arrow_left_hovered = new_la_hover;
            self.tab_bar.arrow_right_hovered = new_ra_hover;
            if changed {
                self.request_redraw();
            }
        }

        // v0.9: extend editor drag-selection inside the prompt box.
        if self.interaction.prompt_dragging {
            if let Some(pos) = self.pixel_to_editor_pos(x, y) {
                if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                    t.editor_mut().buffer.extend_selection(pos);
                    self.request_redraw();
                }
            }
        }

        if self.sessions.active_mut().selection_handler.selecting {
            if self.block_view_active() {
                if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                    self.sessions
                        .active_mut()
                        .selection_handler
                        .extend_block_view(bv_pos);
                    self.request_redraw();
                }
            } else {
                let pos = self.pixel_to_grid(x, y);
                self.sessions.active_mut().selection_handler.extend(pos);
                self.request_redraw();
            }
        }

        // PTY mouse reporting always speaks grid coordinates.
        let pos = self.pixel_to_grid(x, y);
        self.send_mouse_event(MouseButton::Left, MouseAction::Move, pos);
    }

    /// Check if a click (x, y) lands on a popup border drag handle.
    /// Returns a DragState if so, enabling resize-drag.
    /// Uses the actual popup rectangles stored by the renderer (not
    /// approximations), so hot-zone detection is accurate.
    pub(super) fn check_popup_border_drag(&self, x: f64, y: f64) -> Option<DragState> {
        let renderer = self.renderer.as_ref()?;
        let (cw, ch) = (renderer.cell_width() as f32, renderer.cell_height() as f32);
        // Completion geometry is derived from the current editor state and
        // shared Scene, rather than a rectangle retained by the last frame.
        if let Some(scene) = self.completion_scene() {
            match crate::completion_component::completion_target_at(&scene, x as f32, y as f32) {
                Some(crate::completion_component::CompletionTarget::ResizeWidth) => {
                    return Some(DragState {
                        target: DragTarget::Right,
                        start_x: x,
                        start_y: y,
                        start_scale: self.interaction.popup_width_scale,
                        start_rows: self.interaction.popup_max_rows,
                        cell_w: cw,
                        cell_h: ch,
                    });
                }
                Some(crate::completion_component::CompletionTarget::ResizeHeight) => {
                    return Some(DragState {
                        target: DragTarget::Top,
                        start_x: x,
                        start_y: y,
                        start_scale: self.interaction.popup_width_scale,
                        start_rows: self.interaction.popup_max_rows,
                        cell_w: cw,
                        cell_h: ch,
                    });
                }
                _ => {}
            }
        }

        if let Some(scene) = self.palette_scene() {
            let target =
                match crate::palette_component::palette_target_at(&scene, x as f32, y as f32) {
                    Some(crate::palette_component::PaletteTarget::ResizeWidth) => DragTarget::Right,
                    Some(crate::palette_component::PaletteTarget::ResizeHeight) => DragTarget::Top,
                    _ => return None,
                };
            return Some(DragState {
                target,
                start_x: x,
                start_y: y,
                start_scale: self.interaction.popup_width_scale,
                start_rows: self.interaction.popup_max_rows,
                cell_w: cw,
                cell_h: ch,
            });
        }
        None
    }

    /// Update popup dimensions during a border drag.
    pub(super) fn update_popup_drag(&mut self, x: f64, y: f64, drag: &DragState) {
        match drag.target {
            DragTarget::Right => {
                // Width: delta-x adjusts the popup width scale.
                let dx = (x - drag.start_x) as f32;
                let vp_w = self
                    .renderer
                    .as_ref()
                    .map(|r| r.viewport_width())
                    .unwrap_or(800.0);
                let scale_delta = dx / vp_w;
                self.interaction.popup_width_scale =
                    (drag.start_scale + scale_delta).clamp(0.3, 0.95);
            }
            DragTarget::Top => {
                // Height: delta-y (upward = more rows).
                let dy = (drag.start_y - y) as f32;
                let row_delta = (dy / drag.cell_h) as i32;
                let new_rows = (drag.start_rows as i32 + row_delta).clamp(3, 20) as usize;
                self.interaction.popup_max_rows = new_rows;
            }
        }
        self.request_redraw();
    }

    /// Execute a context menu action based on click position.
    pub(super) fn execute_context_menu(&mut self, menu: &ContextMenu, click_x: f32, click_y: f32) {
        let hit = self.renderer.as_ref().and_then(|renderer| {
            let ctx = renderer.layout_ctx?;
            let layout =
                crate::layout::layout_context_menu(&ctx, menu.x, menu.y, renderer.scale() as f32);
            let scene =
                crate::context_menu_component::build_context_menu_scene(layout, CONTEXT_MENU_ITEMS);
            crate::context_menu_component::context_menu_item_at(&scene, click_x, click_y)
        });
        if let Some(i) = hit {
            let action = CONTEXT_MENU_ITEMS[i].1;
            self.run_context_action(menu.block_id, action);
            self.request_redraw();
            return;
        }
        // Click outside menu items — just close (already taken).
        self.request_redraw();
    }

    /// Run a context menu action on the target block.
    /// `block_id` is `None` for the in-flight (running) command.
    pub(super) fn run_context_action(&mut self, block_id: Option<BlockId>, action: &str) {
        let mut clipboard_text = None;
        {
            let Some(terminal) = &mut self.sessions.active_mut().terminal else {
                return;
            };

            match action {
                "copy_command" | "copy_output" if block_id.is_none() => {
                    if let Some(live) = terminal.block_tracker().in_flight() {
                        let text = if action == "copy_command" {
                            live.command.to_string()
                        } else {
                            live.output.to_string()
                        };
                        clipboard_text = Some(text);
                    }
                }
                "copy_command" | "copy_output" => {
                    let bid = block_id.expect("copy branch has a finalized block id");
                    let block = terminal
                        .block_tracker()
                        .session_blocks()
                        .iter()
                        .find(|b| b.id == bid);
                    if let Some(block) = block {
                        clipboard_text = Some(if action == "copy_command" {
                            block.command.clone()
                        } else {
                            block.output.clone()
                        });
                    }
                }
                "toggle_fold" => {
                    if let Some(bid) = block_id {
                        terminal.block_tracker_mut().toggle_collapse(bid);
                    }
                    // In-flight blocks can't be folded (no finalized block yet).
                }
                // W4: copy the block's command into the editor buffer so the user
                // can tweak parameters and re-submit (Warp-style "rerun").
                "send_to_input" => {
                    if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                        let cmd = if let Some(bid) = block_id {
                            terminal
                                .block_tracker()
                                .session_blocks()
                                .iter()
                                .find(|b| b.id == bid)
                                .map(|b| b.command.clone())
                        } else {
                            terminal
                                .block_tracker()
                                .in_flight()
                                .map(|f| f.command.to_string())
                        };
                        if let Some(cmd) = cmd.filter(|cmd| !cmd.is_empty()) {
                            terminal.editor_mut().buffer.set_text(&cmd);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(text) = clipboard_text.as_ref() {
            info!(len = text.len(), "context action copied to clipboard");
        }
        self.drain_effects(effect::context_clipboard_effects(clipboard_text));
    }

    /// Handle scroll wheel.
    pub(super) fn handle_scroll(&mut self, delta: winit::event::MouseScrollDelta, x: f64, y: f64) {
        // v0.9 fix: drain pending PTY messages BEFORE checking alt-screen
        // state. When `less` (or any alt-screen app) starts, the enter
        // sequence (`\x1b[?1049h`) is still in the channel until the next
        // `process_messages` call. Without this drain, the first wheel
        // events see `alt_active = false` and fall through to the
        // viewport-scroll branch (which does nothing on alt screen). After
        // a keyboard event triggers a redraw → process_messages → alt_active
        // becomes true, the wheel starts working — which matches the user
        // report "scrolling works only after pressing a key".
        //
        // Drain anything already available. If the alt-screen sequence has
        // not arrived yet, the transition route below queues this gesture and
        // replays it from `process_messages` once parsing reaches alt screen.
        self.pump_pty();
        self.process_messages();

        // v1.2: If the scroll event is over the tab bar, adjust the tab bar
        // horizontal scroll offset instead of scrolling the terminal. This
        // lets the user navigate overflowed tabs via trackpad / wheel.
        if let Some(renderer) = &self.renderer {
            let bar_h = renderer.tab_bar_height();
            if y as f32 <= bar_h && self.sessions.len() > 1 {
                let cw = renderer.cell_width() as f32;
                let scroll_step = cw * 15.0; // scroll ~1 tab width per notch
                let delta_px = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, v) => {
                        // Vertical wheel in tab bar → horizontal scroll.
                        -v * scroll_step
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        // Trackpad: use deltaX if significant, else convert deltaY.
                        if pos.x.abs() > pos.y.abs() {
                            -(pos.x as f32)
                        } else {
                            -(pos.y as f32) * 0.5
                        }
                    }
                };
                self.tab_bar.scroll_offset = (self.tab_bar.scroll_offset + delta_px).max(0.0);
                self.clamp_tab_scroll();
                self.request_redraw();
                return;
            }
        }

        let lines = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, v) => {
                if v > 0.0 {
                    v.ceil() as usize
                } else {
                    v.floor().abs() as usize
                }
            }
            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                let v = pos.y / 40.0; // approx 40px per line
                if v > 0.0 {
                    v.ceil() as usize
                } else {
                    v.floor().abs() as usize
                }
            }
        };

        if lines == 0 {
            return;
        }

        // Short-lived immutable borrow to read the mode flags up-front —
        // avoids holding a long-lived mutable borrow of `terminal` across
        // later accesses to `block_scroll_offset`, `renderer`, etc.
        let tui_starting = self.sessions.active_mut().tui_scroll_window_active();
        let (mouse_protocol_active, alt_screen_active, app_cursor_keys, mouse_protocol, sgr_mouse) = {
            let Some(t) = self.sessions.active().terminal.as_ref() else {
                return;
            };
            // v1.0 fix: capture mouse_protocol here and sync it into the
            // InputHandler below (after this immutable borrow ends). Without
            // this sync, InputHandler.mouse_protocol stays `Off` forever (its
            // setters are test-only), so `encode_scroll`/`encode_mouse` hit
            // their `if Off { return None }` guards and silently drop every
            // mouse event — mouse-aware apps (vim `set mouse=a`, tmux, htop)
            // never receive wheel/click input. Mirrors the `app_cursor_keys`
            // sync in handle_key_event.
            let mp = t.mouse_protocol;
            let sgr = t.sgr_mouse;
            (
                mp != MouseProtocol::Off,
                t.is_alt_screen_active(),
                t.app_cursor_keys,
                mp,
                sgr,
            )
        };
        // Apply all terminal-controlled input modes before encoding this
        // gesture. Vim/less commonly enable DECCKM before the first wheel;
        // using a stale default would emit CSI arrows instead of SS3 arrows.
        self.sessions.active_mut().input_handler.app_cursor_keys = app_cursor_keys;
        self.sessions.active_mut().input_handler.mouse_protocol = mouse_protocol;
        self.sessions.active_mut().input_handler.sgr_mouse = sgr_mouse;

        // Check if mouse protocol is active — forward scroll to PTY
        if mouse_protocol_active {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.interaction.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            if self.interaction.mods.state().alt_key() {
                m |= Modifiers::ALT;
            }
            if self.interaction.mods.state().control_key() {
                m |= Modifiers::CONTROL;
            }
            if let Some(bytes) = self
                .sessions
                .active_mut()
                .input_handler
                .encode_scroll(up, pos.col, pos.row, m)
            {
                if let Some(pty) = &self.sessions.active_mut().pty {
                    let _ = pty.write_sync(&bytes);
                }
            }
            return;
        }

        // A TUI launched from the editor can receive its first wheel gesture
        // before the PTY reader has delivered/parsing has reached CSI ?1049h.
        // Keep the gesture per-tab for one 50ms protocol grace period. It is
        // encoded for the TUI if alternate screen arrives, otherwise it falls
        // back to ordinary local viewport scrolling (so normal commands do not
        // lose their first gesture during the two-second launch window).
        if tui_starting && !alt_screen_active {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let rows = if up { lines as i32 } else { -(lines as i32) };
            let pos = self.pixel_to_grid(x, y);
            let mut m = Modifiers::empty();
            if self.interaction.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            if self.interaction.mods.state().alt_key() {
                m |= Modifiers::ALT;
            }
            if self.interaction.mods.state().control_key() {
                m |= Modifiers::CONTROL;
            }
            if self
                .sessions
                .active_mut()
                .queue_tui_scroll(rows, pos.col, pos.row, m)
            {
                if let Some(delay) = self.sessions.active_mut().take_tui_scroll_wake_delay() {
                    let proxy = self.proxy.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        let _ = proxy.send_event(AppEvent::Wake);
                    });
                }
                return;
            }
        }

        // Alt-screen apps (less, vim, man, etc.) don't use mouse protocol but
        // still benefit from wheel scroll: translate to Up/Down arrow key
        // sequences so the pager scrolls its content natively.
        if alt_screen_active {
            let up = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
            };
            let key = if up { KeyCode::Up } else { KeyCode::Down };
            let mut m = Modifiers::empty();
            if self.interaction.mods.state().shift_key() {
                m |= Modifiers::SHIFT;
            }
            let single = self.sessions.active_mut().input_handler.encode_key(key, m);
            if !single.is_empty() {
                let mut batch = Vec::with_capacity(single.len() * lines);
                for _ in 0..lines {
                    batch.extend_from_slice(&single);
                }
                if let Some(pty) = &self.sessions.active_mut().pty {
                    let _ = pty.write_sync(&batch);
                }
            }
            return;
        }

        // Otherwise, scroll the terminal viewport.
        let up = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
            winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
        };
        let rows = if up { lines as i32 } else { -(lines as i32) };
        self.scroll_local_view(rows);
    }

    /// Apply signed rows to the normal terminal/block viewport. Positive rows
    /// move toward older content; negative rows move back toward the prompt.
    pub(super) fn scroll_local_view(&mut self, rows: i32) {
        if rows == 0 {
            return;
        }
        let up = rows > 0;
        let lines = rows.unsigned_abs() as usize;
        let block_view = self
            .sessions
            .active_mut()
            .terminal
            .as_ref()
            .is_some_and(Terminal::show_block_view);

        // Block view uses a dedicated scroll offset (not grid.scroll_offset,
        // which is clamped to grid scrollback — the wrong proxy for block
        // content like headers/commands/separators).
        if block_view {
            // Cap scroll speed at 1 row per wheel notch in the block view.
            // macOS trackpad inertia can send 3-4 lines per tick, which skips
            // past content too fast for comfortable reading.
            let scroll_lines = lines.min(1);
            // Compute metrics via a short-lived immutable borrow of `terminal`
            // so we can later mutate `block_scroll_offset` (same Tab, but a
            // disjoint field — allowed once the immutable borrow ends).
            let (total, prompt_lines) = {
                let Some(t) = self.sessions.active().terminal.as_ref() else {
                    return;
                };
                let cols = t.grid().num_cols;
                let (total, _) = block_content_metrics(t, cols);
                (total, t.editor().buffer.lines.len())
            };
            // Compute visible rows from the renderer's actual geometry
            // (pitch = ch * 1.1, region = viewport minus prompt box).
            // The old code used grid().num_rows which overcounts because
            // the block view uses a 10% taller line pitch and doesn't
            // occupy the full viewport (prompt box eats space).
            let visible = self
                .renderer
                .as_ref()
                .map(|r| {
                    let terminal = self.sessions.active().terminal.as_ref();
                    let cwd_header = terminal.is_some_and(|t| {
                        crate::layout::block_cwd_header_active(
                            t.effective_input_mode() == weft_core::input::InputMode::Editor,
                            t.cwd().is_some(),
                        )
                    });
                    r.block_visible_rows(prompt_lines, cwd_header)
                })
                .unwrap_or(1);
            let max_scroll = total.saturating_sub(visible);
            if up {
                let tab = self.sessions.active_mut();
                tab.scroll_up_by(scroll_lines);
                tab.clamp_block_scroll(max_scroll);
            } else {
                self.sessions.active_mut().scroll_down_by(scroll_lines);
            }
        } else {
            // Grid view scroll — needs mutable terminal.
            if let Some(terminal) = &mut self.sessions.active_mut().terminal {
                let grid = &mut terminal.grid_mut();
                if up {
                    grid.scroll_up_history(lines);
                } else {
                    grid.scroll_down_history(lines);
                }
            }
        }
        self.request_redraw();
    }

    /// Send a mouse event to the PTY if mouse protocol is active.
    pub(super) fn send_mouse_event(&self, button: MouseButton, action: MouseAction, pos: GridPos) {
        let Some(terminal) = self.sessions.active().terminal.as_ref() else {
            return;
        };
        if terminal.mouse_protocol == MouseProtocol::Off {
            return;
        }
        let mut m = Modifiers::empty();
        if self.interaction.mods.state().shift_key() {
            m |= Modifiers::SHIFT;
        }
        if self.interaction.mods.state().alt_key() {
            m |= Modifiers::ALT;
        }
        if self.interaction.mods.state().control_key() {
            m |= Modifiers::CONTROL;
        }
        let bytes = self
            .sessions
            .active()
            .input_handler
            .encode_mouse(button, action, pos.col, pos.row, m);
        if let Some(bytes) = bytes {
            if let Some(pty) = &self.sessions.active().pty {
                let _ = pty.write_sync(&bytes);
            }
        }
    }
}
