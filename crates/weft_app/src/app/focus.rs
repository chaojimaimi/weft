//! Focus management helpers extracted from `main.rs`.
//!
//! These track the keyboard focus before a modal surface opens so it can be
//! restored (visually / for accessibility) when the modal closes. The actual
//! keyboard routing is implicit — when an overlay closes, its key handler
//! stops capturing, so input naturally returns to the editor. The
//! `prev_focus` field is for future semantic/a11y use.

impl crate::App {
    /// v0.9: close the find bar and reset its state. Used when another modal
    /// (palette, panel, …) opens so only one owns keyboard input.
    ///
    /// v1.14.0 (PLAN_v1140 T1) two-step split: this is now the CLOSING step
    /// — it starts the 1→0 fade and flips `interactive` off immediately,
    /// while `open` stays true (visible) until `finish_find_close` performs
    /// the real teardown on a draw-frame maintenance pass. Reduce Motion
    /// lands in a tween-less Closing that finishes on the next pass.
    pub(crate) fn close_find(&mut self) {
        if !self.find.open || self.find.reveal.is_closing() {
            return;
        }
        self.reset_ime_context("find closed");
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.find.reveal.begin_close(now, animated);
    }

    /// v1.14.0 (T1): the real find teardown (the second step of the split).
    /// Called from the draw-frame maintenance pass when the close tween
    /// lands, and synchronously by the reopen paths.
    pub(crate) fn finish_find_close(&mut self) {
        if !self.find.reveal.is_closing() {
            return;
        }
        self.find.reveal.snap_to_end();
        self.find.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v1.14.0 (T1): start the find bar's open reveal (0→1). Reopening while
    /// Closing first runs the real teardown (防抖 — PLAN_v1140 §2.1); an
    /// already-steady-open bar must not restart its fade.
    pub(crate) fn begin_find_reveal(&mut self) {
        if self.find.reveal.is_closing() {
            self.finish_find_close();
        } else if self.find.open {
            return;
        }
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.find.reveal.begin_open(now, animated);
    }

    /// v0.9: close the command palette and reset its state. Used when
    /// another modal (find bar, …) opens so only one owns keyboard input.
    /// v1.8.1: also cancels any in-flight AI request so the background
    /// task doesn't try to write into a closed palette.
    ///
    /// v1.14.0 (T1): CLOSING step — see `close_find` for the split contract.
    /// The AI cancel + content reset move to `finish_palette_close` so the
    /// fading frame keeps painting the real content.
    pub(crate) fn close_palette(&mut self) {
        if !self.palette.open || self.palette.reveal.is_closing() {
            return;
        }
        self.reset_ime_context("palette closed");
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.palette.reveal.begin_close(now, animated);
    }

    /// v1.14.0 (T1): the real palette teardown (second step of the split).
    pub(crate) fn finish_palette_close(&mut self) {
        if !self.palette.reveal.is_closing() {
            return;
        }
        self.palette.reveal.snap_to_end();
        self.ai_state.cancel_all();
        self.palette.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v1.14.0 (T1): start the palette's open reveal (see `begin_find_reveal`).
    pub(crate) fn begin_palette_reveal(&mut self) {
        if self.palette.reveal.is_closing() {
            self.finish_palette_close();
        } else if self.palette.open {
            return;
        }
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.palette.reveal.begin_open(now, animated);
    }

    /// v1.0 S1: close the Settings panel, discarding any unsaved draft
    /// changes. Used when another modal opens so only one owns keyboard
    /// input.
    ///
    /// v1.2.11: 如果存在未保存的预览改动（dirty=true），需要把 renderer
    /// 回滚到 `config_state.config` 的状态——否则用户 Esc 关闭后，renderer
    /// 还停留在预览的字体/透明度/padding 上，与 config 不一致。
    /// 使用 `revert_renderer_to_config` 而非 `apply_config`，因为后者基于
    /// `config_state.config` 做 diff（而 config 没变，diff 为空，不会 restore）。
    /// `revert_renderer_to_config` 无条件重 apply，确保 renderer 与 config
    /// 重新对齐。
    ///
    /// v1.14.0 (T1): CLOSING step — see `close_find`. The dirty-revert +
    /// content teardown move to `finish_settings_close` so the fading frame
    /// keeps painting the panel exactly as the user left it.
    pub(crate) fn close_settings(&mut self) {
        if !self.settings.open || self.settings.reveal.is_closing() {
            return;
        }
        self.reset_ime_context("settings closed");
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.settings.reveal.begin_close(now, animated);
    }

    /// v1.14.0 (T1): the real settings teardown (second step of the split).
    pub(crate) fn finish_settings_close(&mut self) {
        if !self.settings.reveal.is_closing() {
            return;
        }
        self.settings.reveal.snap_to_end();
        let was_dirty = self.settings.dirty;
        self.settings.close();
        self.clear_prev_focus_if_no_modal();
        if was_dirty {
            self.revert_renderer_to_config();
        }
    }

    /// v1.14.0 (T1): start the settings panel's open reveal (see
    /// `begin_find_reveal`).
    pub(crate) fn begin_settings_reveal(&mut self) {
        if self.settings.reveal.is_closing() {
            self.finish_settings_close();
        } else if self.settings.open {
            return;
        }
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.settings.reveal.begin_open(now, animated);
    }

    /// v1.14.0 (T1): start the note editor's open reveal (palette twin; the
    /// note close points live in app/keyboard.rs per the plan's 例外).
    pub(crate) fn begin_note_reveal(&mut self) {
        if self.note_editor.reveal.is_closing() {
            self.finish_note_close();
        } else if self.note_editor.open {
            return;
        }
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.note_editor.reveal.begin_open(now, animated);
    }

    /// v1.14.0 (T1): CLOSING step for the note editor. Called from the two
    /// note close points in app/keyboard.rs (Esc cancel + Enter commit).
    pub(crate) fn begin_note_close(&mut self) {
        if !self.note_editor.open || self.note_editor.reveal.is_closing() {
            return;
        }
        let now = std::time::Instant::now();
        let animated = crate::app::motion::gated(self.window_runtime.reduce_motion);
        self.note_editor.reveal.begin_close(now, animated);
    }

    /// v1.14.0 (T1): the real note teardown (second step of the split).
    pub(crate) fn finish_note_close(&mut self) {
        if !self.note_editor.reveal.is_closing() {
            return;
        }
        self.note_editor.reveal.snap_to_end();
        self.note_editor.close();
    }

    /// v1.14.0 (T1): per-frame reveal maintenance, run at the top of
    /// `run_redraw`. Two duties:
    /// 1. drop finished OPEN tweens — releases the MOTION_LIVE count while
    ///    the settled value stays 1.0 (a lingering tween would pin the tick
    ///    thread at its 16ms cadence forever);
    /// 2. finish finished CLOSE tweens — the real teardown that actually
    ///    sets `open = false` (PLAN_v1140 §2.1 Closing boundary).
    pub(crate) fn maintain_overlay_reveals(&mut self, now: std::time::Instant) {
        self.palette.reveal.drop_finished_open_tween(now);
        self.settings.reveal.drop_finished_open_tween(now);
        self.find.reveal.drop_finished_open_tween(now);
        self.note_editor.reveal.drop_finished_open_tween(now);
        if self.palette.reveal.close_finished(now) {
            self.finish_palette_close();
        }
        if self.settings.reveal.close_finished(now) {
            self.finish_settings_close();
        }
        if self.find.reveal.close_finished(now) {
            self.finish_find_close();
        }
        if self.note_editor.reveal.close_finished(now) {
            self.finish_note_close();
        }
    }

    /// v1.14.0 (T1): Reduce Motion bridge for the overlay reveals, called at
    /// the 1Hz appearance poll (lifecycle_controller) at the SAME update
    /// point as `set_zoom_reduce_motion`. When the gate closes, every
    /// in-flight reveal snaps to its end value; a Closing surface's end
    /// value is "closed", so its real teardown runs immediately. Turning
    /// Reduce Motion OFF needs no catch-up — the next open/close animates.
    pub(crate) fn apply_overlay_reduce_motion(&mut self, reduce: bool) {
        if !reduce {
            return;
        }
        self.palette.reveal.snap_to_end();
        if self.palette.reveal.is_closing() {
            self.finish_palette_close();
        }
        self.settings.reveal.snap_to_end();
        if self.settings.reveal.is_closing() {
            self.finish_settings_close();
        }
        self.find.reveal.snap_to_end();
        if self.find.reveal.is_closing() {
            self.finish_find_close();
        }
        self.note_editor.reveal.snap_to_end();
        if self.note_editor.reveal.is_closing() {
            self.finish_note_close();
        }
    }

    /// Compute the current logical [`FocusId`] from the overlay state. Mirrors
    /// the priority in `OverlayInputOwner::resolve`.
    pub(crate) fn compute_current_focus(&self) -> Option<crate::scene::FocusId> {
        // v1.14.0 (T1): interactive predicates — a Closing surface has
        // already handed input off, so it must not claim the focus slot.
        let active_tab = self.sessions.active_idx();
        crate::paint::command_surface::compute_current_focus(
            self.palette.interactive(),
            self.settings.interactive(),
            self.find.interactive(),
            self.interaction.context_menu.is_some(),
            self.panel.open && self.panel.search_focused,
            /* editor_active */ true,
            self.sessions
                .tab(active_tab)
                .and_then(|tab| tab.with_terminal(|t| t.editor().is_completing()))
                .unwrap_or(false),
            active_tab,
        )
    }

    /// Save the current focus before opening a modal. Does not overwrite an
    /// already-saved focus (so a second modal opening on top of the first
    /// preserves the *original* focus).
    pub(crate) fn save_focus_for_modal(&mut self, opening: crate::scene::FocusId) {
        let current = self.compute_current_focus();
        let prev = crate::paint::command_surface::save_focus_for_modal(
            current,
            self.interaction.prev_focus,
            opening,
        );
        self.interaction.prev_focus = prev;
    }

    /// Clear the saved focus when all modals are closed. Called from each
    /// modal's close path.
    pub(crate) fn clear_prev_focus_if_no_modal(&mut self) {
        if !self.palette.open
            && !self.find.open
            && !self.settings.open
            && self.interaction.context_menu.is_none()
        {
            self.interaction.prev_focus = None;
        }
    }
}
