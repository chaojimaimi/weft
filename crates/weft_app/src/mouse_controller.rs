// Mouse move/drag/scroll/context-menu dispatch. Grew with F3 sidebar resize
// + panel virtualization scroll handling; remaining size is the irreducible
// per-event-type dispatch (move/press/release/wheel) with overlay-specific
// branches.
//! Mouse movement, drag, context-menu, and scroll controller.

use super::*;
use crate::selection::{autoscroll_ramp_rows, down_autoscroll_threshold, AutoscrollDir};

impl App {
    /// Handle mouse release.
    pub(super) fn handle_mouse_release(
        &mut self,
        _x: f64,
        _y: f64,
        button: winit::event::MouseButton,
        terminal_session: Option<u64>,
        report_to_pty: bool,
    ) {
        // v1.11: tab drag-to-reorder end. A drag past the threshold commits
        // the reorder once (ghost drag model — the Vec was untouched during
        // the gesture); a sub-threshold release either performs the deferred
        // × close (`close_on_release`) or is a plain click (tab already
        // switched on press).
        if button == winit::event::MouseButton::Left {
            // Drag-selection autoscroll: releasing the left button ends any
            // edge-held drag — drop the drag position and stop the 40ms
            // timer so the viewport freezes immediately.
            self.interaction.selection_drag_pos = None;
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
            if let Some(drag) = self.interaction.tab_drag.take() {
                if drag.moved {
                    let n = self.sessions.len();
                    if drag.drag_index < n {
                        let insert = drag.insert_index.min(n.saturating_sub(1));
                        if !crate::layout::is_noop(drag.drag_index, insert) {
                            self.sessions.move_tab(drag.drag_index, insert);
                            self.scroll_active_tab_into_view();
                        }
                    }
                    self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
                } else if drag.close_on_release {
                    self.perform_close_request(drag.drag_index);
                }
                self.request_redraw();
                return;
            }
        }
        // F3-3: sidebar resize drag end — persist the new width to config.
        if button == winit::event::MouseButton::Left
            && self.interaction.sidebar_drag.take().is_some()
        {
            let new_width = self
                .renderer
                .as_ref()
                .and_then(|r| r.sidebar_width_override);
            // v1.5.0: persist via SOURCE config so an active profile's
            // overrides aren't flattened into base on save. Effective is
            // also updated so runtime reads see the new value.
            self.config_state.config.window.sidebar_width = new_width;
            if let Some(source) = self.config_state.source_config.as_mut() {
                source.window.sidebar_width = new_width;
            }
            if let Err(e) = self.config_state.source().save() {
                tracing::warn!(error = ?e, "failed to persist sidebar_width");
            }
            // Restore cursor based on current hover state.
            let icon = if self.sidebar_resize_hit(_x as f32, _y as f32, 4.0) {
                winit::window::CursorIcon::EwResize
            } else {
                winit::window::CursorIcon::Default
            };
            if let Some(window) = &self.window {
                window.set_cursor(icon);
            }
            self.request_redraw();
            return;
        }
        // v1.3.2: pane divider resize drag end — clear state, restore cursor.
        // No persistence (ratio is in-memory only, per V13 non-goal).
        if button == winit::event::MouseButton::Left
            && self.interaction.pane_divider_drag.take().is_some()
        {
            let icon = if let Some(d) = self.pane_divider_hit_test(_x as f32, _y as f32) {
                match d.axis {
                    crate::paint::pane_dividers::DividerAxis::Vertical => {
                        winit::window::CursorIcon::EwResize
                    }
                    crate::paint::pane_dividers::DividerAxis::Horizontal => {
                        winit::window::CursorIcon::NsResize
                    }
                }
            } else {
                winit::window::CursorIcon::Default
            };
            if let Some(window) = &self.window {
                window.set_cursor(icon);
            }
            self.request_redraw();
            return;
        }
        if button == winit::event::MouseButton::Left && self.finish_panel_scrollbar_drag() {
            return;
        }
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
            let release_tab = match terminal_session {
                Some(session_id) => self.sessions.tab_index_by_session_id(session_id),
                None => Some(self.sessions.active_idx()),
            };
            if let Some(t) = release_tab
                .and_then(|tab| self.sessions.tab_mut(tab))
                .and_then(|tab| tab.terminal.as_mut())
            {
                if !t.editor().buffer.has_selection() {
                    // has_selection returns false when anchor==cursor, so
                    // explicitly clear the anchor to drop the empty selection.
                    t.editor_mut().buffer.clear_selection();
                }
            }
            self.request_redraw();
        }

        let pos = self.pixel_to_grid(_x, _y);
        let release_tab = terminal_session
            .and_then(|session_id| self.sessions.tab_index_by_session_id(session_id));
        if let Some(tab) = release_tab.and_then(|idx| self.sessions.tab_mut(idx)) {
            tab.selection_handler.end();
            // v1.10.20: selection release re-syncs the primary history view —
            // the drag deferred its exit (改动 2); back at FollowBottom the
            // live grid returns.
            tab.sync_primary_history_view();
        } else if terminal_session.is_none() && !self.sessions.is_empty() {
            let tab = self.sessions.active_mut();
            tab.selection_handler.end();
            tab.sync_primary_history_view();
        }

        let btn = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return,
        };
        if report_to_pty {
            if let Some(tab) = release_tab {
                self.send_mouse_event_to_session(tab, btn, MouseAction::Release, pos);
            }
        }
    }

    /// Handle mouse movement.
    pub(super) fn handle_mouse_move(&mut self, x: f64, y: f64) {
        // F3-3: sidebar resize drag — update the renderer's sidebar width
        // from the pointer delta. The drag persists on release.
        if self.update_sidebar_drag(x) {
            return;
        }
        // v1.3.2: pane divider resize drag — update the split ratio from the
        // pointer position. PTY resize is throttled by the existing mechanism
        // in recompute_layout (30ms active / 100ms background).
        if self.update_pane_divider_drag(x as f32, y as f32) {
            return;
        }
        if self.update_panel_scrollbar_drag(y as f32) {
            return;
        }
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
            .map(|t| (t.mouse_protocol(), t.sgr_mouse()));
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
        let in_terminal_content = self.terminal_content_contains(x, y);
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
        // F3-3: hover the sidebar's right edge → EwResize cursor. Takes
        // precedence over text/arrow so the resize affordance is discoverable
        // even when the pointer came from inside the terminal content or a
        // TUI app has mouse reporting on (the sidebar is app chrome, not PTY).
        let sidebar_resize_hovered = self.sidebar_resize_hit(x as f32, y as f32, 4.0);
        // v1.3.2: pane divider hover — show resize cursor when hovering a divider.
        let pane_divider_hovered = self.pane_divider_hit_test(x as f32, y as f32);
        if let Some(window) = &self.window {
            let icon = if let Some(d) = pane_divider_hovered {
                match d.axis {
                    crate::paint::pane_dividers::DividerAxis::Vertical => {
                        winit::window::CursorIcon::EwResize
                    }
                    crate::paint::pane_dividers::DividerAxis::Horizontal => {
                        winit::window::CursorIcon::NsResize
                    }
                }
            } else if sidebar_resize_hovered {
                winit::window::CursorIcon::EwResize
            } else if mouse_reporting_active {
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

        // F3-1: Block hover detection — track which finalized block the
        // cursor is over so the renderer can show inline copy/fold action
        // buttons on the header row. Only active in block view, with no
        // modal overlays open and no active drag/selection.
        let new_block_hovered = if self.block_view_active()
            && self.interaction.context_menu.is_none()
            && !self.settings.open
            && !self.palette.open
            && !self.interaction.prompt_dragging
        {
            match self.block_at(y as f32) {
                Some(Some(id)) => Some(id),
                _ => None,
            }
        } else {
            None
        };
        if new_block_hovered != self.interaction.block_hovered {
            self.interaction.block_hovered = new_block_hovered;
            self.request_redraw();
        }
        let new_action_hovered = self.renderer.as_ref().and_then(|renderer| {
            crate::block_component::block_header_action_at(
                &renderer.hit_regions,
                x as f32,
                y as f32,
            )
        });
        if new_action_hovered != self.interaction.block_action_hovered {
            self.interaction.block_action_hovered = new_action_hovered;
            self.request_redraw();
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
                // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): mouse-move NO
                // LONGER pumps the autoscroll — a stream of move events each
                // scrolling+extending stacked scrolls (the "停滞+瞬移" event
                // backlog source). Move only records the pointer for the 40ms
                // timer and extends the endpoint when the pointer is inside
                // the content band; the 40ms timer is the SOLE scroll driver.
                self.interaction.selection_drag_pos = Some((x, y));
                if let Some(anchor) = self.pixel_to_block_view_pos(x, y) {
                    self.sessions
                        .active_mut()
                        .selection_handler
                        .extend_block_view(anchor);
                }
                self.arm_selection_autoscroll(y);
                self.request_redraw();
            } else {
                // Grid view: viewport-relative `GridPos` selection — scrolling
                // mid-drag would silently corrupt the copy range (see
                // docs/FIX_DRAG_AUTOSCROLL.md "Grid 后续任务"). v1.10.20: a
                // primary-screen TUI drag held past the top edge migrates
                // into the primary history snapshot view instead — the
                // migration runs on the 40ms timer (arm below), not on move.
                self.interaction.selection_drag_pos = Some((x, y));
                if self.selection_autoscroll_overshoot(y).is_none() {
                    let pos = self.pixel_to_grid(x, y);
                    self.sessions.active_mut().selection_handler.extend(pos);
                }
                self.arm_selection_autoscroll(y);
                self.request_redraw();
            }
        }

        // PTY mouse reporting always speaks grid coordinates.
        if in_terminal_content {
            let pos = self.pixel_to_grid(x, y);
            self.send_mouse_event(MouseButton::Left, MouseAction::Move, pos);
        }
    }

    /// Check if a click (x, y) lands on a popup border drag handle.
    /// Returns a DragState if so, enabling resize-drag.
    /// Uses the actual popup rectangles stored by the renderer (not
    /// approximations), so hot-zone detection is accurate.
    pub(super) fn check_popup_border_drag(&self, x: f64, y: f64) -> Option<DragState> {
        let renderer = self.renderer.as_ref()?;
        let ch = renderer.cell_height() as f32;
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
        // v1.7.3-C: Block data cloned inside the terminal borrow for
        // `export_block` — consumed after the borrow drops so the annotation
        // store can be accessed without a borrow conflict.
        let mut export_block_data: Option<weft_core::blocks::Block> = None;
        {
            let Some(terminal) = &mut self.sessions.active_mut().terminal else {
                // Annotation-only actions (toggle_bookmark / add_note) don't
                // need the terminal — fall through to the post-borrow block.
                if matches!(action, "toggle_bookmark" | "add_note") {
                    self.run_annotation_action(block_id, action);
                }
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
                            block.output.to_string()
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
                // v1.7.3-C: Clone block data for export. The actual markdown
                // generation + file write happens after the terminal borrow
                // drops (needs annotation store access).
                "export_block" => {
                    if let Some(bid) = block_id {
                        if let Some(block) = terminal
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .find(|b| b.id == bid)
                        {
                            export_block_data = Some(block.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        // Terminal borrow dropped — annotation/export actions run here.
        if action == "toggle_fold" {
            if let (Some(renderer), Some(id)) = (&self.renderer, block_id) {
                renderer.block_layout_cache.borrow_mut().invalidate(id.0);
            }
        }
        if matches!(action, "toggle_bookmark" | "add_note" | "export_block") {
            self.run_annotation_action_with_export(block_id, action, export_block_data);
        }
        // v1.8.2: AI diagnose — trigger after terminal borrow drops.
        if action == "diagnose" {
            if let Some(block_id) = block_id {
                self.spawn_block_diagnose(block_id);
            }
        }
        if let Some(text) = clipboard_text.as_ref() {
            info!(len = text.len(), "context action copied to clipboard");
        }
        self.drain_effects(effect::context_clipboard_effects(clipboard_text));
    }

    /// Handle scroll wheel.
    pub(super) fn handle_scroll(
        &mut self,
        delta: winit::event::MouseScrollDelta,
        phase: winit::event::TouchPhase,
        x: f64,
        y: f64,
    ) {
        // v1.10.23 (FIX_LIVE_BLOCK_SCROLL_PERF): the per-event
        // `pump_pty + process_messages` (v0.9 alt-screen-entry fix) was
        // removed — the redraw path (`handle_redraw_requested`) pumps the
        // PTY on every frame, so a wheel event sees terminal state at most
        // one frame stale (a single event during the CSI ?1049h transition
        // can still be misrouted, then self-corrects next frame). The tab /
        // panel / block-view branches below don't depend on freshly parsed
        // PTY state; no branch was found that requires the in-place drain.

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

        // F3-4: When the scroll event is over the history sidebar, adjust the
        // panel's block-level scroll offset instead of scrolling the terminal.
        // Up (toward older history) → scroll_offset increases; Down → decreases.
        let over_panel = self.panel.open
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| x < r.sidebar_width() as f64);
        if over_panel {
            let panel_lines = match delta {
                winit::event::MouseScrollDelta::LineDelta(_, v) => {
                    if v > 0.0 {
                        v.ceil() as usize
                    } else {
                        v.floor().abs() as usize
                    }
                }
                winit::event::MouseScrollDelta::PixelDelta(pos) => {
                    let v = pos.y / 40.0;
                    if v > 0.0 {
                        v.ceil() as usize
                    } else {
                        v.floor().abs() as usize
                    }
                }
            };
            if panel_lines > 0 {
                let up = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, v) => v > 0.0,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
                };
                if up {
                    self.panel.scroll_offset = self.panel.scroll_offset.saturating_add(panel_lines);
                } else {
                    self.panel.scroll_offset = self.panel.scroll_offset.saturating_sub(panel_lines);
                }
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
            }
            return;
        }

        // Precise macOS trackpads emit many sub-row PixelDelta events for one
        // gesture. Accumulate them by physical cell height; treating every
        // non-zero event as a wheel notch makes Vim jump straight to an edge.
        let cell_height = self
            .renderer
            .as_ref()
            .map_or(40.0, |renderer| renderer.cell_height() as f64);
        let block_view = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .is_some_and(Terminal::show_block_view);
        if block_view {
            if let winit::event::MouseScrollDelta::PixelDelta(pos) = delta {
                if phase == winit::event::TouchPhase::Cancelled {
                    return;
                }
                // v1.10.21: during an alt-screen history peek a plain
                // precise scroll means "interact with the app" — one flick
                // drops the peek and returns to the live TUI (Warp parity;
                // see FIX_ALT_PEEK_WARP_ALIGNMENT). Shift+scroll falls
                // through to the fractional block scroll below.
                let peeking = self
                    .sessions
                    .active()
                    .terminal
                    .as_ref()
                    .is_some_and(weft_core::vt::Terminal::is_alt_screen_history_peek);
                if peeking && !self.interaction.mods.state().shift_key() {
                    // snap_to_bottom clears the flag and arms the gate's
                    // re-entry lockout (exit edge detected inside).
                    self.sessions.active_mut().snap_to_bottom();
                    self.request_redraw();
                    return;
                }
                // v1.10.19: A1-fix parity with scroll_local_view — compute
                // max_scroll LIVE from the layout cache instead of the
                // per-frame cached value. The cached metrics are None during
                // grid-mode execution (primary-screen TUI), which clamped to
                // 0 and kicked the PixelDelta fast path straight back to
                // FollowBottom — the trackpad's first flick then discarded
                // the scroll-up the user had just started.
                let max_scroll = self
                    .renderer
                    .as_ref()
                    .and_then(|renderer| {
                        let terminal = self.sessions.active().terminal.as_ref()?;
                        let pane_session_id = self.sessions.active().pane_session_id;
                        let (_total, _visible, max) =
                            renderer.block_scroll_metrics(terminal, pane_session_id);
                        Some(max)
                    })
                    .unwrap_or(0);
                self.sessions
                    .active_mut()
                    .scroll_block_fractional((pos.y / cell_height.max(1.0)) as f32, max_scroll);
                self.request_redraw();
                return;
            }
        }
        let rows = crate::scroll_input::terminal_scroll_rows(
            delta,
            phase,
            cell_height,
            &mut self.interaction.precise_scroll,
        );
        tracing::debug!(
            ?delta,
            ?phase,
            rows,
            residual = self.interaction.precise_scroll.residual_pixels(),
            "terminal scroll quantized"
        );
        if rows == 0 {
            return;
        }
        let up = rows > 0;
        let lines = rows.unsigned_abs() as usize;

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
            let mp = t.mouse_protocol();
            let sgr = t.sgr_mouse();
            (
                t.accepts_mouse_reporting_input(),
                t.is_alt_screen_active(),
                t.app_cursor_keys(),
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

        // Shift disables mouse routing for this gesture on the PRIMARY screen so
        // the user can browse terminal history even while a primary-screen TUI
        // captures the mouse. (Alt-screen Shift is handled in the alt block
        // below as an arrow-key escape hatch for pagers like less/man.)
        if self.interaction.mods.state().shift_key() && !alt_screen_active {
            self.scroll_local_view(rows);
            return;
        }

        // Check if mouse protocol is active — forward scroll to PTY
        if mouse_protocol_active {
            if !self.terminal_content_contains(x, y) {
                return;
            }
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
                let mut batch = Vec::with_capacity(bytes.len() * lines);
                for _ in 0..lines {
                    batch.extend_from_slice(&bytes);
                }
                let _ = self.sessions.active_mut().write_user_input(&batch);
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
            if !self.terminal_content_contains(x, y) {
                return;
            }
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

        // Alt-screen apps (less, vim, man, omp, …) don't use mouse protocol
        // but own the screen. v1.10.21: wheel routing lives in
        // `alt_wheel_controller` (pure table in `alt_peek::route`, Warp
        // parity — see FIX_ALT_PEEK_WARP_ALIGNMENT): plain wheels forward
        // arrows to the TUI, Shift+wheel-up enters the history peek, a
        // plain wheel inside the peek returns to the live TUI instantly.
        // Mouse-reporting apps and the primary screen never reach here.
        if alt_screen_active {
            self.handle_alt_screen_wheel(rows, lines, up);
            return;
        }

        // Otherwise, scroll the terminal viewport.
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
        if up {
            // v1.10.20: a wheel scroll while a grid selection is active must
            // go through the same migration entry point as the drag path —
            // entering the history view with a live grid selection would
            // orphan it (L3). Migration failure degrades: keep the grid
            // selection, do not scroll (would drift its anchor, L2).
            let (selecting, has_grid_selection, tui_active) = {
                let tab = self.sessions.active();
                (
                    tab.selection_handler.selecting,
                    tab.selection_handler.selection.is_some(),
                    tab.terminal
                        .as_ref()
                        .is_some_and(weft_core::vt::Terminal::primary_screen_app_active),
                )
            };
            if selecting && has_grid_selection && tui_active {
                if !self.migrate_grid_selection_to_primary_history() {
                    self.request_redraw();
                    return;
                }
            } else {
                let entered = self.sessions.active_mut().enter_primary_history_if_active();
                tracing::debug!(
                    entered,
                    rows,
                    app_active = tui_active,
                    "SCROLL_DIAG: scroll up into primary history"
                );
            }
        }
        let block_view = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .is_some_and(Terminal::show_block_view);
        if block_view {
            // A1: compute max_scroll LIVE from the layout cache. The per-frame
            // cached value is None during grid-mode execution (primary-screen TUI),
            // which would clamp to 0 and immediately exit the primary_history_view
            // just entered above — making scroll-up during a running command a
            // no-op. Live compute uses the retained cache (history is stable while
            // a command runs), so the wheel can actually detach into history.
            let max_scroll = self
                .renderer
                .as_ref()
                .and_then(|renderer| {
                    let terminal = self.sessions.active().terminal.as_ref()?;
                    let pane_session_id = self.sessions.active().pane_session_id;
                    let (_total, _visible, max) =
                        renderer.block_scroll_metrics(terminal, pane_session_id);
                    Some(max)
                })
                .unwrap_or(0);
            tracing::debug!(
                up,
                lines,
                max_scroll,
                current = self.sessions.active().block_scroll(),
                "SCROLL_DIAG: block-view scroll"
            );
            if up {
                let tab = self.sessions.active_mut();
                tab.scroll_up_by(lines);
                tab.clamp_block_scroll(max_scroll);
            } else {
                self.sessions.active_mut().scroll_down_by(lines);
            }
        } else {
            // Grid view scroll — needs mutable terminal.
            // v1.10.20 改动 4 (drift guard): a grid selection is
            // viewport-relative; scrolling would silently drift its anchor
            // (L2). No migration path exists here (non-TUI grid), so clear
            // the selection instead of corrupting the copy range. Only when
            // the offset actually moves — offset 0 + scroll-down is a no-op.
            let (had_selection, offset_will_change) = {
                let terminal = self.sessions.active().terminal.as_ref();
                match terminal {
                    Some(t) => {
                        let grid = t.grid();
                        let offset = grid.scroll_offset;
                        let max = grid.scrollback.len();
                        let new = if up {
                            (offset + lines).min(max)
                        } else {
                            offset.saturating_sub(lines)
                        };
                        (
                            self.sessions.active().selection_handler.selection.is_some(),
                            new != offset,
                        )
                    }
                    None => (false, false),
                }
            };
            if had_selection && offset_will_change {
                tracing::debug!("cleared grid selection before viewport scroll (drift guard)");
                self.sessions.active_mut().selection_handler.clear();
            }
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

    /// Edge-band overshoot for the drag-selection autoscroll. Returns which
    /// direction (relative to the content band) the pointer overshoots and by
    /// how many pixels — measured from the band threshold (one line outside
    /// the content edge in block view; the top content edge in grid mode).
    /// Shared by the move handler (which only ARMS the 40ms timer) and the
    /// timer pump (which scrolls + extends). `None` inside the band.
    pub(super) fn selection_autoscroll_overshoot(&self, y: f64) -> Option<(AutoscrollDir, f32)> {
        if !self.sessions.active().selection_handler.selecting {
            return None;
        }
        if self.block_view_active() {
            let (top, bottom) = self.block_content_vbounds()?;
            let ch = self
                .renderer
                .as_ref()
                .map(|r| r.cell_height() as f32)
                .unwrap_or(20.0);
            // v1.10.26 Batch D (D-4): the Down-band inner shift applies ONLY
            // in a snapshot/history view (primary_history_view or an active
            // alt peek), where no prompt chrome sits below the content. A
            // regular block view keeps the `bottom + ch` band — the prompt
            // chrome below makes it physically reachable even when the
            // geometry reads as full-bleed (a wide border/table fills the
            // pane), which otherwise mis-triggered a Down scroll from the
            // last visible line (v1.10.25 ML2).
            let snapshot_context =
                self.sessions
                    .active()
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| {
                        terminal.primary_history_view() || terminal.is_alt_screen_history_peek()
                    });
            let down_threshold = down_autoscroll_threshold(bottom, ch, snapshot_context);
            if (y as f32) < top - ch {
                Some((AutoscrollDir::Up, (top - ch) - y as f32))
            } else if (y as f32) > down_threshold {
                Some((AutoscrollDir::Down, y as f32 - down_threshold))
            } else {
                None
            }
        } else {
            // Grid mode (primary-screen TUI): only the top edge can trigger —
            // a drag held past it migrates into the primary history view.
            let tui_active = self
                .sessions
                .active()
                .terminal
                .as_ref()
                .is_some_and(Terminal::primary_screen_app_active);
            if !tui_active {
                return None;
            }
            let (top, _bottom) = self.grid_content_vbounds()?;
            let ch = self
                .renderer
                .as_ref()
                .map(|r| r.cell_height() as f32)
                .unwrap_or(20.0);
            if (y as f32) < top - ch {
                Some((AutoscrollDir::Up, (top - ch) - y as f32))
            } else {
                None
            }
        }
    }

    /// v1.10.26: arm/disarm the 40ms autoscroll timer from a mouse move. This
    /// is the ONLY thing a move does about autoscroll — the scroll itself
    /// happens on the timer (see `pump_selection_autoscroll`). Resets the
    /// fractional row carry when the pointer leaves the band.
    fn arm_selection_autoscroll(&mut self, y: f64) {
        if self.selection_autoscroll_overshoot(y).is_some() {
            self.window_runtime
                .selection_autoscroll_active
                .store(true, Ordering::Relaxed);
        } else {
            self.interaction.selection_autoscroll_carry = 0.0;
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
        }
    }

    /// Drag-selection autoscroll: when the pointer is held past the block
    /// content edge, scroll the viewport toward it and extend the selection
    /// endpoint to the newly revealed edge row. Returns true when a
    /// scroll+extend happened (the caller requests a redraw). v1.10.26
    /// (FIX_SELECTION_CONTENT_ANCHORS): called ONLY from the redraw entry on
    /// the 40ms timer wake — mouse-move events arm the timer
    /// (`arm_selection_autoscroll`) but never pump (the old move-driven pump
    /// stacked scrolls under event pressure: "停滞+瞬移"). Speed follows the
    /// Warp ramp — `autoscroll_ramp_rows(overshoot)` rows per tick, the
    /// fractional remainder carried so long drags accumulate sub-row progress.
    pub(super) fn pump_selection_autoscroll(&mut self) -> bool {
        let Some((x, y)) = self.interaction.selection_drag_pos else {
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
            return false;
        };
        if !self.block_view_active() {
            // v1.10.20: grid-mode drag autoscroll for primary-screen TUIs.
            // The grid selection is viewport-relative and cannot scroll; a
            // drag held past the top edge migrates the anchor into the
            // primary history snapshot view, where the block branch below
            // takes over (scroll + extend). Failure degrades: keep the grid
            // selection visible, never switch views and orphan it.
            if self.selection_autoscroll_overshoot(y).is_none() {
                self.window_runtime
                    .selection_autoscroll_active
                    .store(false, Ordering::Relaxed);
                return false;
            }
            if !self.migrate_grid_selection_to_primary_history() {
                tracing::warn!(
                    y,
                    "TUI drag autoscroll migration failed; keeping grid selection"
                );
                self.window_runtime
                    .selection_autoscroll_active
                    .store(false, Ordering::Relaxed);
                return false;
            }
        }
        let Some((dir, overshoot_px)) = self.selection_autoscroll_overshoot(y) else {
            self.window_runtime
                .selection_autoscroll_active
                .store(false, Ordering::Relaxed);
            return false;
        };
        // Live max_scroll (same A1 pattern as `scroll_local_view`): the
        // per-frame cached metric is None during grid-mode execution, which
        // would clamp to 0 and make the autoscroll a no-op.
        let max_scroll = self
            .renderer
            .as_ref()
            .and_then(|renderer| {
                let terminal = self.sessions.active().terminal.as_ref()?;
                let pane_session_id = self.sessions.active().pane_session_id;
                Some(renderer.block_scroll_metrics(terminal, pane_session_id).2)
            })
            .unwrap_or(0);
        // Warp ramp speed, whole rows with fractional carry.
        let raw = autoscroll_ramp_rows(overshoot_px);
        let total = self.interaction.selection_autoscroll_carry + raw;
        let steps = total.floor().max(1.0) as usize;
        self.interaction.selection_autoscroll_carry = total - steps as f32;
        // At the history top/bottom the saturating scroll has no net effect;
        // detect that so the 40ms timer can stop instead of idle-spinning
        // (before = immutable read, then the mutable scroll, then re-read).
        let before = self.sessions.active().block_scroll();
        let tab = self.sessions.active_mut();
        match dir {
            AutoscrollDir::Up => {
                tab.scroll_up_by(steps);
                tab.clamp_block_scroll(max_scroll);
            }
            AutoscrollDir::Down => {
                tab.scroll_down_by(steps);
                tab.clamp_block_scroll(max_scroll);
            }
        }
        let moved = before != self.sessions.active().block_scroll();
        // Extend the endpoint: clamp y into the content band so the shared
        // row hit-test naturally lands on the new edge row.
        let (top, bottom) = self.block_content_vbounds().unwrap_or((0.0, 0.0));
        let cy = (y as f32).clamp(top, (bottom - 1.0).max(top));
        if let Some(anchor) = self.pixel_to_block_view_pos(x, cy as f64) {
            self.sessions
                .active_mut()
                .selection_handler
                .extend_block_view(anchor);
        }
        self.window_runtime
            .selection_autoscroll_active
            .store(moved, Ordering::Relaxed);
        true
    }
}
