//! `DEC private-mode (`handle_dec_private_mode` / `set_mouse_protocol` / `swap_alt`)` bodies for the Terminal facade. vt/mod.rs keeps the struct,
//! `process()`, and core accessors (v1.13.8 S2 zero-behavior file-budget
//! split; `impl Terminal` cross-file blocks per the screen_exit /
//! kitty_keyboard precedent). Bodies moved verbatim.
use super::Terminal;
use crate::grid::Cursor;
use crate::input::MouseProtocol;

impl Terminal {
    /// Swap the primary and alternate screen buffers (DEC 1049/47).
    ///
    /// `save_cursor_and_clear` distinguishes the two modes:
    /// - `true` (1049): save the primary cursor, clear the alt screen on
    ///   entry, restore the cursor on exit.
    /// - `false` (47): plain swap, no cursor save/restore, no clear.
    ///
    /// Modelled on Alacritty's `swap_alt` (O(1) `mem::swap`) with the
    /// parameterised clear/restore semantics from Warp's `SwapScreen` mode.
    fn swap_alt(&mut self, save_cursor_and_clear: bool) {
        if !self.capabilities.alt_active {
            if save_cursor_and_clear {
                self.saved_cursor = Some(self.grid.cursor.clone());
                self.alt_grid.clear();
            }
            // Alternate screen starts fresh with the cursor at home (0,0).
            self.alt_grid.cursor = Cursor::default();
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.capabilities.alt_active = true;
            // v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): stamp the entry instant so
            // tui_cols_kind() can apply the sustained-alt hysteresis (continuous
            // residency >= 250ms before reporting Full).
            self.capabilities.alt_active_since = Some(std::time::Instant::now());
            // OSC 8 state is viewport-relative — entering the alt screen
            // invalidates any cell_map entries from the primary grid.
            self.hyperlinks.clear_cell_map();
            self.active_hyperlink_id = None;
            // FIX_ORPHAN_PARSE_ERROR_OUTPUT: a TUI taking the alt screen
            // invalidates the staged pre-exec line bytes — drop them so no
            // later orphan `133;D` can synthesize a block from dead context.
            self.preexec_staging.clear();
        } else {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            if save_cursor_and_clear {
                if let Some(c) = self.saved_cursor.take() {
                    self.grid.cursor = c;
                }
            }
            self.capabilities.alt_active = false;
            // v1.10.30 (FIX_LESS_ALT_COLS_JUMP): record the exit instant so
            // subsequent re-entries can distinguish isolated vs burst entry.
            self.capabilities.alt_last_exit = Some(std::time::Instant::now());
            // v1.10.28 (FIX_TRANSIENT_ALT_COLS_FLIP): leaving the alt screen
            // clears the sustained-residency stamp; the primary screen has no
            // Full-width entitlement.
            self.capabilities.alt_active_since = None;
            // v1.10.12: leaving the alt screen ends any in-progress history
            // peek so show_block_view() reverts to the primary-screen rules
            // and the renderer shows the restored primary grid, not a stale
            // BlockView overlay.
            //
            // v1.10.19: kept unconditional — the resize feedback loop that
            // used to hammer this path (primary-screen TUI SIGWINCH repaints
            // toggling DEC 1049 every ~130ms, each toggle clearing the peek
            // and yanking a scrolled-up history back to the grid) is broken
            // at the source (stable cols + winsize dedup + rescale debounce),
            // so the only alt exits here are real ones, where clearing the
            // peek is correct (see alt_screen_exit_clears_history_peek).
            self.capabilities.alt_screen_history_peek = false;
            // Restoring the primary grid — alt-screen hyperlinks are gone.
            self.hyperlinks.clear_cell_map();
            self.active_hyperlink_id = None;
        }
        tracing::info!(
            active = self.capabilities.alt_active,
            rows = self.grid.num_rows,
            cols = self.grid.num_cols,
            "alt-screen toggled"
        );
        // v1.10.26 Batch D (D-2): count every real flip for the app-layer
        // batch-diff detection and burst-storm signature.
        self.alt_flip_count = self.alt_flip_count.wrapping_add(1);
    }

    /// Handle DEC private mode set/reset (CSI ? <n> h/l).
    ///
    /// v1.11.15 (FIX A): this parse is the AUTHORITATIVE undo of the
    /// reader scanner's set — the pre-clear runs UNCONDITIONALLY (h and l
    /// both clear; the set_mouse_protocol change guard must not gate it).
    pub(super) fn handle_dec_private_mode(&mut self, mode: u16, set: bool) {
        if crate::input::mouse_suppress::MOUSE_SUPPRESS_CLEAR_MODES.contains(&mode) {
            if let Some(flag) = &self.mouse_suppress {
                flag.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        match mode {
            1 => self.capabilities.app_cursor_keys = set, // DECCKM
            6 => {
                // DECOM: CUP is relative to the scroll region.
                self.origin_mode = set;
                tracing::debug!(set, "DECOM origin mode toggled");
            }
            7 => { /* DECAWM — auto wrap mode, always on */ }
            25 => {
                self.cursor_visible = set; // DECTCEM — cursor show/hide
            }
            47 | 1049 => {
                // Swap only on a real state change.
                if set != self.capabilities.alt_active {
                    self.swap_alt(mode == 1049);
                }
                if !set {
                    self.synchronized_output_started = None;
                }
            }
            2004 => self.bracketed_paste = set, // Bracketed paste
            2026 => {
                if set {
                    self.begin_primary_screen_synchronized_frame();
                    self.synchronized_output_started
                        .get_or_insert_with(std::time::Instant::now);
                } else {
                    self.finish_primary_screen_synchronized_frame();
                    self.synchronized_output_started = None;
                }
                tracing::debug!(set, "DEC synchronized output toggled");
            }
            9 => {
                let new = if set {
                    MouseProtocol::X10
                } else {
                    MouseProtocol::Off
                };
                self.set_mouse_protocol(new, 9);
            }
            1000 => {
                let new = if set {
                    MouseProtocol::Normal
                } else {
                    MouseProtocol::Off
                };
                self.set_mouse_protocol(new, 1000);
            }
            1002 => {
                let new = if set {
                    MouseProtocol::ButtonEvent
                } else {
                    MouseProtocol::Off
                };
                self.set_mouse_protocol(new, 1002);
            }
            1003 => {
                let new = if set {
                    MouseProtocol::AnyEvent
                } else {
                    MouseProtocol::Off
                };
                self.set_mouse_protocol(new, 1003);
            }
            // v1.0 fix: SGR-1006 mouse ENCODING (selects the format of mouse
            // reports, independent of whether reporting is on). vim/tmux/htop
            // enable this together with 1000/1002/1003. Previously ignored →
            // we always emitted SGR format, corrupting apps that expected
            // legacy encoding and leaking bytes as visible text (vim `~@k`).
            1006 => self.capabilities.sgr_mouse = set,
            // SGR pixel-mode (1015) and urxvt-mode (1015): not implemented;
            // apps that request them fall back to our default (SGR-1006 when
            // sgr_mouse, legacy otherwise).
            _ => tracing::trace!(mode, set, "unhandled DEC private mode"),
        }
    }

    /// v1.11.8 (PLAN_v1118 M-B): single write point for the mouse-protocol
    /// DEC modes (9/1000/1002/1003) with a change guard — TUIs re-assert
    /// their DECSET modes on every repaint, so an unconditional log would
    /// spam per prompt/frame. Logs only on an actual protocol transition.
    fn set_mouse_protocol(&mut self, new: MouseProtocol, decset_mode: u16) {
        if new != self.capabilities.mouse_protocol {
            self.capabilities.mouse_protocol = new;
            tracing::info!(?new, decset_mode, "DECSET mouse protocol negotiated");
        }
    }
}
