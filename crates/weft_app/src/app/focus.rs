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
    pub(crate) fn close_find(&mut self) {
        if !self.find.open {
            return;
        }
        self.reset_ime_context("find closed");
        self.find.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v0.9: close the command palette and reset its state. Used when
    /// another modal (find bar, …) opens so only one owns keyboard input.
    /// v1.8.1: also cancels any in-flight AI request so the background
    /// task doesn't try to write into a closed palette.
    pub(crate) fn close_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        self.ai_state.cancel_all();
        self.reset_ime_context("palette closed");
        self.palette.close();
        self.clear_prev_focus_if_no_modal();
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
    pub(crate) fn close_settings(&mut self) {
        if !self.settings.open {
            return;
        }
        self.reset_ime_context("settings closed");
        let was_dirty = self.settings.dirty;
        self.settings.close();
        self.clear_prev_focus_if_no_modal();
        if was_dirty {
            self.revert_renderer_to_config();
        }
    }

    /// Compute the current logical [`FocusId`] from the overlay state. Mirrors
    /// the priority in `OverlayInputOwner::resolve`.
    pub(crate) fn compute_current_focus(&self) -> Option<crate::scene::FocusId> {
        let active_tab = self.sessions.active_idx();
        crate::paint::command_surface::compute_current_focus(
            self.palette.open,
            self.settings.open,
            self.find.open,
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
