//! History panel controller extracted from the application shell.

use super::*;
use crate::block_component::{completed_block_layout_rows, completed_block_output_rows};

impl App {
    /// v1.14.0 (PLAN_v1140 T2): true when the sidebar width animation may
    /// run — expanded (non-Compact) windows only. Compact overlay drawers
    /// ignore `sidebar_width_override` entirely (ui_tokens::sidebar_visual_
    /// width returns the fixed metrics width), so their toggle stays instant.
    fn sidebar_anim_allowed(&self) -> bool {
        crate::app::motion::gated(self.window_runtime.reduce_motion)
            && self
                .renderer
                .as_ref()
                .is_some_and(|r| r.sidebar_push_width() != 0.0)
            // rust-reviewer P3: a drag owns the width channel — starting a
            // tween under it would give the per-frame override write and the
            // drag's set_sidebar_width a double-writer race.
            && self.interaction.sidebar_drag.is_none()
    }

    /// v1.14.0 (T2): start the sidebar OPEN width animation (0→target,
    /// 180ms ease-out). The pre-animation override state is captured and
    /// restored when the tween lands (`sidebar_width_restore`) so a
    /// responsive-default user (`None`) is never frozen into a fixed width.
    /// Called with `panel.open` already true; Reduce Motion / Compact callers
    /// keep the instant path (no tween, override untouched).
    pub(super) fn animate_sidebar_open(&mut self) {
        if !self.sidebar_anim_allowed() {
            return;
        }
        if self.panel.sidebar_width_anim.is_some() && self.panel.sidebar_width_opening {
            return; // already opening/open — the tween keeps driving
        }
        let now = std::time::Instant::now();
        // Start value: a mid-close reverse resumes from the animated width;
        // a closed sidebar sweeps from 0.
        let current = match self.panel.sidebar_width_anim.as_ref() {
            Some(anim) => anim.value_at(now),
            None => 0.0,
        };
        let Some(r) = &self.renderer else {
            return;
        };
        // Target = the width the sidebar settles at AFTER the animation —
        // the pre-animation override's effective logical width (None
        // resolves to the responsive default). While a close tween runs the
        // capture already holds the original value (keep it); otherwise it
        // is the live override, read before the first frame clobbers it.
        let restore = self
            .panel
            .sidebar_width_restore
            .unwrap_or(r.sidebar_width_override);
        let logical_viewport = r.viewport().0 / r.scale() as f32;
        let target = crate::ui_tokens::sidebar_visual_width(logical_viewport, restore);
        self.panel.sidebar_width_restore = Some(restore);
        self.panel.sidebar_width_opening = true;
        // v1.14.1 (F3): a fresh tween applies its first frame immediately.
        self.panel.sidebar_width_anim_last_apply = None;
        self.panel.sidebar_width_anim = Some(crate::app::motion::Tween::new(
            current,
            target,
            crate::app::motion::SIDEBAR_OPEN_DUR,
            now,
        ));
    }

    /// v1.14.0 (T2): start the sidebar CLOSE width animation (current→0,
    /// 150ms ease-out). `panel.open` stays true while the sidebar shrinks;
    /// the real close runs when the tween lands (先动画后状态 — plan §3).
    /// Reduce Motion / Compact callers keep the instant path.
    pub(super) fn animate_sidebar_close(&mut self) {
        if !self.sidebar_anim_allowed() {
            self.panel.close();
            return;
        }
        if self.panel.sidebar_width_anim.is_some() && !self.panel.sidebar_width_opening {
            return; // already closing — let the tween land
        }
        let Some(r) = &self.renderer else {
            self.panel.close();
            return;
        };
        let current = r.sidebar_width() / r.scale() as f32;
        // rust-reviewer P1-3: the close must capture the restore SYMMETRICALLY
        // with the open. Without this, the per-frame tween writes clobber
        // `sidebar_width_override` and the land step restores… the captured
        // value from a possibly-ancient open — a drag-persisted 300pt width
        // was lost forever on close, and a mid-close reopen re-opened the
        // animation channel with the OVERRIDDEN intermediate value baked in.
        let restore = self
            .panel
            .sidebar_width_restore
            .take()
            .unwrap_or(r.sidebar_width_override);
        self.panel.sidebar_width_restore = Some(restore);
        self.panel.sidebar_width_opening = false;
        // v1.14.1 (F3): a fresh tween applies its first frame immediately.
        self.panel.sidebar_width_anim_last_apply = None;
        self.panel.sidebar_width_anim = Some(crate::app::motion::Tween::new(
            current,
            0.0,
            crate::app::motion::SIDEBAR_CLOSE_DUR,
            std::time::Instant::now(),
        ));
    }

    /// v1.14.0 (T2): per-frame sidebar width animation maintenance, run at
    /// the top of `run_redraw` (before every `sidebar_width()` consumer).
    /// In-flight frames write the tween value STRAIGHT into
    /// `renderer.sidebar_width_override` (bypassing `set_sidebar_width`'s
    /// `[240, 360]` clamp and `sidebar_visual_width`'s second clamp — the
    /// plan's 直写通道) and recompute layout so the terminal reflows each
    /// frame (per-frame SIGWINCH; the zoom 220ms per-frame resize is the
    /// accepted precedent). Landing restores the pre-animation override and
    /// performs the real close for a closing animation.
    pub(super) fn maintain_sidebar_width_anim(&mut self) {
        let now = std::time::Instant::now();
        let (value, finished) = match self.panel.sidebar_width_anim.as_ref() {
            Some(anim) => (anim.value_at(now), anim.finished(now)),
            None => return,
        };
        // v1.14.1 (F3): 8ms apply-throttle (PLAN_v1141 §3.2.2) — the
        // override write + recompute_layout run at most once per
        // RESIZE_COMMIT_MIN_INTERVAL (the drag path's PTY-commit cadence;
        // the tween value is still computed every frame above). A finished
        // tween always applies so the landing below is never stalled
        // (终值必写).
        if !crate::app::motion::should_apply(
            now,
            self.panel.sidebar_width_anim_last_apply,
            finished,
        ) {
            return;
        }
        self.panel.sidebar_width_anim_last_apply = Some(now);
        if !finished {
            if let Some(r) = &mut self.renderer {
                r.sidebar_width_override = Some(value);
            }
            self.recompute_layout();
            return;
        }
        // Land: restore the pre-animation override state, then finish.
        let opening = self.panel.sidebar_width_opening;
        let restore = self.panel.sidebar_width_restore.take().unwrap_or(None);
        self.panel.sidebar_width_anim = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = restore;
        }
        if !opening {
            self.panel.close();
        }
        self.recompute_layout();
        self.request_redraw();
    }

    /// v1.14.0 (T2): cancel an in-flight sidebar width animation, freezing
    /// the current animated value into the override (拖拽起始取消动画取当前
    /// 值 — plan §3). The drag channel owns the width from here on; the
    /// restore capture is dropped (the drag release persists its own value).
    pub(super) fn cancel_sidebar_width_anim(&mut self) {
        let Some(anim) = self.panel.sidebar_width_anim.take() else {
            return;
        };
        let current = anim.value_at(std::time::Instant::now());
        self.panel.sidebar_width_restore = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = Some(current);
        }
    }

    /// v1.14.0 (T2): Reduce Motion flip — snap an in-flight sidebar width
    /// animation to its end value immediately (lifecycle 1Hz bridge).
    pub(super) fn snap_sidebar_width_anim(&mut self) {
        if self.panel.sidebar_width_anim.is_none() {
            return;
        }
        let opening = self.panel.sidebar_width_opening;
        let restore = self.panel.sidebar_width_restore.take().unwrap_or(None);
        self.panel.sidebar_width_anim = None;
        self.panel.sidebar_width_opening = false;
        if let Some(r) = &mut self.renderer {
            r.sidebar_width_override = restore;
        }
        if !opening {
            self.panel.close();
        }
        self.recompute_layout();
    }

    pub(super) fn update_sidebar_drag(&mut self, pointer_x: f64) -> bool {
        let Some(drag) = self.interaction.sidebar_drag else {
            return false;
        };
        let Some(renderer) = &mut self.renderer else {
            return true;
        };
        let width = crate::ui_tokens::sidebar_width_after_drag(
            drag.start_width,
            drag.start_x,
            pointer_x,
            renderer.scale() as f32,
        );
        // v1.14.2 (A2/PLAN §2): the drag path previously ran
        // set_sidebar_width + recompute_layout on EVERY CursorMoved (zero
        // throttle) — the defect-two firehose. It now shares the F3 tween
        // gate's 8ms cadence (`app::motion::should_apply` =
        // RESIZE_COMMIT_MIN_INTERVAL): an in-window event only parks its
        // width in `pending_drag_width`; a due event applies the current
        // width and clears the park. `handle_mouse_release` writes the
        // parked final width (终值必写).
        let due = drag_apply_step(
            &mut self.panel.sidebar_drag_last_apply,
            &mut self.panel.pending_drag_width,
            std::time::Instant::now(),
            width,
        );
        if due.is_none() {
            return true;
        }
        renderer.set_sidebar_width(Some(width));
        self.recompute_layout();
        self.request_redraw();
        true
    }

    /// v1.3.2: Update the pane divider ratio from the pointer position during
    /// a drag. Computes `new_ratio` from the drag state's `bounds` + the
    /// current pointer, calls `tab.set_pane_ratio`, then `recompute_layout`
    /// (which handles the throttled PTY resize). No-op if no drag is active.
    pub(super) fn update_pane_divider_drag(&mut self, x: f32, y: f32) -> bool {
        let Some(drag) = self.interaction.pane_divider_drag else {
            return false;
        };
        let [bx0, _by0, bx1, _by1] = drag.bounds;
        let new_ratio = match drag.axis {
            crate::paint::pane_dividers::DividerAxis::Vertical => {
                ((x - bx0) / (bx1 - bx0).max(1.0)).clamp(0.0, 1.0)
            }
            crate::paint::pane_dividers::DividerAxis::Horizontal => {
                let [_bx0, by0, _bx1, by1] = drag.bounds;
                ((y - by0) / (by1 - by0).max(1.0)).clamp(0.0, 1.0)
            }
        };
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — nothing to
        // re-ratio; keep consuming the drag (same as the no-divider arm).
        match self
            .sessions
            .active_mut()
            .map(|tab| tab.set_pane_ratio(drag.first, drag.second, new_ratio))
        {
            Some(Ok(true)) => {
                self.recompute_layout();
                self.request_redraw();
                true
            }
            Some(Ok(false)) | None => true, // no divider / no tab; keep consuming
            Some(Err(e)) => {
                tracing::warn!(error = ?e, "pane divider drag: set_ratio failed; aborting drag");
                self.interaction.pane_divider_drag = None;
                false // release the drag so subsequent moves don't re-log
            }
        }
    }

    pub(super) fn finish_panel_scrollbar_drag(&mut self) -> bool {
        if self.interaction.panel_scrollbar_drag.take().is_none() {
            return false;
        }
        self.request_redraw();
        true
    }

    pub(super) fn update_panel_scrollbar_drag(&mut self, pointer_y: f32) -> bool {
        let Some(drag) = self.interaction.panel_scrollbar_drag else {
            return false;
        };
        self.panel.scroll_offset = crate::panel_scrollbar::scroll_offset_for_pointer(
            &drag.layout,
            pointer_y,
            drag.grab_offset,
        );
        self.clamp_panel_scroll();
        self.clamp_panel_selection();
        self.request_redraw();
        true
    }

    /// F3-4: Maximum number of block rows that fit in the panel's visible list
    /// area, based on the renderer's actual viewport and cell height. Falls
    /// back to the grid row count when the renderer isn't available yet.
    /// v1.13.18 P4: derived from the shared PanelLayout geometry
    /// (`layout::panel_visible_row_budget`) instead of the removed
    /// `(vp_h/ch) − 2` approximation, so paint, scroll clamp, hit-test and
    /// accessibility all read ONE number.
    fn panel_max_visible(&self) -> usize {
        if let Some(r) = self.renderer.as_ref() {
            let chrome_top = r.layout_ctx.map(|c| c.chrome_top).unwrap_or(0.0);
            let ch = r.cell_height() as f32;
            let layout = crate::layout::layout_panel(
                chrome_top,
                r.cell_width() as f32,
                ch,
                r.sidebar_width(),
                r.viewport().1,
            );
            let rows_bottom = crate::layout::panel_rows_bottom(
                &layout,
                r.viewport().1,
                self.panel_footer_visible(),
                ch,
            );
            return crate::layout::panel_visible_row_budget(
                layout.list_top,
                layout.row_height,
                ch,
                rows_bottom,
            );
        }
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| t.grid().num_rows))
            .unwrap_or(0)
    }

    /// F3-4: Total count of blocks matching the panel query (no truncation).
    /// Used to clamp `scroll_offset` so the list can't scroll past the end.
    pub(super) fn panel_total_filtered(&self) -> usize {
        let total = self
            .sessions
            .active()
            .and_then(|tab| {
                tab.with_terminal(|t| {
                    panel_filtered_count(t.block_tracker().blocks(), &self.panel.query)
                })
            })
            .unwrap_or(0);
        total
    }

    /// F3-4: Clamp `panel.scroll_offset` to `[0, max(0, total - visible)]`.
    pub(super) fn clamp_panel_scroll(&mut self) {
        let total = self.panel_total_filtered();
        let visible = self.panel_max_visible();
        self.panel.scroll_offset =
            crate::paint::ui_helpers::clamp_panel_scroll(total, visible, self.panel.scroll_offset);
    }

    /// v1.13.18 P4: the「加载更早」footer band shows iff the tab has ANY
    /// finished block — the same criterion `paint/panel.rs` and the panel
    /// scene builders already use. Query filtering is deliberately NOT
    /// applied (a `panel_total_filtered()`-based criterion would fork the
    /// paint vs hit-test footer visibility).
    pub(super) fn panel_footer_visible(&self) -> bool {
        self.sessions
            .active()
            .and_then(|tab| tab.with_terminal(|t| !t.block_tracker().blocks().is_empty()))
            .unwrap_or(false)
    }

    /// Keep the selection inside the filtered list. v1.13.18 P3:
    /// `panel.selection` is LIST-space (0 = newest), so the bound is the
    /// filtered total — no longer the per-window visible count.
    pub(super) fn clamp_panel_selection(&mut self) {
        let total = self.panel_total_filtered();
        self.panel.selection =
            crate::paint::ui_helpers::clamp_panel_list_selection(self.panel.selection, total);
    }

    /// v1.13.18 P3: after a list-space selection move, slide the viewport
    /// window so the selection stays visible (keep-visible), then clamp the
    /// offset. Pure math lives in `ui_helpers::panel_keep_visible_offset`.
    pub(super) fn keep_panel_selection_visible(&mut self) {
        let visible = self.panel_max_visible();
        self.panel.scroll_offset = crate::paint::ui_helpers::panel_keep_visible_offset(
            self.panel.selection,
            self.panel.scroll_offset,
            visible,
        );
        self.clamp_panel_scroll();
    }

    /// The [`BlockId`] of the currently selected panel row, if any.
    /// v1.13.18 P3: list-space `nth(selection)` over the newest-first
    /// filtered list — no `scroll_offset` window, so the selection follows
    /// one command while the list scrolls.
    pub(super) fn panel_selected_block_id(&self) -> Option<BlockId> {
        let terminal = self.sessions.active().and_then(|tab| tab.lock_terminal())?;
        terminal
            .block_tracker()
            .blocks()
            .iter()
            .rev()
            .filter(|b| block_matches_query(b, &self.panel.query))
            .nth(self.panel.selection)
            .map(|b| b.id)
    }

    /// v0.9 fix: send the panel's currently-selected command to the prompt
    /// editor (Warp-style "click/Enter to rerun"). Looks up the block by id,
    /// strips any prompt prefix, and sets the editor buffer. Silently no-ops
    /// when not at the prompt (command running / alt-screen active) to avoid
    /// stashing text that would resurface unexpectedly.
    pub(super) fn send_panel_selection_to_input(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        // Borrow the terminal immutably to find the command, then release
        // before mutating the editor.
        let cmd: Option<String> = {
            let Some(t) = self.sessions.active().and_then(|tab| tab.lock_terminal()) else {
                return;
            };
            if t.effective_input_mode() != weft_core::input::InputMode::Editor {
                return;
            }
            t.block_tracker()
                .blocks()
                .iter()
                .find(|b| b.id == block_id)
                .map(|b| strip_prompt_prefix(&b.command))
        };
        if let Some(cmd) = cmd {
            if !cmd.is_empty() {
                if let Some(mut t) = self
                    .sessions
                    .active_mut()
                    .and_then(|tab| tab.lock_terminal())
                {
                    t.editor_mut().buffer.set_text(&cmd);
                    // v0.9: select all so Cmd+C copies the command without
                    // needing a drag-select first. The user can still adjust
                    // the selection by clicking in the prompt box.
                    t.editor_mut().buffer.select_all();
                }
                // Unfocus the panel so the editor gets subsequent keystrokes.
                self.panel.search_focused = false;
                self.request_redraw();
            }
        }
    }

    /// v0.9 W2: scroll the terminal's block view so the panel-selected block
    /// is visible, and arm a 1.5s accent highlight. Called when the user
    /// clicks a row in the history panel (or presses Cmd+Enter while the
    /// panel is open).
    pub(super) fn scroll_to_panel_selection(&mut self) {
        let block_id = match self.panel_selected_block_id() {
            Some(id) => id,
            None => return,
        };
        self.scroll_to_block_id(block_id);
    }

    pub(super) fn scroll_to_block_id(&mut self, block_id: BlockId) {
        // Only meaningful in block view (grid view has no block layout).
        if !self.block_view_active() {
            return;
        }
        // T10 P1 (D9 rule 2): `set_block_scroll` below re-enters the
        // terminal lock (→ sync_primary_history_view), so this guard covers
        // only the read/layout phase and is dropped before the scroll.
        let target = {
            let Some(term) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            else {
                return;
            };
            let cwd_header_active = crate::layout::block_cwd_header_active(
                term.effective_input_mode() == weft_core::input::InputMode::Editor,
                term.cwd().is_some(),
            );
            let prompt_lines = crate::block_component::block_prompt_lines(&term);
            let blocks = term.block_tracker().session_blocks();
            let viewport_rows = term.grid().num_rows;
            let cols = term.grid().num_cols;
            let block_idx = blocks.iter().position(|b| b.id == block_id);
            let Some(block_idx) = block_idx else { return };

            // Count rows from the bottom up to the target block's command line.
            // Layout (bottom→top): Output[N-1] at row 0, …, Output[0] at row
            // N-1, Command at row N, then the accessible Header band and gap.
            // For each block BELOW the target (i.e. with higher index), add its
            // full height using the renderer's current Header row span.
            let header_rows = self.renderer.as_ref().map_or(1, |r| r.block_header_rows());
            let mut rows_from_bottom = 0usize;
            for (i, b) in blocks.iter().enumerate().rev() {
                if i == block_idx {
                    break;
                }
                rows_from_bottom +=
                    completed_block_layout_rows(b, cols, header_rows, viewport_rows);
            }
            rows_from_bottom += completed_block_output_rows(&blocks[block_idx], cols);
            // Position the block's command line at ~1/3 from the bottom of the
            // viewport so the user sees the command + most of its output above.
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            let visible = renderer.block_visible_rows(prompt_lines, cwd_header_active);
            // v1.11.16: `saturating_sub` already floors at 0 — the trailing
            // `.max(0)` was dead (clippy::unnecessary_min_or_max under CI's
            // `-D warnings`).
            rows_from_bottom.saturating_sub(visible / 3)
        };
        if let Some(tab) = self.sessions.active_mut() {
            tab.set_block_scroll(target);
        }

        // Arm the highlight: accent border around the block for 1.5s.
        self.panel.highlight = Some(block_id);
        self.panel.highlight_until =
            Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
        self.request_redraw();
    }

    pub(super) fn navigate_to_block(&mut self, block_id: BlockId) -> bool {
        let target = self
            .sessions
            .tabs()
            .iter()
            .enumerate()
            .find_map(|(tab_index, tab)| {
                tab.panes().find_map(|(pane_id, pane)| {
                    pane.with_terminal(|terminal| {
                        terminal
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .any(|block| block.id == block_id)
                    })
                    .unwrap_or(false)
                    .then_some((tab_index, pane_id))
                })
            });
        let Some((tab_index, pane_id)) = target else {
            return false;
        };
        self.sessions.set_active(tab_index);
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — target lookup
        // already returned false above, so `None` here is defensive only.
        if self
            .sessions
            .active_mut()
            .is_some_and(|tab| tab.set_active_pane(pane_id).is_err())
        {
            return false;
        }
        self.scroll_to_block_id(block_id);
        true
    }

    /// Local scrollback navigation (page up/down, top, bottom).
    pub(super) fn scroll_action(&mut self, action: Action) {
        let header_rows = self.renderer.as_ref().map_or(1, |r| r.block_header_rows());
        // v1.12.25 (audit 3-B, P1-01): empty-tabs transient — no view to
        // scroll; ignore the action.
        let Some(tab) = self.sessions.active_mut() else {
            return;
        };
        if matches!(
            action,
            Action::ScrollPageUp | Action::ScrollLineUp | Action::ScrollToTop
        ) {
            tab.enter_primary_history_if_active();
        }
        // T10 P1 (D9 rule 2): the block-branch scroll calls re-enter the
        // terminal lock (set_block_scroll → sync_primary_history_view), so the
        // guard below only covers the read/metrics phase and is released
        // before the match.
        //
        // [P2 TOCTOU 登记·评审裁定本轮不修] the read/metrics lock and the
        // apply lock are deliberately SEPARATE (the scroll calls re-enter);
        // the parse worker can change grid/metrics between the two, so the
        // scroll target can be computed from one-batch-stale metrics. Same
        // staleness the pre-worker pump produced between its read and the
        // same-frame scroll; accepted for P2.
        let (rows, _cols, block_view, max_scroll) = {
            let Some(terminal) = tab.lock_terminal() else {
                return;
            };
            let rows = terminal.grid().num_rows;
            let cols = terminal.grid().num_cols;
            // Block view uses a dedicated scroll offset.
            let block_view = terminal.show_block_view();
            let max_scroll = if block_view {
                let cache = self
                    .renderer
                    .as_ref()
                    .map(|r| r.block_layout_cache.borrow());
                let (total, _) = block_content_metrics_with_cache(
                    &terminal,
                    cols,
                    header_rows,
                    cache.as_deref(),
                    None,
                );
                // Compute visible rows from the renderer's actual geometry.
                let prompt_lines = crate::block_component::block_prompt_lines(&terminal);
                let visible = self
                    .renderer
                    .as_ref()
                    .map(|r| {
                        let cwd_header = crate::layout::block_cwd_header_active(
                            terminal.effective_input_mode() == weft_core::input::InputMode::Editor,
                            terminal.cwd().is_some(),
                        );
                        r.block_visible_rows(prompt_lines, cwd_header)
                    })
                    .unwrap_or(rows);
                total.saturating_sub(visible)
            } else {
                0
            };
            (rows, cols, block_view, max_scroll)
        };
        if block_view {
            match action {
                Action::ScrollPageUp => {
                    tab.scroll_up_by(rows);
                    tab.clamp_block_scroll(max_scroll);
                }
                Action::ScrollPageDown => {
                    tab.scroll_down_by(rows);
                }
                Action::ScrollLineUp => {
                    tab.scroll_up_by(1);
                    tab.clamp_block_scroll(max_scroll);
                }
                Action::ScrollLineDown => {
                    tab.scroll_down_by(1);
                }
                Action::ScrollToTop => {
                    tab.set_block_scroll(max_scroll);
                }
                Action::ScrollToBottom => tab.snap_to_bottom(),
                _ => {}
            }
        } else {
            let Some(mut terminal) = tab.lock_terminal() else {
                return;
            };
            let grid = terminal.grid_mut();
            match action {
                Action::ScrollPageUp => grid.scroll_up_history(rows),
                Action::ScrollPageDown => grid.scroll_down_history(rows),
                Action::ScrollLineUp => grid.scroll_up_history(1),
                Action::ScrollLineDown => grid.scroll_down_history(1),
                Action::ScrollToTop => grid.scroll_to_top(),
                Action::ScrollToBottom => grid.scroll_to_bottom(),
                _ => {}
            }
        }
        self.request_redraw();
    }
}

// ── v1.14.2 (A2/PLAN §2): sidebar drag throttle — pure decision kernels ──
//
// Extracted from `update_sidebar_drag` / `handle_mouse_release` so the
// throttle window and the release-time final write are unit-testable
// without App/winit state (the plan's 应用决策提为纯函数测试).

/// One CursorMoved during a sidebar drag. Returns `Some(width)` when the
/// caller must write + reflow NOW (≥8ms since the last apply, per
/// `app::motion::should_apply` — the F3 tween gate's shared cadence), or
/// `None` inside the window: the width parks in `pending` and lands on a
/// due frame or at release.
pub(crate) fn drag_apply_step(
    last_apply: &mut Option<std::time::Instant>,
    pending: &mut Option<f32>,
    now: std::time::Instant,
    width: f32,
) -> Option<f32> {
    if !crate::app::motion::should_apply(now, *last_apply, false) {
        *pending = Some(width);
        return None;
    }
    *last_apply = Some(now);
    *pending = None;
    Some(width)
}

/// The drag release's landing width (终值必写): a parked in-window width
/// wins over the last applied override; with nothing parked the applied
/// value stands.
pub(crate) fn drag_release_width(pending: Option<f32>, applied: Option<f32>) -> Option<f32> {
    pending.or(applied)
}

#[cfg(test)]
mod drag_throttle_tests {
    use super::{drag_apply_step, drag_release_width};
    use std::time::{Duration, Instant};

    const WINDOW: Duration = crate::effect::RESIZE_COMMIT_MIN_INTERVAL;

    /// 节流窗口内多事件只落一次 recompute: events at t0 / +3ms / +6ms
    /// apply once (the first); the in-window widths park instead.
    #[test]
    fn throttle_window_coalesces_burst_events() {
        let mut last = None;
        let mut pending = None;
        let t0 = Instant::now();
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0, 240.0),
            Some(240.0)
        );
        assert_eq!(pending, None);
        assert_eq!(
            drag_apply_step(
                &mut last,
                &mut pending,
                t0 + Duration::from_millis(3),
                250.0
            ),
            None,
            "inside the 8ms window the event must not apply"
        );
        assert_eq!(pending, Some(250.0));
        assert_eq!(
            drag_apply_step(
                &mut last,
                &mut pending,
                t0 + Duration::from_millis(6),
                260.0
            ),
            None
        );
        assert_eq!(pending, Some(260.0), "the newest width parks (latest-wins)");
        // First event past the window applies again and clears the park.
        assert_eq!(
            drag_apply_step(&mut last, &mut pending, t0 + WINDOW, 270.0),
            Some(270.0)
        );
        assert_eq!(pending, None);
    }

    /// 释放必应用终值: a parked width wins over the last applied override;
    /// with nothing parked the applied value stands (zero-move release).
    #[test]
    fn release_lands_pending_over_applied() {
        assert_eq!(
            drag_release_width(Some(260.0), Some(240.0)),
            Some(260.0),
            "终值必写 — the parked width must land"
        );
        assert_eq!(drag_release_width(None, Some(240.0)), Some(240.0));
        assert_eq!(drag_release_width(None, None), None);
    }
}
