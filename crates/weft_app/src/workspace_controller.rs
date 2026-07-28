//! v1.6.2: Workspace controller — bridges runtime session state
//! (`SessionManager` / `SplitTree` / `Tab` / `Pane`) and the serializable
//! [`WorkspaceDocument`] DTO.
//!
//! ## Responsibilities
//!
//! Capture — walk the live tab/pane tree and produce a `WorkspaceDocument`
//! (cwd + editor draft at each leaf, split direction + ratio at each
//! internal node, active tab/pane indices, profile name, window size).
//!
//! Restore — load a `WorkspaceDocument` and rebuild the session tree by
//! opening tabs and splitting panes to match the saved topology. The PTY
//! is NOT restored; each leaf spawns a fresh shell in the saved cwd.
//! Editor drafts are inserted but NEVER auto-executed.
//!
//! Interactive save/open — show native NSSavePanel / NSOpenPanel and pipe
//! the chosen path into save / restore.
//!
//! ## Degradation
//!
//! - A saved cwd that no longer exists falls back to `$HOME` (the PTY
//!   spawn path already handles this by inheriting the weft process cwd
//!   when `chdir` fails).
//! - A saved profile name that no longer exists falls back to base config
//!   and surfaces a diagnostic.
//! - Pane creation failures (PTY spawn failure) are logged but don't abort
//!   the restore — the tab stays usable with whatever panes succeeded.
//!
//! ## Tree rebuild algorithm
//!
//! [`SplitTree::split_leaf`] replaces a leaf `PaneId` with
//! `Split { first: Leaf(id), second: Leaf(new_id) }`. The `first` child
//! is always the pane that was split. This means a leaf can be split
//! multiple times (each split wraps it in another `Split` node, keeping
//! `Leaf(id)` as the `first` child all the way down).
//!
//! The rebuild exploits this: `build_subtree(node, pane_id)` recursively
//! builds `node`'s subtree using `pane_id` as the root leaf. After
//! building `first`, `pane_id` is still a leaf (nested inside `first`'s
//! subtree), so we can split it again to create `second`'s root pane.

use std::path::PathBuf;

use tracing::{info, warn};
use weft_core::pane_layout::{PaneId, PaneTree, SplitDirection};
use weft_core::workspace::{WorkspaceDocument, WorkspaceError, WorkspacePaneNode, WorkspaceWindow};

use crate::pane::Pane;
use crate::tab::Tab;
use crate::App;

/// Type alias for the closure used by [`build_subtree`] to create a new pane
/// when descending into a `Split` node's second child. The closure receives
/// the leaf pane id to split, the split direction + ratio, and the new pane's
/// cwd; it returns the new pane's id on success or `None` on spawn failure.
type SplitFn<'a> = &'a mut dyn FnMut(&mut Tab, PaneId, SplitDirection, f32, &str) -> Option<PaneId>;

/// Per-pane payload captured from the live session tree. The workspace
/// controller walks the `SplitTree` and produces a `PaneTree<PanePayload>`
/// which is then converted to `WorkspacePaneNode`.
#[derive(Clone, Debug)]
struct PanePayload {
    cwd: Option<PathBuf>,
    draft: String,
}

impl App {
    // ── Capture (runtime → DTO) ───────────────────────────────────────

    /// v1.6.2: Capture the current session state as a
    /// [`WorkspaceDocument`]. The caller supplies the workspace `name`
    /// (used in the palette / window title). The window size is read from
    /// the live window; the profile name from the active config source.
    ///
    /// Returns `None` when there are no tabs (nothing to save).
    pub(super) fn capture_workspace(&self, name: String) -> Option<WorkspaceDocument> {
        if self.sessions.is_empty() {
            return None;
        }
        let tabs = self
            .sessions
            .tabs()
            .iter()
            .map(|tab| self.capture_tab(tab))
            .collect();
        let active_tab = self.sessions.active_idx();
        let window = self.capture_window();
        let profile = self.active_profile_name().map(str::to_owned);
        Some(WorkspaceDocument {
            version: weft_core::workspace::WORKSPACE_VERSION,
            name,
            profile,
            window,
            tabs,
            active_tab,
        })
    }

    /// Capture a single tab's split tree + active pane index.
    fn capture_tab(&self, tab: &Tab) -> weft_core::workspace::WorkspaceTab {
        let split_tree = tab.split_tree();
        let active_pane_id = tab.active_pane_id();

        // Walk the tree, capturing (cwd, draft) at each leaf. The closure
        // borrows `tab` to read the pane's terminal state.
        let tree = split_tree.export_tree(|pane_id| self.capture_pane(tab, pane_id));

        // Compute active_pane_index before moving `tree` into the conversion.
        // The index is the DFS position of the active pane in the split tree.
        let active_pane_index = if tree.is_some() {
            let panes: Vec<PaneId> = split_tree.panes();
            panes.iter().position(|p| *p == active_pane_id).unwrap_or(0)
        } else {
            0
        };

        let panes = match tree {
            None => WorkspacePaneNode::Pane {
                // Empty tree — shouldn't happen for a live tab, but
                // produce a valid fallback rather than panicking.
                cwd: PathBuf::from("/"),
                draft: String::new(),
            },
            Some(tree) => pane_tree_to_workspace_node(tree),
        };

        weft_core::workspace::WorkspaceTab {
            panes,
            active_pane_index,
        }
    }

    /// Read a pane's cwd (live OSC 7 or restored snapshot) and editor
    /// draft text. Returns `PanePayload` with `cwd: None` when the pane
    /// has no terminal (PTY spawn failure — rare).
    fn capture_pane(&self, tab: &Tab, pane_id: PaneId) -> PanePayload {
        let pane = tab.pane(pane_id);
        let Some(pane) = pane else {
            return PanePayload {
                cwd: None,
                draft: String::new(),
            };
        };
        let cwd = pane
            .terminal
            .as_ref()
            .and_then(|t| t.cwd())
            .or_else(|| {
                pane.restored_snapshot
                    .as_ref()
                    .and_then(|s| s.cwd.as_deref())
            })
            .map(PathBuf::from);
        let draft = pane
            .terminal
            .as_ref()
            .map(|t| t.editor().text())
            .unwrap_or_default();
        PanePayload { cwd, draft }
    }

    /// Read the live window's inner size in logical pixels. Falls back to
    /// the config's window size when the window isn't available (early
    /// boot, headless test).
    fn capture_window(&self) -> WorkspaceWindow {
        if let Some(window) = self.window.as_ref() {
            let scale = window.scale_factor();
            let size = window.inner_size().to_logical::<u32>(scale);
            return WorkspaceWindow {
                width: size.width,
                height: size.height,
            };
        }
        let cfg = &self.config_state.config;
        WorkspaceWindow {
            width: cfg.window.width,
            height: cfg.window.height,
        }
    }

    // ── Restore (DTO → runtime) ───────────────────────────────────────

    /// v1.6.2: Restore session state from a [`WorkspaceDocument`].
    ///
    /// Closes all existing tabs and rebuilds the session tree from the
    /// document. The PTY is NOT restored — each leaf spawns a fresh
    /// shell in the saved cwd. Editor drafts are inserted but NEVER
    /// auto-executed.
    ///
    /// # Errors
    ///
    /// - [`WorkspaceRestoreError::NoTabs`] — the document has no tabs.
    /// - [`WorkspaceRestoreError::ProfileSwitch`] — the saved profile
    ///   doesn't exist; falls back to base but surfaces the error.
    pub(super) fn restore_workspace(
        &mut self,
        doc: &WorkspaceDocument,
    ) -> Result<(), WorkspaceRestoreError> {
        if doc.tabs.is_empty() {
            return Err(WorkspaceRestoreError::NoTabs);
        }

        // Step 1: switch profile if specified. A missing profile falls
        // back to base with a diagnostic — the restore continues.
        //
        // v1.6.2 review C3: previously this returned early, aborting the
        // entire restore (tabs not closed, window not resized, etc.)
        // despite the doc comment saying "restore continues". Now we
        // track the non-fatal error and return it at the end, after the
        // full restore has completed.
        let mut profile_warning: Option<WorkspaceRestoreError> = None;
        if let Some(profile) = &doc.profile {
            if !profile.is_empty() && self.active_profile_name() != Some(profile.as_str()) {
                if self.profile_names_sorted().iter().any(|p| p == profile) {
                    if let Err(e) = self.switch_profile(Some(profile)) {
                        tracing::warn!(error = %e, profile = %profile, "profile switch failed during workspace restore");
                    }
                } else {
                    tracing::warn!(
                        profile = %profile,
                        "workspace profile not found, falling back to base"
                    );
                    profile_warning = Some(WorkspaceRestoreError::ProfileNotFound {
                        profile: profile.clone(),
                    });
                }
            }
        }

        // Step 2: close all existing tabs. We don't run block-finish
        // hooks here because the caller (palette entry) is expected to
        // have already persisted pending blocks via save_all_tabs.
        while !self.sessions.is_empty() {
            self.sessions.close_active();
        }

        // Step 3: restore window size.
        if let Some(window) = self.window.as_ref() {
            let new_size =
                winit::dpi::LogicalSize::new(doc.window.width as f64, doc.window.height as f64);
            let _ = window.request_inner_size(new_size);
        }

        // Step 4: rebuild tabs. Each tab's split tree is rebuilt
        // recursively by splitting the initial pane.
        for (tab_idx, tab_doc) in doc.tabs.iter().enumerate() {
            self.restore_tab(
                &tab_doc.panes,
                tab_doc.active_pane_index,
                tab_idx == doc.active_tab,
            );
        }

        // Step 5: set active tab.
        self.sessions
            .set_active(doc.active_tab.min(self.sessions.len().saturating_sub(1)));

        self.reset_ime_context("workspace restored");
        self.refresh_find_for_active_tab();
        self.request_redraw();

        // Return the non-fatal profile warning (if any) after a successful
        // restore. Ok(()) when the profile was found or unset.
        match profile_warning {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Restore a single tab from a [`WorkspacePaneNode`] tree.
    ///
    /// The first leaf's cwd is used to spawn the initial tab; subsequent
    /// leaves are created by splitting panes in the saved directions and
    /// ratios. Editor drafts are inserted via `EditorBuffer::set_text`
    /// (NEVER auto-executed).
    ///
    /// `active_pane_index` is the DFS index of the pane that should be
    /// focused after restore (v1.6.2 review C4: previously captured but
    /// never applied — the tab always ended with pane 0 active).
    fn restore_tab(
        &mut self,
        panes: &WorkspacePaneNode,
        active_pane_index: usize,
        is_active: bool,
    ) {
        let (rows, cols) = self.current_size();
        let scrollback = self.config_state.config.scrollback.lines;

        // Find the root leaf's cwd to open the initial tab.
        let root_cwd = root_leaf_cwd(panes);
        let root_draft = root_leaf_draft(panes);

        // Open the tab with the root leaf's cwd.
        let tab_idx =
            self.sessions
                .open_tab(rows, cols, scrollback, &self.proxy, root_cwd.as_deref());

        // Apply block_id_allocator + palette (mirrors new_tab).
        if let Some(block_id_allocator) = self
            .sessions
            .block_store()
            .map(weft_core::persistence::BlockStore::block_id_allocator)
        {
            if let Some(terminal) = self
                .sessions
                .tab_mut(tab_idx)
                .and_then(|tab| tab.terminal.as_mut())
            {
                terminal
                    .block_tracker_mut()
                    .use_shared_id_allocator(block_id_allocator);
            }
        }
        if let Some(t) = self
            .sessions
            .tab_mut(tab_idx)
            .and_then(|tab| tab.terminal.as_mut())
        {
            if let Some(r) = &self.renderer {
                t.set_palette(r.theme().palette);
            }
        }

        // Set the root leaf's editor draft.
        if !root_draft.is_empty() {
            if let Some(terminal) = self
                .sessions
                .tab_mut(tab_idx)
                .and_then(|tab| tab.terminal.as_mut())
            {
                terminal.editor_mut().buffer.set_text(&root_draft);
            }
        }

        // v1.6.2 review M5: set the root pane's restored_snapshot so
        // `terminal.cwd()` falls back to the saved cwd before OSC 7 is
        // reported by the freshly-spawned shell. Non-root panes get their
        // restored_snapshot set in `build_subtree` via `set_pane_cwd`.
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            let root_pane_id = tab.active_pane_id();
            if let Some(pane) = tab.pane_mut(root_pane_id) {
                if pane.restored_snapshot.is_none() {
                    pane.restored_snapshot = Some(weft_core::persistence::TabSnapshot {
                        position: 0,
                        active: false,
                        cwd: root_cwd.clone(),
                        block_scroll_offset: 0,
                        editor_buffer: String::new(),
                        shell_phase: "AtPrompt".to_string(),
                    });
                }
            }
        }

        // Recursively rebuild the split tree. The initial pane's id is
        // the tab's active pane id (just created).
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            let initial_pane_id = tab.active_pane_id();
            let proxy = self.proxy.clone();
            let mut split_fn =
                |tab: &mut Tab, leaf: PaneId, dir: SplitDirection, ratio: f32, cwd: &str| {
                    let new_pane = Pane::spawn(rows, cols, scrollback, &proxy, Some(cwd));
                    tab.split_pane_with_pane(leaf, dir, ratio, new_pane)
                        .map_err(|e| {
                            warn!(?e, "workspace restore: split failed, skipping subtree");
                            e
                        })
                        .ok()
                };
            build_subtree(tab, panes, initial_pane_id, &mut split_fn);
        }

        // v1.6.2 review C4: restore the saved active pane. The DFS order
        // of `split_tree().panes()` matches the order leaves were created
        // by `build_subtree`, which matches the saved document's DFS order.
        if active_pane_index > 0 {
            if let Some(tab) = self.sessions.tab_mut(tab_idx) {
                let panes_list = tab.split_tree().panes();
                if active_pane_index < panes_list.len() {
                    let target_id = panes_list[active_pane_index];
                    if let Err(e) = tab.set_active_pane(target_id) {
                        warn!(
                            ?e,
                            index = active_pane_index,
                            "workspace restore: failed to set active pane"
                        );
                    }
                }
            }
        }

        // Set active tab if this is the active one in the document.
        if is_active {
            self.sessions.set_active(tab_idx);
        }
    }

    // ── Interactive save / open ───────────────────────────────────────

    /// v1.6.2: Show NSSavePanel, capture the current session, and write
    /// it to the chosen path as YAML.
    ///
    /// Errors are logged and surfaced via the settings error field (when
    /// Settings is open) or the status hint. Cancel returns `Ok(())`.
    pub(super) fn workspace_save_interactive(
        &mut self,
        mtm: objc2_foundation::MainThreadMarker,
    ) -> Result<(), WorkspaceInteractionError> {
        let path = crate::macos_file_dialog::pick_workspace_save_path(mtm)?
            .ok_or(WorkspaceInteractionError::Cancelled)?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace")
            .to_owned();
        let Some(doc) = self.capture_workspace(name) else {
            return Err(WorkspaceInteractionError::NothingToSave);
        };
        doc.save(&path)
            .map_err(WorkspaceInteractionError::Workspace)?;
        info!(path = %path.display(), "workspace saved");
        Ok(())
    }

    /// v1.6.2: Show NSOpenPanel, load a workspace YAML, and restore the
    /// session tree. Cancel returns `Ok(())`.
    pub(super) fn workspace_open_interactive(
        &mut self,
        mtm: objc2_foundation::MainThreadMarker,
    ) -> Result<(), WorkspaceInteractionError> {
        let path = crate::macos_file_dialog::pick_workspace_open_path(mtm)?
            .ok_or(WorkspaceInteractionError::Cancelled)?;
        let doc = WorkspaceDocument::load(&path).map_err(WorkspaceInteractionError::Workspace)?;
        self.restore_workspace(&doc)
            .map_err(|e| WorkspaceInteractionError::Restore {
                source: e,
                path: path.clone(),
            })?;
        info!(path = %path.display(), "workspace restored");
        Ok(())
    }
}

// ── Free helpers ───────────────────────────────────────────────────────

/// Convert a `PaneTree<PanePayload>` (captured from the live tree) into
/// a serializable `WorkspacePaneNode`. The conversion is a 1:1 shape
/// mapping with `PanePayload` → `(cwd, draft)` at each leaf.
fn pane_tree_to_workspace_node(tree: PaneTree<PanePayload>) -> WorkspacePaneNode {
    match tree {
        PaneTree::Leaf(payload) => WorkspacePaneNode::Pane {
            cwd: payload.cwd.unwrap_or_else(|| PathBuf::from("/")),
            draft: payload.draft,
        },
        PaneTree::Split {
            direction,
            ratio,
            first,
            second,
        } => WorkspacePaneNode::Split {
            direction,
            ratio,
            first: Box::new(pane_tree_to_workspace_node(*first)),
            second: Box::new(pane_tree_to_workspace_node(*second)),
        },
    }
}

/// Recursively rebuild a tab's split tree from a [`WorkspacePaneNode`].
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
fn build_subtree(tab: &mut Tab, node: &WorkspacePaneNode, pane_id: PaneId, mut split_fn: SplitFn) {
    match node {
        WorkspacePaneNode::Pane { cwd: _, draft } => {
            // Base case: set the draft on this pane. The pane already
            // exists (created by the tab-opening path for the root leaf,
            // or by split_fn for non-root leaves). The draft is set here
            // so every leaf gets its draft regardless of whether it's
            // a root, first child, or second child.
            if !draft.is_empty() {
                if let Some(pane) = tab.pane_mut(pane_id) {
                    if let Some(terminal) = pane.terminal.as_mut() {
                        terminal.editor_mut().buffer.set_text(draft);
                    }
                }
            }
        }
        WorkspacePaneNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            // Step 1: Split `pane_id` to create `second`'s root pane.
            // This must happen BEFORE building `first`'s subtree so the
            // tree topology matches the saved document.
            let second_cwd = root_leaf_cwd(second).unwrap_or_else(|| "/".to_string());
            let new_pane_id = match split_fn(tab, pane_id, *direction, *ratio, &second_cwd) {
                Some(id) => id,
                None => return,
            };

            // Step 2: set up the new pane's cwd + draft.
            set_pane_draft(tab, new_pane_id, second);
            set_pane_cwd(tab, new_pane_id, second);

            // Step 3: recursively build `first`'s subtree. `pane_id` is
            // still a leaf (the `first` child of the new Split), so
            // `split_leaf` inside recursive calls can find and replace it.
            build_subtree(tab, first, pane_id, &mut split_fn);

            // Step 4: recursively build `second`'s subtree.
            build_subtree(tab, second, new_pane_id, &mut split_fn);
        }
    }
}

/// Set the editor draft on a pane's terminal. No-op if the pane has no
/// terminal (PTY spawn failure).
fn set_pane_draft(tab: &mut Tab, pane_id: PaneId, node: &WorkspacePaneNode) {
    if let WorkspacePaneNode::Pane { draft, .. } = node {
        if draft.is_empty() {
            return;
        }
        if let Some(pane) = tab.pane_mut(pane_id) {
            if let Some(terminal) = pane.terminal.as_mut() {
                terminal.editor_mut().buffer.set_text(draft);
            }
        }
    }
}

/// Set the cwd on a pane's restored_snapshot (so launch_cwd returns it
/// even before the shell reports OSC 7). No-op for Split nodes.
fn set_pane_cwd(tab: &mut Tab, pane_id: PaneId, node: &WorkspacePaneNode) {
    if let WorkspacePaneNode::Pane { cwd, .. } = node {
        if let Some(pane) = tab.pane_mut(pane_id) {
            if pane.restored_snapshot.is_none() {
                pane.restored_snapshot = Some(weft_core::persistence::TabSnapshot {
                    position: 0,
                    active: false,
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                    block_scroll_offset: 0,
                    editor_buffer: String::new(),
                    shell_phase: "AtPrompt".to_string(),
                });
            }
        }
    }
}

/// Find the root leaf's cwd (the leftmost-topmost pane's cwd). Used to
/// open the initial tab.
fn root_leaf_cwd(node: &WorkspacePaneNode) -> Option<String> {
    match node {
        WorkspacePaneNode::Pane { cwd, .. } => {
            if cwd.as_os_str().is_empty() {
                None
            } else {
                Some(cwd.to_string_lossy().into_owned())
            }
        }
        WorkspacePaneNode::Split { first, .. } => root_leaf_cwd(first),
    }
}

/// Find the root leaf's editor draft.
fn root_leaf_draft(node: &WorkspacePaneNode) -> String {
    match node {
        WorkspacePaneNode::Pane { draft, .. } => draft.clone(),
        WorkspacePaneNode::Split { first, .. } => root_leaf_draft(first),
    }
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Errors raised during workspace restore (DTO → runtime).
#[derive(Debug)]
pub enum WorkspaceRestoreError {
    /// The document has no tabs.
    NoTabs,
    /// The saved profile name doesn't exist in the current config.
    /// Restore continues with the base profile.
    ProfileNotFound { profile: String },
}

impl std::fmt::Display for WorkspaceRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTabs => write!(f, "workspace document has no tabs"),
            Self::ProfileNotFound { profile } => {
                write!(f, "workspace profile '{profile}' not found, using base")
            }
        }
    }
}

impl std::error::Error for WorkspaceRestoreError {}

/// Errors raised during interactive save / open.
#[derive(Debug)]
pub enum WorkspaceInteractionError {
    /// User cancelled the file panel.
    Cancelled,
    /// There are no tabs to save (SessionManager is empty).
    NothingToSave,
    /// File panel error (not on main thread, unexpected response, etc.).
    FilePanel(crate::macos_file_dialog::FilePanelError),
    /// Workspace load/save error (I/O, YAML, validation).
    Workspace(WorkspaceError),
    /// Restore failed. The path is included for diagnostic context.
    Restore {
        source: WorkspaceRestoreError,
        path: PathBuf,
    },
}

impl std::fmt::Display for WorkspaceInteractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "workspace file panel cancelled"),
            Self::NothingToSave => write!(f, "no tabs to save"),
            Self::FilePanel(e) => write!(f, "workspace file panel error: {e}"),
            Self::Workspace(e) => write!(f, "workspace error: {e}"),
            Self::Restore { source, path } => {
                write!(
                    f,
                    "workspace restore failed for {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceInteractionError {}

impl From<crate::macos_file_dialog::FilePanelError> for WorkspaceInteractionError {
    fn from(e: crate::macos_file_dialog::FilePanelError) -> Self {
        Self::FilePanel(e)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::pane_layout::PaneTree;

    #[test]
    fn pane_tree_leaf_to_workspace_node_uses_cwd_and_draft() {
        let tree = PaneTree::Leaf(PanePayload {
            cwd: Some(PathBuf::from("/home/user")),
            draft: "ls".into(),
        });
        let node = pane_tree_to_workspace_node(tree);
        match node {
            WorkspacePaneNode::Pane { cwd, draft } => {
                assert_eq!(cwd, PathBuf::from("/home/user"));
                assert_eq!(draft, "ls");
            }
            _ => panic!("expected Pane"),
        }
    }

    #[test]
    fn pane_tree_leaf_with_no_cwd_falls_back_to_root() {
        let tree = PaneTree::Leaf(PanePayload {
            cwd: None,
            draft: String::new(),
        });
        let node = pane_tree_to_workspace_node(tree);
        match node {
            WorkspacePaneNode::Pane { cwd, draft } => {
                assert_eq!(cwd, PathBuf::from("/"));
                assert!(draft.is_empty());
            }
            _ => panic!("expected Pane"),
        }
    }

    #[test]
    fn pane_tree_split_maps_direction_and_ratio() {
        let tree = PaneTree::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.3,
            first: Box::new(PaneTree::Leaf(PanePayload {
                cwd: Some(PathBuf::from("/a")),
                draft: String::new(),
            })),
            second: Box::new(PaneTree::Leaf(PanePayload {
                cwd: Some(PathBuf::from("/b")),
                draft: "build".into(),
            })),
        };
        let node = pane_tree_to_workspace_node(tree);
        match node {
            WorkspacePaneNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                assert_eq!(direction, SplitDirection::Vertical);
                assert!((ratio - 0.3).abs() < 0.001);
                match &*first {
                    WorkspacePaneNode::Pane { cwd, .. } => {
                        assert_eq!(*cwd, PathBuf::from("/a"))
                    }
                    _ => panic!("expected Pane"),
                }
                match &*second {
                    WorkspacePaneNode::Pane { cwd, draft } => {
                        assert_eq!(*cwd, PathBuf::from("/b"));
                        assert_eq!(draft, "build");
                    }
                    _ => panic!("expected Pane"),
                }
            }
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn root_leaf_cwd_and_draft_follow_first_chain() {
        // Split { first: Pane(A), second: Split { first: Pane(B), second: Pane(C) } }
        // Root leaf is A (follows `first` chain to the bottom).
        let node = WorkspacePaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Pane {
                cwd: "/a".into(),
                draft: "ls".into(),
            }),
            second: Box::new(WorkspacePaneNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/b".into(),
                    draft: "build".into(),
                }),
                second: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/c".into(),
                    draft: String::new(),
                }),
            }),
        };
        assert_eq!(root_leaf_cwd(&node).as_deref(), Some("/a"));
        assert_eq!(root_leaf_draft(&node), "ls");
    }

    #[test]
    fn root_leaf_cwd_empty_returns_none() {
        let node = WorkspacePaneNode::Pane {
            cwd: "".into(),
            draft: String::new(),
        };
        assert_eq!(root_leaf_cwd(&node), None);
    }

    #[test]
    fn build_subtree_single_pane_is_noop() {
        // A single Pane node should not split — the pane already exists.
        use crate::pane::Pane;
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let pane_id = tab.active_pane_id();
        let node = WorkspacePaneNode::Pane {
            cwd: "/tmp".into(),
            draft: "hello".into(),
        };
        let mut split_fn =
            |tab: &mut Tab, _leaf: PaneId, dir: SplitDirection, ratio: f32, _cwd: &str| {
                let new_pane = Pane::with_terminal_only(1000);
                tab.split_pane_with_pane(_leaf, dir, ratio, new_pane).ok()
            };
        build_subtree(&mut tab, &node, pane_id, &mut split_fn);
        // Tree should still have one pane.
        assert_eq!(tab.pane_count(), 1);
    }

    #[test]
    fn build_subtree_two_pane_split_creates_second_pane() {
        use crate::pane::Pane;
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let pane_id = tab.active_pane_id();
        let node = WorkspacePaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Pane {
                cwd: "/a".into(),
                draft: String::new(),
            }),
            second: Box::new(WorkspacePaneNode::Pane {
                cwd: "/b".into(),
                draft: "build".into(),
            }),
        };
        let mut split_fn =
            |tab: &mut Tab, leaf: PaneId, dir: SplitDirection, ratio: f32, _cwd: &str| {
                let new_pane = Pane::with_terminal_only(1000);
                tab.split_pane_with_pane(leaf, dir, ratio, new_pane).ok()
            };
        build_subtree(&mut tab, &node, pane_id, &mut split_fn);
        assert_eq!(tab.pane_count(), 2);
        // The new pane should have the draft "build".
        let panes = tab.split_tree().panes();
        assert_eq!(panes.len(), 2);
        // The second pane (new) should have the draft.
        let second_pane = tab.pane(panes[1]).unwrap();
        let terminal = second_pane.terminal.as_ref().unwrap();
        assert_eq!(terminal.editor().text(), "build");
    }

    #[test]
    fn build_subtree_nested_split_creates_three_panes() {
        // Split { first: Pane(A), second: Split { first: Pane(B), second: Pane(C) } }
        // Root leaf A is split to add B, then B is split to add C.
        use crate::pane::Pane;
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let pane_id = tab.active_pane_id();
        let node = WorkspacePaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Pane {
                cwd: "/a".into(),
                draft: "ls".into(),
            }),
            second: Box::new(WorkspacePaneNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/b".into(),
                    draft: "build".into(),
                }),
                second: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/c".into(),
                    draft: "test".into(),
                }),
            }),
        };
        let mut split_fn =
            |tab: &mut Tab, leaf: PaneId, dir: SplitDirection, ratio: f32, _cwd: &str| {
                let new_pane = Pane::with_terminal_only(1000);
                tab.split_pane_with_pane(leaf, dir, ratio, new_pane).ok()
            };
        build_subtree(&mut tab, &node, pane_id, &mut split_fn);
        assert_eq!(tab.pane_count(), 3);
        // Verify drafts.
        let panes = tab.split_tree().panes();
        assert_eq!(panes.len(), 3);
        // Pane 0 is the root (A) — its draft was set by restore_tab, not
        // build_subtree. Skip checking it here.
        // Pane 1 is B — draft "build".
        let pane_b = tab.pane(panes[1]).unwrap();
        assert_eq!(pane_b.terminal.as_ref().unwrap().editor().text(), "build");
        // Pane 2 is C — draft "test".
        let pane_c = tab.pane(panes[2]).unwrap();
        assert_eq!(pane_c.terminal.as_ref().unwrap().editor().text(), "test");
    }

    #[test]
    fn build_subtree_first_child_is_split_creates_correct_topology() {
        // Split { first: Split { first: Pane(A), second: Pane(B) }, second: Pane(C) }
        // Root leaf A is split to add B (inside first child), then A is split
        // again to add C. This tests the fix for the bug where splitting the
        // active pane (B) instead of the root leaf (A) would produce the wrong
        // topology.
        use crate::pane::Pane;
        let mut tab = Tab::with_single_pane(Pane::with_terminal_only(1000));
        let pane_id = tab.active_pane_id();
        let node = WorkspacePaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/a".into(),
                    draft: "ls".into(),
                }),
                second: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/b".into(),
                    draft: "build".into(),
                }),
            }),
            second: Box::new(WorkspacePaneNode::Pane {
                cwd: "/c".into(),
                draft: "test".into(),
            }),
        };
        let mut split_fn =
            |tab: &mut Tab, leaf: PaneId, dir: SplitDirection, ratio: f32, _cwd: &str| {
                let new_pane = Pane::with_terminal_only(1000);
                tab.split_pane_with_pane(leaf, dir, ratio, new_pane).ok()
            };
        build_subtree(&mut tab, &node, pane_id, &mut split_fn);
        assert_eq!(tab.pane_count(), 3);
        // Verify drafts: B should have "build", C should have "test".
        let panes = tab.split_tree().panes();
        assert_eq!(panes.len(), 3);
        // Pane 0 is A (root), Pane 1 is B, Pane 2 is C.
        let pane_b = tab.pane(panes[1]).unwrap();
        assert_eq!(pane_b.terminal.as_ref().unwrap().editor().text(), "build");
        let pane_c = tab.pane(panes[2]).unwrap();
        assert_eq!(pane_c.terminal.as_ref().unwrap().editor().text(), "test");
    }

    #[test]
    fn workspace_restore_error_displays_nicely() {
        let e = WorkspaceRestoreError::NoTabs;
        assert_eq!(format!("{e}"), "workspace document has no tabs");
        let e = WorkspaceRestoreError::ProfileNotFound {
            profile: "dark".into(),
        };
        assert_eq!(
            format!("{e}"),
            "workspace profile 'dark' not found, using base"
        );
    }

    #[test]
    fn workspace_interaction_error_cancelled_displays() {
        let e = WorkspaceInteractionError::Cancelled;
        assert_eq!(format!("{e}"), "workspace file panel cancelled");
    }
}
