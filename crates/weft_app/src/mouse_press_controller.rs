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
            if let Some(renderer) = &self.renderer {
                let xf = x as f32;
                let yf = y as f32;
                for hit in &renderer.settings_hits {
                    let [x0, y0, x1, y1] = hit.rect;
                    if xf >= x0 && xf < x1 && yf >= y0 && yf < y1 {
                        use crate::renderer::SettingsHitKind;
                        match hit.kind {
                            SettingsHitKind::Tab(tab) => {
                                if self.settings.tab != tab {
                                    self.settings.tab = tab;
                                    self.settings.selection = 0;
                                    self.settings.scroll_offset = 0;
                                }
                            }
                            SettingsHitKind::Theme(i) => {
                                self.settings.selection = i;
                                self.apply_settings_selection();
                            }
                            SettingsHitKind::CloseButton => {
                                self.settings.open = false;
                                self.settings.error = None;
                            }
                            SettingsHitKind::SaveButton => {
                                self.save_settings_draft(true);
                            }
                            SettingsHitKind::ApplyButton => {
                                self.save_settings_draft(false);
                            }
                        }
                        self.request_redraw();
                        return;
                    }
                }
                // Click inside the panel's bounding box but not on any
                // hit region — still consume the event so the click doesn't
                // fall through to the terminal underneath.
                if let Some([bx0, by0, bx1, by1]) = renderer.settings_popup_rect {
                    if xf >= bx0 && xf < bx1 && yf >= by0 && yf < by1 {
                        return;
                    }
                }
                // Click outside the panel — close it (Warp-style: clicking
                // outside dismisses modal overlays). This matches the
                // behavior of the Command Palette.
                self.settings.open = false;
                self.settings.error = None;
                self.request_redraw();
                return;
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
                                if self.close_tab() {
                                    // App continues with remaining tabs.
                                }
                            } else {
                                // Close a background tab — remove and adjust index.
                                self.sessions.tabs.remove(idx);
                                if idx < self.sessions.active_tab {
                                    self.sessions.active_tab -= 1;
                                }
                                self.tab_bar.hovered_tab = None;
                                self.request_redraw();
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
                            return;
                        }
                        None => {}
                    }
                    // v1.1: Click in the tab-bar background (not on any tab,
                    // not on the traffic lights). This is a draggable region
                    // (movableByWindowBackground handles the drag). Detect a
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
                    }
                    return;
                }
            }
        }

        // v0.9 W2: history panel click → select row + scroll terminal to block.
        // Handled before PTY mouse reporting so panel clicks work even inside
        // TUI apps that captured the mouse.
        if button == winit::event::MouseButton::Left && self.panel.open {
            if let Some(renderer) = &self.renderer {
                // v0.9 W5: panel is now a LEFT sidebar anchored at x = 0 with
                // width = sidebar_width().
                let width_px = renderer.sidebar_width();
                let panel_x = 0.0;
                let ch = renderer.cell_height() as f64;
                // v0.9 fix: list_top must include chrome_top (tab bar height)
                // to match the renderer's panel content offset. Without this,
                // row clicks were misaligned by one tab-bar height.
                let chrome_top = renderer.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0) as f64;
                let xf = x as f32;
                let yf = y;
                if xf >= panel_x && xf < panel_x + width_px && yf > 0.0 {
                    // v0.9 fix: match the renderer's Warp-style panel layout:
                    //   header  at chrome_top + ch*0.4
                    //   search  at chrome_top + ch*1.6, height ch*1.4
                    //   list    at chrome_top + ch*1.6 + ch*1.4 + ch*0.4
                    let field_pad_y = ch * 1.6;
                    let field_h = ch * 1.4;
                    let search_top = chrome_top + field_pad_y;
                    let search_bottom = chrome_top + field_pad_y + field_h;
                    let list_top = chrome_top + field_pad_y + field_h + ch * 0.4;
                    let row_h = ch * 1.1;
                    if yf >= list_top {
                        // Click on a history row: select it AND focus the
                        // panel so Up/Down keys navigate the list (Warp-style).
                        // Single click only selects + scrolls + highlights
                        // the block; double-click (or Enter) sends the command
                        // to the prompt editor.
                        self.panel.search_focused = true;
                        let clicked = ((yf - list_top) / row_h) as usize;
                        let max_rows =
                            visible_panel_rows(renderer.viewport().1, renderer.cell_height());
                        if clicked < max_rows {
                            // v0.9: detect double-click on the same row.
                            let now = std::time::Instant::now();
                            let is_double = self
                                .panel
                                .last_click
                                .map(|(t, row)| {
                                    t.elapsed() < std::time::Duration::from_millis(400)
                                        && row == clicked
                                })
                                .unwrap_or(false);
                            self.panel.last_click = Some((now, clicked));
                            self.panel.selection = clicked;
                            self.clamp_panel_selection();
                            // Scroll terminal to the selected block + highlight.
                            self.scroll_to_panel_selection();
                            if is_double {
                                // Double-click: send the command to the prompt.
                                self.send_panel_selection_to_input();
                            }
                            return;
                        }
                    } else if yf >= search_top && yf < search_bottom {
                        // Click in the search input field: focus it so keyboard
                        // input goes to panel_query (bug 6 fix).
                        self.panel.search_focused = true;
                        self.request_redraw();
                        return;
                    }
                } else {
                    // Click outside the panel: unfocus search (but keep panel open).
                    if self.panel.search_focused {
                        self.panel.search_focused = false;
                        self.request_redraw();
                    }
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
                    for row in &renderer.block_view_rows {
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
                .renderer
                .as_ref()
                .and_then(|r| r.prompt_box_rect.get())
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
                            let rows_snapshot = self
                                .renderer
                                .as_ref()
                                .map(|r| r.block_view_rows.clone())
                                .unwrap_or_default();
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
                self.paste_from_clipboard();
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
                                let rows_snapshot = self
                                    .renderer
                                    .as_ref()
                                    .map(|r| r.block_view_rows.clone())
                                    .unwrap_or_default();
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
