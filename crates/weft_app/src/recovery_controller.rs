//! v1.6.3: Recovery controller — orchestrates crash recovery snapshot
//! writes, startup detection, and the three recovery paths (Restore /
//! Ignore / Delete).
//!
//! ## Debounce
//!
//! [`RecoveryController::write_snapshot_if_changed`] serializes the
//! `WorkspaceDocument` to YAML and hashes it. If the hash matches the last
//! successful write, the write is skipped entirely — no disk I/O, no
//! `.bak` churn. This is the same content-equality debounce pattern used
//! by [`SnapshotPersistenceState`], applied to the recovery snapshot.
//!
//! ## Startup detection
//!
//! [`RecoveryController::detect_startup_recovery`] is called once at app
//! startup. It checks the clean-shutdown marker:
//!
//! - **Marker present** → last session ended cleanly. Delete any stale
//!   snapshot, remove the marker, return [`StartupRecovery::Clean`].
//! - **Marker absent** → unclean shutdown. Try to load the snapshot. If a
//!   valid snapshot exists, return [`StartupRecovery::UncleanShutdown`]
//!   with the loaded snapshot. If the snapshot is missing or corrupt,
//!   return [`StartupRecovery::NoSnapshot`].
//!
//! In all cases, the marker is consumed (removed) so the next launch
//! detects unclean unless a clean exit writes it again.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use tracing::{info, warn};

use weft_core::recovery::{RecoveryError, RecoveryPaths, RecoverySnapshot};
use weft_core::workspace::WorkspaceDocument;

use crate::App;

/// Result of startup recovery detection.
#[derive(Debug)]
pub(crate) enum StartupRecovery {
    /// Clean shutdown detected — no recovery needed. Any stale snapshot
    /// has been deleted.
    Clean,
    /// No snapshot exists (or it was corrupt beyond recovery). Start fresh.
    NoSnapshot,
    /// Unclean shutdown detected and a valid snapshot is available.
    /// The caller should show a recovery prompt and decide what to do.
    UncleanShutdown {
        snapshot: RecoverySnapshot,
    },
}

/// Orchestrates crash recovery snapshot lifecycle.
///
/// Owned by the `App` struct. The 1 Hz autosave poller calls
/// [`write_snapshot_if_changed`] after each `save_changed_tabs` cycle.
/// Clean exit paths call [`mark_clean_shutdown`].
pub(crate) struct RecoveryController {
    paths: RecoveryPaths,
    /// Hash of the YAML serialization of the last successfully-written
    /// snapshot. Used for content-equality debouncing.
    last_written_hash: Option<u64>,
}

impl RecoveryController {
    /// Create a new controller rooted at `<cache_dir>/recovery/`.
    /// If `cache_dir` is `None`, the controller is a no-op (all methods
    /// return `Ok` without doing anything). This handles the rare case
    /// where `weft_cache_dir()` can't resolve a cache directory.
    pub(crate) fn new(cache_dir: Option<&Path>) -> Self {
        let paths = match cache_dir {
            Some(dir) => RecoveryPaths::new(dir),
            None => RecoveryPaths::new(Path::new("/dev/null")),
        };
        Self {
            paths,
            last_written_hash: None,
        }
    }

    /// v1.6.3: Detect recovery state at startup.
    ///
    /// This is the main entry point for crash recovery. Called once from
    /// `App::resumed()` before the normal tab-restore path.
    ///
    /// See the module docs for the detection logic.
    pub(crate) fn detect_startup_recovery(&mut self) -> StartupRecovery {
        // If the cache dir is the /dev/null sentinel, we can't do recovery.
        if self.paths.root() == Path::new("/dev/null/recovery") {
            return StartupRecovery::NoSnapshot;
        }

        // Ensure the recovery directory exists.
        if let Err(e) = self.paths.ensure_root() {
            warn!(error = %e, "failed to create recovery directory");
            return StartupRecovery::NoSnapshot;
        }

        if self.paths.clean_marker_exists() {
            // Clean shutdown — delete any stale snapshot and consume the marker.
            info!("clean shutdown detected, deleting stale recovery snapshot");
            if let Err(e) = self.paths.delete_snapshot() {
                warn!(error = %e, "failed to delete stale recovery snapshot");
            }
            if let Err(e) = self.paths.remove_clean_marker() {
                warn!(error = %e, "failed to remove clean shutdown marker");
            }
            return StartupRecovery::Clean;
        }

        // Unclean shutdown — try to load the snapshot.
        match RecoverySnapshot::load(&self.paths.snapshot_path()) {
            Ok(snapshot) => {
                // Check if the snapshot is stale (older than 7 days).
                if snapshot.is_stale() {
                    info!(created_at = snapshot.created_at, "recovery snapshot is stale, deleting");
                    if let Err(e) = self.paths.delete_snapshot() {
                        warn!(error = %e, "failed to delete stale recovery snapshot");
                    }
                    return StartupRecovery::NoSnapshot;
                }

                // Consume the marker (if any — shouldn't exist here, but
                // clean up just in case).
                let _ = self.paths.remove_clean_marker();

                info!(
                    created_at = snapshot.created_at,
                    tabs = snapshot.workspace.tabs.len(),
                    "unclean shutdown detected, recovery snapshot available"
                );
                StartupRecovery::UncleanShutdown { snapshot }
            }
            Err(RecoveryError::Io(_)) => {
                // No snapshot file — fresh start.
                let _ = self.paths.remove_clean_marker();
                StartupRecovery::NoSnapshot
            }
            Err(e) => {
                // Corrupt snapshot — try .bak, already handled by load().
                // If we get here, both main and .bak are unreadable.
                warn!(error = %e, "recovery snapshot is corrupt, deleting");
                let _ = self.paths.delete_snapshot();
                let _ = self.paths.remove_clean_marker();
                StartupRecovery::NoSnapshot
            }
        }
    }

    /// v1.6.3: Write a recovery snapshot if the content has changed since
    /// the last successful write.
    ///
    /// Called from the 1 Hz autosave poller. The snapshot is built from
    /// the current session state via `App::capture_workspace`.
    ///
    /// Returns `true` if a write occurred, `false` if the content was
    /// unchanged (debounced).
    pub(crate) fn write_snapshot_if_changed(
        &mut self,
        workspace: &WorkspaceDocument,
    ) -> Result<bool, RecoveryError> {
        // Skip if cache dir is the sentinel.
        if self.paths.root() == Path::new("/dev/null/recovery") {
            return Ok(false);
        }

        // Ensure the recovery directory exists.
        self.paths.ensure_root()?;

        // Content-equality debounce: hash ONLY the workspace content (not
        // the timestamp, which changes every call). This skips writes when
        // the session state hasn't changed since the last successful write.
        let ws_yaml = workspace.to_yaml()?;
        let hash = hash_str(&ws_yaml);
        if self.last_written_hash == Some(hash) {
            return Ok(false);
        }

        // Build the full snapshot (with fresh timestamp) and write it.
        let snapshot = RecoverySnapshot::from_workspace(workspace.clone(), false);
        let start = std::time::Instant::now();
        snapshot.save(&self.paths.snapshot_path())?;
        let elapsed = start.elapsed();

        self.last_written_hash = Some(hash);
        info!(elapsed_ms = elapsed.as_millis() as u64, "recovery snapshot written");
        Ok(true)
    }

    /// v1.6.3: Write the clean-shutdown marker.
    ///
    /// Called from all clean exit paths (window close, last tab closed,
    /// performance probe exit). On the next launch, the marker is detected
    /// and the snapshot is deleted — no recovery prompt is shown.
    pub(crate) fn mark_clean_shutdown(&self) {
        if self.paths.root() == Path::new("/dev/null/recovery") {
            return;
        }
        if let Err(e) = self.paths.write_clean_marker() {
            warn!(error = %e, "failed to write clean shutdown marker");
        }
    }

    /// v1.6.3: Delete the recovery snapshot permanently.
    ///
    /// Called when the user chooses "Delete" in the recovery prompt, or
    /// when the snapshot is corrupt beyond recovery.
    pub(crate) fn delete_snapshot(&self) -> Result<(), RecoveryError> {
        self.paths.delete_snapshot()
    }

    /// v1.6.3: Keep the snapshot but don't restore from it.
    ///
    /// Called when the user chooses "Ignore" in the recovery prompt.
    /// The snapshot is preserved in case the user wants to recover later
    /// (e.g. by manually loading the file).
    pub(crate) fn ignore_snapshot(&self) {
        info!("recovery snapshot ignored (preserved on disk)");
    }

    /// Reset the debounce state. Called after a restore to force the next
    /// `write_snapshot_if_changed` to actually write (the restored state
    /// may be identical to the snapshot, but we want a fresh write to
    /// update the timestamp).
    pub(crate) fn reset_debounce(&mut self) {
        self.last_written_hash = None;
    }

    /// The recovery paths (for diagnostics / testing).
    #[allow(dead_code)]
    pub(crate) fn paths(&self) -> &RecoveryPaths {
        &self.paths
    }
}

// ── App integration ────────────────────────────────────────────────────

impl App {
    /// v1.6.3: Run crash recovery detection at startup.
    ///
    /// Called from `resumed()` before the normal tab-snapshot restore.
    /// Returns `true` if a recovery snapshot was successfully restored
    /// (the normal restore should be skipped), `false` otherwise.
    pub(super) fn run_startup_recovery(&mut self) -> bool {
        match self.recovery.detect_startup_recovery() {
            StartupRecovery::Clean => {
                // Clean shutdown — proceed with normal tab-snapshot restore.
                false
            }
            StartupRecovery::NoSnapshot => {
                // No recovery snapshot — proceed with normal restore.
                false
            }
            StartupRecovery::UncleanShutdown { snapshot } => {
                // Unclean shutdown with a valid snapshot — show the
                // recovery prompt.
                let age_secs = snapshot.created_at;
                let tab_count = snapshot.workspace.tabs.len();
                if let Some(mtm) = objc2_foundation::MainThreadMarker::new() {
                    match crate::macos_alert::show_recovery_prompt(mtm, age_secs, tab_count) {
                        Ok(crate::macos_alert::RecoveryPromptResponse::Restore) => {
                            info!("user chose to restore from recovery snapshot");
                            match self.restore_workspace(&snapshot.workspace) {
                                Ok(()) => {
                                    info!("recovery snapshot restored successfully");
                                    self.recovery.reset_debounce();
                                    return true;
                                }
                                Err(e) => {
                                    warn!(error = %e, "recovery restore failed, falling back to normal startup");
                                }
                            }
                        }
                        Ok(crate::macos_alert::RecoveryPromptResponse::Ignore) => {
                            info!("user chose to ignore recovery snapshot");
                            self.recovery.ignore_snapshot();
                        }
                        Ok(crate::macos_alert::RecoveryPromptResponse::Delete) => {
                            info!("user chose to delete recovery snapshot");
                            if let Err(e) = self.recovery.delete_snapshot() {
                                warn!(error = %e, "failed to delete recovery snapshot");
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "recovery prompt failed, proceeding with normal startup");
                        }
                    }
                } else {
                    warn!("recovery prompt skipped: not on main thread");
                }
                false
            }
        }
    }
}

/// Hash a string using the default hasher. Used for content-equality
/// debouncing — we only care about "changed vs unchanged", not
/// cryptographic strength.
fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use weft_core::workspace::{WorkspaceDocument, WorkspaceWindow};

    fn test_tempdir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "weft-recovery-ctrl-{}-{}-{}",
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

    fn sample_workspace(name: &str) -> WorkspaceDocument {
        WorkspaceDocument {
            version: 1,
            name: name.into(),
            profile: None,
            window: WorkspaceWindow {
                width: 800,
                height: 600,
            },
            tabs: Vec::new(),
            active_tab: 0,
        }
    }

    #[test]
    fn detect_startup_clean_shutdown_deletes_snapshot() {
        let tmp = test_tempdir("clean");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // Write a snapshot + clean marker (simulating a clean exit).
        let snap = RecoverySnapshot::from_workspace(sample_workspace("test"), true);
        ctrl.paths.ensure_root().unwrap();
        snap.save(&ctrl.paths.snapshot_path()).unwrap();
        ctrl.paths.write_clean_marker().unwrap();
        assert!(ctrl.paths.snapshot_exists());

        // Detect: should be Clean, snapshot deleted.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::Clean));
        assert!(!ctrl.paths.snapshot_exists());
        assert!(!ctrl.paths.clean_marker_exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detect_startup_unclean_shutdown_loads_snapshot() {
        let tmp = test_tempdir("unclean");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // Write a snapshot WITHOUT a clean marker (simulating a crash).
        let snap = RecoverySnapshot::from_workspace(sample_workspace("crash"), false);
        ctrl.paths.ensure_root().unwrap();
        snap.save(&ctrl.paths.snapshot_path()).unwrap();

        // Detect: should be UncleanShutdown with the snapshot.
        let result = ctrl.detect_startup_recovery();
        match result {
            StartupRecovery::UncleanShutdown { snapshot } => {
                assert_eq!(snapshot, snap);
            }
            other => panic!("expected UncleanShutdown, got {:?}", other),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detect_startup_no_snapshot_returns_no_snapshot() {
        let tmp = test_tempdir("empty");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // No snapshot, no marker.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::NoSnapshot));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn detect_startup_corrupt_snapshot_returns_no_snapshot() {
        let tmp = test_tempdir("corrupt");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // Write a corrupt snapshot (no .bak).
        ctrl.paths.ensure_root().unwrap();
        std::fs::write(ctrl.paths.snapshot_path(), "not valid yaml {{{").unwrap();

        // Detect: should be NoSnapshot (corrupt, deleted).
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::NoSnapshot));
        assert!(!ctrl.paths.snapshot_exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn write_snapshot_if_changed_debounces_unchanged_content() {
        let tmp = test_tempdir("debounce");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let ws = sample_workspace("test");

        // First write: should write.
        let wrote = ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(wrote);

        // Second write (same content): should skip.
        let wrote = ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(!wrote);

        // Third write (changed content): should write.
        let ws2 = sample_workspace("changed");
        let wrote = ctrl.write_snapshot_if_changed(&ws2).unwrap();
        assert!(wrote);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn write_snapshot_creates_bak_on_second_write() {
        let tmp = test_tempdir("bak");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let ws1 = sample_workspace("first");

        // First write: no .bak.
        ctrl.write_snapshot_if_changed(&ws1).unwrap();
        assert!(!ctrl.paths.snapshot_bak_path().exists());

        // Second write (changed): .bak should exist.
        let ws2 = sample_workspace("second");
        ctrl.write_snapshot_if_changed(&ws2).unwrap();
        assert!(ctrl.paths.snapshot_bak_path().exists());

        // .bak should contain the first snapshot's workspace.
        let bak_snap = RecoverySnapshot::load_file(&ctrl.paths.snapshot_bak_path()).unwrap();
        assert_eq!(bak_snap.workspace.name, "first");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn mark_clean_shutdown_writes_marker() {
        let tmp = test_tempdir("marker");
        let ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        assert!(!ctrl.paths.clean_marker_exists());

        ctrl.mark_clean_shutdown();
        assert!(ctrl.paths.clean_marker_exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn delete_snapshot_removes_files() {
        let tmp = test_tempdir("del");
        let ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        // Write a snapshot.
        let snap = RecoverySnapshot::from_workspace(sample_workspace("test"), false);
        snap.save(&ctrl.paths.snapshot_path()).unwrap();
        assert!(ctrl.paths.snapshot_exists());

        // Delete.
        ctrl.delete_snapshot().unwrap();
        assert!(!ctrl.paths.snapshot_exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reset_debounce_forces_next_write() {
        let tmp = test_tempdir("reset");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let ws = sample_workspace("test");

        // First write.
        ctrl.write_snapshot_if_changed(&ws).unwrap();

        // Same content — skipped.
        let wrote = ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(!wrote);

        // Reset debounce — next write should happen even with same content.
        ctrl.reset_debounce();
        let wrote = ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(wrote);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_clean_shutdown() {
        let tmp = test_tempdir("life-clean");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(ctrl.paths.snapshot_exists());

        // 2. Clean exit: write marker.
        ctrl.mark_clean_shutdown();

        // 3. Next launch: detect clean.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::Clean));
        assert!(!ctrl.paths.snapshot_exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_unclean_shutdown_with_restore() {
        let tmp = test_tempdir("life-restore");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed(&ws).unwrap();

        // 2. Crash: no marker.

        // 3. Next launch: detect unclean.
        let result = ctrl.detect_startup_recovery();
        match result {
            StartupRecovery::UncleanShutdown { snapshot } => {
                assert_eq!(snapshot.workspace, ws);
            }
            other => panic!("expected UncleanShutdown, got {:?}", other),
        }

        // 4. User chooses Restore: the snapshot is kept (rollback point).
        // The App will call restore_workspace(snapshot.workspace) and then
        // reset_debounce() so the next write captures the restored state.

        // 5. After restore, new session writes overwrite the snapshot.
        ctrl.reset_debounce();
        let new_ws = sample_workspace("restored");
        ctrl.write_snapshot_if_changed(&new_ws).unwrap();

        // 6. Clean exit.
        ctrl.mark_clean_shutdown();

        // 7. Next launch: clean.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::Clean));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_unclean_shutdown_with_ignore() {
        let tmp = test_tempdir("life-ignore");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed(&ws).unwrap();

        // 2. Crash.

        // 3. Next launch: detect unclean.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::UncleanShutdown { .. }));

        // 4. User chooses Ignore: snapshot is kept.
        ctrl.ignore_snapshot();
        assert!(ctrl.paths.snapshot_exists());

        // 5. New session writes overwrite the snapshot.
        let new_ws = sample_workspace("fresh");
        ctrl.write_snapshot_if_changed(&new_ws).unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_unclean_shutdown_with_delete() {
        let tmp = test_tempdir("life-delete");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed(&ws).unwrap();

        // 2. Crash.

        // 3. Next launch: detect unclean.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::UncleanShutdown { .. }));

        // 4. User chooses Delete: snapshot is deleted.
        ctrl.delete_snapshot().unwrap();
        assert!(!ctrl.paths.snapshot_exists());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn none_cache_dir_makes_controller_noop() {
        let mut ctrl = RecoveryController::new(None);

        // Detect: no snapshot.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::NoSnapshot));

        // Write: no-op, returns false.
        let ws = sample_workspace("test");
        let wrote = ctrl.write_snapshot_if_changed(&ws).unwrap();
        assert!(!wrote);

        // Mark clean: no-op (no panic).
        ctrl.mark_clean_shutdown();
    }

    #[test]
    fn stale_snapshot_is_deleted_on_startup() {
        let tmp = test_tempdir("stale");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // Write a stale snapshot (30 days old).
        let mut snap = RecoverySnapshot::from_workspace(sample_workspace("old"), false);
        snap.created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            - (30 * 24 * 60 * 60);
        ctrl.paths.ensure_root().unwrap();
        snap.save(&ctrl.paths.snapshot_path()).unwrap();

        // Detect: should be NoSnapshot (stale, deleted).
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::NoSnapshot));
        assert!(!ctrl.paths.snapshot_exists());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
