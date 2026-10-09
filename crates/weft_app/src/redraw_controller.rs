//! Redraw processing extracted from window event dispatch.
//!
//! v1.12.25 (3-B-2 P2-02): the pump/process segment and the owned pre-draw
//! snapshot helpers moved verbatim to the `redraw/` submodule family
//! (`pump_process.rs` / `snapshots.rs`); `run_redraw` keeps the draw segment
//! and the tail, calling the phases through `PhaseOutcome`.

use super::*;
use crate::redraw::PhaseOutcome;

impl App {
    /// v1.11.10 (PLAN_v11110 M-B/D-e): the RedrawRequested entry — honors
    /// both early returns (synchronized-output suppression, route-consume).
    pub(super) fn handle_redraw_requested(&mut self) {
        self.run_redraw(false);
    }

    /// v1.11.10 (PLAN_v11110 M-B/D-d/D-e): the Resized-branch entry during a
    /// live drag — same-tick synchronous draw that bypasses the two early
    /// returns below (see the WHY comments on each gate).
    pub(super) fn handle_redraw_requested_forced(&mut self) {
        self.run_redraw(true);
    }

    fn run_redraw(&mut self, forced: bool) {
        // v1.12.25 (3-B-2 P2-02): pump/process 段（原 :55-236）外移到
        // redraw/pump_process.rs，方法体逐字平移；段内 4 处早返回转
        // PhaseOutcome::Abort，此处 match 后原样续 return（返回时机不变）。
        // `had_output` 是跨段共享变量（原 :69 局部），经返回值显式传递。
        let (outcome, had_output) = self.redraw_pump_phase();
        match outcome {
            PhaseOutcome::Abort => return,
            PhaseOutcome::Continue => {}
        }
        let outcome = self.redraw_process_phase(forced, had_output);
        match outcome {
            PhaseOutcome::Abort => return,
            PhaseOutcome::Continue => {}
        }

        // v0.9 H1: borrow the active Tab once and access its fields
        // (terminal guard / ime_preedit / selection_handler / block scroll)
        // as disjoint borrows. `active_mut()` borrows `self.sessions`
        // mutably; `self.renderer` is a disjoint field of `App`, so
        // `(&mut self.renderer, tab.lock_terminal())` can coexist — the T10
        // guard is owned and carries no borrow of the tab. Reading
        // `prev_drawn_tab()` before `active_mut()` avoids a borrow conflict.
        let active = self.sessions.active_idx();
        // Compute the tab bar state BEFORE borrowing `self.sessions`
        // mutably below: `tab_bar_state()` reads tab labels and would
        // conflict with `active_mut()`. The returned `TabBarDrawState`
        // is owned and lives for the whole draw call.
        let tab_bar = self.tab_bar_state();
        // v1.11 audit (PLAN_audit_fix_batch3 C4): the pre-draw state the
        // overlay/settings builders need is captured as OWNED per-domain
        // snapshots while `self` is only shared-borrowed. The old inline
        // locals compiled only because every borrowed projection stayed in
        // this function body; hoisted into helpers, any `&self`-tied view
        // struct would conflict with the `&mut sessions` borrow below.
        // AiSettingsView / PaletteFormView are therefore constructed AT
        // their use point from these snapshots, never stored in a struct
        // that crosses the `active_mut()` boundary.
        let settings_is_narrow = self.settings_is_narrow();
        let terminal_owns_ime = self.overlay_input_owner().is_none();
        // v1.12.26 (P1-01): computed here beside `terminal_owns_ime` — the
        // owner probe borrows all of `&self`, which conflicts with the
        // `terminal` borrow held from :80 down to the anim_active consumer
        // below, so the result is hoisted into this owned local.
        let overlay_text_input_active = self.overlay_input_owner().is_some_and(|owner| {
            crate::input_router::OverlayInputOwner::TEXT_INPUT_HOLDERS.contains(&owner)
        });
        // C1 gating: with the settings panel closed the whole settings
        // construction chain is skipped (String collection included) — the
        // panel was the only consumer of this data (评审已核实).
        let settings_open = self.settings.open;
        let settings_ai_snap = settings_open.then(|| self.ai_settings_snapshot());
        let settings_owned_snap = settings_open.then(|| self.settings_owned_snapshot());
        let palette_snap = self.palette_snapshot();
        let prev_drawn = self.sessions.prev_drawn_tab();
        // v1.7.3-C: snapshot the bookmarked-block set before the mutable
        // `tab` borrow below. `annotation_store()` borrows `self.sessions`
        // immutably, which would conflict with `active_mut()`.
        // v1.11 audit (PLAN_audit_fix_batch3 C3): refcount bump, not a clone.
        let bookmarked_blocks = std::sync::Arc::clone(&self.bookmarked_blocks);
        // v1.12.25 (audit 3-B, P1-01): unreachable — the empty-tabs guard at
        // the top of run_redraw already returned; this arm is type-forced,
        // not a panic path.
        let Some(tab) = self.sessions.active_mut() else {
            return;
        };
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
        tab.with_terminal(|terminal| {
            if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                terminal
                    .editor_mut()
                    .buffer
                    .ensure_cursor_visible(prompt_max_rows);
            }
        });
        // v1.3 Batch 5.5 / v1.11 audit P1-1 (PLAN_audit_fix_batch3 C2):
        // compute pane layouts BEFORE the mutable `pane` borrow below. The
        // split tree is read-only structure; background panes' Terminals are
        // read-only during draw (only the active pane's selection_handler
        // mutates).
        //
        // ORDER CONTRACT (Tab::active_pane_views): the read-only
        // `split_tree().layout` walk below MUST complete before
        // `active_pane_views()` is called — the returned views hold `&mut
        // Tab` for the rest of the draw scope, so nothing else may touch
        // the tab in that window.
        let content_rect: crate::layout::Rect = match self.renderer.as_ref() {
            Some(renderer_ref) => {
                let chrome_top = renderer_ref
                    .tab_bar_height()
                    .max(renderer_ref.titlebar_height());
                let sidebar_placement = crate::ui_tokens::sidebar_placement(
                    self.panel.open,
                    renderer_ref.sidebar_width(),
                    renderer_ref.sidebar_push_width(),
                );
                [
                    renderer_ref.padding_x + sidebar_placement.terminal_push_width,
                    renderer_ref.padding_y + chrome_top,
                    renderer_ref.viewport.0 - renderer_ref.padding_x,
                    renderer_ref.viewport.1 - renderer_ref.padding_y,
                ]
            }
            None => [0.0, 0.0, 0.0, 0.0],
        };
        let pane_layouts_snapshot: Vec<(weft_core::pane_layout::PaneId, crate::layout::Rect)> =
            tab.split_tree().layout(content_rect);
        let active_pane_id = tab.active_pane_id();
        let active_pane_rect = pane_layouts_snapshot
            .iter()
            .find(|(id, _)| *id == active_pane_id)
            .map(|(_, rect)| *rect)
            .unwrap_or(content_rect);
        // T10 P1: the active pane's guard plus every background pane's guard
        // are scoped to this block — `clear_background_grid_dirty` below
        // re-locks the background panes, so the guards MUST drop first
        // (D9 rule 2: no nested locks).
        {
            let crate::tab::PaneViewSet {
                active: pane,
                backgrounds,
            } = tab.active_pane_views();
            // Pair each background view with its layout rect — the same fusion the raw-pointer
            // bridge used to do inline. Empty for single-pane tabs; renderer-None's degenerate
            // [0,0,0,0] rects are never consumed (draw is gated on `Some(renderer)` below).
            // T10 P1: `terminal: &view.terminal` reborrows the background pane's
            // owned guard — the draw is the only consumer (guard covers the frame).
            let background_panes: Vec<crate::renderer::PaneRenderInfo> = pane_layouts_snapshot
                .iter()
                .filter(|(id, _)| *id != active_pane_id)
                .filter_map(|(id, rect)| {
                    let view = backgrounds.iter().find(|b| b.pane_id == *id)?;
                    Some(crate::renderer::PaneRenderInfo {
                        rect: *rect,
                        terminal: &view.terminal,
                        block_scroll: view.block_scroll,
                        submit_on_ctrl_enter: self.config_state.config.editor.submit_on_ctrl_enter,
                        pane_session_id: view.pane_session_id,
                    })
                })
                .collect();
            tracing::debug!(
                content_rect = ?content_rect,
                active_id = ?active_pane_id,
                active_rect = ?active_pane_rect,
                pane_layouts = ?pane_layouts_snapshot,
                bg_count = background_panes.len(),
                "draw pane layouts"
            );
            // T10 P1: the active pane's terminal guard covers the whole draw.
            // `as_deref_mut` reborrow keeps the guard local (its scope ends after
            // the dirty-clear) while `terminal` behaves as `&mut Terminal`.
            let mut terminal_guard = pane.lock_terminal();
            if let Some(terminal) = terminal_guard.as_deref_mut() {
                if let Some(renderer) = &mut self.renderer {
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
                                _ => unreachable!("variant excluded by matches! above"), // v1.12.23 audit batch 1: message, not bare unreachable
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
                                    PaletteEntry::Runbook(entry) => (
                                        entry.command.clone(),
                                        entry.description.clone(),
                                        "Runbook",
                                    ),
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

                    // v1.11 audit (PLAN_audit_fix_batch3 C1/C4): rebuild the
                    // borrowed views at their use point from the owned snapshots —
                    // `view_params`' receiver is `&SettingsState`, so its borrows
                    // project onto `self.settings` only and stay disjoint from the
                    // `&mut sessions` borrow held via `tab`. `palette_entries` and
                    // `palette_banner`/`palette_submode_input` above stay built in
                    // the pane-borrowed section by design (C4 快照边界).
                    let palette_form_view =
                        palette_snap
                            .form
                            .as_ref()
                            .map(|form| crate::overlay::PaletteFormView {
                                workflow_name: &form.workflow_name,
                                fields: &form.fields,
                                current_field: form.current_field,
                            });
                    let settings_params = settings_ai_snap.as_ref().and_then(|ai_snap| {
                        settings_owned_snap.as_ref().map(|owned| {
                            let settings_ai = crate::overlay::AiSettingsView {
                                enabled: self.settings.draft.ai.is_configured(),
                                model: self.settings.draft.ai.model.as_deref().unwrap_or(""),
                                base_url: &ai_snap.base_url,
                                max_tokens: self.settings.draft.ai.effective_max_tokens(),
                                timeout_secs: self.settings.draft.ai.effective_timeout_secs()
                                    as u32,
                                enable_command_generation: self
                                    .settings
                                    .draft
                                    .ai
                                    .enable_command_generation,
                                enable_error_diagnosis: self
                                    .settings
                                    .draft
                                    .ai
                                    .enable_error_diagnosis,
                                models: &ai_snap.model_names,
                                connection_status: &ai_snap.connection_label,
                                testing: self.ai_connection_status.is_testing(),
                                observability: &ai_snap.observability,
                            };
                            self.settings
                                .view_params(owned, settings_ai, settings_is_narrow)
                        })
                    });
                    let overlays = crate::overlay::build_overlay_stack(
                        terminal,
                        crate::overlay::PanelViewParams {
                            panel_width: renderer.sidebar_width(),
                            panel_open: self.panel.open,
                            panel_query: &self.panel.query,
                            panel_ime_preedit: &self.panel.ime_preedit,
                            panel_selection: self.panel.selection,
                            panel_expanded: self.panel.expanded,
                            panel_search_focused: self.panel.search_focused,
                            panel_scroll_offset: self.panel.scroll_offset,
                        },
                        crate::overlay::ImeViewParams {
                            ime_preedit: &pane.ime_preedit,
                            ime_preedit_cursor: pane.ime_preedit_cursor,
                            terminal_owns_ime,
                        },
                        crate::overlay::PaletteViewParams {
                            palette_open: self.palette.open,
                            palette_query: &self.palette.query,
                            palette_selection: self.palette.selection,
                            palette_entries: &palette_entries,
                            palette_banner: &palette_banner,
                            palette_submode_input: &palette_submode_input,
                            palette_ime_preedit: &self.palette.ime_preedit,
                            palette_ime_preedit_cursor: self.palette.ime_preedit_cursor,
                            palette_form: palette_form_view.as_ref(),
                        },
                        crate::overlay::PromptViewParams {
                            prompt_selection: terminal.editor().buffer.selection_range(),
                            submit_on_ctrl_enter: self
                                .config_state
                                .config
                                .editor
                                .submit_on_ctrl_enter,
                        },
                        settings_params.as_ref(),
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
                            let offset = grid.scroll_offset().min(sb_len);
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
                            ime_preedit: self.find.ime_preedit.clone(),
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
                            ime_preedit: self.note_editor.ime_preedit.clone(),
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
                    let is_running =
                        terminal.block_tracker().phase() == ShellPhase::CommandExecuting;
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
                            crate::frame_trace::FrameTraceRecorder::begin(
                                true,
                                self.frame_id,
                                reason,
                            );
                    }
                    renderer.draw(
                        terminal,
                        &mut pane.selection_handler,
                        blink_on,
                        self.window_runtime.cursor_blink_phase,
                        &overlays,
                        block_scroll,
                        scroll_metrics,
                        self.interaction.scrollbar_hovered
                            || self.interaction.scrollbar_drag.is_some(),
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
                    // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING) DEBUG
                    // probe (stage 4/4): first present after a Resized — closes the
                    // resize-blank-interval chain (stretch → dimension-only →
                    // repaint).
                    // v1.11.10 (PLAN_v11110 M-B/D-f): fields extended with the
                    // live-resize/forced context; delta > 2ms is a warning (the
                    // same-tick draw target) instead of the default debug level —
                    // machine self-evidence for the drag-stretch fix.
                    if let Some(since) = renderer.take_resize_present_probe() {
                        let since_ms = since.elapsed().as_millis();
                        if since_ms > 2 {
                            tracing::warn!(
                                since_resize_ms = since_ms,
                                live_resize = renderer.live_resize_active,
                                forced,
                                "RESIZE_PROBE first_present",
                            );
                        } else {
                            tracing::debug!(
                                since_resize_ms = since_ms,
                                live_resize = renderer.live_resize_active,
                                forced,
                                "RESIZE_PROBE first_present",
                            );
                        }
                    }
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
                        // v1.12.24.1 (P1-3): note-first — the note editor owns IME
                        // input while open (keyboard.rs captures all keys; the ime
                        // controller intercepts pre-router), so the candidate window
                        // anchors at the note card. `.or` keeps palette fallback.
                        let note_area = self
                            .note_editor
                            .open
                            .then(|| crate::paint::text::note_editor_ime_area(&ctx));
                        // v1.12.26 (P1-02): the find bar owns IME composition now —
                        // anchor the macOS candidate window at the bar's query input
                        // instead of the terminal cursor (palette/note precedent).
                        let find_area = self
                            .find
                            .open
                            .then(|| crate::paint::text::find_ime_area(&ctx));
                        // v1.12.26 (P1-03): same anchor treatment for the panel
                        // search box, gated on the box actually holding keyboard
                        // focus (mirrors the OverlayInputOwner::PanelSearch gate).
                        let panel_area =
                            (self.panel.open && self.panel.search_focused).then(|| {
                                crate::paint::text::panel_ime_area(&ctx, renderer.sidebar_width())
                            });
                        crate::ime::update_cursor_area(
                            window,
                            ctx,
                            terminal,
                            note_area
                                .or(palette_snap.ime_area)
                                .or(find_area)
                                .or(panel_area),
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
                    } else if overlay_text_input_active {
                        // v1.12.26 (P1-01): an open text-input overlay (palette/
                        // find/panel-search/note) must keep its own caret blinking
                        // regardless of the terminal cursor's state — the old
                        // terminal-only terms froze the shared phase (invisible
                        // when e.g. a command was executing, since the blink thread
                        // stopped waking the loop and no frame was produced).
                        true
                    } else if terminal.show_block_view() {
                        terminal.block_tracker().phase() == ShellPhase::AtPrompt
                    } else {
                        terminal.cursor_visible && terminal.cursor_style.is_blinking()
                    };
                    self.window_runtime
                        .cursor_anim_active
                        .store(anim_active, Ordering::Relaxed);
                    // F3-2 + P1: timer keeps running while a command executes, with no
                    // reduce-motion term — animation is suppressed downstream: the
                    // frame prep above pins renderer.spinner_phase to -1.0 under
                    // reduce_motion and row_paint's `>= 0.0` gate skips the glyph
                    // (spinner_char_for_phase's reduce branch is unreachable there).
                    // The wake instead refreshes the P1 elapsed label — information,
                    // not animation (per-second text; negligible cost, no second
                    // cadence). Grid view stays excluded: F3-2's flicker lesson.
                    let spinner_active = terminal.block_tracker().phase()
                        == ShellPhase::CommandExecuting
                        && terminal.show_block_view();
                    self.window_runtime
                        .spinner_anim_active
                        .store(spinner_active, Ordering::Relaxed);
                }
                // (from the former post-draw block) clear the active pane's
                // dirty rows under the same guard — unconditional exactly as
                // before the T10 migration (runs with or without a renderer).
                terminal.grid_mut().clear_all_dirty();
            }
        }
        // v1.0 P0-b: clear the grid's per-row dirty flags now that
        // the renderer has consumed them. (T10 P1: the active-pane half was
        // moved into the scoped draw block above — it clears under the same
        // guard, unconditionally exactly as before.) The next frame will
        // mark rows dirty only if new PTY output / cursor movement changes
        // them, enabling incremental rendering. The `set_prev_drawn_tab`
        // call comes after `tab`'s last use so NLL releases the borrow.
        // v1.12.2 B3-2 (PLAN_S2_render): background panes' dirty flags are
        // consumed by the per-pane incremental row caches the same way —
        // clear them so the next frame marks only newly written rows.
        tab.clear_background_grid_dirty(active_pane_id);
        self.sessions.set_prev_drawn_tab(active);
        self.update_accessibility_tree();

        // No busy-loop redraw here: the PTY reader thread and the
        // cursor-blink timer wake the loop via `AppEvent::Wake`
        // whenever there is work (see `user_event`). This lets the CPU
        // idle instead of spinning at vsync.
    }

    // v1.12.25 (3-B-2 P2-02): the owned pre-draw snapshot helpers
    // (palette_ime_cursor_area / ai_settings_snapshot /
    // settings_owned_snapshot / palette_snapshot + their structs) moved
    // verbatim to redraw/snapshots.rs — no local remnants.
}
