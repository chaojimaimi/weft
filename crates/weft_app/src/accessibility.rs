//! Native AppKit accessibility bridge for Metal-rendered UI.
//!
//! Weft has no `NSControl` hierarchy: Scene nodes are painted directly into a
//! Metal layer. This module mirrors the current Scene semantics as
//! `NSAccessibilityElement` children of winit's `NSView`. Press actions are
//! sent back through the winit event loop and reuse the normal mouse router.

mod bridge;
pub(crate) use bridge::{install_event_proxy, AccessibilityBridge};

#[cfg(test)]
use bridge::view_space_frame;

use std::hash::{Hash, Hasher};

#[cfg(test)]
use crate::accessibility_model::structure_changed;
use crate::accessibility_model::{
    append_keyed_semantics, role_is_pressable, selected_state, stable_id, tree_is_valid,
    AccessibilityAction, AccessibilityNode, BlockTextKey,
};
use crate::scene::{SemanticNode, SemanticRole};
use crate::{App, CONTEXT_MENU_ITEMS};
use weft_core::blocks::BlockId;

fn push_semantic(
    output: &mut Vec<AccessibilityNode>,
    id: &str,
    parent: Option<&str>,
    semantic: &SemanticNode,
    pressable: bool,
) {
    output.push(AccessibilityNode::from_semantic(
        id, parent, semantic, pressable,
    ));
}

impl App {
    pub(super) fn update_accessibility_tree(&mut self) {
        let nodes = self.accessibility_snapshot();
        if !tree_is_valid(&nodes) {
            tracing::error!(node_count = nodes.len(), "invalid accessibility tree");
            return;
        }
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some(window) = self.window.as_ref() else {
            return;
        };
        self.accessibility
            .update(window, nodes, renderer.scale(), renderer.viewport().1);
    }

    fn accessibility_snapshot(&mut self) -> Vec<AccessibilityNode> {
        let Some(renderer) = self.renderer.as_ref() else {
            return Vec::new();
        };
        let Some(ctx) = renderer.layout_ctx else {
            return Vec::new();
        };

        let mut semantics = Vec::new();
        if self.settings.open {
            if let Some(scene) = self.accessibility_settings_scene() {
                append_keyed_semantics(
                    &mut semantics,
                    "settings/dialog",
                    &scene.semantics,
                    true,
                    true,
                    |_index, node| format!("settings/item/{:016x}", stable_id(&node.label)),
                );
            }
            return semantics;
        }
        if self.palette.open {
            if let Some(mut scene) = self.palette_scene() {
                if let Some(query) = scene.semantics.get_mut(1) {
                    if query.role == SemanticRole::TextField {
                        query.state = self.palette.accessibility_query().to_owned();
                    }
                }
                let selected_bounds = scene.hits.iter().find_map(|hit| {
                    (hit.target
                        == crate::palette_component::PaletteTarget::Item(self.palette.selection))
                    .then(|| hit.bounds())
                });
                if let Some(node) = selected_bounds.and_then(|bounds| {
                    scene
                        .semantics
                        .iter_mut()
                        .find(|node| node.bounds == bounds)
                }) {
                    node.state = "selected".into();
                }
                if let Some(dialog) = scene.semantics.first() {
                    push_semantic(&mut semantics, "palette/dialog", None, dialog, false);
                }
                if let Some(query) = scene.semantics.get(1) {
                    push_semantic(
                        &mut semantics,
                        "palette/query",
                        Some("palette/dialog"),
                        query,
                        false,
                    );
                }
                if scene.semantics.len() > 2 {
                    let list = SemanticNode {
                        role: SemanticRole::List,
                        label: "Palette results".into(),
                        bounds: scene.semantics[0].bounds,
                        focus: None,
                        state: String::new(),
                    };
                    push_semantic(
                        &mut semantics,
                        "palette/list",
                        Some("palette/dialog"),
                        &list,
                        false,
                    );
                    for (index, item) in scene.semantics.iter().skip(2).enumerate() {
                        let target = scene
                            .hits
                            .iter()
                            .find_map(|hit| (hit.bounds() == item.bounds).then_some(hit.target));
                        let identity = match (&self.palette.submode, target) {
                            (
                                crate::palette_state::PaletteSubMode::SelectTheme { .. },
                                Some(crate::palette_component::PaletteTarget::Item(_)),
                            ) => Some(format!("theme/{:016x}", stable_id(&item.label))),
                            (
                                crate::palette_state::PaletteSubMode::Search,
                                Some(crate::palette_component::PaletteTarget::Item(target)),
                            ) => self
                                .palette
                                .results
                                .get(target)
                                .map(crate::palette_state::PaletteEntry::accessibility_key),
                            _ => None,
                        };
                        let (identity, pressable) = identity.map_or_else(
                            || {
                                (
                                    format!("readonly/{index}/{:016x}", stable_id(&item.label)),
                                    false,
                                )
                            },
                            |identity| (identity, true),
                        );
                        push_semantic(
                            &mut semantics,
                            &format!("palette/item/{identity}"),
                            Some("palette/list"),
                            item,
                            pressable,
                        );
                        if pressable {
                            if let Some(node) = semantics.last_mut() {
                                node.action = match &self.palette.submode {
                                    crate::palette_state::PaletteSubMode::Search => {
                                        Some(AccessibilityAction::PaletteEntry(identity))
                                    }
                                    crate::palette_state::PaletteSubMode::SelectTheme {
                                        ..
                                    } => {
                                        Some(AccessibilityAction::PaletteTheme(item.label.clone()))
                                    }
                                    _ => node.action.clone(),
                                };
                            }
                        }
                    }
                }
            }
            return semantics;
        }
        if let Some(menu) = self.interaction.context_menu.as_ref() {
            let layout =
                crate::layout::layout_context_menu(&ctx, menu.x, menu.y, renderer.scale() as f32);
            let mut scene =
                crate::context_menu_component::build_context_menu_scene(layout, CONTEXT_MENU_ITEMS);
            if let Some(item) = scene.semantics.get_mut(menu.selection + 1) {
                item.state = "selected".into();
            }
            append_keyed_semantics(
                &mut semantics,
                "context-menu/menu",
                &scene.semantics,
                true,
                true,
                |index, _node| format!("context-menu/action/{}", index - 1),
            );
            for (index, node) in semantics.iter_mut().skip(1).enumerate() {
                node.action = Some(AccessibilityAction::ContextMenuItem {
                    session_id: menu.session_id,
                    index,
                });
            }
            return semantics;
        }

        let tab_state = self.tab_bar_state();
        let strip = crate::layout::layout_tab_strip(crate::layout::TabStripInput {
            viewport_width: self.tab_bar_layout_right(),
            bar_height: renderer.tab_bar_height(),
            cell_width: renderer.cell_width() as f32,
            padding_x: renderer.padding_x(),
            chrome_left: ctx.chrome_left,
            traffic_lights_width: renderer.traffic_lights_width(),
            tab_count: self.sessions.len(),
            requested_scroll_offset: self.tab_bar.scroll_offset,
        });
        let mut tab_scene = crate::tab_bar_component::build_tab_bar_scene(
            strip,
            self.sessions.len(),
            renderer.cell_width() as f32,
            renderer.cell_height() as f32,
        );
        let mut tab_index = 0;
        for node in &mut tab_scene.semantics {
            if node.role == SemanticRole::Tab {
                node.label = tab_state
                    .labels
                    .get(tab_index)
                    .cloned()
                    .unwrap_or_else(|| format!("Tab {}", tab_index + 1));
                if tab_index == tab_state.active_tab {
                    node.state = "selected".into();
                }
                tab_index += 1;
            }
        }
        let tab_container = SemanticNode {
            role: SemanticRole::TabList,
            label: "Tabs".into(),
            bounds: [0.0, 0.0, renderer.viewport().0, renderer.tab_bar_height()],
            focus: None,
            state: String::new(),
        };
        push_semantic(
            &mut semantics,
            "tabs/container",
            None,
            &tab_container,
            false,
        );
        let mut session_index = 0;
        for node in &tab_scene.semantics {
            let id = if node.role == SemanticRole::Tab {
                let session_id = self
                    .sessions
                    .tab(session_index)
                    .map_or(session_index as u64, |tab| tab.session_id);
                session_index += 1;
                format!("tabs/session/{session_id}")
            } else {
                format!("tabs/action/{:016x}", stable_id(&node.label))
            };
            push_semantic(
                &mut semantics,
                &id,
                Some("tabs/container"),
                node,
                role_is_pressable(&node.role),
            );
            if let Some(accessibility_node) = semantics.last_mut() {
                accessibility_node.action = if node.role == SemanticRole::Tab {
                    self.sessions
                        .tab(session_index.saturating_sub(1))
                        .map(|tab| AccessibilityAction::SwitchSession(tab.session_id))
                } else if node.label == "New tab" {
                    Some(AccessibilityAction::NewTab)
                } else {
                    accessibility_node.action.clone()
                };
            }
        }

        if let Some(layout) = self.terminal_layout() {
            let mut block_cache_update = None;
            // R2-3: sticky header label + bounds for accessibility, filled
            // inside the block-view branch where `rows` are already computed.
            // Batch 5 Step 3: also carry the block_id so we can emit Button
            // semantics for the copy/fold actions on the sticky header.
            let mut sticky_header: Option<(String, [f32; 4], BlockId)> = None;
            let text = self.sessions.active().map_or_else(String::new, |tab| {
                // T10 P1 (D9 rule 2): the guard scope below must not
                // re-enter locking helpers — the block branch uses the
                // guard-carrying compute_block_view_rows_for instead of
                // the locking wrapper.
                let Some(terminal) = tab.lock_terminal() else {
                    return String::new();
                };
                if terminal.show_block_view() {
                    let blocks = terminal.block_tracker().session_blocks();
                    let mut fold_hasher = std::collections::hash_map::DefaultHasher::new();
                    for block in blocks {
                        block.id.hash(&mut fold_hasher);
                        block.collapsed.hash(&mut fold_hasher);
                    }
                    let key = BlockTextKey {
                        session_id: tab.session_id,
                        block_count: blocks.len(),
                        last_output_len: blocks.last().map_or(0, |block| block.output.len()),
                        live_output_len: terminal
                            .block_tracker()
                            .in_flight()
                            .map_or(0, |live| live.output.len()),
                        scroll: tab.block_scroll(),
                        editor_hash: stable_id(&terminal.editor().buffer.text()),
                        cwd_hash: terminal.cwd().map_or(0, stable_id),
                        git_branch_hash: terminal.git_branch().map_or(0, stable_id),
                        fold_hash: fold_hasher.finish(),
                        width_bits: ((layout.content.right - layout.content.left) as f32).to_bits(),
                        height_bits: ((layout.content.bottom - layout.content.top) as f32)
                            .to_bits(),
                    };
                    if self.accessibility.block_text_key == Some(key) {
                        return self.accessibility.block_text.clone();
                    }
                    let Some((mut rows, _, _)) = self.compute_block_view_rows_for(tab, &terminal)
                    else {
                        return String::new();
                    };
                    rows.retain(|row| {
                        row.y_bottom > layout.content.top as f32
                            && row.y_top < layout.content.bottom as f32
                    });
                    rows.sort_by(|a, b| a.y_top.total_cmp(&b.y_top));

                    // R2-3: while rows are still available, check for a
                    // sticky header block and capture its label + bounds
                    // for the accessibility node pushed below.
                    let clip_top = layout.content.top as f32;
                    let clip_bottom = layout.content.bottom as f32;
                    if let Some(sid) =
                        crate::paint::block_view::sticky_block_id(&rows, clip_top, clip_bottom)
                    {
                        if let Some(block) = blocks.iter().find(|b| b.id == sid) {
                            let cell_h = layout.cell_height as f32;
                            // At most 2 rows (CWD + command); use the
                            // full band so screen readers cover both.
                            let sticky_bottom = clip_top + cell_h * 2.0;
                            sticky_header = Some((
                                format!("Header: {}", block.command),
                                [
                                    layout.content.left as f32,
                                    clip_top,
                                    layout.content.right as f32,
                                    sticky_bottom,
                                ],
                                sid,
                            ));
                        }
                    }

                    let text = rows
                        .into_iter()
                        .filter(|row| row.is_selectable() && !row.text.is_empty())
                        .map(|row| row.text)
                        .collect::<Vec<_>>()
                        .join("\n");
                    block_cache_update = Some((key, text.clone()));
                    return text;
                }
                (0..terminal.grid().num_rows)
                    .map(|row| terminal.grid().displayed_row_text(row))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            let terminal_semantic = SemanticNode {
                role: SemanticRole::TextArea,
                label: "Terminal".into(),
                bounds: [
                    layout.content.left as f32,
                    layout.content.top as f32,
                    layout.content.right as f32,
                    layout.content.bottom as f32,
                ],
                focus: None,
                state: text,
            };
            semantics.push(AccessibilityNode::from_semantic(
                "terminal",
                None,
                &terminal_semantic,
                false,
            ));
            if let Some((key, text)) = block_cache_update {
                self.accessibility.block_text_key = Some(key);
                self.accessibility.block_text = text;
            }

            // R2-3 Phase 1: expose the sticky header as a ListItem so screen
            // readers announce the pinned command. Previously the sticky band
            // was visible but accessibility-invisible (a "dead paint band").
            // Batch 5 Step 3: also emit Button semantics for the copy/fold
            // actions so VoiceOver users can activate them independently.
            if let Some((label, bounds, block_id)) = sticky_header {
                let node = SemanticNode {
                    role: SemanticRole::ListItem,
                    label,
                    bounds,
                    focus: None,
                    state: String::new(),
                };
                push_semantic(&mut semantics, "sticky-header", None, &node, false);

                // Compute copy/fold button bounds using the same geometry as
                // the paint path (block_header_action_rects). The PressPoint
                // action falls through to handle_mouse_press which hits the
                // existing BlockActionCopy/BlockActionFold HitRegions.
                let (copy_rect, fold_rect) = crate::paint::block_view::block_header_action_rects(
                    crate::layout::block_content_x_bounds(&ctx).1,
                    layout.content.top as f32,
                    layout.cell_height as f32,
                    layout.cell_width as f32,
                    renderer.scale(),
                );
                let copy_node = SemanticNode {
                    role: SemanticRole::Button,
                    label: "Copy command".to_string(),
                    bounds: copy_rect,
                    focus: None,
                    state: String::new(),
                };
                push_semantic(
                    &mut semantics,
                    &format!("sticky-header/copy/{:016x}", block_id.0),
                    Some("sticky-header"),
                    &copy_node,
                    true,
                );
                let fold_label = self
                    .sessions
                    .active()
                    .and_then(|tab| {
                        // T10 P1: the fold verdict is computed inside the
                        // guard scope (the matched block borrows the terminal)
                        // and only the owned label escapes.
                        tab.with_terminal(|terminal| {
                            terminal
                                .block_tracker()
                                .session_blocks()
                                .iter()
                                .find(|b| b.id == block_id)
                                .map(|block| {
                                    if block.collapsed {
                                        "Expand block"
                                    } else {
                                        "Collapse block"
                                    }
                                })
                        })
                        .flatten()
                    })
                    .unwrap_or("Toggle fold")
                    .to_string();
                let fold_node = SemanticNode {
                    role: SemanticRole::Button,
                    label: fold_label,
                    bounds: fold_rect,
                    focus: None,
                    state: String::new(),
                };
                push_semantic(
                    &mut semantics,
                    &format!("sticky-header/fold/{:016x}", block_id.0),
                    Some("sticky-header"),
                    &fold_node,
                    true,
                );
            }
        }

        if self.panel.open {
            let layout = crate::layout::layout_panel(
                ctx.chrome_top,
                renderer.cell_width() as f32,
                renderer.cell_height() as f32,
                renderer.sidebar_width(),
                renderer.viewport().1,
            );
            let max_rows = crate::paint::ui_helpers::visible_panel_rows(
                renderer.viewport().1,
                renderer.cell_height(),
            );
            // T10 P1: panel_display lends &Block from the terminal, so the
            // rows are down-owned to (id, command) inside the guard scope.
            let display = self
                .sessions
                .active()
                .and_then(|tab| {
                    tab.with_terminal(|terminal| {
                        crate::paint::ui_helpers::panel_display(
                            terminal.block_tracker().blocks(),
                            &self.panel.query,
                            self.panel.scroll_offset,
                            max_rows,
                        )
                        .into_iter()
                        .map(|block| (block.id, block.command.clone()))
                        .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default();
            // v1.11.2 X4: footer button only when the tab actually has blocks.
            let has_blocks = self
                .sessions
                .active()
                .and_then(|tab| tab.with_terminal(|t| !t.block_tracker().blocks().is_empty()))
                .unwrap_or(false);
            let mut scene = crate::panel_component::build_panel_scene(
                layout.panel_rect,
                layout.search_field_rect,
                layout.list_top,
                layout.row_height,
                display.len(),
                has_blocks.then_some(layout.footer_rect),
            );
            if let Some(search) = scene.semantics.first_mut() {
                search.state = self.panel.query.clone();
            }
            for (node, block) in scene.semantics.iter_mut().skip(1).zip(&display) {
                node.label = crate::paint::ui_helpers::strip_prompt_prefix(&block.1);
            }
            if let Some(selected) = scene.semantics.get_mut(self.panel.selection + 1) {
                selected.state = "selected".into();
            }
            if let Some(search) = scene.semantics.first() {
                push_semantic(&mut semantics, "panel/search", None, search, false);
            }
            if scene.semantics.len() > 1 {
                let list = SemanticNode {
                    role: SemanticRole::List,
                    label: "Command history".into(),
                    bounds: layout.panel_rect,
                    focus: None,
                    state: String::new(),
                };
                push_semantic(&mut semantics, "panel/list", None, &list, false);
                for (item, block) in scene.semantics.iter().skip(1).zip(&display) {
                    push_semantic(
                        &mut semantics,
                        &format!("panel/item/{}", block.0 .0),
                        Some("panel/list"),
                        item,
                        true,
                    );
                }
            }
            // v1.11.2 X4: expose the footer button (last node when present).
            if scene
                .semantics
                .last()
                .is_some_and(|n| n.role == SemanticRole::Button)
            {
                if let Some(footer) = scene.semantics.last() {
                    push_semantic(&mut semantics, "panel/load-older", None, footer, false);
                }
            }
        }
        if self.find.open {
            let total = if self.block_view_active() {
                self.find.block_matches.len()
            } else {
                self.find.matches.len()
            };
            let mut scene =
                crate::find_component::build_find_scene(crate::layout::layout_find(&ctx, total));
            if let Some(query) = scene.semantics.first_mut() {
                query.state = self.find.query.clone();
            }
            let dialog = SemanticNode {
                role: SemanticRole::Dialog,
                label: "Find".into(),
                bounds: scene.semantics[0].bounds,
                focus: Some(crate::scene::FocusId::FindQuery),
                state: String::new(),
            };
            push_semantic(&mut semantics, "find/dialog", None, &dialog, false);
            for (index, item) in scene.semantics.iter().enumerate() {
                push_semantic(
                    &mut semantics,
                    &format!("find/node/{index}"),
                    Some("find/dialog"),
                    item,
                    role_is_pressable(&item.role),
                );
            }
        }
        if let Some(mut scene) = self.completion_scene() {
            let selected_index = self
                .sessions
                .active()
                .and_then(|tab| {
                    tab.with_terminal(|terminal| {
                        terminal
                            .editor()
                            .completion_view()
                            .map(|(_, selected)| selected)
                    })
                })
                .flatten();
            if let Some(selected) = selected_index {
                let selected_bounds = scene.hits.iter().find_map(|hit| {
                    (hit.target == crate::completion_component::CompletionTarget::Item(selected))
                        .then(|| hit.bounds())
                });
                if let Some(node) = selected_bounds.and_then(|bounds| {
                    scene
                        .semantics
                        .iter_mut()
                        .find(|node| node.bounds == bounds)
                }) {
                    node.state = "selected".into();
                }
            }
            // Completion mouse acceptance is not implemented; expose readable
            // candidates without AXPress rather than falling through to PTY.
            append_keyed_semantics(
                &mut semantics,
                "completion/container",
                &scene.semantics,
                true,
                false,
                |_index, node| {
                    let target = scene
                        .hits
                        .iter()
                        .find_map(|hit| (hit.bounds() == node.bounds).then_some(hit.target));
                    let item = match target {
                        Some(crate::completion_component::CompletionTarget::Item(item)) => item,
                        _ => usize::MAX,
                    };
                    format!("completion/item/{item}/{:016x}", stable_id(&node.label))
                },
            );
        }
        semantics
    }

    fn accessibility_settings_scene(
        &self,
    ) -> Option<crate::scene::Scene<crate::settings_component::SettingsTarget>> {
        let renderer = self.renderer.as_ref()?;
        let cw = renderer.cell_width() as f32;
        let ch = renderer.cell_height() as f32;
        let (vp_w, vp_h) = renderer.viewport();
        let footer_pair_widths = crate::settings_component::settings_footer_widths(cw);
        let layout = crate::layout::layout_settings(
            vp_w,
            vp_h,
            cw,
            ch,
            crate::overlay::SettingsTab::ALL.len(),
            self.settings.error.is_some(),
            &footer_pair_widths,
            self.settings_is_narrow(),
            self.settings.drill_down,
        )?;
        let total_rows = self.settings_tab_row_count();
        let row_window = crate::settings_component::settings_scene_row_window(
            self.settings.tab,
            total_rows,
            self.settings_theme_views().len(),
            layout.max_rows,
            self.settings.scroll_offset,
        );
        let theme_count = row_window.0;
        let mut scene = crate::settings_component::build_settings_scene(
            &layout,
            crate::overlay::SettingsTab::ALL.as_slice(),
            self.settings.tab,
            row_window,
            ch,
            // v1.5.1: profile_count = number of profiles + 1 (for "Base").
            self.profile_names_sorted().len() + 1,
        );
        if theme_count > 0 {
            let themes = self.settings_theme_views();
            for (node, theme) in scene
                .semantics
                .iter_mut()
                .filter(|node| {
                    node.role == SemanticRole::ListItem && node.label.starts_with("Theme ")
                })
                .zip(themes.iter())
            {
                node.label = theme.label.clone();
                node.state = selected_state(theme.name == self.settings.draft.theme.name);
            }
        }
        Some(scene)
    }
}

#[cfg(test)]
#[path = "accessibility_tests.rs"]
mod tests;
