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

use crate::palette_search_worker::index_workspace_document;
use crate::pane::Pane;
use crate::tab::Tab;
use crate::App;

#[path = "workspace_profile.rs"]
mod workspace_profile;
pub(crate) use workspace_profile::WorkspaceRestoreOutcome;
use workspace_profile::{profile_restore_target, ProfileRestoreTarget, WorkspaceRestoreWarning};

// v1.12.28 (P1-02 ⑤): the recursive split-tree rebuild moved behind a
// payload-generic trait (`workspace_pane_rebuild.rs`) so the SQLite
// split-persistence restore reuses the exact topology algorithm instead of
// copy-pasting it (拓扑陷阱: the split-before-descend order is load-bearing).
// This file is at its zero-margin allowlist ceiling — the extraction is net
// negative here.
#[path = "workspace_pane_rebuild.rs"]
mod pane_rebuild;
pub(crate) use pane_rebuild::{build_subtree, PaneTreeRebuild};

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
            .with_terminal(|t| t.cwd().map(str::to_owned))
            .flatten()
            .or_else(|| pane.restored_cwd_fallback().map(str::to_owned))
            .map(PathBuf::from);
        let draft = pane
            .with_terminal(|t| t.editor().text())
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
    ///
    /// A missing profile is reported in [`WorkspaceRestoreOutcome`] after
    /// the workspace has been restored with the base profile.
    pub(super) fn restore_workspace(
        &mut self,
        doc: &WorkspaceDocument,
    ) -> Result<WorkspaceRestoreOutcome, WorkspaceRestoreError> {
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
        let active_profile = self.active_profile_name().map(str::to_owned);
        let requested_profile = doc.profile.as_deref().filter(|name| !name.is_empty());
        let requested_exists = requested_profile.is_some_and(|requested| {
            self.profile_names_sorted()
                .iter()
                .any(|available| available == requested)
        });
        let profile_target = profile_restore_target(
            requested_profile,
            active_profile.as_deref(),
            requested_exists,
        );
        let profile_warning = match profile_target {
            ProfileRestoreTarget::Keep => None,
            ProfileRestoreTarget::Base => {
                self.switch_profile(None)
                    .map_err(|e| WorkspaceRestoreError::ProfileSwitch {
                        profile: None,
                        message: e.to_string(),
                    })?;
                None
            }
            ProfileRestoreTarget::Named(profile) => {
                self.switch_profile(Some(&profile)).map_err(|e| {
                    WorkspaceRestoreError::ProfileSwitch {
                        profile: Some(profile),
                        message: e.to_string(),
                    }
                })?;
                None
            }
            ProfileRestoreTarget::MissingUseBase(profile) => {
                self.switch_profile(None)
                    .map_err(|e| WorkspaceRestoreError::ProfileSwitch {
                        profile: None,
                        message: e.to_string(),
                    })?;
                Some(WorkspaceRestoreWarning::ProfileNotFound { profile })
            }
            ProfileRestoreTarget::MissingAlreadyBase(profile) => {
                Some(WorkspaceRestoreWarning::ProfileNotFound { profile })
            }
        };

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

        Ok(WorkspaceRestoreOutcome::new(profile_warning))
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
        // v1.11.2 X4: retention cap for the restored tab and its split panes.
        let blocks_limit = self.config_state.config.blocks.retained_limit;

        // Find the root leaf's cwd to open the initial tab.
        let root_cwd = root_leaf_cwd(panes);
        let root_draft = root_leaf_draft(panes);

        // Open the tab with the root leaf's cwd.
        let tab_idx =
            self.sessions
                .open_tab(rows, cols, scrollback, &self.proxy, root_cwd.as_deref());
        // v1.11.2 X4: propagate the block retention cap to the restored tab.
        // PLAN_v11217 §3.5 (T4): the configured output cap rides the same walk.
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            crate::config_controller::apply_blocks_retained_limit(tab, blocks_limit);
            crate::config_controller::apply_blocks_output_cap(
                tab,
                self.config_state.config.blocks.output_cap_mib,
            );
            // v1.11.7 (P2-3): inject the user's TUI render tier.
            crate::config_controller::apply_tui_render_mode(
                tab,
                self.config_state.config.experimental.tui_render_mode,
            );
        }

        // Apply block_id_allocator + palette (mirrors new_tab).
        if let Some(block_id_allocator) = self
            .sessions
            .block_store()
            .map(weft_core::persistence::BlockStore::block_id_allocator)
        {
            if let Some(mut terminal) = self
                .sessions
                .tab_mut(tab_idx)
                .and_then(|tab| tab.lock_terminal())
            {
                terminal
                    .block_tracker_mut()
                    .use_shared_id_allocator(block_id_allocator);
            }
        }
        if let Some(mut t) = self
            .sessions
            .tab_mut(tab_idx)
            .and_then(|tab| tab.lock_terminal())
        {
            if let Some(r) = &self.renderer {
                t.set_palette(r.theme().palette);
                t.set_background_color(r.theme().background);
            }
        }

        // Set the root leaf's editor draft.
        if !root_draft.is_empty() {
            if let Some(tab) = self.sessions.tab_mut(tab_idx) {
                tab.with_terminal(|terminal| terminal.editor_mut().buffer.set_text(&root_draft));
            }
        }

        // v1.6.2 review M5: set the root pane's cwd fallback (`restored_cwd`) so
        // `terminal.cwd()` falls back to the saved cwd before OSC 7 is
        // reported by the freshly-spawned shell. Non-root panes get their
        // fallback set in `build_subtree`. This only writes the cwd
        // fallback — the real TabSnapshot is attached separately by
        // `attach_recovery_tab_snapshots` (v1.10.24 B1).
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            let root_pane_id = tab.active_pane_id();
            if let Some(pane) = tab.pane_mut(root_pane_id) {
                pane.set_restored_cwd_fallback(root_cwd.clone());
            }
        }

        // Recursively rebuild the split tree. The initial pane's id is
        // the tab's active pane id (just created).
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            let initial_pane_id = tab.active_pane_id();
            let proxy = self.proxy.clone();
            let mut split_fn =
                |tab: &mut Tab, leaf: PaneId, dir: SplitDirection, ratio: f32, cwd: &str| {
                    let mut new_pane = Pane::spawn(rows, cols, scrollback, &proxy, Some(cwd));
                    // v1.11.2 X4: retention cap on workspace-restored splits.
                    new_pane.set_blocks_retained_limit(blocks_limit);
                    new_pane.set_blocks_output_cap(crate::config_controller::output_cap_bytes(
                        self.config_state.config.blocks.output_cap_mib,
                    ));
                    // v1.11.7 (P2-3): TUI render tier on restored splits.
                    new_pane
                        .set_tui_render_mode(self.config_state.config.experimental.tui_render_mode);
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
        if let Some(tab) = self.sessions.tab_mut(tab_idx) {
            let panes_list = tab.split_tree().panes();
            if let Some(target_id) = pane_id_at_index(&panes_list, active_pane_index) {
                if let Err(e) = tab.set_active_pane(target_id) {
                    warn!(
                        ?e,
                        index = active_pane_index,
                        "workspace restore: failed to set active pane"
                    );
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
        index_workspace_document(self.search_index.as_ref(), &path, &doc);
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
        let outcome = self
            .restore_workspace_with_confirmation(&doc)
            .map_err(|e| WorkspaceInteractionError::Restore {
                source: e,
                path: path.clone(),
            })?
            .ok_or(WorkspaceInteractionError::Cancelled)?;
        index_workspace_document(self.search_index.as_ref(), &path, &doc);
        if let Some(warning) = outcome.warning() {
            warn!(warning = %warning, path = %path.display(), "workspace restored with warning");
            self.surface_config_error(&warning.to_string());
        }
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

/// Find the root leaf's cwd (the leftmost-topmost pane's cwd). Used to
/// open the initial tab (and, with a `"/"` fallback, to spawn the second
/// child's pane in `pane_rebuild::build_subtree`).
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

fn pane_id_at_index(panes: &[PaneId], index: usize) -> Option<PaneId> {
    panes.get(index).copied()
}

// ── Errors ─────────────────────────────────────────────────────────────

/// Errors raised during workspace restore (DTO → runtime).
#[derive(Debug)]
pub enum WorkspaceRestoreError {
    /// The document has no tabs.
    NoTabs,
    /// Switching to the workspace's requested profile failed transactionally.
    ProfileSwitch {
        profile: Option<String>,
        message: String,
    },
}

impl std::fmt::Display for WorkspaceRestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTabs => write!(f, "workspace document has no tabs"),
            Self::ProfileSwitch { profile, message } => match profile {
                Some(profile) => {
                    write!(
                        f,
                        "failed to switch to workspace profile '{profile}': {message}"
                    )
                }
                None => write!(f, "failed to switch to base profile: {message}"),
            },
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
        let terminal = second_pane.lock_terminal().unwrap();
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
        assert_eq!(pane_b.lock_terminal().unwrap().editor().text(), "build");
        assert_eq!(pane_b.restored_cwd.as_deref(), Some("/b"));
        // Pane 2 is C — draft "test".
        let pane_c = tab.pane(panes[2]).unwrap();
        assert_eq!(pane_c.lock_terminal().unwrap().editor().text(), "test");
        assert_eq!(pane_c.restored_cwd.as_deref(), Some("/c"));
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
        assert_eq!(pane_b.lock_terminal().unwrap().editor().text(), "build");
        let pane_c = tab.pane(panes[2]).unwrap();
        assert_eq!(pane_c.lock_terminal().unwrap().editor().text(), "test");
    }

    #[test]
    fn workspace_restore_error_displays_nicely() {
        let e = WorkspaceRestoreError::NoTabs;
        assert_eq!(format!("{e}"), "workspace document has no tabs");
        let e = WorkspaceRestoreError::ProfileSwitch {
            profile: None,
            message: "disk full".into(),
        };
        assert_eq!(
            format!("{e}"),
            "failed to switch to base profile: disk full"
        );
    }

    #[test]
    fn workspace_profile_warning_is_a_success_outcome() {
        let warning = WorkspaceRestoreWarning::ProfileNotFound {
            profile: "dark".into(),
        };
        assert_eq!(
            format!("{warning}"),
            "workspace profile 'dark' not found, using base"
        );
        let outcome = WorkspaceRestoreOutcome::new(Some(warning));
        assert!(matches!(
            outcome.warning(),
            Some(WorkspaceRestoreWarning::ProfileNotFound { profile }) if profile == "dark"
        ));
    }

    #[test]
    fn pane_id_at_index_restores_saved_dfs_focus() {
        let panes = [PaneId(10), PaneId(20), PaneId(30)];
        assert_eq!(pane_id_at_index(&panes, 0), Some(PaneId(10)));
        assert_eq!(pane_id_at_index(&panes, 2), Some(PaneId(30)));
        assert_eq!(pane_id_at_index(&panes, 3), None);
    }

    #[test]
    fn workspace_interaction_error_cancelled_displays() {
        let e = WorkspaceInteractionError::Cancelled;
        assert_eq!(format!("{e}"), "workspace file panel cancelled");
    }
}
