//! Terminal accessor methods (getters/setters over `Terminal` state).
//!
//! v1.20.3 hygiene WP1 (1C): moved verbatim out of `vt/mod.rs` to bring that
//! file under its line budget. This is a pure code move — no signature,
//! visibility, or body changes. The items below were `pub` (one `pub(crate)`
//! test helper) in `mod.rs` and keep those exact modifiers: `accessors` is a
//! child module of `vt`, so `pub` resolves to the same external surface.
//!
//! `#[inline]` annotations follow the v1.17 WP2 precedent (cross-file moves of
//! per-frame / per-keystroke hot-path functions must stay in the defining
//! CGU): read-side accessors on the render/input paths carry `#[inline]`;
//! cold one-shot setters and config hooks do not.

use super::*;

impl Terminal {
    #[inline]
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    #[inline]
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// The 256-color palette. Seeded from the theme, mutable by OSC 4/104.
    /// The renderer resolves `CellColor::Palette(i)` against this each frame,
    /// so theme switches and OSC edits recolor the screen on the next draw.
    #[inline]
    pub fn palette(&self) -> &[Color; 256] {
        &self.palette
    }

    /// Reseed the whole palette (used on theme switch). Because cells store
    /// palette *indices*, this recolors every existing `Palette(i)` cell on
    /// the next render — OSC runtime overrides are discarded, matching
    /// "switch theme = reset palette".
    pub fn set_palette(&mut self, palette: [Color; 256]) {
        self.palette = palette;
    }

    pub fn set_background_color(&mut self, color: Color) {
        self.background_color = color;
    }

    #[inline]
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    #[inline]
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Borrow the OSC 8 hyperlink registry (read-only). The renderer uses
    /// this for Cmd+Click URL lookup.
    #[inline]
    pub fn hyperlinks(&self) -> &HyperlinkRegistry {
        &self.hyperlinks
    }

    /// Drop all `(row, col) → URL` mappings. Called after a manual
    /// `grid.scroll_offset` change (e.g. FindInGrid scrolling to a match):
    /// the side-map is viewport-relative, so any scroll invalidates it.
    pub fn clear_hyperlink_cell_map(&mut self) {
        self.hyperlinks.clear_cell_map();
    }

    #[inline]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Current git branch (from OSC 9;git=<branch>), for the prompt header.
    #[inline]
    pub fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    /// Effective input routing right now (passthrough vs. editor).
    #[inline]
    pub fn effective_input_mode(&self) -> InputMode {
        effective_mode(
            self.block_tracker.phase(),
            self.capabilities.alt_active,
            self.block_tracker.bootstrap_ready(),
            self.command_from_editor.is_some(),
        )
    }

    /// True between editor submission and the shell's OSC 133 `B` marker.
    /// Input arriving in this short window belongs to the launched command,
    /// not to the prompt/block view.
    #[inline]
    pub fn command_from_editor_pending(&self) -> bool {
        self.command_from_editor.is_some()
    }

    /// Test observability for the preexec staging buffer (see
    /// `blocks::orphan_finalize_tests`): byte length of staged content.
    #[cfg(test)]
    pub(crate) fn preexec_staging_len(&self) -> usize {
        self.preexec_staging.as_str().len()
    }

    /// Whether the alternate screen buffer is currently active.
    #[inline]
    pub fn is_alt_screen_active(&self) -> bool {
        self.capabilities.alt_active
    }

    /// v1.10.26 Batch D (D-2): total DEC 1049/47 flips ever performed. The
    /// app layer diffs this before/after a `process()` batch to detect
    /// alt-screen toggles even when the batch nets the `alt_active` phase to
    /// zero (an h→l pair counts as two flips).
    #[inline]
    pub fn alt_flip_count(&self) -> u64 {
        self.alt_flip_count
    }

    /// Active mouse reporting mode (DEC modes 9/1000/1002/1003).
    #[inline]
    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.capabilities.mouse_protocol
    }

    /// v1.11.15 (FIX A): inject this pane's reader-side mouse-suppression
    /// flag (app calls it once; the reader thread holds the other Arc).
    pub fn set_mouse_suppress_flag(&mut self, flag: MouseSuppressFlag) {
        self.mouse_suppress = Some(flag);
    }

    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.1): record that a real
    /// user input event was forwarded to the PTY during the current command.
    ///
    /// Call sites are strictly event-layer (the app's keyboard outbound path
    /// and paste forwarder) — automatic replies (XTGETTCAP/DECRQSS/DA/DSR)
    /// and OSC 52 clipboard write-backs never call this, so negotiation
    /// traffic cannot flip a TUI's render mode (P1-1).
    #[inline]
    pub fn note_interactive_stdin(&mut self) {
        // v1.11.8 (M-B): the exemption decision point for the noninteractive
        // tier — log only the false→true transition (the call site is the
        // per-keystroke event layer; the flag itself is per-command until
        // settle clears it).
        if !self.capabilities.interactive_stdin_seen {
            self.capabilities.interactive_stdin_seen = true;
            tracing::debug!("interactive stdin seen — command exempted to the classic tier");
        }
    }

    /// Whether `note_interactive_stdin` fired since the last real command
    /// boundary (used by `show_block_view`'s noninteractive tier and by the
    /// app's headless tests).
    #[inline]
    pub fn interactive_stdin_seen(&self) -> bool {
        self.capabilities.interactive_stdin_seen
    }

    /// v1.11.7 (PLAN_v1117_SHADOW_BLOCK_VIEW §三 M1.2): the configured
    /// primary-screen render tier. Injected by the app from
    /// `[experimental] tui_render_mode` at every Terminal construction site;
    /// `Terminal::new` stays Classic (the baseline anchor).
    #[inline]
    pub fn tui_render_mode(&self) -> TuiRenderMode {
        self.tui_render_mode
    }

    pub fn set_tui_render_mode(&mut self, mode: TuiRenderMode) {
        self.tui_render_mode = mode;
        // v1.11.8 (M-B): the tier decides every `show_block_view()` answer —
        // log the injection point (per Terminal construction, not a hot path).
        tracing::debug!(?mode, "tui render mode set");
    }

    /// v1.19.0 (D10): live block draws from `grid::region` row snapshots.
    #[inline]
    pub fn is_live_block_grid_render(&self) -> bool {
        self.live_block_grid_render
    }

    pub fn set_live_block_grid_render(&mut self, enabled: bool) {
        self.live_block_grid_render = enabled;
        // v1.11.8 (M-B) precedent: log per-Terminal injection points.
        tracing::debug!(enabled, "live block grid render set");
    }

    /// Whether SGR-1006 mouse-report encoding is selected (DEC mode 1006).
    #[inline]
    pub fn sgr_mouse(&self) -> bool {
        self.capabilities.sgr_mouse
    }

    /// Application cursor key mode (DECCKM, CSI ?1h/l).
    #[inline]
    pub fn app_cursor_keys(&self) -> bool {
        self.capabilities.app_cursor_keys
    }

    /// v1.11.4 (PLAN_v1114 §1.1): current kitty keyboard-protocol flags for
    /// the ACTIVE screen (stack top; 0 when the stack is empty or the
    /// protocol is disabled). The app re-reads this on every key event,
    /// mirroring the `app_cursor_keys` sync precedent.
    #[inline]
    pub fn keyboard_protocol_flags(&self) -> u8 {
        if !self.kitty_protocol_enabled {
            return 0;
        }
        self.kitty.flags(self.capabilities.alt_active)
    }

    /// v1.11.4 (PLAN_v1114 §1.3): reset hook target — clear BOTH stacks.
    /// Called at OSC 133;D (command end / back at the prompt) and on
    /// PtyExit, so a crash-killed TUI cannot leave negotiated flags
    /// poisoning the shell that comes back.
    pub fn kitty_reset(&mut self) {
        self.kitty.reset();
    }

    /// v1.11.4 (PLAN_v1114 §3): master switch from
    /// `[compat] kitty_keyboard`; apply_config walks every pane's terminal.
    pub fn set_kitty_protocol_enabled(&mut self, enabled: bool) {
        self.kitty_protocol_enabled = enabled;
    }
}
