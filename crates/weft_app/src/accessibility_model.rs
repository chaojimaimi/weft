//! Pure data model and identity rules for the native accessibility bridge.

use std::hash::{Hash, Hasher};

use crate::scene::{SemanticNode, SemanticRole};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibilityNode {
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    pub(crate) role: SemanticRole,
    pub(crate) label: String,
    pub(crate) bounds: [f32; 4],
    pub(crate) state: String,
    pub(crate) pressable: bool,
}

impl AccessibilityNode {
    pub(crate) fn from_semantic(
        id: impl Into<String>,
        parent: Option<&str>,
        node: &SemanticNode,
        pressable: bool,
    ) -> Self {
        Self {
            id: id.into(),
            parent: parent.map(str::to_owned),
            role: node.role.clone(),
            label: node.label.clone(),
            bounds: node.bounds,
            state: node.state.clone(),
            pressable,
        }
    }
}

pub(crate) fn stable_id(id: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn role_is_pressable(role: &SemanticRole) -> bool {
    matches!(
        role,
        SemanticRole::Button | SemanticRole::ListItem | SemanticRole::MenuItem | SemanticRole::Tab
    )
}

/// Append a scene using business identities supplied by its owner.
///
/// Neither array positions nor duplicate-label occurrence numbers are stable
/// under filtering. The caller must therefore derive each child ID from the
/// target that receives its action (session ID, block ID, fixed action, etc.).
pub(crate) fn append_keyed_semantics(
    output: &mut Vec<AccessibilityNode>,
    container_id: &str,
    semantics: &[SemanticNode],
    container: bool,
    actions: bool,
    mut child_id: impl FnMut(usize, &SemanticNode) -> String,
) {
    for (index, semantic) in semantics.iter().enumerate() {
        let id = if index == 0 && container {
            container_id.to_owned()
        } else {
            child_id(index, semantic)
        };
        let parent = (container && index > 0).then_some(container_id);
        output.push(AccessibilityNode::from_semantic(
            id,
            parent,
            semantic,
            actions && role_is_pressable(&semantic.role),
        ));
    }
}

pub(crate) fn structure_changed(
    previous: &[AccessibilityNode],
    current: &[AccessibilityNode],
) -> bool {
    previous.len() != current.len()
        || previous.iter().zip(current).any(|(a, b)| {
            a.id != b.id || a.parent != b.parent || a.role != b.role || a.pressable != b.pressable
        })
}

pub(crate) fn tree_is_valid(nodes: &[AccessibilityNode]) -> bool {
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    if ids.len() != nodes.len()
        || nodes.iter().any(|node| {
            node.parent
                .as_deref()
                .is_some_and(|parent| parent == node.id || !ids.contains(parent))
        })
    {
        return false;
    }

    let parents = nodes
        .iter()
        .map(|node| (node.id.as_str(), node.parent.as_deref()))
        .collect::<std::collections::HashMap<_, _>>();
    nodes.iter().all(|node| {
        let mut visited = std::collections::HashSet::new();
        let mut current = Some(node.id.as_str());
        while let Some(id) = current {
            if !visited.insert(id) {
                return false;
            }
            current = parents.get(id).copied().flatten();
        }
        true
    })
}
