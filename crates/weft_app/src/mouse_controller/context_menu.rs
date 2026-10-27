//! Context-menu family (execute / hover / hit-test / dispatch), moved
//! verbatim out of `mouse_controller.rs` (v1.15.2 governance cart,
//! zero-rewrite). Child module of the controller (scroll.rs precedent).

use super::*;

impl App {
    /// Execute a context menu action based on click position.
    pub(crate) fn execute_context_menu(&mut self, menu: &ContextMenu, click_x: f32, click_y: f32) {
        let hit = self.context_menu_hit_at_anchor(menu.x, menu.y, click_x, click_y);
        if let Some(i) = hit {
            let action = CONTEXT_MENU_ITEMS[i].1;
            self.run_context_action(menu.block_id, action);
            self.request_redraw();
            return;
        }
        // Click outside menu items — just close (already taken).
        self.request_redraw();
    }

    /// v1.10.34: context menu hover — move the highlighted item as the
    /// pointer slides across the menu, mirroring the keyboard `Select` path
    /// (context_menu_key_action). Before this, the highlight stayed on the
    /// initial item because CursorMoved with an open menu was consumed by
    /// route_modal_pointer and never reached any hover logic.
    pub(crate) fn update_context_menu_hover(&mut self, x: f64, y: f64) {
        let Some((menu_x, menu_y)) = self.interaction.context_menu.as_ref().map(|m| (m.x, m.y))
        else {
            return;
        };
        let hit = self.context_menu_hit_at_anchor(menu_x, menu_y, x as f32, y as f32);
        if let Some(i) = hit {
            if let Some(menu) = self.interaction.context_menu.as_mut() {
                if menu.selection != i {
                    menu.selection = i;
                    self.request_redraw();
                }
            }
        }
    }

    /// Shared hit-test: rebuild the context menu Scene from the menu's
    /// anchor + renderer geometry and resolve which item (if any) contains
    /// the given point. Used by both the click executor and the hover path.
    fn context_menu_hit_at_anchor(
        &self,
        menu_x: f32,
        menu_y: f32,
        x: f32,
        y: f32,
    ) -> Option<usize> {
        self.renderer.as_ref().and_then(|renderer| {
            let ctx = renderer.layout_ctx?;
            let layout =
                crate::layout::layout_context_menu(&ctx, menu_x, menu_y, renderer.scale() as f32);
            let scene =
                crate::context_menu_component::build_context_menu_scene(layout, CONTEXT_MENU_ITEMS);
            crate::context_menu_component::context_menu_item_at(&scene, x, y)
        })
    }

    /// Run a context menu action on the target block.
    /// `block_id` is `None` for the in-flight (running) command.
    pub(crate) fn run_context_action(&mut self, block_id: Option<BlockId>, action: &str) {
        let mut clipboard_text = None;
        // v1.7.3-C: Block data cloned inside the terminal borrow for
        // `export_block` — consumed after the borrow drops so the annotation
        // store can be accessed without a borrow conflict.
        let mut export_block_data: Option<weft_core::blocks::Block> = None;
        {
            let Some(mut terminal) = self
                .sessions
                .active_mut()
                .and_then(|tab| tab.lock_terminal())
            else {
                // Annotation-only actions (toggle_bookmark / add_note) don't
                // need the terminal — fall through to the post-borrow block.
                if matches!(action, "toggle_bookmark" | "add_note") {
                    self.run_annotation_action(block_id, action);
                }
                return;
            };

            match action {
                "copy_command" | "copy_output" | "copy_block" if block_id.is_none() => {
                    if let Some(live) = terminal.block_tracker().in_flight() {
                        let text = if action == "copy_command" {
                            live.command.to_string()
                        } else if action == "copy_output" {
                            live.output.to_string()
                        } else {
                            // v1.10.34: combined copy — cwd + command + output.
                            weft_core::blocks::format_block_for_copy(
                                live.cwd,
                                live.command,
                                live.output,
                            )
                        };
                        if !text.is_empty() {
                            clipboard_text = Some(text);
                        }
                    }
                }
                "copy_command" | "copy_output" | "copy_block" => {
                    let bid = block_id.expect("copy branch has a finalized block id");
                    let block = terminal
                        .block_tracker()
                        .session_blocks()
                        .iter()
                        .find(|b| b.id == bid);
                    if let Some(block) = block {
                        let text = if action == "copy_command" {
                            block.command.clone()
                        } else if action == "copy_output" {
                            block.output.to_string()
                        } else {
                            weft_core::blocks::format_block_for_copy(
                                block.cwd.as_deref(),
                                &block.command,
                                &block.output,
                            )
                        };
                        if !text.is_empty() {
                            clipboard_text = Some(text);
                        }
                    }
                }
                "toggle_fold" => {
                    if let Some(bid) = block_id {
                        terminal.block_tracker_mut().toggle_collapse(bid);
                    }
                    // In-flight blocks can't be folded (no finalized block yet).
                }
                // W4: copy the block's command into the editor buffer so the user
                // can tweak parameters and re-submit (Warp-style "rerun").
                "send_to_input" => {
                    if terminal.effective_input_mode() == weft_core::input::InputMode::Editor {
                        let cmd = if let Some(bid) = block_id {
                            terminal
                                .block_tracker()
                                .session_blocks()
                                .iter()
                                .find(|b| b.id == bid)
                                .map(|b| b.command.clone())
                        } else {
                            terminal
                                .block_tracker()
                                .in_flight()
                                .map(|f| f.command.to_string())
                        };
                        if let Some(cmd) = cmd.filter(|cmd| !cmd.is_empty()) {
                            terminal.editor_mut().buffer.set_text(&cmd);
                        }
                    }
                }
                // v1.7.3-C: Clone block data for export. The actual markdown
                // generation + file write happens after the terminal borrow
                // drops (needs annotation store access).
                "export_block" => {
                    if let Some(bid) = block_id {
                        if let Some(block) = terminal
                            .block_tracker()
                            .session_blocks()
                            .iter()
                            .find(|b| b.id == bid)
                        {
                            export_block_data = Some(block.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        // Terminal borrow dropped — annotation/export actions run here.
        // M5-b P2-2: toggle_fold needs no cache invalidation — the collapsed
        // mismatch routes ensure_cached into the WidthOnly rebuild (L1
        // reused, L2 rebuilt); invalidate() would force a full L1
        // re-enumeration of the block's output. Redraw comes from
        // execute_context_menu's unconditional tail.
        if matches!(action, "toggle_bookmark" | "add_note" | "export_block") {
            self.run_annotation_action_with_export(block_id, action, export_block_data);
        }
        // v1.8.2: AI diagnose — trigger after terminal borrow drops.
        if action == "diagnose" {
            if let Some(block_id) = block_id {
                self.spawn_block_diagnose(block_id);
            }
        }
        if let Some(text) = clipboard_text.as_ref() {
            info!(len = text.len(), "context action copied to clipboard");
        }
        self.drain_effects(effect::context_clipboard_effects(clipboard_text));
    }
}
