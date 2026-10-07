//! v1.12.28 (P1-02 ⑤): payload-generic split-tree rebuild, extracted from
//! `workspace_controller::build_subtree` so the SQLite split-persistence
//! restore (recovery_restore) reuses the EXACT topology algorithm instead of
//! copy-pasting it.
//!
//! WHY an extraction (拓扑陷阱): [`build_subtree`] must split a leaf BEFORE
//! descending into `first`'s subtree — `SplitTree::split_leaf` replaces a
//! leaf with `Split{first: Leaf(id), second: Leaf(new_id)}`, so the order is
//! load-bearing for the resulting tree shape. Two callers now need it
//! (`WorkspacePaneNode` from workspace YAML, `SnapshotPaneNode` from the
//! `tabs.panes` SQL column); the algorithm lives here exactly once, behind
//! the [`PaneTreeRebuild`] trait that abstracts the per-node payload.

use weft_core::pane_layout::{PaneId, SplitDirection};
use weft_core::persistence::{SnapshotPaneNode, TabSnapshot};
use weft_core::workspace::WorkspacePaneNode;

use crate::pane::restore_spawn_cwd;
use crate::tab::Tab;

/// Type alias for the closure used by [`build_subtree`] to create a new pane
/// when descending into a `Split` node's second child. The closure receives
/// the leaf pane id to split, the split direction + ratio, and the new pane's
/// spawn cwd (per-impl fallback semantics; `""` from the snapshot path means
/// "inherit the weft process cwd"); it returns the new pane's id on success
/// or `None` on spawn failure.
pub(crate) type SplitFn<'a> =
    &'a mut dyn FnMut(&mut Tab, PaneId, SplitDirection, f32, &str) -> Option<PaneId>;

/// v1.12.28 (P1-02 ⑤): the shape the shared rebuild recursion needs from a
/// serializable pane tree. Implemented by the workspace YAML node
/// ([`WorkspacePaneNode`]) and the SQLite split-snapshot node
/// ([`SnapshotPaneNode`]).
pub(crate) trait PaneTreeRebuild {
    /// `Some((direction, ratio, first, second))` for a Split node; `None`
    /// for a leaf (the base case → [`PaneTreeRebuild::apply_leaf`]).
    fn split_parts(&self) -> Option<(SplitDirection, f32, &Self, &Self)>;
    /// cwd to spawn this subtree's root leaf with (used for the second
    /// child's pane and, by the callers, for the initial tab spawn).
    /// Per-impl fallback: the workspace path falls back to `"/"`, the
    /// snapshot path returns `""` (= inherit the weft process cwd, the
    /// `restore_spawn_cwd` contract).
    fn root_leaf_spawn_cwd(&self) -> String;
    /// Base case: apply this leaf's payload (cwd fallback + editor draft /
    /// buffer + per-leaf recovery state) to the pane that already exists in
    /// `tab` (created by the tab-opening path for the root leaf, or by the
    /// caller's `split_fn` for non-root leaves).
    fn apply_leaf(&self, tab: &mut Tab, pane_id: PaneId);
}

/// Recursively rebuild a tab's split tree from a [`PaneTreeRebuild`] node.
///
/// `pane_id` is the pane that already exists in `tab` and corresponds to
/// the root leaf of `node`'s subtree.
///
/// For a `Split` node, the algorithm is:
/// 1. Split `pane_id` FIRST to create `second`'s root pane. This produces
///    `Split{first: Leaf(pane_id), second: Leaf(new_pane_id)}`.
/// 2. Set up `second`'s root pane (draft, cwd).
/// 3. Recursively build `first`'s subtree using `pane_id` as root. Since
///    `pane_id` is still a leaf (it's the `first` child of the new Split),
///    `split_leaf` can find and replace it.
/// 4. Recursively build `second`'s subtree using `new_pane_id` as root.
///
/// Splitting BEFORE building subtrees ensures the tree topology matches
/// the saved document. If we built `first` first and then split, the
/// split would wrap `pane_id` inside `first`'s subtree, producing the
/// wrong tree shape.
///
/// `split_fn` creates a new pane by splitting `leaf` in `tab` with the
/// given `direction`, `ratio`, and `cwd`. In production, this spawns a
/// real PTY via `Pane::spawn` + `Tab::split_pane_with_pane`. In tests,
/// it injects a no-PTY pane via `Pane::with_terminal_only`.
pub(crate) fn build_subtree<T: PaneTreeRebuild>(
    tab: &mut Tab,
    node: &T,
    pane_id: PaneId,
    mut split_fn: SplitFn,
) {
    match node.split_parts() {
        Some((direction, ratio, first, second)) => {
            // Step 1: Split `pane_id` to create `second`'s root pane.
            // This must happen BEFORE building `first`'s subtree so the
            // tree topology matches the saved document.
            let second_cwd = second.root_leaf_spawn_cwd();
            let new_pane_id = match split_fn(tab, pane_id, direction, ratio, &second_cwd) {
                Some(id) => id,
                None => return,
            };

            // Step 2: recursively build `first`'s subtree. `pane_id` is
            // still a leaf (the `first` child of the new Split), so
            // `split_leaf` inside recursive calls can find and replace it.
            build_subtree(tab, first, pane_id, &mut split_fn);

            // Step 3: recursively build `second`'s subtree.
            build_subtree(tab, second, new_pane_id, &mut split_fn);
        }
        None => node.apply_leaf(tab, pane_id),
    }
}

impl PaneTreeRebuild for WorkspacePaneNode {
    fn split_parts(&self) -> Option<(SplitDirection, f32, &Self, &Self)> {
        match self {
            WorkspacePaneNode::Pane { .. } => None,
            WorkspacePaneNode::Split {
                direction,
                ratio,
                first,
                second,
            } => Some((*direction, *ratio, first, second)),
        }
    }

    fn root_leaf_spawn_cwd(&self) -> String {
        // Workspace semantics: an unknown cwd spawns at "/" (the historical
        // `root_leaf_cwd(second).unwrap_or("/")` fallback, kept verbatim).
        super::root_leaf_cwd(self).unwrap_or_else(|| "/".to_string())
    }

    fn apply_leaf(&self, tab: &mut Tab, pane_id: PaneId) {
        let WorkspacePaneNode::Pane { cwd, draft } = self else {
            return;
        };
        // Base case: set the draft on this pane. The pane already exists
        // (created by the tab-opening path for the root leaf, or by
        // `split_fn` for non-root leaves). The draft is set here so every
        // leaf gets its draft regardless of whether it's a root, first
        // child, or second child.
        if let Some(pane) = tab.pane_mut(pane_id) {
            pane.set_restored_cwd_fallback(Some(cwd.to_string_lossy().into_owned()));
            if !draft.is_empty() {
                if let Some(terminal) = pane.terminal.as_mut() {
                    terminal.editor_mut().buffer.set_text(draft);
                }
            }
        }
    }
}

impl PaneTreeRebuild for SnapshotPaneNode {
    fn split_parts(&self) -> Option<(SplitDirection, f32, &Self, &Self)> {
        match self {
            SnapshotPaneNode::Pane { .. } => None,
            SnapshotPaneNode::Split {
                direction,
                ratio,
                first,
                second,
            } => Some((*direction, *ratio, first, second)),
        }
    }

    fn root_leaf_spawn_cwd(&self) -> String {
        match self {
            // Snapshot semantics: `restore_spawn_cwd` (empty/absent → ""
            // → inherit the weft process cwd), same as the single-pane tab
            // restore path.
            SnapshotPaneNode::Pane { cwd, .. } => {
                restore_spawn_cwd(cwd.as_deref()).unwrap_or_default()
            }
            SnapshotPaneNode::Split { first, .. } => first.root_leaf_spawn_cwd(),
        }
    }

    fn apply_leaf(&self, tab: &mut Tab, pane_id: PaneId) {
        let SnapshotPaneNode::Pane {
            cwd,
            editor_buffer,
            block_ids,
        } = self
        else {
            return;
        };
        let decoded = TabSnapshot::decode_editor_buffer(editor_buffer);
        if decoded.is_none() {
            tracing::warn!("failed to deserialize per-leaf editor buffer; using empty");
        }
        let Some(pane) = tab.pane_mut(pane_id) else {
            return;
        };
        pane.set_restored_cwd_fallback(restore_spawn_cwd(cwd.as_deref()));
        // v1.12.28 (P1-02 ③): per-leaf attach — synthesize the TabSnapshot
        // shape this leaf needs (block_ids for the hydration loop, cwd for
        // the pre-OSC-7 fallback) and write it to THIS pane's
        // `restored_snapshot` (the `attach_recovery_snapshot` precedent,
        // pane.rs side). Must NOT go through `Tab::restore_from_snapshot` —
        // its Deref lands on the active pane only. `block_scroll_offset`
        // stays 0 here: block scroll is tab-level and applied once after
        // the rebuild via `set_block_scroll`.
        pane.restored_snapshot = Some(TabSnapshot {
            position: 0,
            active: false,
            cwd: restore_spawn_cwd(cwd.as_deref()),
            block_scroll_offset: 0,
            editor_buffer: editor_buffer.clone(),
            shell_phase: "AtPrompt".to_string(),
            block_ids: block_ids.clone(),
            panes: None,
        });
        if let (Some(terminal), Some(buf)) = (pane.terminal.as_mut(), decoded) {
            terminal.editor_mut().buffer = buf;
        }
    }
}
