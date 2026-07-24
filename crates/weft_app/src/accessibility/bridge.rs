//! Native NSAccessibility element class and tree-sync bridge.

use std::cell::Cell;
use std::collections::HashMap;
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
    role_exposes_text_value, stable_id, structure_changed, AccessibilityAction, AccessibilityNode,
    BlockTextKey,
};
use crate::scene::SemanticRole;
use crate::AppEvent;

static EVENT_PROXY: OnceLock<EventLoopProxy<AppEvent>> = OnceLock::new();

pub(crate) fn install_event_proxy(proxy: EventLoopProxy<AppEvent>) {
    let _ = EVENT_PROXY.set(proxy);
}

pub(super) fn view_space_frame(bounds: [f32; 4], scale: f64) -> NSRect {
    let scale = scale.max(f64::EPSILON);
    let x = f64::from(bounds[0]) / scale;
    let y = f64::from(bounds[1]) / scale;
    let width = f64::from((bounds[2] - bounds[0]).max(0.0)) / scale;
    let height = f64::from((bounds[3] - bounds[1]).max(0.0)) / scale;
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

#[derive(Debug)]
pub(super) struct ElementIvars {
    generation: Cell<u64>,
    node_id: u64,
    pressable: Cell<bool>,
}

declare_class!(
    #[derive(Debug)]
    pub(super) struct WeftAccessibilityElement;

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
    pub(super) previous_nodes: Vec<AccessibilityNode>,
    pub(super) elements: HashMap<u64, Retained<WeftAccessibilityElement>>,
    pub(super) generation: u64,
    pub(super) previous_scale: f64,
    pub(super) previous_height: f32,
    pub(super) block_text_key: Option<BlockTextKey>,
    pub(super) block_text: String,
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
