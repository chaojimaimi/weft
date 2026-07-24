//! Redraw processing extracted from window event dispatch.

use super::*;

impl App {
    pub(super) fn handle_redraw_requested(&mut self) {
        self.pump_pty();
        let had_output = self.process_messages();
        if self
            .sessions
            .active()
            .terminal
            .as_ref()
            .is_some_and(Terminal::synchronized_output)
        {
            return;
        }
        if crate::input_router::route_session_input(!self.sessions.is_empty())
            == crate::input_router::SessionInputRoute::Consume
        {
            return;
        }
        self.update_cursor_blink();
        self.update_spinner();
        // FindInGrid debounce: when 150ms have elapsed since the last
        // keystroke, run the search and update `find_matches`.
        self.maybe_refresh_find_results();
        // v0.9 U-P1: drain pending find-worker results (async grid search).
        self.poll_find_worker_results();
        // v0.9 U-D1: poll macOS system appearance (throttled to 1Hz).
        self.poll_system_appearance();

        // During command execution, new output streams in — snap the
        // block view to the bottom so the user sees fresh content.
        // (At prompt / idle, preserve the user's scroll position.)
        // R2-1: also skip snapping when the user has detached to a fixed
        // document row. Without this guard, the first frame of output
        // after scroll_up_by would snap_to_bottom and discard the user's
        // scroll position — the exact bug this enum was introduced to fix.
        if had_output {
            let snap_to_bottom = self
                .sessions
                .active()
                .terminal
                .as_ref()
                .map(|t| {
                    crate::block_component::should_follow_running_output(
                        t.block_tracker().phase(),
                        t.primary_history_view(),
                    )
                })
                .unwrap_or(false)
                && matches!(
                    self.sessions.active().block_scroll_anchor(),
                    crate::tab::BlockScrollAnchor::FollowBottom
                );
            if snap_to_bottom {
                self.sessions.active_mut().snap_to_bottom();
            }
        }

        // If the grid row count drifted from what the terminal holds
        // (font/padding/window-size change, the one-time convergence from the
        // spawn size to the padded size) — recompute.
        let desired = self.grid_dims();
        let current = self
            .sessions
            .active()
            .terminal
            .as_ref()
            .map(|t| (t.grid().num_rows, t.grid().num_cols))
            .unwrap_or((0, 0));
        if desired.0 != 0 && desired.1 != 0 && desired != current {
            self.recompute_layout();
        }

        // Flush the PTY SIGWINCH (TIOCSWINSZ) so the foreground app
        // repaints at the new size.
        //
        // v1.0 fix (live-resize): the active tab's SIGWINCH is now sent
        // on a SHORT throttle (~30ms) instead of the old 100ms settle
        // debounce. Alt-screen TUIs (less/vim/man) only re-render on
        // SIGWINCH — with the old 100ms debounce they never repainted
        // mid-drag, so the screen stayed stale until mouse release (the
        // "content doesn't follow the window until released" report).
        // 30ms ≈ every other vsync at 60Hz: enough coalescing to avoid
        // hammering the app, tight enough that each Resized within a
        // drag still drives a repaint. Background tabs keep the old
        // settle-debounce (their grid is already correct; the SIGWINCH
        // just syncs the shell, and can wait until activation).
        let now = std::time::Instant::now();
        let active_ready = now.duration_since(self.window_runtime.last_resize_instant)
            > std::time::Duration::from_millis(30);
        let cascade_settled = self.window_runtime.last_resize_instant.elapsed()
            > std::time::Duration::from_millis(100);
        let pending: Vec<_> = self
            .sessions
            .tabs()
            .iter()
            .map(|tab| tab.pending_pty_resize)
            .collect();
        let resize_effects = effect::pending_resize_effects(
            &pending,
            self.sessions.active_idx(),
            active_ready,
            cascade_settled,
        );
        self.drain_effects(resize_effects);

        // v0.9 H1: borrow the active Tab once and access its fields
        // (terminal / ime_preedit / selection_handler / block_scroll_offset)
        // as disjoint field borrows. `active_mut()` borrows `self.sessions`
        // mutably; `self.renderer` is a disjoint field of `App`, so
        // `(&mut self.renderer, &tab.terminal)` can coexist. Reading
        // `prev_drawn_tab()` before `active_mut()` avoids a borrow conflict.
        let active = self.sessions.active_idx();
        // Compute the tab bar state BEFORE borrowing `self.sessions`
        // mutably below: `tab_bar_state()` reads tab labels and would
        // conflict with `active_mut()`. The returned `TabBarDrawState`
        // is owned and lives for the whole draw call.
        let tab_bar = self.tab_bar_state();
        // v1.0 S1: compute Settings panel view data before the
        // mutable `tab` borrow below — settings_theme_views() and
        // settings_keybinding_views() borrow self immutably, which
        // would conflict with `active_mut()`.
        let settings_themes = self.settings_theme_views();
        let settings_keybindings = self.settings_keybinding_views();
        // F5: compute settings split-layout state before the mutable `tab`
        // borrow below. These feed build_overlay_stack's new params.
        let settings_is_narrow = self.settings_is_narrow();
        let settings_drill_down = self.settings.drill_down;
        let settings_keybinding_conflict_count =
            settings_keybindings.iter().filter(|v| v.conflict).count();
        let settings_field_errors: &[(String, String)] = &self.settings.field_errors;
        let palette_form_fields = self
            .palette
            .form
            .as_ref()
            .map(WorkflowForm::draw_fields)
            .unwrap_or_default();
        let palette_form_view =
            self.palette
                .form
                .as_ref()
                .map(|form| crate::overlay::PaletteFormView {
                    workflow_name: &form.workflow_name,
                    fields: &palette_form_fields,
                    current_field: form.current_field,
                });
        let prev_drawn = self.sessions.prev_drawn_tab();
        let tab = self.sessions.active_mut();
        // v1.0 P0-b: when the active tab changed since the last frame,
        // the renderer's per-row grid cache is stale — force a full
        // redraw before drawing.
        let tab_changed = active != prev_drawn;
        // F2 P0-1: keep the editor cursor visible inside the clamped (30%
        // viewport) prompt box. Computed once per frame so any cursor move,
        // text edit, or resize is covered before the overlay stack is built.
        let prompt_max_rows = self
            .renderer
            .as_ref()
            .and_then(|r| r.layout_ctx)
            .map(|ctx| {
                let ch = ctx.cell_h;
                if ch <= 0.0 {
                    return 1usize;
                }
                let max_box_h = ctx.viewport.1 * 0.30;
                (((max_box_h / ch).floor() - 2.0).max(1.0) as usize).max(1)
            })
            .unwrap_or(1);
        if let Some(terminal) = tab.terminal.as_mut() {
            if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                terminal
                    .editor_mut()
                    .buffer
                    .ensure_cursor_visible(prompt_max_rows);
            }
        }
        if let (Some(renderer), Some(terminal)) = (&mut self.renderer, &tab.terminal) {
            if tab_changed {
                renderer.force_full_grid_redraw();
            }
            // Sync popup dimensions to renderer (user-adjustable via border drag).
            renderer.set_popup_size(
                self.interaction.popup_width_scale,
                self.interaction.popup_max_rows,
            );
            // Sync context menu target to renderer.
            renderer.context_menu_target = self
                .interaction
                .context_menu
                .as_ref()
                .map(|m| (m.x, m.y, m.block_id, m.selection));

            // Build palette entries as (label, description, kind_label) tuples.
            // v0.9 W2+: in SelectTheme sub-mode, project theme names
            // (filtered from the full list) instead of the generic
            // "Select Theme" builtin label.
            let palette_entries: Vec<(String, String, &str)> =
                if matches!(self.palette.submode, PaletteSubMode::SelectTheme { .. }) {
                    let (buffer, themes) = match &self.palette.submode {
                        PaletteSubMode::SelectTheme { buffer, themes } => {
                            (buffer.clone(), themes.clone())
                        }
                        _ => unreachable!(),
                    };
                    let q = buffer.to_lowercase();
                    themes
                        .iter()
                        .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
                        .map(|n| (n.clone(), String::new(), "Theme"))
                        .collect()
                } else {
                    self.palette
                        .results
                        .iter()
                        .map(|e| match e {
                            PaletteEntry::Workflow(wf) => {
                                (wf.name.clone(), wf.description.clone(), "Workflow")
                            }
                            PaletteEntry::Builtin(b) => {
                                (b.label().to_string(), String::new(), "Builtin")
                            }
                        })
                        .collect()
                };

            // Compute palette banner + submode input from the sub-mode state.
            let (palette_banner, palette_submode_input) = match &self.palette.submode {
                PaletteSubMode::Search => (String::new(), String::new()),
                PaletteSubMode::CreateWorkflow { step, buffer, .. } => {
                    let label = match step {
                        CreateStep::Name => "New workflow — name:",
                        CreateStep::Command => "New workflow — command (use {{var}}):",
                        CreateStep::Done => "Creating...",
                    };
                    (label.to_string(), buffer.clone())
                }
                PaletteSubMode::EditWorkflow { name, buffer, .. } => {
                    (format!("Edit '{name}':"), buffer.clone())
                }
                PaletteSubMode::ConfirmDelete { name, .. } => {
                    (format!("Delete '{name}'? (y/n)"), String::new())
                }
                PaletteSubMode::SelectTheme { buffer, .. } => {
                    ("Select theme:".to_string(), buffer.clone())
                }
            };

            // v1.0 S1: build Settings panel overlay stack. The
            // settings_themes and settings_keybindings Vecs were
            // computed before the mutable `tab` borrow above.
            let overlays = crate::overlay::build_overlay_stack(
                terminal,
                renderer.viewport_width(),
                renderer.sidebar_width(),
                self.panel.open,
                &self.panel.query,
                self.panel.selection,
                self.panel.expanded,
                self.panel.search_focused,
                self.panel.scroll_offset,
                &tab.ime_preedit,
                tab.ime_preedit_cursor,
                self.palette.open,
                &self.palette.query,
                self.palette.selection,
                &palette_entries,
                &palette_banner,
                &palette_submode_input,
                palette_form_view.as_ref(),
                terminal.editor().buffer.selection_range(),
                self.config_state.config.editor.submit_on_ctrl_enter,
                self.settings.open,
                self.settings.tab,
                self.settings.selection,
                self.settings.scroll_offset,
                &self.settings.draft.theme.name,
                &settings_themes,
                &self.settings.draft.font.family,
                self.settings.draft.font.size,
                self.settings.draft.font.line_height,
                self.settings.draft.window.opacity,
                self.settings.draft.window.padding_x,
                self.settings.draft.window.padding_y,
                self.settings.draft.scrollback.lines,
                self.settings.draft.window.width,
                self.settings.draft.window.height,
                self.settings.draft.window.sidebar_width,
                self.settings.draft.editor.submit_on_ctrl_enter,
                &settings_keybindings,
                self.settings.draft.logo.variant,
                self.settings.error.as_deref(),
                settings_is_narrow,
                settings_drill_down,
                settings_keybinding_conflict_count,
                settings_field_errors,
            );
            // v0.8 U6: compute block-content metrics for the dynamic
            // scrollbar thumb (total/visible/max_scroll). None in grid
            // view — the scrollbar only shows in block view anyway.
            let scroll_metrics = if terminal.show_block_view() {
                let cols = terminal.grid().num_cols;
                let cache = renderer.block_layout_cache.borrow();
                let (total, _) = block_content_metrics_with_cache(
                    terminal,
                    cols,
                    renderer.block_header_rows(),
                    Some(&*cache),
                );
                let prompt_lines = crate::block_component::block_prompt_lines(terminal);
                let cwd_header = crate::layout::block_cwd_header_active(
                    terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
                    terminal.cwd().is_some(),
                );
                let visible = renderer.block_visible_rows(prompt_lines, cwd_header);
                let max_scroll = total.saturating_sub(visible);
                Some((total, visible, max_scroll))
            } else {
                None
            };
            // v0.8 B3: populate find overlay state before draw. None
            // when the bar is closed so the renderer skips the overlay.
            // The total includes block-view matches so the count
            // reflects what the user actually sees (block content
            // isn't in the grid).
            // Compute find state values BEFORE the mutable borrow
            // on `renderer` (renderer.find_state = ...).
            // F2 P1-1: pre-compute cursor visibility so the Find highlight can
            // leave a gap over the cursor cell. `has_selection` / `blink_on`
            // are needed here AND again below for `renderer.draw()`, so we
            // compute them once up front.
            let has_selection = tab.selection_handler.selecting
                || tab.selection_handler.block_view_selection.is_some()
                || tab.selection_handler.selection.is_some();
            let blink_on = self.window_runtime.cursor_blink_on && !has_selection;
            let find_state = if self.find.open {
                let grid_total = self.find.matches.len();
                let block_total = self.find.block_matches.len();
                let block_view = terminal.show_block_view();
                let (total, current) = if block_view {
                    (block_total, self.find.block_index + 1)
                } else {
                    (
                        grid_total,
                        if grid_total == 0 {
                            0
                        } else {
                            self.find.index + 1
                        },
                    )
                };
                let truncated = self.find.truncated || self.find.block_truncated;
                let highlight = if !block_view {
                    let grid = terminal.grid();
                    let sb_len = grid.scrollback_len();
                    let offset = grid.scroll_offset.min(sb_len);
                    let unified_base = sb_len - offset;
                    self.find
                        .matches
                        .get(self.find.index)
                        .map(|m| (m.row.saturating_sub(unified_base), m.col, m.len))
                } else {
                    None
                };
                let block_highlight = if block_view {
                    self.find
                        .block_matches
                        .get(self.find.block_index)
                        .map(|m| (m.block_id.0, m.line, m.is_command, m.col, m.len))
                } else {
                    None
                };
                // F2 P1-1: terminal cursor (row, col) + visibility so the
                // Find highlight can leave a gap over the cursor cell.
                let grid = terminal.grid();
                let cursor_pos = Some((grid.cursor.row, grid.cursor.col));
                let prompt_visible =
                    terminal.effective_input_mode() == weft_core::input::InputMode::Editor;
                let show_cursor = crate::terminal_geometry::grid_cursor_visible(
                    terminal.cursor_style,
                    terminal.cursor_visible,
                    blink_on,
                    prompt_visible,
                );
                Some(FindDrawState {
                    query: self.find.query.clone(),
                    current,
                    total,
                    truncated,
                    highlight,
                    cursor_pos,
                    show_cursor,
                    block_highlight,
                    block_matches: if block_view { 0 } else { block_total },
                    regex_mode: self.find.regex_mode,
                    case_sensitive: self.find.case_sensitive,
                    regex_error: self.find.regex_error.clone(),
                    worker_busy: self.find.worker_busy,
                })
            } else {
                None
            };
            renderer.find_state = find_state;
            // v0.9 W2: expire panel highlight after 1.5s.
            if let Some(until) = self.panel.highlight_until {
                if std::time::Instant::now() >= until {
                    self.panel.highlight = None;
                    self.panel.highlight_until = None;
                }
            }
            renderer.panel_highlight = self.panel.highlight;
            renderer.block_hovered = self.interaction.block_hovered;
            renderer.reduce_motion = self.window_runtime.reduce_motion;
            // F3-2: compute spinner phase for the running-command indicator.
            // Only active when a command is executing in block view and the
            // user hasn't enabled Reduce Motion.
            let is_running = terminal.block_tracker().phase() == ShellPhase::CommandExecuting;
            let spinner_phase = if is_running && !self.window_runtime.reduce_motion {
                self.window_runtime.spinner_phase
            } else {
                -1.0
            };
            renderer.spinner_phase = spinner_phase;
            // Pause cursor blink while the user is actively selecting
            // OR while a selection is visible (not yet cleared). A
            // moving or persistent selection is the focus of attention;
            // a blinking caret distracts. Resumes when the selection
            // is cleared (click on empty area / prompt / Esc).
            // (`has_selection` / `blink_on` were computed above before the
            // Find state so the highlight can also use cursor visibility.)
            // M3.5: read block_scroll into a local before the mutable
            // `&mut tab.selection_handler` borrow below — `block_scroll()`
            // takes `&self` and would conflict with the mutable borrow.
            let block_scroll = tab.block_scroll();
            // R3 task 6: arm the per-frame trace recorder and stamp the frame
            // id on the renderer (read by the Metal command-buffer label so the
            // async GPU-completion handler can correlate). The recorder lives
            // on the renderer in a RefCell so draw()/encode_and_present can
            // mark segment boundaries despite holding an immutable layer borrow.
            if self.frame_trace_enabled {
                self.frame_id = self.frame_id.wrapping_add(1).max(1);
                renderer.frame_id.set(self.frame_id);
                let reason = crate::frame_trace::classify_reason(
                    had_output,
                    self.window_runtime.cursor_blink_phase >= 0.0,
                    renderer.spinner_phase >= 0.0,
                    // Live-resize is not currently tracked as a distinct flag;
                    // resize-driven redraws fall into Other until a flag lands.
                    false,
                );
                *renderer.frame_trace.borrow_mut() =
                    crate::frame_trace::FrameTraceRecorder::begin(true, self.frame_id, reason);
            }
            renderer.draw(
                terminal,
                &mut tab.selection_handler,
                blink_on,
                self.window_runtime.cursor_blink_phase,
                &overlays,
                block_scroll,
                scroll_metrics,
                self.interaction.scrollbar_hovered || self.interaction.scrollbar_drag.is_some(),
                &tab_bar,
            );
            // R3 task 6: finish the per-frame trace — drains any GPU-completion
            // messages that landed since last frame and emits the frame line.
            if self.frame_trace_enabled {
                let recorder = renderer
                    .frame_trace
                    .replace(crate::frame_trace::FrameTraceRecorder::disabled());
                recorder.finish(&self.gpu_completion_rx);
            }
            if let (Some(window), Some(ctx)) = (self.window.as_ref(), renderer.layout_ctx) {
                crate::ime::update_cursor_area(window, ctx, terminal);
            }
            // Flicker fix (Step 2): update the shared flag so the blink
            // timer thread knows whether to keep waking the loop. In
            // block view the caret only animates at the prompt; in grid
            // view it animates when the cursor isn't hidden. When no
            // caret is visible, skipping the wake avoids pointless
            // full redraws (the main cause of idle-terminal flicker).
            //
            // F6: Steady cursors (Block/Underline/Bar) don't blink, so they
            // don't need the blink wake either — only BlinkingBlock/
            // BlinkingUnderline/BlinkingBar participate in the timer. When
            // Reduce Motion is on, the cursor is frozen visible (see
            // `update_cursor_blink`), so the wake is also unnecessary.
            let anim_active = if self.window_runtime.reduce_motion {
                false
            } else if terminal.show_block_view() {
                terminal.block_tracker().phase() == ShellPhase::AtPrompt
            } else {
                terminal.cursor_visible && terminal.cursor_style.is_blinking()
            };
            self.window_runtime
                .cursor_anim_active
                .store(anim_active, Ordering::Relaxed);
            // F3-2: keep the spinner timer running while a command is executing
            // so the braille activity indicator animates even without PTY output.
            let spinner_active = terminal.block_tracker().phase() == ShellPhase::CommandExecuting
                && !self.window_runtime.reduce_motion;
            self.window_runtime
                .spinner_anim_active
                .store(spinner_active, Ordering::Relaxed);
        }

        // v1.0 P0-b: clear the grid's per-row dirty flags now that
        // the renderer has consumed them. The next frame will mark
        // rows dirty only if new PTY output / cursor movement changes
        // them, enabling incremental rendering. The `set_prev_drawn_tab`
        // call comes after `tab`'s last use so NLL releases the borrow.
        if let Some(t) = &mut tab.terminal {
            t.grid_mut().clear_all_dirty();
        }
        self.sessions.set_prev_drawn_tab(active);
        self.update_accessibility_tree();

        // No busy-loop redraw here: the PTY reader thread and the
        // cursor-blink timer wake the loop via `AppEvent::Wake`
        // whenever there is work (see `user_event`). This lets the CPU
        // idle instead of spinning at vsync.
    }
}
