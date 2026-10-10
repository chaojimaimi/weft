//! Tab snapshot serialization family (`pane_leaf_snapshot` /
//! `to_snapshot` / `restore_from_snapshot` + the `snapshot_node_from_tree`
//! DFS fold), moved verbatim out of `tab.rs` (v1.13.8 S3 zero-behavior
//! file-budget split; `impl Tab` cross-file block per the tab/scroll.rs
//! precedent).

use super::Tab;
use weft_core::pane_layout::{PaneId, PaneTree};
use weft_core::persistence::{PaneTreeSnapshot, SnapshotPaneNode, TabSnapshot};

impl Tab {
    /// v1.12.28 (P1-02 ②): serialize one leaf pane for the split-tree
    /// snapshot. A live leaf carries its live terminal state (same field
    /// semantics as the single-pane path). A PTY-dead leaf keeps its tree
    /// position with the single-pane PTY-dead semantics (real
    /// `restored_snapshot` first, then a minimal shell from `restored_cwd`).
    /// Documented degradation: a dead leaf reopens as an empty
    /// pending-activation pane.
    fn pane_leaf_snapshot(&self, pane_id: PaneId) -> SnapshotPaneNode {
        let Some(pane) = self.panes.get(&pane_id) else {
            return SnapshotPaneNode::Pane {
                cwd: None,
                editor_buffer: String::new(),
                block_ids: Vec::new(),
            };
        };
        // T10 P1: the live serialization runs inside the guard scope and only
        // the owned node escapes; the PTY-dead fallbacks below are unchanged.
        let live = pane.with_terminal(|terminal| SnapshotPaneNode::Pane {
            cwd: terminal
                .cwd()
                .map(str::to_owned)
                .or_else(|| pane.restored_cwd_fallback().map(str::to_owned)),
            editor_buffer: TabSnapshot::encode_editor_buffer(&terminal.editor().buffer),
            block_ids: terminal.block_tracker().lineage_block_ids(),
        });
        if let Some(node) = live {
            return node;
        }
        if let Some(saved) = pane.restored_snapshot.as_ref() {
            return SnapshotPaneNode::Pane {
                cwd: saved.cwd.clone(),
                editor_buffer: saved.editor_buffer.clone(),
                block_ids: saved.block_ids.clone(),
            };
        }
        SnapshotPaneNode::Pane {
            cwd: pane.restored_cwd.clone(),
            editor_buffer: String::new(),
            block_ids: Vec::new(),
        }
    }

    /// v1.0 H4: Serialize this tab's UI state to a [`TabSnapshot`] for
    /// SQLite persistence. A restored tab whose PTY failed keeps its loaded
    /// snapshot; only a fresh empty tab with no recovery state returns `None`.
    ///
    /// The PTY itself is NOT serialized (impossible to revive). On
    /// restore, the tab shows the saved editor draft + block history;
    /// the user presses Enter to spawn a fresh shell in the saved cwd.
    ///
    /// v1.12.28 (P1-02 ②): multi-pane tabs additionally persist the split
    /// tree (`panes`) — see the branch below.
    pub fn to_snapshot(&self, position: usize, active: bool) -> Option<TabSnapshot> {
        // T10 P1 (D9 rule 2): the terminal state is read under ONE short
        // guard; the multi-pane fold below re-locks per leaf
        // (`pane_leaf_snapshot`), so no guard may be held across it.
        let live = self.with_terminal(|terminal| {
            let shell_phase = match terminal.block_tracker().phase() {
                weft_core::blocks::ShellPhase::NotIntegrated => "NotIntegrated",
                weft_core::blocks::ShellPhase::AtPrompt => "AtPrompt",
                weft_core::blocks::ShellPhase::CommandExecuting => "CommandExecuting",
            };
            (
                terminal.cwd().map(str::to_owned),
                weft_core::persistence::TabSnapshot::encode_editor_buffer(
                    &terminal.editor().buffer,
                ),
                shell_phase,
                terminal.block_tracker().lineage_block_ids(),
            )
        });
        let Some((live_cwd, editor_buffer, shell_phase, block_ids)) = live else {
            // PTY-dead pane: keep the recovery state serializable. A fresh
            // empty tab with no recovery state still returns `None`
            // (unchanged); a pane with the real attached snapshot or only
            // the workspace cwd fallback (v1.10.24 B1) stays serializable —
            // for the fallback-only case we synthesize the same minimal
            // snapshot the old stub carried.
            let mut snapshot = match self.restored_snapshot.clone() {
                Some(snapshot) => snapshot,
                None => {
                    let cwd = self.restored_cwd.clone()?;
                    TabSnapshot {
                        position: 0,
                        active: false,
                        cwd: Some(cwd),
                        block_scroll_offset: 0,
                        editor_buffer: String::new(),
                        shell_phase: "AtPrompt".to_string(),
                        block_ids: Vec::new(),
                        panes: None,
                    }
                }
            };
            if snapshot.cwd.is_none() {
                snapshot.cwd = self.restored_cwd.clone();
            }
            snapshot.position = position;
            snapshot.active = active;
            snapshot.block_scroll_offset = self.block_scroll();
            return Some(snapshot);
        };
        let cwd = live_cwd.or_else(|| self.restored_cwd_fallback().map(str::to_owned));
        // v1.12.28 (P1-02 ②): multi-pane branch FIRST. `pane_count()` is the
        // structural accessor (zoom-safe) — `split_tree().panes()` collapses
        // to `[zoomed]` and `Tab::panes()` iterates a HashMap, so neither may
        // gate this branch. `export_tree` ignores zoom, so the snapshot
        // stores the STRUCTURE tree and a zoomed tab restores un-zoomed
        // (documented). The active leaf's state also fills the top-level
        // legacy fields so single-pane readers stay consistent.
        if self.pane_count() > 1 {
            let active_id = self.active_pane_id();
            let mut index = 0usize;
            let mut active_leaf = 0usize;
            let tree = self
                .split_tree
                .export_tree(|pane_id| (pane_id, self.pane_leaf_snapshot(pane_id)))
                .map(|exported| {
                    snapshot_node_from_tree(exported, active_id, &mut index, &mut active_leaf)
                });
            let panes = tree.map(|tree| PaneTreeSnapshot { tree, active_leaf });
            return Some(TabSnapshot {
                position,
                active,
                cwd,
                block_scroll_offset: self.block_scroll(),
                editor_buffer,
                shell_phase: shell_phase.to_string(),
                block_ids,
                panes,
            });
        }
        // v1.7.6: persist per-tab block ownership so each tab can restore
        // only its own history on next launch (per-tab isolation).
        // v1.12.24 (N-3): use the full lineage (session-produced UNION
        // loaded-from-previous-restore) — the v1.7.6 session-only design
        // dropped the previous generation's recall on every restart-restore
        // cycle, so each ↑ history shrunk by one generation per restart.
        // (cwd / editor_buffer / shell_phase / block_ids were captured under
        // the single short guard at the top of this function.)
        // v1.12.28 (P1-02 ②): single-pane tabs never write the `panes`
        // field — the current path is unchanged apart from the new field.
        Some(TabSnapshot {
            position,
            active,
            cwd,
            block_scroll_offset: self.block_scroll(),
            editor_buffer,
            shell_phase: shell_phase.to_string(),
            block_ids,
            panes: None,
        })
    }

    /// v1.0 H4: Restore the editor draft + block-scroll offset from a
    /// [`TabSnapshot`]. Called after [`Tab::new`] to apply the saved
    /// state. The PTY + terminal are already initialized by `new`;
    /// this just rehydrates the editor buffer and scroll offset.
    ///
    /// Returns `false` if the snapshot's editor buffer JSON was invalid
    /// (the tab stays usable with an empty editor — same as a fresh tab).
    pub fn restore_from_snapshot(&mut self, snap: &TabSnapshot) -> bool {
        self.restored_snapshot = Some(snap.clone());
        self.set_block_scroll(snap.block_scroll_offset);
        match weft_core::persistence::TabSnapshot::decode_editor_buffer(&snap.editor_buffer) {
            Some(buf) => self
                .with_terminal(|terminal| terminal.editor_mut().buffer = buf)
                .is_some(),
            None => {
                tracing::warn!("failed to deserialize editor buffer; using empty");
                false
            }
        }
    }
}

/// v1.12.28 (P1-02 ②): DFS-fold the exported pane tree into a
/// [`SnapshotPaneNode`], recording the DFS index (first child before
/// second — the `export_tree` / `pane_id_at_index` leaf order) of
/// `active_id` into `active_leaf`. `index` counts leaves as they are
/// folded; the caller passes a shared cursor so nested `Split`s keep
/// numbering monotonically.
pub(super) fn snapshot_node_from_tree(
    tree: PaneTree<(PaneId, SnapshotPaneNode)>,
    active_id: PaneId,
    index: &mut usize,
    active_leaf: &mut usize,
) -> SnapshotPaneNode {
    match tree {
        PaneTree::Leaf((pane_id, node)) => {
            if pane_id == active_id {
                *active_leaf = *index;
            }
            *index += 1;
            node
        }
        PaneTree::Split {
            direction,
            ratio,
            first,
            second,
        } => SnapshotPaneNode::Split {
            direction,
            ratio,
            first: Box::new(snapshot_node_from_tree(
                *first,
                active_id,
                index,
                active_leaf,
            )),
            second: Box::new(snapshot_node_from_tree(
                *second,
                active_id,
                index,
                active_leaf,
            )),
        },
    }
}
