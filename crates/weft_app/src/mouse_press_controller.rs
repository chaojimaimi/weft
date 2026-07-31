//! Mouse press routing controller.

use super::*;

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

    pub(super) fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        match crate::input_router::route_modal_mouse(
            self.palette.open,
            self.settings.open,
            self.interaction.context_menu.is_some(),
            button,
        ) {
            crate::input_router::ModalMouseRoute::PaletteLeft => {}
            crate::input_router::ModalMouseRoute::SettingsLeft => {
                self.handle_settings_mouse_press(x as f32, y as f32);
                return;
            }
            crate::input_router::ModalMouseRoute::ContextMenuLeft => {
                if let Some(menu) = self.take_context_menu("context menu closed by click") {
                    self.execute_context_menu(&menu, x as f32, y as f32);
                }
                return;
            }
            crate::input_router::ModalMouseRoute::DismissContextMenu => {
                self.take_context_menu("context menu dismissed by mouse");
                self.request_redraw();
                return;
            }
            crate::input_router::ModalMouseRoute::Consume => return,
            crate::input_router::ModalMouseRoute::Terminal => {}
        }

        if crate::input_router::route_session_input(!self.sessions.is_empty())
            == crate::input_router::SessionInputRoute::Consume
        {
            return;
        }

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
                return;
            }
        }

        // v1.3 Batch 6: focus the pane under the cursor on click. This
        // switches the active pane BEFORE the rest of the mouse handling
        // (which all goes through `active_mut()`), so clicks/selections/
        // PTY mouse events route to the clicked pane. For single-pane tabs
        // `pane_at_pixel` always returns the one pane id — no-op switch.
        if let Some(pane_id) = self.pane_at_pixel(x, y) {
            let tab = self.sessions.active_mut();
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
                    return;
                }
            }
        }

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
            .terminal
            .as_ref()
            .map(|t| (t.mouse_protocol(), t.sgr_mouse()));
        if let Some((mp, sgr)) = modes {
            self.sessions.active_mut().input_handler.mouse_protocol = mp;
            self.sessions.active_mut().input_handler.sgr_mouse = sgr;
        }
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
                        return;
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
                            return;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::ArrowRight) => {
                            let cw = renderer.cell_width() as f32;
                            self.tab_bar.scroll_offset += cw * 15.0;
                            self.clamp_tab_scroll();
                            self.request_redraw();
                            return;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::Close(idx)) => {
                            if idx == self.sessions.active_idx() {
                                let effects = self.close_active_tab_with_confirmation();
                                self.drain_effects(effects);
                            } else {
                                if !self.confirm_background_tab_close(idx) {
                                    return;
                                }
                                // Close a background tab — remove and adjust index.
                                let blocks = self
                                    .sessions
                                    .tab_mut(idx)
                                    .map(crate::tab::Tab::finish_pending_blocks)
                                    .unwrap_or_default();
                                self.sessions.close_background(idx);
                                self.tab_bar.hovered_tab = None;
                                self.request_redraw();
                                let mut effects = Vec::new();
                                if !blocks.is_empty() {
                                    effects.push(crate::effect::Effect::PersistBlocks { blocks });
                                }
                                effects.push(crate::effect::Effect::PersistTabs);
                                self.drain_effects(effects);
                            }
                            return;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::Tab(hit_index)) => {
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
                            self.request_redraw();
                            return;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::NewTab) => {
                            self.new_tab();
                            self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
                            return;
                        }
                        None => {}
                    }
                    // v1.1: Click in the tab-bar background (not on any tab,
                    // not on the traffic lights). This is a draggable region
                    // (winit's native drag_window handles the drag). Detect a
                    // double-click here to toggle maximize, matching the macOS
                    // native titlebar double-click behavior.
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
                    return;
                }
            }
        }

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
                return;
            }
        }
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
                    return;
                }
            }
        }
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
                    return;
                }
                Some(crate::panel_component::PanelTarget::SearchField) => {
                    // Click in the search input field: focus it so keyboard
                    // input goes to panel_query (bug 6 fix).
                    self.panel.search_focused = true;
                    self.request_redraw();
                    return;
                }
                None => {
                    // Click outside the panel (or below the last row):
                    // unfocus search (but keep panel open).
                    if self.panel.search_focused {
                        self.panel.search_focused = false;
                        self.request_redraw();
                    }
                }
            }
        }

        // The block-view scrollbar owns its expanded hit strip before text
        // selection and PTY mouse reporting. Clicking the track jumps the
        // thumb under the pointer and immediately begins a drag.
        if button == winit::event::MouseButton::Left {
            if let Some(layout) = self.active_scrollbar_layout() {
                let xf = x as f32;
                let yf = y as f32;
                if crate::scrollbar_component::contains(layout.hit, xf, yf) {
                    let thumb_h = layout.thumb[3] - layout.thumb[1];
                    let grab_offset =
                        crate::scrollbar_component::thumb_grab_offset(&layout, xf, yf, true)
                            .unwrap_or(thumb_h / 2.0);
                    let offset = crate::scrollbar_component::scroll_offset_for_pointer(
                        &layout,
                        yf,
                        grab_offset,
                    );
                    self.sessions.active_mut().set_block_scroll(offset);
                    self.interaction.scrollbar_drag =
                        Some(crate::scrollbar_component::ScrollbarDragState {
                            layout,
                            grab_offset,
                        });
                    self.interaction.scrollbar_hovered = true;
                    if let Some(window) = &self.window {
                        window.set_cursor(winit::window::CursorIcon::NsResize);
                    }
                    self.sessions.active_mut().selection_handler.clear();
                    self.request_redraw();
                    return;
                }
            }
        }

        // F3-1: Block header hover-action buttons (copy / fold). These are
        // registered as HitRegions during draw() on the header row's right
        // side. Check the renderer's cached hit_regions BEFORE the chevron
        // handler so clicks on the small action buttons don't fall through to
        // text selection. Buttons only render when the block is hovered, but
        // the hit regions are always registered (so clicks work even if the
        // hover state lagged behind by a frame).
        if button == winit::event::MouseButton::Left && self.block_view_active() {
            if let Some(renderer) = &self.renderer {
                let xf = x as f32;
                let yf = y as f32;
                let hit =
                    crate::block_component::block_header_action_at(&renderer.hit_regions, xf, yf);
                if let Some(hit) = hit {
                    match hit {
                        crate::block_component::BlockHeaderAction::Copy(id) => {
                            self.run_context_action(Some(id), "copy_command");
                            self.request_redraw();
                        }
                        crate::block_component::BlockHeaderAction::ToggleFold(id) => {
                            self.run_context_action(Some(id), "toggle_fold");
                            self.request_redraw();
                        }
                        crate::block_component::BlockHeaderAction::Diagnose(id) => {
                            // v1.8.2: Trigger AI diagnose for this failed block.
                            self.spawn_block_diagnose(id);
                        }
                        crate::block_component::BlockHeaderAction::CloseDiagnose(id) => {
                            // v1.8.2: Close the diagnose panel for this block.
                            self.close_block_diagnose(id);
                        }
                    }
                    return;
                }
            }
        }

        // v0.9 W3 (revised): block collapse/expand — only clicking the chevron
        // (▸/▾ in the first cell of a Command row) toggles fold. Clicking the
        // rest of the command line starts a normal text selection instead, so
        // the user can select/copy command text. This reverts the earlier
        // "click anywhere on the command line folds" behavior.
        if button == winit::event::MouseButton::Left && self.block_view_active() {
            if let Some(renderer) = &self.renderer {
                let content_left = self
                    .renderer
                    .as_ref()
                    .and_then(|renderer| renderer.layout_ctx)
                    .map(|ctx| ctx.left())
                    .unwrap_or_else(|| renderer.padding_x());
                let cw = renderer.cell_width() as f32;
                let xf = x as f32;
                let yf = y as f32;
                // Chevron occupies the first cell [content_left, content_left + cw).
                if xf >= content_left && xf < content_left + cw {
                    for row in &self.compute_block_view_rows() {
                        if row.kind == weft_core::selection::BlockViewRowKind::Command
                            && yf >= row.y_top
                            && yf < row.y_bottom
                        {
                            if let Some(bid) = row.block_id {
                                if let Some(term) = self.sessions.active_mut().terminal.as_mut() {
                                    term.block_tracker_mut().toggle_collapse(bid);
                                    if let Some(renderer) = &self.renderer {
                                        renderer.block_layout_cache.borrow_mut().invalidate(bid.0);
                                    }
                                    self.request_redraw();
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }

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
                return;
            }
        }

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
                return;
            }
        }

        // Check for popup border drag (completion or palette).
        if button == winit::event::MouseButton::Left {
            if let Some(drag) = self.check_popup_border_drag(x, y) {
                self.interaction.drag_state = Some(drag);
                return;
            }
        }

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
            return;
        }

        // v0.9: click inside the prompt input box → position the editor
        // cursor at the clicked char and start a mouse-drag selection (so
        // the user can select/copy part of the command). Clicks outside the
        // prompt box clear any active editor selection.
        if button == winit::event::MouseButton::Left {
            let in_prompt = self
                .prompt_box_rect()
                .map(|[x0, y0, x1, y1]| {
                    let xf = x as f32;
                    let yf = y as f32;
                    xf >= x0 && xf <= x1 && yf >= y0 && yf <= y1
                })
                .unwrap_or(false);
            if in_prompt {
                if let Some(pos) = self.pixel_to_editor_pos(x, y) {
                    if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                        t.editor_mut().buffer.start_selection(pos);
                    }
                    self.interaction.prompt_dragging = true;
                    // Clear any block/grid selection so Cmd+C targets the editor.
                    self.sessions.active_mut().selection_handler.clear();
                    self.request_redraw();
                }
                return;
            } else if let Some(t) = self.sessions.active_mut().terminal.as_mut() {
                if t.editor().buffer.has_selection() {
                    t.editor_mut().buffer.clear_selection();
                    self.request_redraw();
                }
            }
            self.interaction.prompt_dragging = false;
        }

        let selecting = !self.mouse_reporting_active();
        let block_view = self.block_view_active();

        if button == winit::event::MouseButton::Left && block_view {
            let selected = self.block_at(y as f32).flatten();
            if selected != self.interaction.block_selected {
                self.interaction.block_selected = selected;
                self.request_redraw();
            }
        }

        // Host chrome is not a clamped alias for Grid row 0. Consume presses
        // there before selection, paste, context-menu or PTY mouse routing.
        // BlockView has its own row model.
        if !block_view && !self.terminal_content_contains(x, y) {
            return;
        }

        match button {
            winit::event::MouseButton::Left => {
                if selecting {
                    if block_view {
                        // Block view: hit-test against the cached visible-row
                        // snapshot and start a block-view selection. The row
                        // snapshot is cloned so the selection stays consistent
                        // with what the user saw at drag start, even if a PTY
                        // update re-lays-out the view mid-drag.
                        if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                            let rows_snapshot = self.compute_block_view_rows();
                            self.sessions
                                .active_mut()
                                .selection_handler
                                .start_block_view(bv_pos, rows_snapshot);
                        } else {
                            // Click missed every selectable row (e.g. on the
                            // prompt box, CWD bar, or empty padding). Clear the
                            // existing selection so the user gets visual
                            // feedback that the previous selection is gone.
                            self.sessions.active_mut().selection_handler.clear();
                        }
                    } else {
                        // Grid view (alt-screen): classic grid selection.
                        let pos = self.pixel_to_grid(x, y);
                        let mode = if self.interaction.mods.state().shift_key() {
                            SelectionMode::Block
                        } else {
                            SelectionMode::Simple
                        };
                        self.sessions
                            .active_mut()
                            .selection_handler
                            .start(pos, mode);
                    }
                }

                // If mouse protocol is active, send mouse event to PTY.
                // Always compute a grid pos for PTY mouse reporting (the
                // foreground program speaks grid coordinates, not block rows).
                let grid_pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Left, MouseAction::Press, grid_pos);
            }
            winit::event::MouseButton::Middle => {
                // Middle click: paste
                self.drain_effects(vec![crate::effect::Effect::Paste {
                    tab: self.sessions.active_idx(),
                }]);
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                // Block view: open context menu on a block. block_at now
                // supports both completed blocks (including those with no
                // output) and the in-flight (running) command.
                if let Some(id) = self.block_at(y as f32) {
                    self.reset_ime_context("context menu opened");
                    self.save_focus_for_modal(crate::scene::FocusId::ContextMenu);
                    self.interaction.context_menu = Some(ContextMenu {
                        session_id: self.sessions.active().session_id,
                        block_id: id,
                        x: x as f32,
                        y: y as f32,
                        selection: 0,
                    });
                    self.request_redraw();
                    return;
                }

                if selecting {
                    // Right click: extend selection.
                    if block_view {
                        if let Some(bv_pos) = self.pixel_to_block_view_pos(x, y) {
                            if self
                                .sessions
                                .active_mut()
                                .selection_handler
                                .block_view_selection
                                .is_none()
                            {
                                let rows_snapshot = self.compute_block_view_rows();
                                self.sessions
                                    .active_mut()
                                    .selection_handler
                                    .start_block_view(bv_pos, rows_snapshot);
                            } else {
                                self.sessions
                                    .active_mut()
                                    .selection_handler
                                    .extend_block_view(bv_pos);
                            }
                        }
                    } else {
                        let pos = self.pixel_to_grid(x, y);
                        if self
                            .sessions
                            .active_mut()
                            .selection_handler
                            .selection
                            .is_none()
                        {
                            self.sessions
                                .active_mut()
                                .selection_handler
                                .start(pos, SelectionMode::Simple);
                        } else {
                            self.sessions.active_mut().selection_handler.extend(pos);
                        }
                    }
                }
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Right, MouseAction::Press, pos);
            }
            _ => {}
        }

        self.request_redraw();
    }
}
