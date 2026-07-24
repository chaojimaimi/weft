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
    pub(crate) fn close_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        self.reset_ime_context("palette closed");
        self.palette.close();
        self.clear_prev_focus_if_no_modal();
    }

    /// v1.0 S1: close the Settings panel, discarding any unsaved draft
    /// changes. Used when another modal opens so only one owns keyboard
    /// input.
    pub(crate) fn close_settings(&mut self) {
        if !self.settings.open {
            return;
        }
        self.reset_ime_context("settings closed");
        self.settings.close();
        self.clear_prev_focus_if_no_modal();
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
                .and_then(|tab| tab.terminal.as_ref())
                .map(|t| t.editor().is_completing())
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
