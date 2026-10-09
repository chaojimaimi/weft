//! Terminal-routing tail stages of `handle_mouse_press` (v1.12.27b P1-02),
//! moved verbatim from `mouse_press_controller.rs` — baseline :643-806.
//! `selecting` / `block_view` (baseline :643-644) are shared locals of the
//! last cascade levels; they stay in the caller skeleton and arrive here as
//! explicit parameters (3-B-2 shared-variable contract).

use super::*;

impl App {
    /// v1.12.27b (P1-02): verbatim move of the block-hover update (baseline
    /// :646-652) — no hit-testing, always falls through.
    pub(crate) fn press_block_hover(
        &mut self,
        y: f64,
        button: winit::event::MouseButton,
        block_view: bool,
    ) {
        if button == winit::event::MouseButton::Left && block_view {
            let selected = self.block_at(y as f32).flatten();
            if selected != self.interaction.block_selected {
                self.interaction.block_selected = selected;
                self.request_redraw();
            }
        }
    }

    /// v1.12.27b (P1-02): verbatim move of the host-chrome guard (baseline
    /// :654-659) — a press outside the terminal content rectangle consumed
    /// the press; inside it falls through to the button match.
    pub(crate) fn press_chrome_guard(&mut self, x: f64, y: f64, block_view: bool) -> PressOutcome {
        // Host chrome is not a clamped alias for Grid row 0. Consume presses
        // there before selection, paste, context-menu or PTY mouse routing.
        // BlockView has its own row model.
        if !block_view && !self.terminal_content_contains(x, y) {
            return PressOutcome::Consumed;
        }
        PressOutcome::NotHit
    }

    /// v1.12.27b (P1-02): verbatim move of the button match (baseline
    /// :661-806). The Right arm's context-menu paths (baseline :748/:760)
    /// consumed the press; normal completion falls through to the tail
    /// `request_redraw` in the caller — exactly as the original did.
    pub(crate) fn press_button_action(
        &mut self,
        x: f64,
        y: f64,
        button: winit::event::MouseButton,
        selecting: bool,
        block_view: bool,
    ) -> PressOutcome {
        match button {
            winit::event::MouseButton::Left => {
                if selecting {
                    // v1.10.26 (rust-reviewer N1): a fresh press resets the
                    // fractional autoscroll carry — a stale sub-row remainder
                    // from the PREVIOUS drag must not jolt the new drag's
                    // first 40ms tick.
                    self.interaction.selection_autoscroll_carry = 0.0;
                    if block_view {
                        // Block view: hit-test to a content anchor and start a
                        // block-view selection. v1.10.26: any mouse_down on a
                        // selectable row starts a NEW selection (the anchor
                        // overrides any previous one — Warp’s “无法取消” fix),
                        // and the document fingerprint is recorded so a
                        // structural change next frame clears a stale one.
                        if let Some(anchor) = self.pixel_to_block_view_pos(x, y) {
                            if let Some(pane) = self.sessions.active_mut() {
                                let fingerprint = pane
                                    .with_terminal(|t| {
                                        crate::selection::block_selection_fingerprint(
                                            t.block_tracker().session_blocks(),
                                            t.screen_head_lines(),
                                        )
                                    })
                                    .unwrap_or_default();
                                crate::selection::start_block_selection(
                                    &mut pane.selection_handler,
                                    anchor,
                                    fingerprint,
                                );
                            }
                        } else {
                            // Click missed every selectable row (e.g. on the
                            // prompt box, CWD bar, or empty padding). Clear the
                            // existing selection so the user gets visual
                            // feedback that the previous selection is gone.
                            if let Some(tab) = self.sessions.active_mut() {
                                tab.selection_handler.clear();
                                // v1.10.20 (S1): clearing the block selection
                                // releases the delayed history-view exit (a
                                // migrated drag's selection is block-space only —
                                // its grid half is already gone) — sync drops the
                                // snapshot view back to the live grid.
                                tab.sync_primary_history_view();
                            }
                        }
                    } else {
                        // Grid view (alt-screen): classic grid selection.
                        let pos = self.pixel_to_grid(x, y);
                        let mode = if self.interaction.mods.state().shift_key() {
                            SelectionMode::Block
                        } else {
                            SelectionMode::Simple
                        };
                        if let Some(tab) = self.sessions.active_mut() {
                            tab.selection_handler.start(pos, mode);
                        }
                    }
                }

                // If mouse protocol is active, send mouse event to PTY.
                // Always compute a grid pos for PTY mouse reporting (the
                // foreground program speaks grid coordinates, not block rows).
                let grid_pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Left, MouseAction::Press, grid_pos);
            }
            winit::event::MouseButton::Middle => {
                // Middle click: paste (v1.12.25 audit 3-B P1-01: no session —
                // nothing to paste into).
                if let Some(tab) = self.sessions.active() {
                    self.drain_effects(vec![crate::effect::Effect::Paste {
                        session_id: tab.session_id,
                    }]);
                }
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Middle, MouseAction::Press, pos);
            }
            winit::event::MouseButton::Right => {
                // Block view: open context menu on a block. block_at now
                // supports both completed blocks (including those with no
                // output) and the in-flight (running) command.
                if let Some(id) = self.block_at(y as f32) {
                    // v1.12.25 (audit 3-B, P1-01): unreachable without an
                    // active tab (block view needs a terminal) — type-forced.
                    let Some(session_id) = self.sessions.active().map(|tab| tab.session_id) else {
                        return PressOutcome::Consumed;
                    };
                    self.reset_ime_context("context menu opened");
                    self.save_focus_for_modal(crate::scene::FocusId::ContextMenu);
                    self.interaction.context_menu = Some(ContextMenu {
                        session_id,
                        block_id: id,
                        x: x as f32,
                        y: y as f32,
                        selection: 0,
                    });
                    self.request_redraw();
                    return PressOutcome::Consumed;
                }

                if selecting {
                    // Right click: extend selection.
                    if block_view {
                        if let Some(anchor) = self.pixel_to_block_view_pos(x, y) {
                            if let Some(pane) = self.sessions.active_mut() {
                                let has_selection =
                                    pane.selection_handler.block_view_selection.is_some();
                                let fingerprint = pane
                                    .with_terminal(|t| {
                                        crate::selection::block_selection_fingerprint(
                                            t.block_tracker().session_blocks(),
                                            t.screen_head_lines(),
                                        )
                                    })
                                    .unwrap_or_default();
                                if has_selection {
                                    pane.selection_handler.extend_block_view(anchor);
                                } else {
                                    crate::selection::start_block_selection(
                                        &mut pane.selection_handler,
                                        anchor,
                                        fingerprint,
                                    );
                                }
                            }
                        }
                    } else {
                        let pos = self.pixel_to_grid(x, y);
                        if let Some(tab) = self.sessions.active_mut() {
                            if tab.selection_handler.selection.is_none() {
                                tab.selection_handler.start(pos, SelectionMode::Simple);
                            } else {
                                tab.selection_handler.extend(pos);
                            }
                        }
                    }
                }
                let pos = self.pixel_to_grid(x, y);
                self.send_mouse_event(MouseButton::Right, MouseAction::Press, pos);
            }
            _ => {}
        }
        PressOutcome::NotHit
    }
}
