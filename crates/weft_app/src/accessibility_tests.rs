use super::{role_is_pressable, structure_changed, view_space_frame, AccessibilityNode};
use crate::accessibility_model::{
    append_keyed_semantics, role_exposes_text_value, tree_is_valid, AccessibilityAction,
};
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
    assert!(node.action.is_some());
}

#[test]
fn text_field_state_is_available_as_current_value() {
    let semantic = SemanticNode {
        role: SemanticRole::TextField,
        label: "Find".into(),
        bounds: [0.0, 0.0, 100.0, 20.0],
        focus: Some(FocusId::FindQuery),
        state: "needle".into(),
    };
    let node = AccessibilityNode::from_semantic("find/query", None, &semantic, false);
    assert_eq!(node.state, "needle");
    assert_eq!(node.role, SemanticRole::TextField);
    assert!(role_exposes_text_value(&node.role));
    assert!(role_exposes_text_value(&SemanticRole::TextArea));
    assert!(!role_exposes_text_value(&SemanticRole::Button));
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
    assert_eq!(
        bridge.resolve_press(7, node_id),
        Some(AccessibilityAction::PressPoint { x: 20.0, y: 30.0 })
    );
    assert_eq!(bridge.resolve_press(6, node_id), None);

    let mut non_action = bridge;
    non_action.previous_nodes[0].action = None;
    assert_eq!(non_action.resolve_press(7, node_id), None);
}

#[test]
fn typed_actions_resolve_exact_business_target_and_reject_stale_generation() {
    let semantic = SemanticNode {
        role: SemanticRole::Tab,
        label: "Build".into(),
        bounds: [10.0, 20.0, 30.0, 40.0],
        focus: Some(FocusId::Tab(0)),
        state: "selected".into(),
    };
    let mut tab = AccessibilityNode::from_semantic("tabs/session/42", None, &semantic, true);
    tab.action = Some(AccessibilityAction::SwitchSession(42));
    let node_id = super::stable_id(&tab.id);
    let bridge = super::AccessibilityBridge {
        previous_nodes: vec![tab],
        generation: 9,
        ..Default::default()
    };
    assert_eq!(
        bridge.resolve_press(9, node_id),
        Some(AccessibilityAction::SwitchSession(42))
    );
    assert_eq!(bridge.resolve_press(8, node_id), None);
}

#[test]
fn geometry_updates_reuse_point_action_but_business_target_changes_rebuild() {
    let semantic = SemanticNode {
        role: SemanticRole::Button,
        label: "New tab".into(),
        bounds: [0.0, 0.0, 20.0, 20.0],
        focus: None,
        state: String::new(),
    };
    let first = AccessibilityNode::from_semantic("new-tab", None, &semantic, true);
    let mut moved = first.clone();
    moved.action = Some(AccessibilityAction::PressPoint { x: 80.0, y: 20.0 });
    assert!(!structure_changed(&[first], &[moved]));

    let mut old_tab = AccessibilityNode::from_semantic("tab", None, &semantic, true);
    old_tab.action = Some(AccessibilityAction::SwitchSession(41));
    let mut new_tab = old_tab.clone();
    new_tab.action = Some(AccessibilityAction::SwitchSession(42));
    assert!(structure_changed(&[old_tab], &[new_tab]));
}

#[test]
fn business_identity_survives_duplicate_label_filtering_and_reordering() {
    let row = |label: &str| SemanticNode {
        role: SemanticRole::ListItem,
        label: label.into(),
        bounds: [0.0, 0.0, 100.0, 20.0],
        focus: None,
        state: String::new(),
    };
    let mut initial = Vec::new();
    let initial_records = [(41, "build"), (42, "build"), (43, "deploy")];
    append_keyed_semantics(
        &mut initial,
        "history",
        &initial_records.map(|(_, label)| row(label)),
        false,
        true,
        |index, _node| format!("history/{}", initial_records[index].0),
    );
    let mut filtered = Vec::new();
    let filtered_records = [(43, "deploy"), (42, "build")];
    append_keyed_semantics(
        &mut filtered,
        "history",
        &filtered_records.map(|(_, label)| row(label)),
        false,
        true,
        |index, _node| format!("history/{}", filtered_records[index].0),
    );

    let id_for = |nodes: &[AccessibilityNode], id: &str| {
        nodes
            .iter()
            .find(|node| node.id == id)
            .map(|node| node.id.clone())
            .unwrap()
    };
    assert_eq!(
        id_for(&initial, "history/42"),
        id_for(&filtered, "history/42")
    );
    assert_eq!(
        id_for(&initial, "history/43"),
        id_for(&filtered, "history/43")
    );
    assert!(!filtered.iter().any(|node| node.id == "history/41"));
}

#[test]
fn accessibility_tree_rejects_duplicate_or_orphaned_nodes() {
    let semantic = SemanticNode {
        role: SemanticRole::Button,
        label: "New tab".into(),
        bounds: [0.0, 0.0, 20.0, 20.0],
        focus: None,
        state: String::new(),
    };
    let root = AccessibilityNode::from_semantic("tabs", None, &semantic, false);
    let child = AccessibilityNode::from_semantic("new-tab", Some("tabs"), &semantic, true);
    assert!(tree_is_valid(&[root.clone(), child.clone()]));
    assert!(!tree_is_valid(&[root.clone(), root]));

    let orphan = AccessibilityNode {
        parent: Some("missing".into()),
        ..child
    };
    assert!(!tree_is_valid(&[orphan]));
}

#[test]
fn accessibility_tree_rejects_parent_cycles() {
    let semantic = SemanticNode {
        role: SemanticRole::List,
        label: "Cycle".into(),
        bounds: [0.0, 0.0, 20.0, 20.0],
        focus: None,
        state: String::new(),
    };
    let a = AccessibilityNode::from_semantic("a", Some("b"), &semantic, false);
    let b = AccessibilityNode::from_semantic("b", Some("a"), &semantic, false);
    assert!(!tree_is_valid(&[a, b]));

    let a = AccessibilityNode::from_semantic("a", Some("b"), &semantic, false);
    let b = AccessibilityNode::from_semantic("b", Some("c"), &semantic, false);
    let c = AccessibilityNode::from_semantic("c", Some("a"), &semantic, false);
    assert!(!tree_is_valid(&[a, b, c]));
}
