//! v1.6.2: Workspace document DTO — stable, serializable description of a
//! terminal session's layout (tabs, pane split tree, CWD, editor draft,
//! profile, window size).
//!
//! The DTO is intentionally separated from runtime objects (`Pane`, `Terminal`,
//! `PTY`, `BlockTracker`) — those cannot be serialized. Instead, each pane
//! leaf carries only `cwd` + `draft`, which is enough to recreate a shell in
//! the same directory with the same editor content.
//!
//! ## Storage
//!
//! - Project workspace: explicit `.weft/workspace.yaml` in the project root.
//! - Personal workspace: `~/Library/Application Support/Weft/workspaces/<name>.yaml`.
//! - Crash snapshots (v1.6.3) use a separate `RecoverySnapshot` type, not
//!   this DTO, so auto-saves can never overwrite user-authored files.
//!
//! All writes use the atomic pattern: write to `<path>.tmp` → `fsync` →
//! rename to `<path>`. A corrupt main file falls back to the last-known-good
//! copy (kept as `<path>.bak`).
//!
//! ## Versioning
//!
//! `version` is a monotonically increasing integer. Unknown future versions
//! are rejected with [`WorkspaceError::UnsupportedVersion`]; the caller can
//! then prompt the user to upgrade. Old versions are migrated forward by
//! [`WorkspaceDocument::migrate`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Current workspace document version. Increment on breaking schema changes.
pub const WORKSPACE_VERSION: u32 = 1;

/// Maximum number of tabs in a workspace. Prevents pathological files from
/// creating thousands of tabs on restore.
pub const MAX_WORKSPACE_TABS: usize = 64;

/// Maximum number of panes per tab. Matches the practical limit of nested
/// splits before the layout becomes unusable.
pub const MAX_WORKSPACE_PANES_PER_TAB: usize = 16;

/// Maximum total editor draft text across all panes (1 MiB). Prevents
/// pathological files from loading megabytes of draft text into memory.
pub const MAX_WORKSPACE_DRAFT_BYTES: usize = 1024 * 1024;

/// Minimum window dimensions in logical pixels. Prevents degenerate
/// workspace files from shrinking the window to near-zero.
pub const MIN_WORKSPACE_WINDOW_DIM: u32 = 100;

/// Maximum window dimensions in logical pixels. Prevents pathological
/// workspace files from requesting a window larger than the display.
pub const MAX_WORKSPACE_WINDOW_DIM: u32 = 16384;

// ── DTO types ───────────────────────────────────────────────────────────

/// Root workspace document. Serialized as YAML.
///
/// # Example YAML
///
/// ```yaml
/// version: 1
/// name: my-project
/// profile: dark
/// window:
///   width: 1200
///   height: 800
/// active_tab: 0
/// tabs:
///   - panes:
///       Pane:
///         cwd: /home/user/project
///         draft: ""
///     active_pane_index: 0
///   - panes:
///       Split:
///         direction: Vertical
///         ratio: 0.5
///         first:
///           Pane:
///             cwd: /home/user/project/src
///             draft: ""
///         second:
///           Pane:
///             cwd: /home/user/project/docs
///             draft: "cargo build"
///     active_pane_index: 0
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDocument {
    /// Schema version. Must be `<= WORKSPACE_VERSION`.
    pub version: u32,
    /// Human-readable workspace name (shown in palette / window title).
    pub name: String,
    /// Profile name to activate on restore. `None` = use base config.
    /// If the profile doesn't exist, restore falls back to base + diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Window dimensions in logical pixels.
    pub window: WorkspaceWindow,
    /// Tabs in display order (left to right).
    pub tabs: Vec<WorkspaceTab>,
    /// Index into `tabs` of the active tab at save time.
    #[serde(default)]
    pub active_tab: usize,
}

/// Window size at save time. Used to restore the window to the same
/// dimensions. In logical (non-scaled) pixels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWindow {
    pub width: u32,
    pub height: u32,
}

/// A single tab's workspace state. The pane split tree is the key structural
/// data — it mirrors [`SplitTree`](crate::pane_layout::SplitTree) but with
/// `cwd` + `draft` at each leaf instead of a `PaneId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceTab {
    /// Pane split tree. A single-pane tab has `WorkspacePaneNode::Pane`.
    pub panes: WorkspacePaneNode,
    /// Index of the active pane in DFS traversal order of `panes`.
    /// 0 for a single-pane tab.
    #[serde(default)]
    pub active_pane_index: usize,
}

/// Recursive pane node. Mirrors the private `Node` enum in `pane_layout.rs`
/// but carries restorable data (`cwd`, `draft`) instead of runtime ids.
///
/// Serialized with internally-tagged enum so the YAML is human-readable:
///
/// ```yaml
/// Pane:
///   cwd: /home/user
///   draft: ""
/// ```
///
/// ```yaml
/// Split:
///   direction: Vertical
///   ratio: 0.5
///   first:
///     Pane: { ... }
///   second:
///     Pane: { ... }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum WorkspacePaneNode {
    /// A single pane (leaf of the split tree).
    Pane {
        /// Working directory to spawn the shell in. If the directory doesn't
        /// exist at restore time, falls back to `$HOME`.
        cwd: PathBuf,
        /// Editor draft text (prompt input). Inserted into the editor but
        /// NEVER auto-executed — the user must press Enter to run it.
        #[serde(default)]
        draft: String,
    },
    /// A split between two sub-trees.
    Split {
        direction: SplitDirection,
        /// Ratio of the split. Clamped to `[0.1, 0.9]` on load.
        ratio: f32,
        first: Box<WorkspacePaneNode>,
        second: Box<WorkspacePaneNode>,
    },
}

/// Re-exported from `pane_layout` so consumers don't need a separate import.
/// Serialized as a plain string ("Horizontal" / "Vertical").
pub use crate::pane_layout::SplitDirection;

// ── Errors ──────────────────────────────────────────────────────────────

/// Errors raised by workspace load/save/validate operations.
#[derive(Debug)]
pub enum WorkspaceError {
    /// File I/O error (read or write).
    Io(std::io::Error),
    /// YAML serialization/deserialization error.
    Yaml(String),
    /// Schema version is newer than `WORKSPACE_VERSION`.
    UnsupportedVersion { found: u32, max: u32 },
    /// Structural validation failed (too many tabs, bad ratio, etc.).
    Validation(String),
    /// Total draft text exceeds `MAX_WORKSPACE_DRAFT_BYTES`.
    DraftTooLarge { bytes: usize, max: usize },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "workspace I/O error: {e}"),
            Self::Yaml(s) => write!(f, "workspace YAML error: {s}"),
            Self::UnsupportedVersion { found, max } => {
                write!(f, "workspace version {found} exceeds supported max {max}")
            }
            Self::Validation(s) => write!(f, "workspace validation error: {s}"),
            Self::DraftTooLarge { bytes, max } => {
                write!(f, "workspace draft text {bytes} bytes exceeds limit {max}")
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<std::io::Error> for WorkspaceError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_yaml_ng::Error> for WorkspaceError {
    fn from(e: serde_yaml_ng::Error) -> Self {
        Self::Yaml(e.to_string())
    }
}

// ── WorkspacePaneNode methods ───────────────────────────────────────────

impl WorkspacePaneNode {
    /// Count leaf panes in this sub-tree.
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Collect all pane leaves in DFS order (first child before second).
    /// Returns `(cwd, draft)` pairs.
    pub fn collect_panes(&self) -> Vec<(&Path, &str)> {
        let mut out = Vec::new();
        self.collect_panes_into(&mut out);
        out
    }

    fn collect_panes_into<'a>(&'a self, out: &mut Vec<(&'a Path, &'a str)>) {
        match self {
            Self::Pane { cwd, draft } => out.push((cwd.as_path(), draft.as_str())),
            Self::Split { first, second, .. } => {
                first.collect_panes_into(out);
                second.collect_panes_into(out);
            }
        }
    }

    /// Total byte size of all draft strings in this sub-tree.
    pub fn draft_byte_size(&self) -> usize {
        match self {
            Self::Pane { draft, .. } => draft.len(),
            Self::Split { first, second, .. } => first.draft_byte_size() + second.draft_byte_size(),
        }
    }

    /// Clamp all split ratios to `[0.1, 0.9]` in place.
    fn clamp_ratios(&mut self) {
        if let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            *ratio = ratio.clamp(0.1, 0.9);
            first.clamp_ratios();
            second.clamp_ratios();
        }
    }
}

// ── WorkspaceDocument methods ───────────────────────────────────────────

impl WorkspaceDocument {
    /// Create a new minimal workspace document with one tab, one pane.
    pub fn new(name: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            version: WORKSPACE_VERSION,
            name: name.into(),
            profile: None,
            window: WorkspaceWindow {
                width: 1200,
                height: 800,
            },
            tabs: vec![WorkspaceTab {
                panes: WorkspacePaneNode::Pane {
                    cwd,
                    draft: String::new(),
                },
                active_pane_index: 0,
            }],
            active_tab: 0,
        }
    }

    /// Serialize to YAML string.
    pub fn to_yaml(&self) -> Result<String, WorkspaceError> {
        serde_yaml_ng::to_string(self).map_err(WorkspaceError::from)
    }

    /// Deserialize from YAML string, then validate.
    pub fn from_yaml(yaml: &str) -> Result<Self, WorkspaceError> {
        let doc: Self = serde_yaml_ng::from_str(yaml).map_err(WorkspaceError::from)?;
        doc.validate()?;
        Ok(doc)
    }

    /// Validate the document's structure and constraints.
    ///
    /// - Version must be `<= WORKSPACE_VERSION`.
    /// - Tabs must not be empty and `<= MAX_WORKSPACE_TABS`.
    /// - Each tab's pane count must be `<= MAX_WORKSPACE_PANES_PER_TAB`.
    /// - `active_tab` must be in bounds.
    /// - `active_pane_index` must be in bounds for each tab.
    /// - Split ratios are clamped to `[0.1, 0.9]`.
    /// - Total draft text must be `<= MAX_WORKSPACE_DRAFT_BYTES`.
    /// - Window dimensions must be within `[MIN_WORKSPACE_WINDOW_DIM,
    ///   MAX_WORKSPACE_WINDOW_DIM]` (v1.6.2 review M6).
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if self.version > WORKSPACE_VERSION {
            return Err(WorkspaceError::UnsupportedVersion {
                found: self.version,
                max: WORKSPACE_VERSION,
            });
        }
        if self.name.trim().is_empty() {
            return Err(WorkspaceError::Validation(
                "workspace name must not be empty".into(),
            ));
        }
        if self.tabs.is_empty() {
            return Err(WorkspaceError::Validation(
                "workspace must have at least one tab".into(),
            ));
        }
        if self.tabs.len() > MAX_WORKSPACE_TABS {
            return Err(WorkspaceError::Validation(format!(
                "workspace has {} tabs, max is {}",
                self.tabs.len(),
                MAX_WORKSPACE_TABS
            )));
        }
        if self.active_tab >= self.tabs.len() {
            return Err(WorkspaceError::Validation(format!(
                "active_tab {} out of bounds (have {} tabs)",
                self.active_tab,
                self.tabs.len()
            )));
        }
        // v1.6.2 review M6: validate window dimensions to prevent
        // degenerate workspace files from shrinking or exploding the
        // window on restore.
        if self.window.width < MIN_WORKSPACE_WINDOW_DIM
            || self.window.height < MIN_WORKSPACE_WINDOW_DIM
        {
            return Err(WorkspaceError::Validation(format!(
                "window dimensions {}x{} below minimum {}x{}",
                self.window.width,
                self.window.height,
                MIN_WORKSPACE_WINDOW_DIM,
                MIN_WORKSPACE_WINDOW_DIM
            )));
        }
        if self.window.width > MAX_WORKSPACE_WINDOW_DIM
            || self.window.height > MAX_WORKSPACE_WINDOW_DIM
        {
            return Err(WorkspaceError::Validation(format!(
                "window dimensions {}x{} exceed maximum {}x{}",
                self.window.width,
                self.window.height,
                MAX_WORKSPACE_WINDOW_DIM,
                MAX_WORKSPACE_WINDOW_DIM
            )));
        }
        let mut total_draft = 0usize;
        for (i, tab) in self.tabs.iter().enumerate() {
            let pane_count = tab.panes.pane_count();
            if pane_count == 0 {
                return Err(WorkspaceError::Validation(format!("tab {i} has no panes")));
            }
            if pane_count > MAX_WORKSPACE_PANES_PER_TAB {
                return Err(WorkspaceError::Validation(format!(
                    "tab {i} has {pane_count} panes, max is {MAX_WORKSPACE_PANES_PER_TAB}"
                )));
            }
            if tab.active_pane_index >= pane_count {
                return Err(WorkspaceError::Validation(format!(
                    "tab {i} active_pane_index {} out of bounds (have {pane_count} panes)",
                    tab.active_pane_index
                )));
            }
            total_draft += tab.panes.draft_byte_size();
        }
        if total_draft > MAX_WORKSPACE_DRAFT_BYTES {
            return Err(WorkspaceError::DraftTooLarge {
                bytes: total_draft,
                max: MAX_WORKSPACE_DRAFT_BYTES,
            });
        }
        Ok(())
    }

    /// Load from a YAML file, with last-known-good fallback.
    ///
    /// If the main file fails to parse, attempts `<path>.bak`. If both fail,
    /// returns the error from the main file.
    pub fn load(path: &Path) -> Result<Self, WorkspaceError> {
        match Self::load_file(path) {
            Ok(doc) => Ok(doc),
            Err(e) => {
                let bak = path.with_extension("yaml.bak");
                if bak.exists() {
                    tracing::warn!(
                        main = %path.display(),
                        bak = %bak.display(),
                        error = %e,
                        "workspace load failed, falling back to .bak"
                    );
                    Self::load_file(&bak)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn load_file(path: &Path) -> Result<Self, WorkspaceError> {
        let yaml = std::fs::read_to_string(path)?;
        let mut doc = Self::from_yaml(&yaml)?;
        // Clamp ratios on load so out-of-range values from hand-edited YAML
        // don't produce degenerate layouts.
        for tab in &mut doc.tabs {
            tab.panes.clamp_ratios();
        }
        Ok(doc)
    }

    /// Save to a YAML file atomically (temp → fsync → rename).
    ///
    /// Before writing, the current file (if any) is backed up to `<path>.bak`.
    /// The temp file is created in the same directory so the rename is atomic
    /// on the same filesystem.
    pub fn save(&self, path: &Path) -> Result<(), WorkspaceError> {
        self.validate()?;
        let yaml = self.to_yaml()?;

        // Back up existing file (if any) to <path>.bak.
        if path.exists() {
            let bak = path.with_extension("yaml.bak");
            if let Err(e) = std::fs::rename(path, &bak) {
                // rename may fail across filesystems; fall back to copy.
                if let Err(copy_err) = std::fs::copy(path, &bak) {
                    tracing::warn!(
                        bak = %bak.display(),
                        rename_err = %e,
                        copy_err = %copy_err,
                        "failed to back up workspace file"
                    );
                }
            }
        }

        // Write to temp file in the same directory.
        let tmp = path.with_extension("yaml.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            use std::io::Write;
            file.write_all(yaml.as_bytes())?;
            file.sync_all()?;
        }

        // Atomic rename.
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Migrate a document from an older version to the current one.
    ///
    /// Currently only version 1 exists, so this is a no-op. Future versions
    /// will add migration steps here.
    pub fn migrate(mut doc: Self) -> Result<Self, WorkspaceError> {
        if doc.version > WORKSPACE_VERSION {
            return Err(WorkspaceError::UnsupportedVersion {
                found: doc.version,
                max: WORKSPACE_VERSION,
            });
        }
        // v1 is the current version — no migration needed.
        doc.version = WORKSPACE_VERSION;
        for tab in &mut doc.tabs {
            tab.panes.clamp_ratios();
        }
        Ok(doc)
    }

    /// Helper for tests: load from a YAML string (not a file).
    #[cfg(test)]
    fn load_from_str(yaml: &str) -> Result<Self, WorkspaceError> {
        let mut doc = Self::from_yaml(yaml)?;
        for tab in &mut doc.tabs {
            tab.panes.clamp_ratios();
        }
        Ok(doc)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> WorkspaceDocument {
        WorkspaceDocument {
            version: 1,
            name: "test-project".into(),
            profile: Some("dark".into()),
            window: WorkspaceWindow {
                width: 1200,
                height: 800,
            },
            active_tab: 0,
            tabs: vec![
                WorkspaceTab {
                    panes: WorkspacePaneNode::Split {
                        direction: SplitDirection::Vertical,
                        ratio: 0.5,
                        first: Box::new(WorkspacePaneNode::Pane {
                            cwd: "/home/user/src".into(),
                            draft: String::new(),
                        }),
                        second: Box::new(WorkspacePaneNode::Pane {
                            cwd: "/home/user/docs".into(),
                            draft: "cargo build".into(),
                        }),
                    },
                    active_pane_index: 0,
                },
                WorkspaceTab {
                    panes: WorkspacePaneNode::Pane {
                        cwd: "/home/user".into(),
                        draft: String::new(),
                    },
                    active_pane_index: 0,
                },
            ],
        }
    }

    #[test]
    fn round_trip_yaml_serialization() {
        let doc = sample_doc();
        let yaml = doc.to_yaml().unwrap();
        let loaded = WorkspaceDocument::from_yaml(&yaml).unwrap();
        assert_eq!(doc, loaded);
    }

    #[test]
    fn round_trip_file_save_load() {
        let dir = std::env::temp_dir().join(format!(
            "weft-workspace-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.yaml");

        let doc = sample_doc();
        doc.save(&path).unwrap();
        let loaded = WorkspaceDocument::load(&path).unwrap();
        assert_eq!(doc, loaded);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_version_rejected() {
        let yaml = "version: 999\nname: bad\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(
            err,
            WorkspaceError::UnsupportedVersion { found: 999, max: 1 }
        ));
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        // No `profile`, no `active_tab` — should default to None / 0.
        let yaml = "version: 1\nname: minimal\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
        let doc = WorkspaceDocument::from_yaml(yaml).unwrap();
        assert_eq!(doc.profile, None);
        assert_eq!(doc.active_tab, 0);
    }

    #[test]
    fn empty_tabs_rejected() {
        let yaml = "version: 1\nname: empty\nwindow:\n  width: 200\n  height: 200\ntabs: []\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::Validation(_)));
    }

    #[test]
    fn active_tab_out_of_bounds_rejected() {
        let yaml = "version: 1\nname: bad\nwindow:\n  width: 200\n  height: 200\nactive_tab: 5\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::Validation(_)));
    }

    #[test]
    fn active_pane_index_out_of_bounds_rejected() {
        let yaml = "version: 1\nname: bad\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 5\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::Validation(_)));
    }

    #[test]
    fn nested_split_round_trips() {
        let doc = WorkspaceDocument {
            version: 1,
            name: "nested".into(),
            profile: None,
            window: WorkspaceWindow {
                width: 800,
                height: 600,
            },
            active_tab: 0,
            tabs: vec![WorkspaceTab {
                panes: WorkspacePaneNode::Split {
                    direction: SplitDirection::Horizontal,
                    ratio: 0.6,
                    first: Box::new(WorkspacePaneNode::Pane {
                        cwd: "/a".into(),
                        draft: "ls".into(),
                    }),
                    second: Box::new(WorkspacePaneNode::Split {
                        direction: SplitDirection::Vertical,
                        ratio: 0.5,
                        first: Box::new(WorkspacePaneNode::Pane {
                            cwd: "/b".into(),
                            draft: String::new(),
                        }),
                        second: Box::new(WorkspacePaneNode::Pane {
                            cwd: "/c".into(),
                            draft: String::new(),
                        }),
                    }),
                },
                active_pane_index: 1,
            }],
        };
        let yaml = doc.to_yaml().unwrap();
        let loaded = WorkspaceDocument::from_yaml(&yaml).unwrap();
        assert_eq!(doc, loaded);
        assert_eq!(loaded.tabs[0].panes.pane_count(), 3);
    }

    #[test]
    fn out_of_range_ratio_clamped_on_load() {
        let yaml = "version: 1\nname: clamp\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Split\n      direction: Vertical\n      ratio: 5.0\n      first:\n        kind: Pane\n        cwd: /a\n        draft: \"\"\n      second:\n        kind: Pane\n        cwd: /b\n        draft: \"\"\n    active_pane_index: 0\n";
        let doc = WorkspaceDocument::load_from_str(yaml).unwrap();
        // The ratio should be clamped to 0.9.
        match &doc.tabs[0].panes {
            WorkspacePaneNode::Split { ratio, .. } => assert!((ratio - 0.9).abs() < 0.001),
            _ => panic!("expected Split"),
        }
    }

    #[test]
    fn draft_too_large_rejected() {
        let big = "x".repeat(MAX_WORKSPACE_DRAFT_BYTES + 1);
        let yaml = format!(
            "version: 1\nname: big\nwindow:\n  width: 200\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"{big}\"\n    active_pane_index: 0\n"
        );
        let err = WorkspaceDocument::from_yaml(&yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::DraftTooLarge { .. }));
    }

    #[test]
    fn corrupt_file_falls_back_to_bak() {
        let dir = std::env::temp_dir().join(format!(
            "weft-workspace-bak-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.yaml");
        let bak = dir.join("workspace.yaml.bak");

        // Save a good doc, then corrupt the main file.
        let doc = sample_doc();
        doc.save(&path).unwrap();
        // The save backed up the (non-existent) prior file, so .bak may not
        // exist. Create it by copying the current file, then corrupt main.
        std::fs::copy(&path, &bak).unwrap();
        std::fs::write(&path, "corrupted yaml {{{{").unwrap();

        // Load should fall back to .bak.
        let loaded = WorkspaceDocument::load(&path).unwrap();
        assert_eq!(loaded, doc);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn collect_panes_dfs_order() {
        let node = WorkspacePaneNode::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(WorkspacePaneNode::Pane {
                cwd: "/a".into(),
                draft: "1".into(),
            }),
            second: Box::new(WorkspacePaneNode::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/b".into(),
                    draft: "2".into(),
                }),
                second: Box::new(WorkspacePaneNode::Pane {
                    cwd: "/c".into(),
                    draft: "3".into(),
                }),
            }),
        };
        let panes = node.collect_panes();
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].0, Path::new("/a"));
        assert_eq!(panes[1].0, Path::new("/b"));
        assert_eq!(panes[2].0, Path::new("/c"));
    }

    #[test]
    fn migrate_noop_for_v1() {
        let doc = sample_doc();
        let migrated = WorkspaceDocument::migrate(doc.clone()).unwrap();
        assert_eq!(doc, migrated);
    }

    #[test]
    fn window_too_small_rejected() {
        let yaml = "version: 1\nname: small\nwindow:\n  width: 50\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::Validation(_)));
    }

    #[test]
    fn window_too_large_rejected() {
        let yaml = "version: 1\nname: huge\nwindow:\n  width: 99999\n  height: 200\ntabs:\n  - panes:\n      kind: Pane\n      cwd: /tmp\n      draft: \"\"\n    active_pane_index: 0\n";
        let err = WorkspaceDocument::from_yaml(yaml).unwrap_err();
        assert!(matches!(err, WorkspaceError::Validation(_)));
    }
}
