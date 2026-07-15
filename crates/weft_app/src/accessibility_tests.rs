use super::{role_is_pressable, structure_changed, view_space_frame, AccessibilityNode};
use crate::scene::{FocusId, SemanticNode, SemanticRole};

#[test]
fn converts_physical_bounds_to_flipped_winit_view_points() {
    let frame = view_space_frame([20.0, 40.0, 220.0, 140.0], 2.0);
    assert_eq!(frame.origin.x, 10.0);
    assert_eq!(frame.origin.y, 20.0);
    assert_eq!(frame.size.width, 100.0);
    assert_eq!(frame.size.height, 50.0);
}

#[test]
fn only_action_roles_are_pressable() {
    assert!(role_is_pressable(&SemanticRole::Button));
    assert!(role_is_pressable(&SemanticRole::Tab));
    assert!(role_is_pressable(&SemanticRole::ListItem));
    assert!(!role_is_pressable(&SemanticRole::TextField));
    assert!(!role_is_pressable(&SemanticRole::Dialog));
}

#[test]
fn bridge_snapshot_preserves_scene_label_bounds_and_state() {
    let semantic = SemanticNode {
        role: SemanticRole::Tab,
        label: "Build".into(),
        bounds: [1.0, 2.0, 30.0, 40.0],
        focus: Some(FocusId::Tab(1)),
        state: "selected, running".into(),
    };
    let node = AccessibilityNode::from_semantic("tabs/1", Some("tabs"), &semantic, true);
    assert_eq!(node.label, "Build");
    assert_eq!(node.bounds, semantic.bounds);
    assert_eq!(node.state, "selected, running");
    assert!(node.pressable);
}

#[test]
fn value_changes_reuse_element_identity_but_structure_changes_do_not() {
    let semantic = SemanticNode {
        role: SemanticRole::TextArea,
        label: "Terminal".into(),
        bounds: [0.0, 0.0, 100.0, 100.0],
        focus: None,
        state: "one".into(),
    };
    let first = AccessibilityNode::from_semantic("terminal", None, &semantic, false);
    let mut changed_value = first.clone();
    changed_value.state = "two".into();
    assert!(!structure_changed(
        std::slice::from_ref(&first),
        &[changed_value]
    ));
    let mut changed_parent = first.clone();
    changed_parent.parent = Some("dialog".into());
    assert!(structure_changed(&[first], &[changed_parent]));
}

#[test]
fn stale_or_non_action_press_is_rejected_before_mouse_routing() {
    let semantic = SemanticNode {
        role: SemanticRole::Button,
        label: "New tab".into(),
        bounds: [10.0, 20.0, 30.0, 40.0],
        focus: None,
        state: String::new(),
    };
    let action = AccessibilityNode::from_semantic("new-tab", None, &semantic, true);
    let node_id = super::stable_id(&action.id);
    let bridge = super::AccessibilityBridge {
        previous_nodes: vec![action],
        generation: 7,
        ..Default::default()
    };
    assert_eq!(bridge.resolve_press(7, node_id), Some((20.0, 30.0)));
    assert_eq!(bridge.resolve_press(6, node_id), None);

    let mut non_action = bridge;
    non_action.previous_nodes[0].pressable = false;
    assert_eq!(non_action.resolve_press(7, node_id), None);
}
