//! Keyboard routing extracted from `main.rs`.
//!
//! `handle_key_event` is the entry point from winit's KeyboardInput. It
//! resolves keybindings, routes to overlay handlers (palette/settings/find/
//! context-menu/panel-search), falls through to the editor, and finally
//! encodes passthrough bytes for the PTY.

use crate::input_router::{OverlayInputContext, OverlayInputOwner};
use crate::macos_system::clipboard_paste;
use tracing::{info, warn};
use weft_core::input::{KeyCode, Modifiers};
use winit::event::Modifiers as WinitModifiers;
use winit::keyboard::KeyCode as WinitKeyCode;

impl crate::App {
    pub(crate) fn overlay_input_owner(&self) -> Option<OverlayInputOwner> {
        OverlayInputOwner::resolve(OverlayInputContext {
            note_editor_open: self.note_editor.open,
            palette_open: self.palette.open,
            settings_open: self.settings.open,
            find_open: self.find.open,
            context_menu_open: self.interaction.context_menu.is_some(),
            panel_search_focused: self.panel.open && self.panel.search_focused,
        })
    }

    pub(crate) fn handle_key_event(
        &mut self,
        kind: weft_core::input::KittyEventKind,
        key_code: WinitKeyCode,
        mods: WinitModifiers,
        text: Option<&str>,
    ) {
        let Some(key) = crate::event_replay::map_winit_key(key_code) else {
            return;
        };

        let mut m = Modifiers::empty();
        if mods.state().shift_key() {
            m |= Modifiers::SHIFT;
        }
        if mods.state().control_key() {
            m |= Modifiers::CONTROL;
        }
        if mods.state().alt_key() {
            m |= Modifiers::ALT;
        }
        if mods.state().super_key() {
            m |= Modifiers::SUPER;
        }

        // v1.11.4 (PLAN_v1114 §2.2, L2 pipe): the kitty negotiation state of
        // the ACTIVE terminal gates how far a non-Press event travels.
        // Releases (winit state=Released) never drove the app pipeline
        // before (the winit arm filtered Pressed) and still must not: only
        // a negotiated event-types (0b10) passthrough app sees them, and
        // then via a dedicated fast path that bypasses every app-side
        // consumer (note editor / keybindings / overlays / editor box) —
        // a Cmd+V Release must never paste into the find bar.
        // v1.12.23 audit batch 1: `self.tab()` is a bare `sessions.active()`
        // index — with every tab closed (last shell `exit`) any keystroke
        // reaching here panicked. Empty tabs reads as "no negotiated terminal"
        // (flags 0, identical to a tab without a terminal), so a Release still
        // early-returns below while session-less actions (Cmd+T NewTab) keep
        // routing through execute_action. Only the state READS are guarded —
        // no whole-handler early return.
        let has_sessions = !self.sessions.is_empty();
        let kitty_flags = if has_sessions {
            self.tab()
                .terminal
                .as_ref()
                .map(|t| t.keyboard_protocol_flags())
                .unwrap_or(0)
        } else {
            0
        };
        if kind == weft_core::input::KittyEventKind::Release {
            if (kitty_flags & weft_core::input::kitty::FLAG_REPORT_EVENT_TYPES) == 0 {
                return;
            }
            // Defense in depth: with flags 0 above, a Release already returned
            // at the gate — this only matters if that invariant ever changes.
            if !has_sessions
                || !self.tab().terminal.as_ref().is_some_and(|t| {
                    t.effective_input_mode() == weft_core::input::InputMode::Passthrough
                })
            {
                return;
            }
            self.forward_key_to_pty(
                key,
                m,
                text,
                kind,
                kitty_flags,
                weft_core::input::InputMode::Passthrough,
            );
            return;
        }

        // v1.7.3-C: Note editor captures all keyboard input when open.
        // Intercepts before keybinding lookup and overlay routing so the
        // user's typing never leaks to the PTY or other overlays.
        if self.note_editor.open {
            self.handle_note_editor_key(key, m, text);
            return;
        }

        let bound_action = self.config_state.keybindings.lookup(key, m);
        let has_terminal = self
            .sessions
            .tab(self.sessions.active_idx())
            .is_some_and(|tab| tab.terminal.is_some());
        match crate::input_router::route_keyboard_entry(has_sessions, has_terminal, bound_action) {
            crate::input_router::KeyboardEntryRoute::Action(action) => {
                self.execute_action(action);
                return;
            }
            crate::input_router::KeyboardEntryRoute::Consume => return,
            crate::input_router::KeyboardEntryRoute::Session => {}
        }

        // Configurable keybindings: resolve (key, mods) → action. If it maps to
        // a weft action (copy/paste/scroll/reload), dispatch and consume; else
        // fall through to encoding the key for the PTY.
        //
        // v0.9 fix: when the Find bar is open, intercept Paste (Cmd+V) and
        // SelectAll (Cmd+A) so they target the find query, not the shell
        // editor. Other Cmd chords (Cmd+R regex toggle, Cmd+I case toggle)
        // are handled inside `handle_find_key` below.
        if self.find.open
            && m.contains(Modifiers::SUPER)
            && matches!(key, KeyCode::Char('v') | KeyCode::Char('a'))
        {
            if key == KeyCode::Char('v') {
                if let Some(text) = clipboard_paste() {
                    self.find.query.push_str(&text);
                    self.arm_find_refresh();
                }
                return;
            }
            if key == KeyCode::Char('a') {
                // Select-all in the find bar: clear and re-type from clipboard?
                // For now, just signal "select all" by moving cursor to end —
                // the find bar is single-line with no selection model. No-op.
                return;
            }
        }
        // v1.8.6: When the Palette is open, intercept Cmd+V so paste targets
        // the palette input (query or submode buffer), not the shell PTY.
        // Without this, bound_action(Action::Paste) fires first and sends
        // bracketed-paste bytes to the PTY, bypassing the palette entirely.
        if self.palette.open && m.contains(Modifiers::SUPER) && key == KeyCode::Char('v') {
            if let Some(text) = clipboard_paste() {
                match &mut self.palette.submode {
                    crate::palette_state::PaletteSubMode::Search => {
                        self.palette.query.push_str(&text);
                        self.palette.selection = 0;
                        self.refresh_palette_results();
                    }
                    crate::palette_state::PaletteSubMode::AiCommand { buffer, .. }
                    | crate::palette_state::PaletteSubMode::CreateWorkflow { buffer, .. }
                    | crate::palette_state::PaletteSubMode::EditWorkflow { buffer, .. }
                    | crate::palette_state::PaletteSubMode::SelectTheme { buffer, .. } => {
                        buffer.push_str(&text);
                    }
                    crate::palette_state::PaletteSubMode::ConfirmDelete { .. } => {}
                }
                self.request_redraw();
            }
            return;
        }
        // v1.8.8: When an overlay (Palette/Settings/Find/ContextMenu/
        // PanelSearch) is open, route to the overlay handler FIRST. This
        // prevents user-configured keybindings (e.g. `esc = "cancel_ai_request"`)
        // from intercepting keys the overlay needs (like Esc to close).
        // If the overlay doesn't consume the key, fall through to keybindings.
        let overlay_owner = self.overlay_input_owner();
        if let Some(owner) = overlay_owner {
            let overlay_consumed = match owner {
                // v1.12.24 (N-1): defensive arm — the note-editor intercept
                // above already returns early, but if routing ever reaches
                // the overlay match with the card open, it consumes all keys.
                OverlayInputOwner::NoteEditor => {
                    self.handle_note_editor_key(key, m, text);
                    true
                }
                OverlayInputOwner::Palette => self.handle_palette_key(key, m, text),
                OverlayInputOwner::Settings => self.handle_settings_key(key, m, text),
                OverlayInputOwner::Find => self.handle_find_key(key, m, text),
                OverlayInputOwner::ContextMenu => self.handle_context_menu_key(key, m),
                OverlayInputOwner::PanelSearch => self.handle_panel_key(key, m),
            };
            if overlay_consumed {
                return;
            }
            // Overlay didn't consume (e.g. modifier chord it ignores) —
            // fall through to keybinding dispatch.
        }

        if let Some(action) = bound_action {
            if self.execute_action(action) {
                return;
            }
        }

        // Editor takeover: at the prompt with integration ready, keys drive the
        // input-box editor instead of being forwarded to the PTY. Enter submits
        // (writes the command); Shift+Enter grows the box. Drops back to
        // passthrough automatically in alt-screen / command-running / SSH.
        let input_mode = self
            .tab()
            .terminal
            .as_ref()
            .map(|t| t.effective_input_mode())
            .unwrap_or(weft_core::input::InputMode::Passthrough);
        if input_mode == weft_core::input::InputMode::Editor {
            let prev_lines = self
                .tab()
                .terminal
                .as_ref()
                .map(|t| t.editor().line_count())
                .unwrap_or(1);
            let consumed = self.handle_editor_key(key, m, text);
            let new_lines = self
                .tab()
                .terminal
                .as_ref()
                .map(|t| t.editor().line_count())
                .unwrap_or(1);
            if new_lines != prev_lines {
                self.recompute_layout();
            }
            if consumed {
                self.request_redraw();
                return;
            }
        }

        // v1.10.26 (FIX_SELECTION_CONTENT_ANCHORS): Esc clears transient
        // selections (Warp parity — "点空/Esc = clear"). The ESC byte still
        // forwards to the PTY below (vim/less keep their own key handling).
        if key == KeyCode::Escape {
            let pane = self.sessions.active_mut();
            if pane.selection_handler.selecting
                || pane.selection_handler.block_view_selection.is_some()
                || pane.selection_handler.selection.is_some()
            {
                pane.selection_handler.clear();
                self.request_redraw();
            }
        }

        self.forward_key_to_pty(key, m, text, kind, kitty_flags, input_mode);
    }

    /// v1.11.4 (PLAN_v1114 §2.1): the sync + encode + effects tail shared by
    /// the normal key path and the L2 release fast path. The kitty branch
    /// applies ONLY in passthrough — the editor/overlay branches consume
    /// their keys first ("天然在前"), but a non-consumed key while the editor
    /// is open must never leak kitty bytes into the PTY. DECCKM precedent
    /// (:200-206): re-read the terminal state per key event.
    fn forward_key_to_pty(
        &mut self,
        key: KeyCode,
        m: Modifiers,
        text: Option<&str>,
        kind: weft_core::input::KittyEventKind,
        kitty_flags: u8,
        input_mode: weft_core::input::InputMode,
    ) {
        let app_cursor_keys = self
            .tab()
            .terminal
            .as_ref()
            .map(|t| t.app_cursor_keys())
            .unwrap_or(false);
        {
            let ih = &mut self.tab_mut().input_handler;
            ih.app_cursor_keys = app_cursor_keys;
            ih.kitty_flags = if input_mode == weft_core::input::InputMode::Passthrough {
                kitty_flags
            } else {
                0
            };
            ih.kitty_event_kind = kind;
        }

        let bytes = crate::ime::encode_passthrough_key(&self.tab().input_handler, key, m, text);
        // Diagnostic (set RUST_LOG=weft_app=debug to see): the exact bytes we
        // send for each key, including whether DECCKM/app-cursor mode is on.
        let input_seq = self.tab_mut().next_input_seq();
        tracing::debug!(
            session_id = self.tab().session_id,
            input_seq,
            ?key,
            ?m,
            app_cursor_keys = self.tab().input_handler.app_cursor_keys,
            ?bytes,
            "key → pty"
        );
        // v1.11.7 (PLAN_v1117 §三 M2.1, D-c/P1-1): this is the keyboard
        // outbound chokepoint — a non-empty encoding means real user input
        // was forwarded to the PTY. Mark the terminal so the noninteractive
        // render tier falls back to the classic takeover for interactive
        // TUIs. Auto-replies (XTGETTCAP/DECRQSS/DA/DSR) and OSC 52 write-
        // backs bypass this path entirely, so negotiation can never taint the
        // flag. Empty encodings (e.g. modifier-only presses) forward nothing
        // and must not count.
        if !bytes.is_empty() {
            if let Some(t) = self.tab_mut().terminal.as_mut() {
                t.note_interactive_stdin();
            }
        }
        // v1.11.11 (M-B): the Effect family targets the stable session id. The
        // active tab exists for the whole handler, so forwarding semantics
        // are unchanged; the drain reverse-looks-up the id at delivery.
        let effects =
            crate::effect::passthrough_key_effects(self.sessions.active().session_id, bytes);
        self.drain_effects(effects);
    }

    /// Handle a key while the panel search box is focused. Returns true if
    /// consumed (search typing / arrow nav / expand / unfocus). Modifier
    /// chords fall through (returns false) so keybindings still work.
    pub(crate) fn handle_panel_key(&mut self, key: KeyCode, mods: Modifiers) -> bool {
        // Let cmd/ctrl/alt chords pass through to keybindings / PTY.
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return false;
        }
        match key {
            // v0.9 fix: Esc unfocuses the search box instead of closing the
            // panel. The panel itself closes via the Cmd+Shift+B keybinding
            // or by clicking outside the sidebar.
            KeyCode::Escape => {
                self.panel.search_focused = false;
                self.request_redraw();
                true
            }
            KeyCode::Up => {
                self.panel.selection = self.panel.selection.saturating_sub(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Down => {
                self.panel.selection = self.panel.selection.saturating_add(1);
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Backspace => {
                self.panel.query.pop();
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            KeyCode::Enter => {
                // v0.9 fix: send the selected command to the prompt input
                // (Warp-style: Enter on a history entry reruns the command).
                self.send_panel_selection_to_input();
                true
            }
            KeyCode::Char(c) if !c.is_control() => {
                self.panel.query.push(c);
                self.clamp_panel_scroll();
                self.clamp_panel_selection();
                self.request_redraw();
                true
            }
            _ => false,
        }
    }

    /// v1.7.3-C: Handle keyboard input for the inline note editor.
    /// Called when `self.note_editor.open` is true — all keys are captured.
    ///
    /// - Enter saves the note to `AnnotationStore::set_note` and closes.
    /// - Esc closes without saving.
    /// - Backspace/Delete removes a char.
    /// - Left/Right/Home/End move the caret.
    /// - Cmd+V pastes from clipboard.
    /// - Printable chars insert at the caret.
    fn handle_note_editor_key(&mut self, key: KeyCode, mods: Modifiers, text: Option<&str>) {
        // Cmd+V: paste from clipboard.
        if mods.contains(Modifiers::SUPER) && key == KeyCode::Char('v') {
            if let Some(pasted) = clipboard_paste() {
                // v1.12.24 (N-1): shared caret-insertion semantics (also used
                // by the IME commit path) — one insertion code path, no forks.
                self.insert_text_into_note_editor(&pasted);
            }
            self.request_redraw();
            return;
        }
        // Let other cmd/ctrl/alt chords fall through (no-op).
        if mods.intersects(Modifiers::SUPER | Modifiers::CONTROL | Modifiers::ALT) {
            return;
        }

        use crate::paint::command_surface::CommandSurfaceKeyAction;
        match crate::paint::command_surface::resolve_command_surface_key(key, mods) {
            CommandSurfaceKeyAction::Cancel => {
                self.note_editor.close();
                // v1.12.24 (N-1): discard residual marked text so Esc isn't
                // intercepted by the IME (focus.rs palette/find precedent;
                // v1.8.7's native discard sits in the else branch the note
                // intercept never reaches).
                self.reset_ime_context("note editor closed");
                self.request_redraw();
                return;
            }
            CommandSurfaceKeyAction::Accept => {
                self.commit_note_editor();
                return;
            }
            CommandSurfaceKeyAction::MoveUp | CommandSurfaceKeyAction::PageUp => {
                // Home: move cursor to start.
                self.note_editor.cursor = 0;
                self.request_redraw();
                return;
            }
            CommandSurfaceKeyAction::MoveDown | CommandSurfaceKeyAction::PageDown => {
                // End: move cursor to end.
                self.note_editor.cursor = self.note_editor.buffer.len();
                self.request_redraw();
                return;
            }
            _ => {}
        }

        match key {
            KeyCode::Backspace => {
                let ne = &mut self.note_editor;
                if ne.cursor > 0 {
                    // Walk back one char boundary (UTF-8 safe).
                    let prev = ne.buffer[..ne.cursor].char_indices().last().map(|(i, _)| i);
                    if let Some(prev) = prev {
                        ne.buffer.replace_range(prev..ne.cursor, "");
                        ne.cursor = prev;
                    }
                }
                self.request_redraw();
            }
            KeyCode::Delete => {
                let ne = &mut self.note_editor;
                if ne.cursor < ne.buffer.len() {
                    let next = ne.buffer[ne.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| ne.cursor + i)
                        .unwrap_or(ne.buffer.len());
                    ne.buffer.replace_range(ne.cursor..next, "");
                }
                self.request_redraw();
            }
            KeyCode::Left => {
                let ne = &mut self.note_editor;
                if ne.cursor > 0 {
                    let prev = ne.buffer[..ne.cursor].char_indices().last().map(|(i, _)| i);
                    if let Some(prev) = prev {
                        ne.cursor = prev;
                    }
                }
                self.request_redraw();
            }
            KeyCode::Right => {
                let ne = &mut self.note_editor;
                if ne.cursor < ne.buffer.len() {
                    let next = ne.buffer[ne.cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| ne.cursor + i)
                        .unwrap_or(ne.buffer.len());
                    ne.cursor = next;
                }
                self.request_redraw();
            }
            KeyCode::Home => {
                self.note_editor.cursor = 0;
                self.request_redraw();
            }
            KeyCode::End => {
                self.note_editor.cursor = self.note_editor.buffer.len();
                self.request_redraw();
            }
            _ => {
                if let KeyCode::Char(c) = key {
                    let resolved = crate::app::helpers::resolve_text_char(
                        text,
                        c,
                        mods.contains(Modifiers::SHIFT),
                    );
                    if !resolved.is_control() {
                        // v1.12.24 review P3: the shared insertion fn (also
                        // serves the IME commit + paste arms) — one code path.
                        let mut utf8 = [0u8; 4];
                        self.insert_text_into_note_editor(resolved.encode_utf8(&mut utf8));
                        self.request_redraw();
                    }
                }
            }
        }
    }

    /// v1.7.3-C: Save the note editor buffer to the annotation store and
    /// close the editor. Called on Enter.
    fn commit_note_editor(&mut self) {
        let target = self.note_editor.target_block_id;
        let buffer = std::mem::take(&mut self.note_editor.buffer);
        self.note_editor.close();
        // v1.12.24 (N-1): same IME cleanup as the Cancel arm — the Enter
        // commit must not leave marked text intercepting the next Esc.
        self.reset_ime_context("note editor closed");

        if let Some(bid) = target {
            if let Some(store) = self.sessions.annotation_store() {
                let note = if buffer.trim().is_empty() {
                    None
                } else {
                    Some(buffer.as_str())
                };
                match store.set_note(bid, note) {
                    Ok(()) => {
                        info!(block_id = ?bid, note_len = buffer.len(), "note saved");
                        // v1.7.3-D: sync the updated annotation to the search
                        // index so the note text is searchable via Palette.
                        self.sync_bookmark_to_search_index(bid);
                    }
                    Err(e) => warn!(error = %e, "failed to save note"),
                }
            }
        }
        self.request_redraw();
    }

    /// v1.12.24 (N-1): insert `text` at the note editor caret (cursor
    /// clamped, caret advances past the insertion). The ONE insertion
    /// semantic shared by the Cmd+V paste arm and the IME commit path —
    /// byte-offset arithmetic identical to the old inline paste arm.
    pub(crate) fn insert_text_into_note_editor(&mut self, text: &str) {
        let ne = &mut self.note_editor;
        let insert_pos = ne.cursor.min(ne.buffer.len());
        ne.buffer.insert_str(insert_pos, text);
        ne.cursor = insert_pos + text.len();
    }
}
