//! Redraw processing extracted from window event dispatch.

use super::*;

impl App {
    pub(super) fn handle_redraw_requested(&mut self) {
        self.pump_pty();
        // Drag-selection autoscroll: the 40ms timer wakes this path while a
        // drag is held past the block content edge, so the viewport keeps
        // scrolling (and the selection extending) even with a still pointer.
        self.pump_selection_autoscroll();
        let had_output = self.process_messages();
        // v1.10.4: if the active pane entered/exited alt-screen (DEC 1049),
        // recompute geometry so PTY cols switch between full-width (TUI)
        // and gutter-subtracted (BlockView). Must happen before the render
        // so the grid dimensions match the new screen mode this frame.
        if self.sessions.active_mut().take_pending_alt_rescale() {
            self.recompute_layout();
        }
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
        // v0.9 U-D1: drain pending find-worker results (async grid search).
        self.poll_find_worker_results();
        // v1.7.1: drain palette search worker results (async FTS5 search).
        self.poll_palette_search_results();
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
        let desired = self
            .terminal_layout()
            .and_then(|layout| {
                self.sessions.active().active_pane_dimensions_for_rect(
                    [
                        layout.content.left as f32,
                        layout.content.top as f32,
                        layout.content.right as f32,
                        layout.content.bottom as f32,
                    ],
                    layout.cell_width as f32,
                    layout.cell_height as f32,
                )
            })
            .unwrap_or((0, 0));
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
        // The active pane commits every redraw: delaying until a resize
        // cascade pauses leaves its PTY at the old width while the Grid is
        // already narrow, so long synchronized progress lines auto-wrap and
        // later `CSI A` repaints cannot erase the extra rows. Background tabs
        // remain settle-debounced; both their PTY and Grid retain the old
        // geometry together until the transaction commits.
        let cascade_settled = self.window_runtime.last_resize_instant.elapsed()
            > std::time::Duration::from_millis(100);
        let pending: Vec<_> = self
            .sessions
            .tabs()
            .iter()
            .map(|tab| tab.pending_pane_resizes())
            .collect();
        let resize_effects =
            effect::pending_resize_effects(&pending, self.sessions.active_idx(), cascade_settled);
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
        let terminal_owns_ime = self.overlay_input_owner().is_none();
        let settings_field_errors: &[(String, String)] = &self.settings.field_errors;
        // v1.5.1: Build profile views for the Settings toolbar. Index 0 is
        // always "Base" (is_active = no active profile); 1..N are sorted
        // profile names. Built before the mutable `tab` borrow below to
        // avoid borrow conflicts.
        let active_profile = self.active_profile_name();
        let profile_names = self.profile_names_sorted();
        let mut settings_profiles: Vec<crate::overlay::SettingsProfileView<'_>> =
            Vec::with_capacity(profile_names.len() + 1);
        settings_profiles.push(crate::overlay::SettingsProfileView {
            name: "Base",
            is_active: active_profile.is_none(),
        });
        for name in &profile_names {
            settings_profiles.push(crate::overlay::SettingsProfileView {
                name: name.as_str(),
                is_active: active_profile == Some(name.as_str()),
            });
        }
        // v1.8.3: Build the LocalAi tab view from the draft AI config +
        // cached model list + connection status. Borrowed lifetimes tie back
        // to `self` so this must be built before the mutable `tab` borrow.
        let settings_ai_model_names: Vec<String> =
            self.ai_models.iter().map(|m| m.name.clone()).collect();
        let settings_ai_connection_label = self.ai_connection_status.label();
        let settings_ai_base_url = crate::ai::client::effective_base_url(&self.settings.draft.ai);
        // v1.8.3: Observability summary — pre-formatted so the renderer only
        // pushes one text run. Empty when no requests have been made (so the
        // row is hidden on a fresh launch). Only aggregate counts + p95
        // latency; no prompt/response text.
        let metrics_snap = self.ai_state.metrics_snapshot();
        let settings_ai_observability = if metrics_snap.requests_total == 0 {
            String::new()
        } else {
            // v1.8.3: Surface truncations when non-zero — they indicate the
            // prompt budget was hit (history/output clipped before sending).
            if metrics_snap.truncations_total > 0 {
                format!(
                    "{} req · {} ok · {} err · {} canc · {} trunc · p95 {}ms",
                    metrics_snap.requests_total,
                    metrics_snap.successes_total,
                    metrics_snap.errors_total,
                    metrics_snap.cancellations_total,
                    metrics_snap.truncations_total,
                    metrics_snap.p95_latency_ms,
                )
            } else {
                format!(
                    "{} req · {} ok · {} err · {} canc · p95 {}ms",
                    metrics_snap.requests_total,
                    metrics_snap.successes_total,
                    metrics_snap.errors_total,
                    metrics_snap.cancellations_total,
                    metrics_snap.p95_latency_ms,
                )
            }
        };
        let settings_ai = crate::overlay::AiSettingsView {
            enabled: self.settings.draft.ai.is_configured(),
            model: self.settings.draft.ai.model.as_deref().unwrap_or(""),
            base_url: &settings_ai_base_url,
            max_tokens: self.settings.draft.ai.effective_max_tokens(),
            timeout_secs: self.settings.draft.ai.effective_timeout_secs() as u32,
            enable_command_generation: self.settings.draft.ai.enable_command_generation,
            enable_error_diagnosis: self.settings.draft.ai.enable_error_diagnosis,
            models: &settings_ai_model_names,
            connection_status: &settings_ai_connection_label,
            testing: self.ai_connection_status.is_testing(),
            observability: &settings_ai_observability,
        };
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
        // v1.7.3-C: snapshot the bookmarked-block set before the mutable
        // `tab` borrow below. `annotation_store()` borrows `self.sessions`
        // immutably, which would conflict with `active_mut()`.
        let bookmarked_blocks = self.bookmarked_blocks.clone();
        // v1.8.5: compute the palette IME cursor area before the mutable
        // `tab` borrow below. `palette_ime_cursor_area` reads from
        // `self.palette` and `self.interaction`, which would conflict
        // with `self.sessions.active_mut()`.
        let palette_ime_area = self
            .renderer
            .as_ref()
            .and_then(|r| r.layout_ctx)
            .filter(|_| self.palette.open)
            .and_then(|ctx| self.palette_ime_cursor_area(ctx));
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
        // v1.3 Batch 5.5: compute pane layouts BEFORE the mutable `pane`
        // borrow below. The split tree is read-only structure; background
        // panes' Terminals are read-only during draw (only the active
        // pane's selection_handler mutates). We collect background terminal
        // pointers as raw `*const Terminal` because `PaneRenderInfo<'a>`
        // holds `&'a Terminal` — a borrow that can't cross the
        // `tab.active_mut()` call (std HashMap doesn't support disjoint
        // &mut / & on different keys). Soundness: `tab` outlives the draw
        // call; background panes' terminals are only read (never mutated)
        // during draw; the only mutation is the active pane's
        // selection_handler.
        let (active_pane_rect, bg_terminal_ptrs, pane_layouts_snapshot, active_pane_id) =
            match self.renderer.as_ref() {
                Some(renderer_ref) => {
                    let chrome_top = renderer_ref
                        .tab_bar_height()
                        .max(renderer_ref.titlebar_height());
                    let panel_open = self.panel.open;
                    let sidebar_placement = crate::ui_tokens::sidebar_placement(
                        panel_open,
                        renderer_ref.sidebar_width(),
                        renderer_ref.sidebar_push_width(),
                    );
                    let chrome_left = sidebar_placement.terminal_push_width;
                    let content_rect: crate::layout::Rect = [
                        renderer_ref.padding_x + chrome_left,
                        renderer_ref.padding_y + chrome_top,
                        renderer_ref.viewport.0 - renderer_ref.padding_x,
                        renderer_ref.viewport.1 - renderer_ref.padding_y,
                    ];
                    let pane_layouts = tab.split_tree().layout(content_rect);
                    let active_id = tab.active_pane_id();
                    let active_rect = pane_layouts
                        .iter()
                        .find(|(id, _)| *id == active_id)
                        .map(|(_, rect)| *rect)
                        .unwrap_or(content_rect);
                    let ptrs: Vec<(crate::layout::Rect, *const Terminal, f32, u64)> = pane_layouts
                        .iter()
                        .filter(|(id, _)| *id != active_id)
                        .filter_map(|(id, rect)| {
                            let pane = tab.pane(*id)?;
                            let terminal = pane.terminal.as_ref()?;
                            Some((
                                *rect,
                                terminal as *const Terminal,
                                pane.block_scroll_anchor.offset_value() as f32
                                    + pane.block_scroll_fraction,
                                pane.pane_session_id,
                            ))
                        })
                        .collect();
                    tracing::debug!(
                        content_rect = ?content_rect,
                        active_id = ?active_id,
                        active_rect = ?active_rect,
                        pane_layouts = ?pane_layouts,
                        bg_count = ptrs.len(),
                        "draw pane layouts"
                    );
                    (active_rect, ptrs, pane_layouts, active_id)
                }
                None => (
                    [0.0, 0.0, 0.0, 0.0],
                    Vec::<(crate::layout::Rect, *const Terminal, f32, u64)>::new(),
                    Vec::new(),
                    weft_core::pane_layout::PaneId(0),
                ),
            };
        // Build PaneRenderInfo from raw pointers. The references are valid
        // for the entire draw scope (tab outlives draw; background terminals
        // are immutable during draw).
        let background_panes: Vec<crate::renderer::PaneRenderInfo> = bg_terminal_ptrs
            .iter()
            .map(
                |(rect, ptr, block_scroll, pane_session_id)| crate::renderer::PaneRenderInfo {
                    rect: *rect,
                    // SAFETY: `ptr` was obtained from `tab.pane(id).terminal`
                    // above. `tab` outlives this scope; background panes'
                    // terminals are not mutated during draw (only the active
                    // pane's selection_handler is mutated, a disjoint Pane).
                    terminal: unsafe { &**ptr },
                    block_scroll: *block_scroll,
                    submit_on_ctrl_enter: self.config_state.config.editor.submit_on_ctrl_enter,
                    pane_session_id: *pane_session_id,
                },
            )
            .collect();
        // v1.3: take a single `&mut Pane` borrow so `terminal` (immutable)
        // and `selection_handler` (mutable, passed to `renderer.draw` below)
        // can be disjoint-field-borrowed from the same pane. Going through
        // `&tab.terminal` / `&mut tab.selection_handler` separately would
        // both deref through `Tab` and conflict.
        let pane = tab.active_mut();
        let terminal_opt = pane.terminal.as_ref();
        if let (Some(renderer), Some(terminal)) = (&mut self.renderer, terminal_opt) {
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
                            // v1.5.1: Profile entries show as
                            // "Switch Profile: <name>" with an active marker
                            // in the description. The kind label is
                            // "Profile" so the renderer can style it.
                            PaletteEntry::Profile { name, active } => {
                                let label = format!("Switch Profile: {name}");
                                let desc = if *active { "active" } else { "" };
                                (label, desc.to_string(), "Profile")
                            }
                            // v1.7.1: Search hits show the document title
                            // (command line for blocks) with the kind label
                            // ("History", "Workflow", etc.).
                            PaletteEntry::SearchHit(hit) => {
                                (hit.doc.title.clone(), String::new(), hit.doc.kind.label())
                            }
                            PaletteEntry::Runbook(entry) => {
                                (entry.command.clone(), entry.description.clone(), "Runbook")
                            }
                            // v1.8.1: AI suggestions show the command with
                            // a risk badge as the kind label.
                            PaletteEntry::AiSuggestion { command, risk } => {
                                (command.clone(), String::new(), risk.label())
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
                // v1.8.1: AI command generation mode.
                PaletteSubMode::AiCommand {
                    buffer,
                    pending_id,
                    error,
                    ..
                } => {
                    let banner = if pending_id.is_some() {
                        "✨ Generating…".to_string()
                    } else if let Some(msg) = error {
                        // v1.8.7: Surface the error inline so the user knows
                        // why generation failed and can retry with the
                        // restored query.
                        format!("✨ AI error: {msg}")
                    } else {
                        "✨ Ask AI (describe command):".to_string()
                    };
                    (banner, buffer.clone())
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
                &pane.ime_preedit,
                pane.ime_preedit_cursor,
                terminal_owns_ime,
                self.palette.open,
                &self.palette.query,
                self.palette.selection,
                &palette_entries,
                &palette_banner,
                &palette_submode_input,
                &self.palette.ime_preedit,
                self.palette.ime_preedit_cursor,
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
                self.settings.draft.theme.minimum_contrast,
                self.settings.draft.window.width,
                self.settings.draft.window.height,
                self.settings.draft.window.sidebar_width,
                self.settings.draft.editor.submit_on_ctrl_enter,
                self.settings.draft.editor.smart_select,
                &settings_keybindings,
                self.settings.draft.logo.variant,
                self.settings.error.as_deref(),
                settings_is_narrow,
                settings_drill_down,
                settings_keybinding_conflict_count,
                settings_field_errors,
                &settings_profiles,
                self.settings.draft.theme.semantic_output_enabled(),
                settings_ai,
            );
            // v0.8 U6: compute block-content metrics for the dynamic
            // scrollbar thumb (total/visible/max_scroll). None in grid
            // view — the scrollbar only shows in block view anyway.
            // A1: computed live via `Renderer::block_scroll_metrics` (same
            // layout-cache path the wheel handler uses) instead of a second
            // inline copy of the formula.
            let scroll_metrics = if terminal.show_block_view() {
                Some(renderer.block_scroll_metrics(terminal, pane.pane_session_id))
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
            let has_selection = pane.selection_handler.selecting
                || pane.selection_handler.block_view_selection.is_some()
                || pane.selection_handler.selection.is_some();
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
            // v1.7.3-C: Populate note editor overlay state before draw.
            // `None` when the editor is closed so the renderer skips the
            // overlay.
            renderer.note_editor_state = if self.note_editor.open {
                Some(crate::paint::overlays::NoteEditorDrawState {
                    buffer: self.note_editor.buffer.clone(),
                    cursor: self.note_editor.cursor,
                })
            } else {
                None
            };
            // v0.9 W2: expire panel highlight after 1.5s.
            if let Some(until) = self.panel.highlight_until {
                if std::time::Instant::now() >= until {
                    self.panel.highlight = None;
                    self.panel.highlight_until = None;
                }
            }
            renderer.panel_highlight = self.panel.highlight;
            renderer.block_hovered = self.interaction.block_hovered;
            renderer.block_selected = self.interaction.block_selected;
            renderer.block_action_hovered = self.interaction.block_action_hovered;
            // v1.7.3-C: assign the pre-computed bookmarked-block set.
            renderer.bookmarked_blocks = bookmarked_blocks;
            // v1.8.2: assign per-block AI diagnose state for inline panel rendering.
            renderer.block_diagnose_state = self.block_diagnose_state.clone();
            // v1.8.2: mirror AI configured flag for header button rendering.
            renderer.ai_configured = self.ai_state.is_configured();
            renderer.reduce_motion = self.window_runtime.reduce_motion;
            renderer.increase_contrast = self.window_runtime.increase_contrast;
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
            // M3.5 / v1.3: read block_scroll into a local before the mutable
            // `&mut pane.selection_handler` borrow below — `block_scroll()`
            // takes `&self` and would conflict with the mutable borrow through
            // the shared `pane` reference.
            let block_scroll =
                pane.block_scroll_anchor.offset_value() as f32 + pane.block_scroll_fraction;
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
                &mut pane.selection_handler,
                blink_on,
                self.window_runtime.cursor_blink_phase,
                &overlays,
                block_scroll,
                scroll_metrics,
                self.interaction.scrollbar_hovered || self.interaction.scrollbar_drag.is_some(),
                &tab_bar,
                // v1.3 Batch 5.5: `active_pane_rect` is the split-tree-
                // computed rect for the active pane; `background_panes`
                // carries the other panes (each choosing block/grid view from
                // its own Terminal state). Both are computed above before
                // the mutable `pane` borrow. Empty for single-pane tabs
                // (the common case — no change from pre-v1.3 behavior).
                active_pane_rect,
                &background_panes,
                // v1.3.1 Batch 7: pane layouts + active pane id for divider
                // and focus-ring rendering in `draw()`.
                &pane_layouts_snapshot,
                active_pane_id,
                pane.pane_session_id,
            );
            crate::performance_probe::report_first_frame_once();
            // R3 task 6: finish the per-frame trace — drains any GPU-completion
            // messages that landed since last frame and emits the frame line.
            if self.frame_trace_enabled {
                let recorder = renderer
                    .frame_trace
                    .replace(crate::frame_trace::FrameTraceRecorder::disabled());
                recorder.finish(&self.gpu_completion_rx);
            }
            if let (Some(window), Some(ctx)) = (self.window.as_ref(), renderer.layout_ctx) {
                // v1.8.5: When the Palette is open, anchor the macOS IME
                // candidate window at the palette input box — not the
                // terminal cursor (which is hidden behind the popup).
                // Without this, CJK IME candidates render off-screen and
                // users can't complete character composition.
                crate::ime::update_cursor_area(
                    window,
                    ctx,
                    terminal,
                    palette_ime_area,
                    renderer.block_view_tui_caret_area.get(),
                );
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
            // F3-2: keep the spinner timer running while a command is executing so the
            // braille activity indicator animates even without PTY output. Gated on
            // block view: the spinner is a block-view element only (block_view.rs:948);
            // in grid mode (primary-screen TUI / alt-screen app owning the screen) there
            // is nothing to animate, so firing the 80ms timer only triggers pointless
            // full grid rebuilds (force_full_grid_redraw) — a major contributor to
            // execution-period flicker. spinner_phase has no grid-mode consumer
            // (tab_bar/status do not use it).
            let spinner_active = terminal.block_tracker().phase() == ShellPhase::CommandExecuting
                && terminal.show_block_view()
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

    /// v1.8.5: Compute the IME cursor area for the Palette input box.
    /// Called every frame when the Palette is open so the macOS candidate
    /// window stays anchored to the palette input (not the terminal cursor
    /// hidden behind the popup).
    fn palette_ime_cursor_area(
        &self,
        ctx: crate::layout::LayoutCtx,
    ) -> Option<crate::ime::ImeCursorArea> {
        use crate::palette_component::derive_palette_layout;

        let layout = derive_palette_layout(
            &ctx,
            self.palette.results.len(),
            self.palette.selection,
            self.palette.form.as_ref().map(|f| f.var_names.len()),
            self.interaction.popup_max_rows,
            self.interaction.popup_width_scale,
        );
        let search = match layout {
            crate::palette_component::PaletteLayout::Search(s) => s,
            _ => return None,
        };
        // The input text starts at query_x. In sub-modes with a banner,
        // the actual input begins after the banner text, but for IME
        // cursor positioning the query_x is close enough — the candidate
        // window just needs to be visible near the palette.
        Some(crate::ime::ImeCursorArea {
            x: search.query_x,
            y: search.query_y,
            width: ctx.cell_w,
            height: ctx.cell_h,
        })
    }
}
