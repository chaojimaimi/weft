//! v1.6.3: Crash recovery snapshot — a debounced, atomically-written
//! capture of the live session state, stored separately from user-authored
//! workspaces so auto-saves can never overwrite user files.
//!
//! ## Lifecycle
//!
//! 1. **During session**: a 1 Hz poller calls [`RecoveryController::write_snapshot_if_changed`]
//!    (app layer). Content-equality debouncing skips writes when nothing
//!    changed since the last successful write.
//! 2. **Clean exit**: the app writes the clean-shutdown marker via
//!    [`RecoveryPaths::write_clean_marker`]. On the next launch, the marker
//!    is detected, the snapshot is deleted, and no recovery prompt is shown.
//! 3. **Unclean exit (crash / kill)**: the marker is absent. On the next
//!    launch, [`RecoveryPaths::clean_marker_exists`] returns `false` and
//!    [`RecoverySnapshot::load`] is attempted. If a snapshot exists, the app
//!    shows a recovery prompt (Restore / Ignore / Delete).
//! 4. **Corrupt snapshot**: [`RecoverySnapshot::load`] falls back to the
//!    `.bak` copy. If both are corrupt, the app proceeds to a blank terminal
//!    with a diagnostic — recovery never blocks normal startup.
//!
//! ## Storage
//!
//! All recovery files live under `<cache_dir>/recovery/`:
//!
//! - `snapshot.yaml` — current snapshot (written debounced during session)
//! - `snapshot.yaml.bak` — last-known-good (kept by the atomic write pattern)
//! - `.clean_shutdown` — marker file (present = last exit was clean)
//!
//! ## Rollback
//!
//! After a restore, the snapshot file is kept as-is. If the user crashes
//! again before any debounced write, they can restore again from the same
//! snapshot. Once a debounced write happens (content changed), the old
//! snapshot moves to `.bak`. On the next clean shutdown, the marker is
//! written; on the subsequent launch, the snapshot is deleted.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::workspace::WorkspaceDocument;

/// Current recovery snapshot version. Increment on breaking schema changes.
pub const RECOVERY_VERSION: u32 = 1;

/// Maximum age of a recovery snapshot before it's considered stale (7 days).
/// Snapshots older than this are deleted on startup without prompting.
pub const MAX_SNAPSHOT_AGE_SECS: u64 = 7 * 24 * 60 * 60;

/// Error type for recovery operations.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml serialization error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("workspace error: {0}")]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error("unsupported recovery version: found {found}, max {max}")]
    UnsupportedVersion { found: u32, max: u32 },
    #[error("snapshot file is corrupt or empty")]
    Corrupt,
}

/// Snapshot of session state for crash recovery. Stored separately from
/// user-authored workspaces (never overwrites user files).
///
/// Wraps a [`WorkspaceDocument`] with recovery-specific metadata: a timestamp
/// and a `clean_shutdown` flag distinguishing "written by clean exit" from
/// "written by debounced mid-session save".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoverySnapshot {
    /// Schema version. Must be `<= RECOVERY_VERSION`.
    pub version: u32,
    /// Unix timestamp (seconds) when the snapshot was written.
    pub created_at: u64,
    /// `true` if the snapshot was written by a clean shutdown path.
    /// `false` if from a debounced mid-session write.
    #[serde(default)]
    pub clean_shutdown: bool,
    /// The captured session state (reuses the Workspace DTO).
    pub workspace: WorkspaceDocument,
}

impl RecoverySnapshot {
    /// Create a new snapshot from a workspace document.
    pub fn from_workspace(ws: WorkspaceDocument, clean_shutdown: bool) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            version: RECOVERY_VERSION,
            created_at,
            clean_shutdown,
            workspace: ws,
        }
    }

    /// Serialize to YAML.
    pub fn to_yaml(&self) -> Result<String, RecoveryError> {
        Ok(serde_yaml::to_string(self)?)
    }

    /// Deserialize from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self, RecoveryError> {
        let snap: Self = serde_yaml::from_str(yaml)?;
        if snap.version > RECOVERY_VERSION {
            return Err(RecoveryError::UnsupportedVersion {
                found: snap.version,
                max: RECOVERY_VERSION,
            });
        }
        Ok(snap)
    }

    /// Check if the snapshot is stale (older than [`MAX_SNAPSHOT_AGE_SECS`]).
    pub fn is_stale(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.created_at) > MAX_SNAPSHOT_AGE_SECS
    }

    /// Save to a YAML file atomically (temp → fsync → rename).
    ///
    /// Before writing, the current file (if any) is backed up to
    /// `<path>.bak`. The temp file is created in the same directory so
    /// the rename is atomic on the same filesystem.
    pub fn save(&self, path: &Path) -> Result<(), RecoveryError> {
        let yaml = self.to_yaml()?;

        // Back up existing file (if any) to <path>.bak.
        if path.exists() {
            let bak = bak_path(path);
            if let Err(e) = std::fs::rename(path, &bak) {
                // rename may fail across filesystems; fall back to copy.
                if let Err(copy_err) = std::fs::copy(path, &bak) {
                    tracing::warn!(
                        bak = %bak.display(),
                        rename_err = %e,
                        copy_err = %copy_err,
                        "failed to back up recovery snapshot"
                    );
                }
            }
        }

        // Write to temp file in the same directory.
        let tmp = tmp_path(path);
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

    /// Load from a YAML file, falling back to `.bak` if the main file is
    /// corrupt or missing.
    pub fn load(path: &Path) -> Result<Self, RecoveryError> {
        match Self::load_file(path) {
            Ok(snap) => Ok(snap),
            Err(e) => {
                let bak = bak_path(path);
                if bak.exists() {
                    tracing::warn!(
                        main = %path.display(),
                        bak = %bak.display(),
                        error = %e,
                        "recovery snapshot load failed, falling back to .bak"
                    );
                    Self::load_file(&bak)
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn load_file(path: &Path) -> Result<Self, RecoveryError> {
        let yaml = std::fs::read_to_string(path)?;
        if yaml.trim().is_empty() {
            return Err(RecoveryError::Corrupt);
        }
        Self::from_yaml(&yaml)
    }

    /// Delete the snapshot file and its `.bak` backup. Idempotent — does
    /// nothing if neither file exists.
    pub fn delete(path: &Path) -> Result<(), RecoveryError> {
        let bak = bak_path(path);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        if bak.exists() {
            std::fs::remove_file(&bak)?;
        }
        Ok(())
    }
}

/// Compute the `.bak` path for a given snapshot path.
fn bak_path(path: &Path) -> PathBuf {
    // For "snapshot.yaml" → "snapshot.yaml.bak"
    // For "foo/bar.yaml" → "foo/bar.yaml.bak"
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// Compute the `.tmp` path for a given snapshot path.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Paths for recovery state. All under `<cache_dir>/recovery/`.
#[derive(Clone, Debug)]
pub struct RecoveryPaths {
    root: PathBuf,
}

impl RecoveryPaths {
    /// Create a new `RecoveryPaths` rooted at `<cache_dir>/recovery/`.
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            root: cache_dir.join("recovery"),
        }
    }

    /// The recovery directory root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the main snapshot file.
    pub fn snapshot_path(&self) -> PathBuf {
        self.root.join("snapshot.yaml")
    }

    /// Path to the `.bak` backup.
    pub fn snapshot_bak_path(&self) -> PathBuf {
        bak_path(&self.snapshot_path())
    }

    /// Path to the clean-shutdown marker.
    pub fn clean_marker_path(&self) -> PathBuf {
        self.root.join(".clean_shutdown")
    }

    /// Ensure the recovery root directory exists.
    pub fn ensure_root(&self) -> Result<(), RecoveryError> {
        std::fs::create_dir_all(&self.root)?;
        Ok(())
    }

    /// Returns `true` if the clean-shutdown marker exists (last session
    /// ended cleanly).
    pub fn clean_marker_exists(&self) -> bool {
        self.clean_marker_path().exists()
    }

    /// Write the clean-shutdown marker. Called from all clean exit paths.
    pub fn write_clean_marker(&self) -> Result<(), RecoveryError> {
        self.ensure_root()?;
        let marker = self.clean_marker_path();
        // Write a timestamp so we can diagnose stale markers.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(&marker, now.to_string())?;
        Ok(())
    }

    /// Remove the clean-shutdown marker. Called at startup after detection.
    pub fn remove_clean_marker(&self) -> Result<(), RecoveryError> {
        let marker = self.clean_marker_path();
        if marker.exists() {
            std::fs::remove_file(&marker)?;
        }
        Ok(())
    }

    /// Returns `true` if a snapshot file exists (main or `.bak`).
    pub fn snapshot_exists(&self) -> bool {
        self.snapshot_path().exists() || self.snapshot_bak_path().exists()
    }

    /// Delete the snapshot and its `.bak` backup. Idempotent.
    pub fn delete_snapshot(&self) -> Result<(), RecoveryError> {
        RecoverySnapshot::delete(&self.snapshot_path())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{WorkspaceDocument, WorkspaceWindow};

    fn sample_snapshot(clean_shutdown: bool) -> RecoverySnapshot {
        let ws = WorkspaceDocument {
            version: 1,
            name: "test".into(),
            profile: None,
            window: WorkspaceWindow {
                width: 800,
                height: 600,
            },
            tabs: Vec::new(),
            active_tab: 0,
        };
        RecoverySnapshot::from_workspace(ws, clean_shutdown)
    }

    /// Create a unique temp directory for test isolation. The caller is
    /// responsible for cleaning up (tests are short-lived; leaks are OK).
    fn test_tempdir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "weft-recovery-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_yaml_serialization() {
        let snap = sample_snapshot(false);
        let yaml = snap.to_yaml().unwrap();
        let loaded = RecoverySnapshot::from_yaml(&yaml).unwrap();
        assert_eq!(snap, loaded);
    }

    #[test]
    fn round_trip_file_save_load() {
        let tmp = test_tempdir("rt");
        let path = tmp.join("snapshot.yaml");
        let snap = sample_snapshot(false);
        snap.save(&path).unwrap();
        let loaded = RecoverySnapshot::load(&path).unwrap();
        assert_eq!(snap, loaded);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn corrupt_file_falls_back_to_bak() {
        let tmp = test_tempdir("corrupt");
        let path = tmp.join("snapshot.yaml");
        let bak = bak_path(&path);

        // Write a valid snapshot first.
        let snap = sample_snapshot(false);
        snap.save(&path).unwrap();
        // .bak should NOT exist yet (only created on subsequent saves).
        // Manually create a .bak by copying.
        std::fs::copy(&path, &bak).unwrap();

        // Now corrupt the main file.
        std::fs::write(&path, "not valid yaml {{{").unwrap();

        // Load should fall back to .bak.
        let loaded = RecoverySnapshot::load(&path).unwrap();
        assert_eq!(snap, loaded);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn empty_file_is_treated_as_corrupt() {
        let tmp = test_tempdir("empty");
        let path = tmp.join("snapshot.yaml");
        std::fs::write(&path, "").unwrap();
        let result = RecoverySnapshot::load(&path);
        assert!(result.is_err());
        match result {
            Err(RecoveryError::Corrupt) => {}
            other => panic!("expected Corrupt, got {:?}", other),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unknown_version_rejected() {
        let yaml = "version: 999\ncreated_at: 0\nclean_shutdown: false\nworkspace:\n  version: 1\n  name: x\n  window:\n    width: 1\n    height: 1\n  tabs: []\n  active_tab: 0\n";
        let result = RecoverySnapshot::from_yaml(yaml);
        assert!(matches!(result, Err(RecoveryError::UnsupportedVersion { found: 999, .. })));
    }

    #[test]
    fn delete_removes_main_and_bak() {
        let tmp = test_tempdir("delete");
        let path = tmp.join("snapshot.yaml");
        let bak = bak_path(&path);

        // Save twice so .bak exists.
        let snap = sample_snapshot(false);
        snap.save(&path).unwrap();
        // Manually create .bak (save only creates .bak on the SECOND write).
        std::fs::copy(&path, &bak).unwrap();
        assert!(path.exists());
        assert!(bak.exists());

        RecoverySnapshot::delete(&path).unwrap();
        assert!(!path.exists());
        assert!(!bak.exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = test_tempdir("idempotent");
        let path = tmp.join("snapshot.yaml");
        // Neither file exists — delete should succeed.
        RecoverySnapshot::delete(&path).unwrap();
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn bak_path_appends_bak_suffix() {
        let path = Path::new("/tmp/recovery/snapshot.yaml");
        let bak = bak_path(path);
        assert_eq!(bak, PathBuf::from("/tmp/recovery/snapshot.yaml.bak"));
    }

    #[test]
    fn tmp_path_appends_tmp_suffix() {
        let path = Path::new("/tmp/recovery/snapshot.yaml");
        let tmp = tmp_path(path);
        assert_eq!(tmp, PathBuf::from("/tmp/recovery/snapshot.yaml.tmp"));
    }

    // ── RecoveryPaths tests ───────────────────────────────────────────

    #[test]
    fn recovery_paths_resolves_under_cache_dir() {
        let cache = Path::new("/tmp/weft-cache");
        let paths = RecoveryPaths::new(cache);
        assert_eq!(paths.root(), Path::new("/tmp/weft-cache/recovery"));
        assert_eq!(
            paths.snapshot_path(),
            PathBuf::from("/tmp/weft-cache/recovery/snapshot.yaml")
        );
        assert_eq!(
            paths.clean_marker_path(),
            PathBuf::from("/tmp/weft-cache/recovery/.clean_shutdown")
        );
    }

    #[test]
    fn clean_marker_write_and_detect() {
        let tmp = test_tempdir("marker");
        let paths = RecoveryPaths::new(&tmp);

        // Initially no marker.
        assert!(!paths.clean_marker_exists());

        // Write marker.
        paths.write_clean_marker().unwrap();
        assert!(paths.clean_marker_exists());

        // Remove marker.
        paths.remove_clean_marker().unwrap();
        assert!(!paths.clean_marker_exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn snapshot_exists_checks_main_and_bak() {
        let tmp = test_tempdir("exists");
        let paths = RecoveryPaths::new(&tmp);
        paths.ensure_root().unwrap();

        // No snapshot yet.
        assert!(!paths.snapshot_exists());

        // Write snapshot.
        let snap = sample_snapshot(false);
        snap.save(&paths.snapshot_path()).unwrap();
        assert!(paths.snapshot_exists());

        // Delete main, keep .bak — should still report exists.
        let bak = paths.snapshot_bak_path();
        std::fs::copy(paths.snapshot_path(), &bak).unwrap();
        std::fs::remove_file(paths.snapshot_path()).unwrap();
        assert!(paths.snapshot_exists());

        // Delete both.
        paths.delete_snapshot().unwrap();
        assert!(!paths.snapshot_exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn save_creates_bak_on_second_write() {
        let tmp = test_tempdir("bak2");
        let path = tmp.join("snapshot.yaml");
        let bak = bak_path(&path);

        // First save: no .bak (nothing to back up).
        let snap1 = sample_snapshot(false);
        snap1.save(&path).unwrap();
        assert!(!bak.exists());

        // Second save: .bak should exist (backing up snap1).
        let mut snap2 = snap1.clone();
        snap2.clean_shutdown = true;
        snap2.save(&path).unwrap();
        assert!(bak.exists());

        // .bak should contain snap1 (the previous version).
        let bak_snap = RecoverySnapshot::load_file(&bak).unwrap();
        assert_eq!(bak_snap, snap1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn is_stale_returns_true_for_old_snapshots() {
        let mut snap = sample_snapshot(false);
        // Set created_at to 30 days ago.
        snap.created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            - (30 * 24 * 60 * 60);
        assert!(snap.is_stale());
    }

    #[test]
    fn is_stale_returns_false_for_recent_snapshots() {
        let snap = sample_snapshot(false);
        assert!(!snap.is_stale());
    }

    #[test]
    fn full_lifecycle_clean_shutdown() {
        // Simulate: clean exit → next launch detects marker → deletes snapshot.
        let tmp = test_tempdir("clean");
        let paths = RecoveryPaths::new(&tmp);
        paths.ensure_root().unwrap();

        // 1. Write a snapshot during session.
        let snap = sample_snapshot(false);
        snap.save(&paths.snapshot_path()).unwrap();
        assert!(paths.snapshot_exists());

        // 2. Clean exit: write marker.
        paths.write_clean_marker().unwrap();

        // 3. Next launch: marker exists → clean shutdown.
        assert!(paths.clean_marker_exists());

        // 4. Clean shutdown → delete snapshot + remove marker.
        paths.delete_snapshot().unwrap();
        paths.remove_clean_marker().unwrap();

        // 5. No recovery needed.
        assert!(!paths.snapshot_exists());
        assert!(!paths.clean_marker_exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_unclean_shutdown() {
        // Simulate: crash → next launch detects no marker → snapshot available.
        let tmp = test_tempdir("unclean");
        let paths = RecoveryPaths::new(&tmp);
        paths.ensure_root().unwrap();

        // 1. Write a snapshot during session.
        let snap = sample_snapshot(false);
        snap.save(&paths.snapshot_path()).unwrap();

        // 2. Crash: NO marker written.
        // (simulated — marker is absent)

        // 3. Next launch: marker absent → unclean shutdown.
        assert!(!paths.clean_marker_exists());

        // 4. Snapshot exists → can offer recovery.
        assert!(paths.snapshot_exists());
        let loaded = RecoverySnapshot::load(&paths.snapshot_path()).unwrap();
        assert_eq!(loaded, snap);

        // 5. Remove marker (consumed by the detection).
        paths.remove_clean_marker().unwrap();
        std::fs::remove_dir_all(&tmp).ok();
    }
}
