//! Native AppKit accessibility bridge for Metal-rendered UI.
//!
//! Weft has no `NSControl` hierarchy: Scene nodes are painted directly into a
//! Metal layer. This module mirrors the current Scene semantics as
//! `NSAccessibilityElement` children of winit's `NSView`. Press actions are
//! sent back through the winit event loop and reuse the normal mouse router.

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol};
use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityElement, NSAccessibilityFrameInView,
    NSAccessibilityGroupRole, NSAccessibilityListRole, NSAccessibilityMenuItemRole,
    NSAccessibilityMenuRole, NSAccessibilityRadioButtonRole, NSAccessibilityRowRole,
    NSAccessibilityTabGroupRole, NSAccessibilityTextAreaRole, NSAccessibilityTextFieldRole, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString};
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::accessibility_model::{
    append_keyed_semantics, role_exposes_text_value, role_is_pressable, selected_state, stable_id,
    structure_changed, tree_is_valid, AccessibilityAction, AccessibilityNode, BlockTextKey,
};
use crate::scene::{SemanticNode, SemanticRole};
use crate::{App, AppEvent, CONTEXT_MENU_ITEMS};

static EVENT_PROXY: OnceLock<EventLoopProxy<AppEvent>> = OnceLock::new();

pub(crate) fn install_event_proxy(proxy: EventLoopProxy<AppEvent>) {
    let _ = EVENT_PROXY.set(proxy);
}

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

fn view_space_frame(bounds: [f32; 4], scale: f64) -> NSRect {
    let scale = scale.max(f64::EPSILON);
    let x = f64::from(bounds[0]) / scale;
    let y = f64::from(bounds[1]) / scale;
    let width = f64::from((bounds[2] - bounds[0]).max(0.0)) / scale;
    let height = f64::from((bounds[3] - bounds[1]).max(0.0)) / scale;
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

#[derive(Debug)]
struct ElementIvars {
    generation: Cell<u64>,
    node_id: u64,
    pressable: Cell<bool>,
}

declare_class!(
    #[derive(Debug)]
    struct WeftAccessibilityElement;

    unsafe impl ClassType for WeftAccessibilityElement {
        type Super = NSAccessibilityElement;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "WeftAccessibilityElement";
    }

    impl DeclaredClass for WeftAccessibilityElement {
        type Ivars = ElementIvars;
    }

    unsafe impl NSObjectProtocol for WeftAccessibilityElement {}

    unsafe impl WeftAccessibilityElement {
        #[method(accessibilityPerformPress)]
        fn accessibility_perform_press(&self) -> Bool {
            if !self.ivars().pressable.get() {
                return false.into();
            }
            EVENT_PROXY
                .get()
                .is_some_and(|proxy| {
                    proxy
                        .send_event(AppEvent::AccessibilityPress {
                            generation: self.ivars().generation.get(),
                            node_id: self.ivars().node_id,
                        })
                        .is_ok()
                })
                .into()
        }
    }
);

impl WeftAccessibilityElement {
    fn new(node: &AccessibilityNode, generation: u64) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ElementIvars {
            generation: Cell::new(generation),
            node_id: stable_id(&node.id),
            pressable: Cell::new(node.action.is_some()),
        });
        unsafe { msg_send_id![super(this), init] }
    }
}

#[derive(Default)]
pub(crate) struct AccessibilityBridge {
    previous_nodes: Vec<AccessibilityNode>,
    elements: HashMap<u64, Retained<WeftAccessibilityElement>>,
    generation: u64,
    previous_scale: f64,
    previous_height: f32,
    block_text_key: Option<BlockTextKey>,
    block_text: String,
}

impl AccessibilityBridge {
    pub(crate) fn resolve_press(
        &self,
        generation: u64,
        node_id: u64,
    ) -> Option<AccessibilityAction> {
        if generation != self.generation {
            return None;
        }
        let node = self
            .previous_nodes
            .iter()
            .find(|node| stable_id(&node.id) == node_id)?;
        node.action.clone()
    }

    pub(crate) fn update(
        &mut self,
        window: &Window,
        nodes: Vec<AccessibilityNode>,
        scale: f64,
        viewport_height: f32,
    ) {
        if self.previous_nodes == nodes
            && self.previous_scale == scale
            && self.previous_height == viewport_height
        {
            return;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let raw = window.window_handle().ok().map(|handle| handle.as_raw());
            let Some(RawWindowHandle::AppKit(appkit)) = raw else {
                return false;
            };
            let Some(view): Option<Retained<NSView>> =
                Retained::retain(appkit.ns_view.as_ptr().cast())
            else {
                return false;
            };
            let view_object: &AnyObject = &*((&*view as *const NSView).cast::<AnyObject>());
            let structure_changed = structure_changed(&self.previous_nodes, &nodes);
            if structure_changed {
                self.generation = self.generation.wrapping_add(1).max(1);
                let mut previous = std::mem::take(&mut self.elements);
                for node in &nodes {
                    let node_id = stable_id(&node.id);
                    let element = previous
                        .remove(&node_id)
                        .unwrap_or_else(|| WeftAccessibilityElement::new(node, self.generation));
                    element.ivars().generation.set(self.generation);
                    element.ivars().pressable.set(node.action.is_some());
                    self.elements.insert(node_id, element);
                }
            }

            for node in &nodes {
                let element = self.elements.get(&stable_id(&node.id)).expect("AX element");
                element.setAccessibilityElement(true);
                element.setAccessibilityEnabled(true);
                element.setAccessibilityRole(Some(native_role(&node.role)));
                let label = NSString::from_str(&node.label);
                element.setAccessibilityLabel(Some(&label));
                element.setAccessibilityIdentifier(Some(&NSString::from_str(&node.id)));
                let parent = node
                    .parent
                    .as_ref()
                    .and_then(|id| self.elements.get(&stable_id(id)))
                    .map(|element| {
                        &*(element.as_ref() as *const WeftAccessibilityElement).cast::<AnyObject>()
                    })
                    .unwrap_or(view_object);
                element.setAccessibilityParent(Some(parent));
                // WinitView is flipped (top-left origin). Let AppKit convert
                // that exact view-space rect to the screen-space AXFrame;
                // hierarchy and geometry then remain independent.
                element.setAccessibilityFrame(NSAccessibilityFrameInView(
                    &view,
                    view_space_frame(node.bounds, scale),
                ));
                let state = NSString::from_str(&node.state);
                if role_exposes_text_value(&node.role) {
                    let value: &AnyObject = &*((&*state as *const NSString).cast::<AnyObject>());
                    element.setAccessibilityValue(Some(value));
                    element.setAccessibilityValueDescription(None);
                    element.setAccessibilitySelected(false);
                } else {
                    element.setAccessibilityValue(None);
                    element.setAccessibilityValueDescription(
                        (!node.state.is_empty()).then_some(&state),
                    );
                    element.setAccessibilitySelected(node.state.contains("selected"));
                }
            }

            if structure_changed {
                for node in &nodes {
                    let child_objects: Vec<Retained<AnyObject>> = nodes
                        .iter()
                        .filter(|child| child.parent.as_deref() == Some(&node.id))
                        .filter_map(|child| self.elements.get(&stable_id(&child.id)).cloned())
                        .map(|element| Retained::cast(element))
                        .collect();
                    let children = NSArray::from_vec(child_objects);
                    self.elements[&stable_id(&node.id)].setAccessibilityChildren(Some(&children));
                }
            }
            let root_children = NSArray::from_vec(
                nodes
                    .iter()
                    .filter(|node| node.parent.is_none())
                    .filter_map(|node| self.elements.get(&stable_id(&node.id)).cloned())
                    .map(|element| Retained::cast(element))
                    .collect(),
            );
            view.setAccessibilityElement(true);
            view.setAccessibilityRole(Some(NSAccessibilityGroupRole));
            view.setAccessibilityLabel(Some(&NSString::from_str("Weft terminal window")));
            view.setAccessibilityChildren(Some(&root_children));
            true
        }));

        if matches!(result, Ok(true)) {
            self.previous_nodes = nodes;
            self.previous_scale = scale;
            self.previous_height = viewport_height;
        } else if result.is_err() {
            tracing::warn!("failed to update AppKit accessibility tree");
        }
    }
}

fn native_role(role: &SemanticRole) -> &'static objc2_app_kit::NSAccessibilityRole {
    unsafe {
        match role {
            SemanticRole::Button => NSAccessibilityButtonRole,
            SemanticRole::TextField => NSAccessibilityTextFieldRole,
            SemanticRole::List => NSAccessibilityListRole,
            SemanticRole::ListItem => NSAccessibilityRowRole,
            SemanticRole::Dialog => NSAccessibilityGroupRole,
            SemanticRole::Menu => NSAccessibilityMenuRole,
            SemanticRole::MenuItem => NSAccessibilityMenuItemRole,
            SemanticRole::Tab => NSAccessibilityRadioButtonRole,
            SemanticRole::TabList => NSAccessibilityTabGroupRole,
            SemanticRole::TextArea => NSAccessibilityTextAreaRole,
        }
    }
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
            viewport_width: renderer.viewport().0,
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
            let text =
                self.sessions
                    .active()
                    .terminal
                    .as_ref()
                    .map_or_else(String::new, |terminal| {
                        if terminal.show_block_view() {
                            let blocks = terminal.block_tracker().session_blocks();
                            let mut fold_hasher = std::collections::hash_map::DefaultHasher::new();
                            for block in blocks {
                                block.id.hash(&mut fold_hasher);
                                block.collapsed.hash(&mut fold_hasher);
                            }
                            let key = BlockTextKey {
                                session_id: self.sessions.active().session_id,
                                block_count: blocks.len(),
                                last_output_len: blocks
                                    .last()
                                    .map_or(0, |block| block.output.len()),
                                live_output_len: terminal
                                    .block_tracker()
                                    .in_flight()
                                    .map_or(0, |live| live.output.len()),
                                scroll: self.sessions.active().block_scroll(),
                                editor_hash: stable_id(&terminal.editor().buffer.text()),
                                cwd_hash: terminal.cwd().map_or(0, stable_id),
                                git_branch_hash: terminal.git_branch().map_or(0, stable_id),
                                fold_hash: fold_hasher.finish(),
                                width_bits: ((layout.content.right - layout.content.left) as f32)
                                    .to_bits(),
                                height_bits: ((layout.content.bottom - layout.content.top) as f32)
                                    .to_bits(),
                            };
                            if self.accessibility.block_text_key == Some(key) {
                                return self.accessibility.block_text.clone();
                            }
                            let mut rows = self.compute_block_view_rows();
                            rows.retain(|row| {
                                row.y_bottom > layout.content.top as f32
                                    && row.y_top < layout.content.bottom as f32
                            });
                            rows.sort_by(|a, b| a.y_top.total_cmp(&b.y_top));
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
            let display = self
                .sessions
                .active()
                .terminal
                .as_ref()
                .map(|terminal| {
                    crate::paint::ui_helpers::panel_display(
                        terminal.block_tracker().blocks(),
                        &self.panel.query,
                        self.panel.scroll_offset,
                        max_rows,
                    )
                })
                .unwrap_or_default();
            let mut scene = crate::panel_component::build_panel_scene(
                layout.panel_rect,
                layout.search_field_rect,
                layout.list_top,
                layout.row_height,
                display.len(),
            );
            if let Some(search) = scene.semantics.first_mut() {
                search.state = self.panel.query.clone();
            }
            for (node, block) in scene.semantics.iter_mut().skip(1).zip(&display) {
                node.label = crate::paint::ui_helpers::strip_prompt_prefix(&block.command);
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
                        &format!("panel/item/{}", block.id.0),
                        Some("panel/list"),
                        item,
                        true,
                    );
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
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.editor().completion_view())
                .map(|(_, selected)| selected);
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
        let theme_count = if self.settings.tab == crate::overlay::SettingsTab::Appearance {
            self.settings_theme_views().len().min(layout.max_rows)
        } else {
            0
        };
        let mut scene = crate::settings_component::build_settings_scene(
            &layout,
            crate::overlay::SettingsTab::ALL.as_slice(),
            self.settings.tab,
            theme_count,
            ch,
        );
        if theme_count > 0 {
            let themes = self.settings_theme_views();
            for (node, theme) in scene
                .semantics
                .iter_mut()
                .filter(|node| node.role == SemanticRole::ListItem)
                .zip(themes.iter())
            {
                node.label = theme.label.into();
                node.state = selected_state(theme.name == self.settings.draft.theme.name);
            }
        }
        Some(scene)
    }
}

#[cfg(test)]
#[path = "accessibility_tests.rs"]
mod tests;
