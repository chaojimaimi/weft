//! Pure data model and identity rules for the native accessibility bridge.

use std::hash::{Hash, Hasher};

use crate::scene::{SemanticNode, SemanticRole};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AccessibilityAction {
    PressPoint { x: f64, y: f64 },
    NewTab,
    SwitchSession(u64),
    ContextMenuItem { session_id: u64, index: usize },
    PaletteEntry(String),
    PaletteTheme(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockTextKey {
    pub(crate) session_id: u64,
    pub(crate) block_count: usize,
    pub(crate) last_output_len: usize,
    pub(crate) live_output_len: usize,
    pub(crate) scroll: usize,
    pub(crate) editor_hash: u64,
    pub(crate) cwd_hash: u64,
    pub(crate) git_branch_hash: u64,
    pub(crate) fold_hash: u64,
    pub(crate) width_bits: u32,
    pub(crate) height_bits: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessibilityNode {
    pub(crate) id: String,
    pub(crate) parent: Option<String>,
    pub(crate) role: SemanticRole,
    pub(crate) label: String,
    pub(crate) bounds: [f32; 4],
    pub(crate) state: String,
    pub(crate) action: Option<AccessibilityAction>,
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
            action: pressable.then(|| AccessibilityAction::PressPoint {
                x: f64::from((node.bounds[0] + node.bounds[2]) * 0.5),
                y: f64::from((node.bounds[1] + node.bounds[3]) * 0.5),
            }),
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

pub(crate) fn role_exposes_text_value(role: &SemanticRole) -> bool {
    matches!(role, SemanticRole::TextArea | SemanticRole::TextField)
}

pub(crate) fn selected_state(selected: bool) -> String {
    if selected {
        "selected".into()
    } else {
        String::new()
    }
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
            a.id != b.id
                || a.parent != b.parent
                || a.role != b.role
                || !same_action_identity(&a.action, &b.action)
        })
}

fn same_action_identity(
    left: &Option<AccessibilityAction>,
    right: &Option<AccessibilityAction>,
) -> bool {
    match (left, right) {
        (None, None)
        | (
            Some(AccessibilityAction::PressPoint { .. }),
            Some(AccessibilityAction::PressPoint { .. }),
        )
        | (Some(AccessibilityAction::NewTab), Some(AccessibilityAction::NewTab)) => true,
        (
            Some(AccessibilityAction::SwitchSession(left)),
            Some(AccessibilityAction::SwitchSession(right)),
        ) => left == right,
        (
            Some(AccessibilityAction::ContextMenuItem {
                session_id: left_session,
                index: left_index,
            }),
            Some(AccessibilityAction::ContextMenuItem {
                session_id: right_session,
                index: right_index,
            }),
        ) => left_session == right_session && left_index == right_index,
        (
            Some(AccessibilityAction::PaletteEntry(left)),
            Some(AccessibilityAction::PaletteEntry(right)),
        ) => left == right,
        (
            Some(AccessibilityAction::PaletteTheme(left)),
            Some(AccessibilityAction::PaletteTheme(right)),
        ) => left == right,
        _ => false,
    }
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
