//! Mouse press routing controller.

use super::*;

impl App {
    pub(super) fn handle_mouse_press(&mut self, x: f64, y: f64, button: winit::event::MouseButton) {
        // v1.0 fix: sync InputHandler.mouse_protocol + sgr_mouse from the
        // Terminal's VT-parsed values before any mouse-event encoding. Without
        // this the handler's copy stays `Off` (its setters are test-only) and
        // `encode_mouse` returns None — mouse-aware apps (vim `set mouse=a`,
        // tmux, htop) never receive clicks/drags. sgr_mouse selects the report
        // format (SGR-1006 vs legacy) — sending the wrong format corrupts the
        // app (vim `~@k`). Mirrors the scroll-path sync.
        if let Some(t) = &self.sessions.tabs[self.sessions.active_tab].terminal {
            let (mp, sgr) = (t.mouse_protocol, t.sgr_mouse);
            self.sessions.tabs[self.sessions.active_tab]
                .input_handler
                .mouse_protocol = mp;
            self.sessions.tabs[self.sessions.active_tab]
                .input_handler
                .sgr_mouse = sgr;
        }
        // v1.0 S1-b: Settings panel click handling — checked first so
        // settings clicks work even inside TUI apps that captured the mouse
        // (the panel is modal and overlays everything). When the panel is
        // open, ALL left-clicks are either dispatched to a hit region or
        // consumed (clicks outside any region do nothing — they don't fall
        // through to the terminal / PTY).
        if button == winit::event::MouseButton::Left && self.settings.open {
            let xf = x as f32;
            let yf = y as f32;
            use crate::settings_component::SettingsTarget;
            match self.settings_target_at(xf, yf) {
                Some(SettingsTarget::Tab(tab)) => {
                    if self.settings.tab != tab {
                        self.settings.tab = tab;
                        self.settings.selection = 0;
                        self.settings.scroll_offset = 0;
                    }
                    self.request_redraw();
                    return;
                }
                Some(SettingsTarget::Theme(i)) => {
                    self.settings.selection = i;
                    self.apply_settings_selection();
                    self.request_redraw();
                    return;
                }
                Some(SettingsTarget::CloseButton) => {
                    self.settings.open = false;
                    self.settings.error = None;
                    self.request_redraw();
                    return;
                }
                Some(SettingsTarget::SaveButton) => {
                    self.save_settings_draft(true);
                    self.request_redraw();
                    return;
                }
                Some(SettingsTarget::ApplyButton) => {
                    self.save_settings_draft(false);
                    self.request_redraw();
                    return;
                }
                None => {
                    // Distinguish "inside panel box but missed all hits"
                    // (consume) from "outside panel" (close). We rebuild the
                    // layout just for the box rect — the hit test already
                    // failed so this is cheap.
                    if self.point_inside_settings_box(xf, yf) {
                        return;
                    }
                    // Click outside the panel — close it (Warp-style).
                    self.settings.open = false;
                    self.settings.error = None;
                    self.request_redraw();
                    return;
                }
            }
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
                            if idx == self.sessions.active_tab {
                                let effects = self.close_tab();
                                self.drain_effects(effects);
                            } else {
                                // Close a background tab — remove and adjust index.
                                self.sessions.tabs.remove(idx);
                                if idx < self.sessions.active_tab {
                                    self.sessions.active_tab -= 1;
                                }
                                self.tab_bar.hovered_tab = None;
                                self.request_redraw();
                                self.drain_effects(vec![crate::effect::Effect::PersistTabs]);
                            }
                            return;
                        }
                        Some(crate::tab_bar_component::TabBarTarget::Tab(hit_index)) => {
                            if self.sessions.active_tab != hit_index {
                                self.reset_ime_context("tab clicked");
                                self.sessions.active_tab = hit_index;
                                self.refresh_find_for_active_tab();
                            }
                            self.tab_bar.hovered_tab = None;
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
                    self.sessions.tabs[self.sessions.active_tab].block_scroll_offset = offset;
                    self.interaction.scrollbar_drag =
                        Some(crate::scrollbar_component::ScrollbarDragState {
                            layout,
                            grab_offset,
                        });
                    self.interaction.scrollbar_hovered = true;
                    if let Some(window) = &self.window {
                        window.set_cursor(winit::window::CursorIcon::NsResize);
                    }
                    self.sessions.tabs[self.sessions.active_tab]
                        .selection_handler
                        .clear();
                    self.request_redraw();
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
                    .terminal_layout()
                    .map(|layout| layout.content.left as f32)
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
                                if let Some(term) = self.sessions.tabs[self.sessions.active_tab]
                                    .terminal
                                    .as_mut()
                                {
                                    term.block_tracker_mut().toggle_collapse(bid);
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
                open_url(&url);
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
                        self.find.regex_mode = !self.find.regex_mode;
                        // Force immediate re-search so toggle is reflected
                        // (matches ToggleCase behavior — without this, typing
                        // the regex first and then toggling .* won't apply
                        // the regex mode to the existing query).
                        self.find.last_key = Some(std::time::Instant::now());
                        self.request_redraw();
                    }
                    FindButtonAction::ToggleCase => {
                        self.find.case_sensitive = !self.find.case_sensitive;
                        // Force immediate re-search so toggle is reflected.
                        self.find.last_key = Some(std::time::Instant::now());
                        self.request_redraw();
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
                return;
            }
        }

        // If context menu is open, handle click as menu selection.
        // (v0.9 fix: removed the "click any block to fold" handler that
        // prevented text selection on block output. Folding is now solely
        // via the chevron click handler above — W3.)
        if button == winit::event::MouseButton::Left {
            if let Some(menu) = self.interaction.context_menu.take() {
                self.execute_context_menu(&menu, x as f32, y as f32);
                return;
            }
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
                    if let Some(t) = self.sessions.tabs[self.sessions.active_tab]
                        .terminal
                        .as_mut()
                    {
                        t.editor_mut().buffer.start_selection(pos);
                    }
                    self.interaction.prompt_dragging = true;
                    // Clear any block/grid selection so Cmd+C targets the editor.
                    self.sessions.tabs[self.sessions.active_tab]
                        .selection_handler
                        .clear();
                    self.request_redraw();
                }
                return;
            } else if let Some(t) = self.sessions.tabs[self.sessions.active_tab]
                .terminal
                .as_mut()
            {
                if t.editor().buffer.has_selection() {
                    t.editor_mut().buffer.clear_selection();
                    self.request_redraw();
                }
            }
            self.interaction.prompt_dragging = false;
        }

        let selecting = !self.mouse_reporting_active();
        let block_view = self.block_view_active();

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
                            self.sessions.tabs[self.sessions.active_tab]
                                .selection_handler
                                .start_block_view(bv_pos, rows_snapshot);
                        } else {
                            // Click missed every selectable row (e.g. on the
                            // prompt box, CWD bar, or empty padding). Clear the
                            // existing selection so the user gets visual
                            // feedback that the previous selection is gone.
                            self.sessions.tabs[self.sessions.active_tab]
                                .selection_handler
                                .clear();
                        }
                    } else {
                        // Grid view (alt-screen): classic grid selection.
                        let pos = self.pixel_to_grid(x, y);
                        let mode = if self.interaction.mods.state().shift_key() {
                            SelectionMode::Block
                        } else {
                            SelectionMode::Simple
                        };
                        self.sessions.tabs[self.sessions.active_tab]
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
                    tab: self.sessions.active_tab,
                }]);
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                // If context menu is open, right-click closes it.
                if self.interaction.context_menu.is_some() {
                    self.interaction.context_menu = None;
                    self.request_redraw();
                    return;
                }

                // Block view: open context menu on a block. block_at now
                // supports both completed blocks (including those with no
                // output) and the in-flight (running) command.
                if let Some(id) = self.block_at(y as f32) {
                    self.interaction.context_menu = Some(ContextMenu {
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
                            if self.sessions.tabs[self.sessions.active_tab]
                                .selection_handler
                                .block_view_selection
                                .is_none()
                            {
                                let rows_snapshot = self.compute_block_view_rows();
                                self.sessions.tabs[self.sessions.active_tab]
                                    .selection_handler
                                    .start_block_view(bv_pos, rows_snapshot);
                            } else {
                                self.sessions.tabs[self.sessions.active_tab]
                                    .selection_handler
                                    .extend_block_view(bv_pos);
                            }
                        }
                    } else {
                        let pos = self.pixel_to_grid(x, y);
                        if self.sessions.tabs[self.sessions.active_tab]
                            .selection_handler
                            .selection
                            .is_none()
                        {
                            self.sessions.tabs[self.sessions.active_tab]
                                .selection_handler
                                .start(pos, SelectionMode::Simple);
                        } else {
                            self.sessions.tabs[self.sessions.active_tab]
                                .selection_handler
                                .extend(pos);
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
