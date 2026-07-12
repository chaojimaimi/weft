//! Redraw processing extracted from window event dispatch.

use super::*;

impl App {
    pub(super) fn handle_redraw_requested(&mut self) {
        self.pump_pty();
        let had_output = self.process_messages();
        self.update_cursor_blink();
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
        if had_output {
            if let Some(t) = &self.sessions.tabs[self.sessions.active_tab].terminal {
                if t.block_tracker().phase() == ShellPhase::CommandExecuting {
                    self.sessions.tabs[self.sessions.active_tab].block_scroll_offset = 0;
                }
            }
        }

        // If the grid row count drifted from what the terminal holds
        // (font/padding/window-size change, or the one-time convergence
        // from the spawn size to the padded size) — recompute. Mode
        // transitions no longer cause drift: the grid is always
        // full-window and the input box is a non-resizing overlay.
        let desired = self.grid_dims();
        let current = self.sessions.tabs[self.sessions.active_tab]
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
            .tabs
            .iter()
            .map(|tab| tab.pending_pty_resize)
            .collect();
        let resize_effects = effect::pending_resize_effects(
            &pending,
            self.sessions.active_tab,
            active_ready,
            cascade_settled,
        );
        self.drain_effects(resize_effects);

        // v0.9 H1: borrow the active Tab once and access its fields
        // (terminal / ime_preedit / selection_handler / block_scroll_offset)
        // as disjoint field borrows. Indexing `self.sessions.tabs[i]` repeatedly
        // would prevent Rust from splitting borrows across the
        // `&tab.terminal` (immutable) and `&mut tab.selection_handler`
        // (mutable) needed by `renderer.draw`. `self.sessions.tabs` and
        // `self.renderer` are disjoint fields of `App`, so both can be
        // mutably borrowed at once.
        let active = self.sessions.active_tab;
        // Compute the tab bar state BEFORE borrowing `self.sessions.tabs`
        // mutably below: `tab_bar_state()` reads `self.sessions.tabs[*]`
        // labels and would conflict with `&mut self.sessions.tabs[active]`.
        // The returned `TabBarDrawState` is owned and lives for the
        // whole draw call.
        let tab_bar = self.tab_bar_state();
        // v1.0 S1: compute Settings panel view data before the
        // mutable `tab` borrow below — settings_theme_views() and
        // settings_keybinding_views() borrow self immutably, which
        // would conflict with &mut self.sessions.tabs[active].
        let settings_themes = self.settings_theme_views();
        let settings_keybindings = self.settings_keybinding_views();
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
        let tab = &mut self.sessions.tabs[active];
        // v1.0 P0-b: when the active tab changed since the last frame,
        // the renderer's per-row grid cache is stale — force a full
        // redraw before drawing.
        let tab_changed = active != self.sessions.prev_drawn_tab;
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
                .map(|m| (m.x, m.y, m.block_id));

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
                &tab.ime_preedit,
                self.palette.open,
                &self.palette.query,
                self.palette.selection,
                &palette_entries,
                &palette_banner,
                &palette_submode_input,
                palette_form_view.as_ref(),
                terminal.editor().buffer.selection_range(),
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
                &settings_keybindings,
                self.settings.draft.logo.variant,
                self.settings.error.as_deref(),
            );
            // v0.8 U6: compute block-content metrics for the dynamic
            // scrollbar thumb (total/visible/max_scroll). None in grid
            // view — the scrollbar only shows in block view anyway.
            let scroll_metrics = if terminal.show_block_view() {
                let cols = terminal.grid().num_cols;
                let (total, _) = block_content_metrics(terminal, cols);
                let prompt_lines = terminal.editor().buffer.lines.len();
                let visible = renderer.block_visible_rows(prompt_lines);
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
                Some(FindDrawState {
                    query: self.find.query.clone(),
                    current,
                    total,
                    truncated,
                    highlight,
                    block_highlight,
                    block_matches: if block_view { 0 } else { block_total },
                    regex_mode: self.find.regex_mode,
                    case_sensitive: self.find.case_sensitive,
                    regex_error: self.find.regex_error.clone(),
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
            // Pause cursor blink while the user is actively selecting
            // OR while a selection is visible (not yet cleared). A
            // moving or persistent selection is the focus of attention;
            // a blinking caret distracts. Resumes when the selection
            // is cleared (click on empty area / prompt / Esc).
            let has_selection = tab.selection_handler.selecting
                || tab.selection_handler.block_view_selection.is_some()
                || tab.selection_handler.selection.is_some();
            let blink_on = self.window_runtime.cursor_blink_on && !has_selection;
            renderer.draw(
                terminal,
                &mut tab.selection_handler,
                blink_on,
                self.window_runtime.cursor_blink_phase,
                &overlays,
                tab.block_scroll_offset,
                scroll_metrics,
                self.interaction.scrollbar_hovered || self.interaction.scrollbar_drag.is_some(),
                &tab_bar,
            );
            // Flicker fix (Step 2): update the shared flag so the blink
            // timer thread knows whether to keep waking the loop. In
            // block view the caret only animates at the prompt; in grid
            // view it animates when the cursor isn't hidden. When no
            // caret is visible, skipping the wake avoids pointless
            // full redraws (the main cause of idle-terminal flicker).
            let anim_active = if terminal.show_block_view() {
                terminal.block_tracker().phase() == ShellPhase::AtPrompt
            } else {
                terminal.cursor_visible
            };
            self.window_runtime
                .cursor_anim_active
                .store(anim_active, Ordering::Relaxed);
        }

        // v1.0 P0-b: clear the grid's per-row dirty flags now that
        // the renderer has consumed them. The next frame will mark
        // rows dirty only if new PTY output / cursor movement changes
        // them, enabling incremental rendering.
        self.sessions.prev_drawn_tab = active;
        if let Some(t) = &mut tab.terminal {
            t.grid_mut().clear_all_dirty();
        }

        // No busy-loop redraw here: the PTY reader thread and the
        // cursor-blink timer wake the loop via `AppEvent::Wake`
        // whenever there is work (see `user_event`). This lets the CPU
        // idle instead of spinning at vsync.
    }
}
