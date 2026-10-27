// Mouse move/drag/scroll/context-menu dispatch. Grew with F3 sidebar resize
// + panel virtualization scroll handling; remaining size is the irreducible
// per-event-type dispatch (move/press/release/wheel) with overlay-specific
// branches.
//! Mouse movement, drag, context-menu, and scroll controller.

use super::*;

// v1.14.0 (rust-reviewer batch split): the drag-selection autoscroll pump,
// moved verbatim out of scroll.rs (800-line gate).
mod autoscroll;
// v1.15.2: context-menu family, moved verbatim (zero-rewrite).
mod context_menu;
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
                    // v1.14.0 (PLAN_v1140 T3): commit reorders the tab Vec —
                    // index-keyed tweens lose their meaning, and the
                    // post-commit natural slots equal the tween targets, so
                    // clearing lands seamlessly (提交时全部清空吸附).
                    self.clear_tab_bar_tweens();
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
            // v1.14.2 (A2): 终值必写 — a width parked by the drag throttle
            // lands before persistence, then the throttle state clears
            // (panel_controller::drag_apply_step parks in-window widths;
            // without this the release could persist a stale value).
            let applied = self
                .renderer
                .as_ref()
                .and_then(|r| r.sidebar_width_override);
            let pending = self.panel.pending_drag_width.take();
            self.panel.sidebar_drag_last_apply = None;
            let new_width = crate::panel_controller::drag_release_width(pending, applied);
            // v1.14.3 (T1): the release's final write IS an application — it
            // draws SAME-TICK (forced) and the trailing `request_redraw` is
            // dropped (强制与异步请求互斥); a zero-move release (no parked
            // width, cursor restore only) keeps the existing trailing redraw.
            let final_applied = new_width != applied;
            if final_applied {
                if let Some(r) = &mut self.renderer {
                    r.set_sidebar_width(new_width);
                }
                self.recompute_layout();
            }
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
            if final_applied {
                self.handle_redraw_requested_forced();
            } else {
                self.request_redraw();
            }
            return;
        }
        // v1.3.2: pane divider resize drag end — clear state, restore cursor.
        // No persistence (ratio is in-memory only, per V13 non-goal).
        // v1.14.3 (T2): 终值必写 — the ratio parked by the drag's 8ms gate
        // lands here; T1: a landed final ratio draws same-tick (forced), a
        // zero-move release keeps the trailing async redraw. (Body in
        // `app::divider_drag` — this file is at its line budget.)
        if button == winit::event::MouseButton::Left && self.finish_pane_divider_drag(_x, _y) {
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
        // v1.13.17 S2: the non-modal surfaces' row hover runs FIRST (before
        // the drag early-returns below) so an active drag can clear it —
        // update_surface_row_hover guards on drag state internally.
        self.update_surface_row_hover(x, y);
        // F3-3: sidebar resize drag + v1.3.2 pane divider drag. The drag
        // persists on release.
        // v1.14.3 (T1): three-state dispatch (`App::drag_move_consumed`) —
        // an Applied drag draws SAME-TICK (forced, the `Resized`-branch
        // precedent) so the pointer never leads the frame by one vsync; a
        // Consumed (throttle-parked) event draws NOTHING (the next Applied
        // event produces the frame); NotConsumed falls through. 强制与异步
        // 请求互斥: the Applied arm never also `request_redraw`.
        // v1.14.3 (T2): the divider ratio write is throttled by the NEW 8ms
        // gate (`update_pane_divider_drag` → `drag_apply_step`, state on
        // `InteractionState.divider_drag_last_apply`/`pending_divider_ratio`,
        // 终值必写 at release); the PTY resize throttle inside
        // recompute_layout (30ms active / 100ms background) still applies
        // underneath — the comment this replaces pointed only at that.
        let sidebar = self.update_sidebar_drag(x);
        if self.drag_move_consumed(sidebar) {
            return;
        }
        let divider = self.update_pane_divider_drag(x as f32, y as f32);
        if self.drag_move_consumed(divider) {
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
            && !self.settings.interactive()
            && !self.palette.interactive()
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
            && !self.settings.interactive()
            && !self.palette.interactive()
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

    /// v1.13.17 S2: palette row hover — the palette is modal
    /// (route_modal_pointer consumes CursorMoved while it is open), so this
    /// is the only pointer handling it receives while open
    /// (update_context_menu_hover precedent). Recomputes from the shared
    /// Palette Scene; a miss (outside any row) clears the highlight.
    pub(super) fn update_palette_hover(&mut self, x: f64, y: f64) {
        let hit = self.palette_row_at(x, y);
        if hit != self.palette.hovered_row {
            self.palette.hovered_row = hit;
            self.request_redraw();
        }
    }

    /// v1.13.17 S2: settings target hover — same modal story as the palette.
    /// The state stores exactly what `settings_target_at` resolves; only the
    /// row targets (SidebarCategory / Theme / ContentRow) get a painted
    /// hover background, so button targets simply never highlight.
    pub(super) fn update_settings_hover(&mut self, x: f64, y: f64) {
        let hit = self.settings_target_at(x as f32, y as f32);
        if hit != self.settings.hovered {
            self.settings.hovered = hit;
            self.request_redraw();
        }
    }

    /// v1.13.17 S2: transient row hover for the two NON-modal surfaces
    /// (completion popup + history panel). CursorMoved reaches
    /// handle_mouse_move for them (route_modal_pointer lets it through when
    /// no modal overlay is open), so unlike palette/settings this runs on
    /// the free-move path. An active drag gesture owns the pointer: hover
    /// clears and stays cleared until the next free move (plan §2.2).
    pub(super) fn update_surface_row_hover(&mut self, x: f64, y: f64) {
        let drag_active = self.interaction.drag_state.is_some()
            || self.interaction.scrollbar_drag.is_some()
            || self.interaction.panel_scrollbar_drag.is_some()
            || self.interaction.sidebar_drag.is_some()
            || self.interaction.pane_divider_drag.is_some();
        let (completion_hit, panel_hit) = if drag_active {
            (None, None)
        } else {
            let completion = self.completion_scene().and_then(|scene| {
                match crate::completion_component::completion_target_at(&scene, x as f32, y as f32)
                {
                    Some(crate::completion_component::CompletionTarget::Item(index)) => Some(index),
                    _ => None,
                }
            });
            // `panel_target_at` already gates on panel.open; a miss (search
            // field, footer, outside the panel) clears the highlight.
            let panel = self
                .panel_target_at(x as f32, y as f32)
                .and_then(|target| match target {
                    crate::panel_component::PanelTarget::Row(index) => Some(index),
                    _ => None,
                });
            (completion, panel)
        };
        if completion_hit != self.interaction.completion_hovered_row {
            self.interaction.completion_hovered_row = completion_hit;
            self.request_redraw();
        }
        if panel_hit != self.panel.hovered_row {
            self.panel.hovered_row = panel_hit;
            self.request_redraw();
        }
    }
}
