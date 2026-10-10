// Mouse move/drag/scroll/context-menu dispatch. Grew with F3 sidebar resize
// + panel virtualization scroll handling; remaining size is the irreducible
// per-event-type dispatch (move/press/release/wheel) with overlay-specific
// branches.
//! Mouse movement, drag, context-menu, and scroll controller.

use super::*;

mod scroll;

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
            if let Some(mut t) = release_tab
                .and_then(|tab| self.sessions.tab_mut(tab))
                .and_then(|tab| tab.lock_terminal())
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
            if let Some(tab) = self.sessions.active_mut() {
                tab.selection_handler.end();
                tab.sync_primary_history_view();
            }
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
            if let Some(tab) = self.sessions.active_mut() {
                tab.set_block_scroll(offset);
            }
            self.request_redraw();
            return;
        }
        // v1.0 fix: sync mouse_protocol + sgr_mouse (see handle_mouse_press)
        // so move-event encoding (ButtonEvent/AnyEvent drag reporting) reflects
        // the app's actual mouse mode and report format.
        let modes = self
            .sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| (t.mouse_protocol(), t.sgr_mouse())));
        if let Some((mp, sgr)) = modes {
            if let Some(tab) = self.sessions.active_mut() {
                tab.input_handler.mouse_protocol = mp;
                tab.input_handler.sgr_mouse = sgr;
            }
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
                if let Some(mut t) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.lock_terminal())
                {
                    t.editor_mut().buffer.extend_selection(pos);
                    self.request_redraw();
                }
            }
        }

        if self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.selection_handler.selecting)
        {
            if self.block_view_active() {
                // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): mouse-move NO
                // LONGER pumps the autoscroll — a stream of move events each
                // scrolling+extending stacked scrolls (the "停滞+瞬移" event
                // backlog source). Move only records the pointer for the 40ms
                // timer and extends the endpoint when the pointer is inside
                // the content band; the 40ms timer is the SOLE scroll driver.
                self.interaction.selection_drag_pos = Some((x, y));
                if let Some(anchor) = self.pixel_to_block_view_pos(x, y) {
                    if let Some(tab) = self.sessions.active_mut() {
                        tab.selection_handler.extend_block_view(anchor);
                    }
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
                    if let Some(tab) = self.sessions.active_mut() {
                        tab.selection_handler.extend(pos);
                    }
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
        let hit = self.context_menu_hit_at_anchor(menu.x, menu.y, click_x, click_y);
        if let Some(i) = hit {
            let action = CONTEXT_MENU_ITEMS[i].1;
            self.run_context_action(menu.block_id, action);
            self.request_redraw();
            return;
        }
        // Click outside menu items — just close (already taken).
        self.request_redraw();
    }

    /// v1.10.34: context menu hover — move the highlighted item as the
    /// pointer slides across the menu, mirroring the keyboard `Select` path
    /// (context_menu_key_action). Before this, the highlight stayed on the
    /// initial item because CursorMoved with an open menu was consumed by
    /// route_modal_pointer and never reached any hover logic.
    pub(super) fn update_context_menu_hover(&mut self, x: f64, y: f64) {
        let Some((menu_x, menu_y)) = self.interaction.context_menu.as_ref().map(|m| (m.x, m.y))
        else {
            return;
        };
        let hit = self.context_menu_hit_at_anchor(menu_x, menu_y, x as f32, y as f32);
        if let Some(i) = hit {
            if let Some(menu) = self.interaction.context_menu.as_mut() {
                if menu.selection != i {
                    menu.selection = i;
                    self.request_redraw();
                }
            }
        }
    }

    /// Shared hit-test: rebuild the context menu Scene from the menu's
    /// anchor + renderer geometry and resolve which item (if any) contains
    /// the given point. Used by both the click executor and the hover path.
    fn context_menu_hit_at_anchor(
        &self,
        menu_x: f32,
        menu_y: f32,
        x: f32,
        y: f32,
    ) -> Option<usize> {
        self.renderer.as_ref().and_then(|renderer| {
            let ctx = renderer.layout_ctx?;
            let layout =
                crate::layout::layout_context_menu(&ctx, menu_x, menu_y, renderer.scale() as f32);
            let scene =
                crate::context_menu_component::build_context_menu_scene(layout, CONTEXT_MENU_ITEMS);
            crate::context_menu_component::context_menu_item_at(&scene, x, y)
        })
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
            let Some(mut terminal) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            else {
                // Annotation-only actions (toggle_bookmark / add_note) don't
                // need the terminal — fall through to the post-borrow block.
                if matches!(action, "toggle_bookmark" | "add_note") {
                    self.run_annotation_action(block_id, action);
                }
                return;
            };

            match action {
                "copy_command" | "copy_output" | "copy_block" if block_id.is_none() => {
                    if let Some(live) = terminal.block_tracker().in_flight() {
                        let text = if action == "copy_command" {
                            live.command.to_string()
                        } else if action == "copy_output" {
                            live.output.to_string()
                        } else {
                            // v1.10.34: combined copy — cwd + command + output.
                            weft_core::blocks::format_block_for_copy(
                                live.cwd,
                                live.command,
                                live.output,
                            )
                        };
                        if !text.is_empty() {
                            clipboard_text = Some(text);
                        }
                    }
                }
                "copy_command" | "copy_output" | "copy_block" => {
                    let bid = block_id.expect("copy branch has a finalized block id");
                    let block = terminal
                        .block_tracker()
                        .session_blocks()
                        .iter()
                        .find(|b| b.id == bid);
                    if let Some(block) = block {
                        let text = if action == "copy_command" {
                            block.command.clone()
                        } else if action == "copy_output" {
                            block.output.to_string()
                        } else {
                            weft_core::blocks::format_block_for_copy(
                                block.cwd.as_deref(),
                                &block.command,
                                &block.output,
                            )
                        };
                        if !text.is_empty() {
                            clipboard_text = Some(text);
                        }
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
        // M5-b P2-2: toggle_fold needs no cache invalidation — the collapsed
        // mismatch routes ensure_cached into the WidthOnly rebuild (L1
        // reused, L2 rebuilt); invalidate() would force a full L1
        // re-enumeration of the block's output. Redraw comes from
        // execute_context_menu's unconditional tail.
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
}
