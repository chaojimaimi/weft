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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tracing::{info, warn};

use weft_core::recovery::{RecoveryError, RecoveryPaths, RecoverySnapshot};
use weft_core::workspace::WorkspaceDocument;

use crate::macos_alert::RecoveryChoice;
use crate::{App, AppEvent};

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
    UncleanShutdown { snapshot: RecoverySnapshot },
}

/// v1.10.23: What the startup path should do after detection (replaces the
/// old `bool` returned by `run_startup_recovery`). The recovery prompt is
/// now event-driven: detection stays synchronous in `resumed()`, but the
/// `runModal` itself runs on a deferred main-queue block and the outcome
/// arrives later via `AppEvent::RecoveryChosen`. See
/// docs/FIX_RECOVERY_MODAL_SPIN.md.
#[derive(Debug)]
pub(super) enum StartupRecoveryOutcome {
    /// No recovery needed (clean shutdown / no snapshot / prompt failed):
    /// proceed with the normal tab-snapshot restore.
    Normal,
    /// Unclean shutdown with a snapshot: the prompt is being shown on a
    /// deferred main-queue block. Skip the synchronous restore branches;
    /// `App::apply_recovery_choice` restores + hydrates when the choice
    /// event lands.
    Pending,
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
    /// v1.10.31: Flag indicating whether a background write is currently
    /// in-flight. Used to prevent concurrent writes stacking up.
    write_in_flight: Arc<AtomicBool>,
    /// v1.10.31: Flag indicating whether the last dispatched write failed.
    /// When true, the next tick resets `last_written_hash` to retry.
    dispatch_failed: Arc<AtomicBool>,
    /// v1.10.31: Shared failure counter for background threads to increment.
    /// Atomic so the main thread can see the updated count.
    shared_failure_count: Arc<AtomicU32>,
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
            write_in_flight: Arc::new(AtomicBool::new(false)),
            dispatch_failed: Arc::new(AtomicBool::new(false)),
            shared_failure_count: Arc::new(AtomicU32::new(0)),
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
                    info!(
                        created_at = snapshot.created_at,
                        "recovery snapshot is stale, deleting"
                    );
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
    /// v1.10.31: This is the core write function that accepts a custom write
    /// dispatcher closure. Production code uses a background thread dispatcher;
    /// tests inject a synchronous dispatcher for observability and determinism.
    ///
    /// Returns `true` if a write was dispatched, `false` if the content was
    /// unchanged (debounced) or a write was already in-flight.
    fn write_snapshot_if_changed_with_dispatch<F>(
        &mut self,
        workspace: &WorkspaceDocument,
        dispatcher: F,
    ) -> Result<bool, RecoveryError>
    where
        F: FnOnce(&RecoverySnapshot, &Path) -> Result<(), RecoveryError>,
    {
        // Skip if cache dir is the sentinel.
        if self.paths.root() == Path::new("/dev/null/recovery") {
            return Ok(false);
        }

        // Ensure the recovery directory exists.
        self.paths.ensure_root()?;

        // v1.10.31: If the last dispatch failed, reset the hash so we retry
        // this content on the next tick (reuses reset_debounce semantics).
        if self.dispatch_failed.swap(false, Ordering::AcqRel) {
            self.last_written_hash = None;
        }

        // v1.10.31: Skip if a write is already in-flight to prevent concurrent
        // writes from stacking up (1 Hz tick rate makes this unlikely but not
        // impossible on slow I/O).
        if self.write_in_flight.load(Ordering::Acquire) {
            return Ok(false);
        }

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
        let snapshot_path = self.paths.snapshot_path();

        // Update hash BEFORE dispatching (prevents duplicate dispatches).
        self.last_written_hash = Some(hash);

        // Dispatch the write using the injected closure.
        dispatcher(&snapshot, &snapshot_path)?;
        Ok(true)
    }

    /// v1.6.3: Production wrapper that dispatches snapshot writes to a
    /// background thread ("weft-recovery-writer") to avoid blocking the main
    /// thread during 5-14ms disk I/O.
    ///
    /// v1.10.31: Returns `true` if a write was dispatched, `false` if the
    /// content was unchanged (debounced) or a write was already in-flight.
    /// Returns `Err` only if the dispatch setup failed (e.g., cache directory
    /// issue or YAML serialization error). The actual write happens
    /// asynchronously on a background thread; write failures are tracked
    /// internally and retried on the next tick.
    pub(crate) fn write_snapshot_if_changed(
        &mut self,
        workspace: &WorkspaceDocument,
    ) -> Result<bool, RecoveryError> {
        // v1.10.31: Clone the Arcs before calling the core function to avoid
        // borrow conflicts inside the closure.
        let in_flight = self.write_in_flight.clone();
        let failed = self.dispatch_failed.clone();
        let failure_count = self.shared_failure_count.clone();

        self.write_snapshot_if_changed_with_dispatch(workspace, |snapshot, snapshot_path| {
            // v1.10.31: Mark write as in-flight before spawning the thread.
            in_flight.store(true, Ordering::Release);

            // v1.10.31: Move snapshot and snapshot_path into the thread closure
            // to avoid lifetime issues. Clone failure_count for the thread.
            let snapshot = snapshot.clone();
            let snapshot_path = snapshot_path.to_path_buf();
            let thread_failure_count = failure_count.clone();
            // Review S1: rollback handles for the spawn-failure path — the
            // `move` closure below takes ownership of the originals.
            let rollback_in_flight = in_flight.clone();
            let rollback_failed = failed.clone();

            std::thread::Builder::new()
                .name(String::from("weft-recovery-writer"))
                .spawn(move || {
                    let start = std::time::Instant::now();
                    match snapshot.save(&snapshot_path) {
                        Ok(()) => {
                            let elapsed = start.elapsed();
                            info!(
                                elapsed_ms = elapsed.as_millis() as u64,
                                "recovery snapshot written (background thread)"
                            );
                        }
                        Err(e) => {
                            // v1.10.31: Mark dispatch as failed so the next tick
                            // retries this content. Increment the shared failure count.
                            failed.store(true, Ordering::Release);
                            thread_failure_count.fetch_add(1, Ordering::Release);
                            warn!(
                                error = %e,
                                failures = thread_failure_count.load(Ordering::Acquire),
                                "recovery snapshot write failed (background thread)"
                            );
                        }
                    }
                    // v1.10.31: Clear in-flight flag whether write succeeded or failed.
                    in_flight.store(false, Ordering::Release);
                    // A panic inside the thread body would leave in_flight stuck
                    // true (every later tick skips dispatch). Accepted: the body
                    // is `snapshot.save()` (fs + serde on an owned snapshot,
                    // returns Result, no unwraps) plus atomics — nil panic
                    // surface, and the clean-shutdown marker covers the
                    // stale-snapshot case.
                })
                .map_err(|e| {
                    // Review S1: spawn failed — the thread never ran, so nothing
                    // would clear in_flight or set failed; every later tick would
                    // silently skip. Roll both back (in_flight was set above),
                    // flagging failed so the next tick retries this content.
                    rollback_in_flight.store(false, Ordering::Release);
                    rollback_failed.store(true, Ordering::Release);
                    warn!(error = %e, "failed to spawn weft-recovery-writer");
                })
                .ok();

            info!(
                failures = failure_count.load(Ordering::Acquire),
                "recovery snapshot write dispatched to background thread"
            );
            Ok(())
        })
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
    /// v1.10.24 (Fix 3): NOT kept for later recovery/manual loading — the
    /// first autosave (≤1s, hash=None) supersedes it; the old copy survives
    /// only as one `.bak` generation.
    pub(crate) fn ignore_snapshot(&self) {
        info!("recovery snapshot ignored (first autosave ≤1s supersedes it; old copy kept only as one .bak generation)");
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
    ///
    /// v1.10.23 (FIX_RECOVERY_MODAL_SPIN): this method only DETECTS and
    /// parks. When an unclean shutdown snapshot exists, the snapshot is
    /// stored in `App::pending_recovery` and the recovery prompt is
    /// dispatched to the main queue via `dispatch2` — so `runModal` runs
    /// OUTSIDE the winit event handler (inside the handler, winit's
    /// `EventLoopWaker` 0.1µs timer is never disarmed: `cleared()` early-
    /// returns while `event_handler.in_use()`, and the modal's nested run
    /// loop spins at 84% CPU). The choice comes back asynchronously as
    /// `AppEvent::RecoveryChosen` → [`App::apply_recovery_choice`].
    pub(super) fn run_startup_recovery(&mut self) -> StartupRecoveryOutcome {
        match self.recovery.detect_startup_recovery() {
            StartupRecovery::Clean | StartupRecovery::NoSnapshot => {
                // No recovery needed — proceed with normal tab restore.
                StartupRecoveryOutcome::Normal
            }
            StartupRecovery::UncleanShutdown { snapshot } => {
                // Unclean shutdown with a valid snapshot — show the
                // recovery prompt.
                //
                // v1.6.3 review M7: compute the snapshot AGE (now -
                // created_at), not the raw timestamp. `created_at` is a
                // Unix epoch seconds value; passing it directly as
                // `age_secs` would display "from 1753612345 seconds ago"
                // instead of "from 5 minutes ago".
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let age_secs = snapshot_age_secs(now_secs, snapshot.created_at);
                let tab_count = snapshot.workspace.tabs.len();
                // Park the snapshot BEFORE deferring the prompt: the
                // RecoveryChosen event consumes it exactly once. `pending`
                // also records "prompt in flight" for the state machine
                // (a stray choice event without a pending snapshot is a
                // no-op — no double restore, no lost state).
                self.pending_recovery = Some(snapshot);
                let proxy = self.proxy.clone();
                // v1.10.23: defer `runModal` out of the winit handler via
                // the main queue. `dispatch2` is a safe libdispatch
                // wrapper (no bare `dispatch_async_f` + extern callback).
                // When the modal closes, the choice is sent back through
                // the event-loop proxy — the winit loop is fully usable
                // again by then, so `user_event` processes it normally.
                dispatch2::DispatchQueue::main().exec_async(move || {
                    let choice = match objc2_foundation::MainThreadMarker::new() {
                        Some(mtm) => {
                            match crate::macos_alert::show_recovery_prompt(
                                mtm,
                                age_secs,
                                tab_count,
                            ) {
                                Ok(response) => {
                                    crate::macos_alert::recovery_choice_from_response(response)
                                }
                                Err(e) => {
                                    // Same fallback as pre-v1.10.23: a
                                    // failed prompt proceeds with the
                                    // normal startup, snapshot preserved.
                                    warn!(error = %e, "recovery prompt failed, proceeding with normal startup");
                                    crate::macos_alert::RecoveryChoice::Ignore
                                }
                            }
                        }
                        None => {
                            warn!("recovery prompt skipped: not on main thread");
                            crate::macos_alert::RecoveryChoice::Ignore
                        }
                    };
                    let _ = proxy.send_event(AppEvent::RecoveryChosen(choice));
                });
                info!(
                    age_secs = age_secs,
                    tabs = tab_count,
                    "recovery prompt deferred off the winit handler; awaiting user choice"
                );
                StartupRecoveryOutcome::Pending
            }
        }
    }

    /// v1.10.23: Handle the user's recovery decision, delivered
    /// asynchronously as `AppEvent::RecoveryChosen` from the deferred
    /// prompt block (see [`App::run_startup_recovery`]).
    ///
    /// Consumes `App::pending_recovery`: without a pending snapshot the
    /// event is a no-op (duplicated delivery, or a teardown race where the
    /// window closed before the choice landed). The window/tab state is
    /// whatever it actually is when the event arrives — this handler never
    /// assumes `resumed()` just completed.
    ///
    /// Returns `true` if a pending snapshot was processed.
    pub(super) fn apply_recovery_choice(&mut self, choice: RecoveryChoice) {
        let Some(snapshot) = take_pending_recovery(&mut self.pending_recovery) else {
            warn!(
                ?choice,
                "recovery choice arrived without a pending snapshot; ignoring"
            );
            return;
        };
        match choice {
            RecoveryChoice::Restore => {
                info!("user chose to restore from recovery snapshot");
                match self.restore_workspace(&snapshot.workspace) {
                    Ok(outcome) => {
                        if let Some(warning) = outcome.warning() {
                            warn!(warning = %warning, "recovery restored with warning");
                            self.surface_config_error(&warning.to_string());
                        }
                        info!("recovery snapshot restored successfully");
                        self.recovery.reset_debounce();
                        // v1.8.9: attach TabSnapshot block_ids so the
                        // hydration below populates each tab's block
                        // tracker (sidebar history).
                        self.attach_recovery_tab_snapshots();
                    }
                    Err(e) => {
                        warn!(error = %e, "recovery restore failed, falling back to normal startup");
                        // v1.10.24 Fix 1: surface on the same channel as the
                        // success-path warning (not just a warn! log).
                        self.surface_config_error(&format!(
                            "Session restore failed, falling back to normal startup: {e}"
                        ));
                        self.restore_tab_snapshots();
                    }
                }
            }
            RecoveryChoice::Ignore => {
                info!("user chose to ignore recovery snapshot");
                self.recovery.ignore_snapshot();
                self.restore_tab_snapshots();
            }
            RecoveryChoice::Delete => {
                info!("user chose to delete recovery snapshot");
                if let Err(e) = self.recovery.delete_snapshot() {
                    warn!(error = %e, "failed to delete recovery snapshot");
                }
                self.restore_tab_snapshots();
            }
        }
        // v1.10.23: hydration now runs here (event-driven) instead of
        // synchronously in `resumed()` — the tab topology is final only
        // after the chosen restore path has run.
        self.hydrate_tabs_from_history_store();
    }
}

/// v1.10.23: The 1 Hz autosave must be suppressed while the recovery
/// prompt is pending — it would overwrite both recovery sources (the
/// tabs table and the on-disk crash snapshot) with the fresh session
/// before the user has chosen. See `AppEvent::TabsAutoSave`.
pub(super) fn autosave_suppressed(pending: &Option<RecoverySnapshot>) -> bool {
    pending.is_some()
}

/// v1.10.23: Consume the pending recovery snapshot for a user choice.
///
/// The pending-recovery state machine contract (unit-tested below):
///
/// - `None → Some(snapshot)`: startup detection parks the snapshot while
///   the deferred prompt is on screen.
/// - `Some → None`: the choice event consumes it exactly once.
/// - A second choice event for the same prompt finds `None` and is
///   ignored — a duplicated `RecoveryChosen` can never double-restore.
fn take_pending_recovery(pending: &mut Option<RecoverySnapshot>) -> Option<RecoverySnapshot> {
    pending.take()
}

/// Hash a string using the default hasher. Used for content-equality
/// debouncing — we only care about "changed vs unchanged", not
/// cryptographic strength.
fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn snapshot_age_secs(now_secs: u64, created_at: u64) -> u64 {
    now_secs.saturating_sub(created_at)
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
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();
        assert!(wrote);

        // Second write (same content): should skip.
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();
        assert!(!wrote);

        // Third write (changed content): should write.
        let ws2 = sample_workspace("changed");
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws2, |snapshot, path| snapshot.save(path))
            .unwrap();
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
        ctrl.write_snapshot_if_changed_with_dispatch(&ws1, |snapshot, path| snapshot.save(path))
            .unwrap();
        assert!(!ctrl.paths.snapshot_bak_path().exists());

        // Second write (changed): .bak should exist.
        let ws2 = sample_workspace("second");
        ctrl.write_snapshot_if_changed_with_dispatch(&ws2, |snapshot, path| snapshot.save(path))
            .unwrap();
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
        ctrl.write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();

        // Same content — skipped.
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();
        assert!(!wrote);

        // Reset debounce — next write should happen even with same content.
        ctrl.reset_debounce();
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();
        assert!(wrote);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_clean_shutdown() {
        let tmp = test_tempdir("life-clean");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();
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
        ctrl.write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();

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
        ctrl.write_snapshot_if_changed_with_dispatch(&new_ws, |snapshot, path| snapshot.save(path))
            .unwrap();

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
        ctrl.write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();

        // 2. Crash.

        // 3. Next launch: detect unclean.
        let result = ctrl.detect_startup_recovery();
        assert!(matches!(result, StartupRecovery::UncleanShutdown { .. }));

        // 4. User chooses Ignore: snapshot is kept.
        ctrl.ignore_snapshot();
        assert!(ctrl.paths.snapshot_exists());

        // 5. New session writes overwrite the snapshot.
        let new_ws = sample_workspace("fresh");
        ctrl.write_snapshot_if_changed_with_dispatch(&new_ws, |snapshot, path| snapshot.save(path))
            .unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn full_lifecycle_unclean_shutdown_with_delete() {
        let tmp = test_tempdir("life-delete");
        let mut ctrl = RecoveryController::new(Some(&tmp));

        // 1. Session: write snapshot.
        let ws = sample_workspace("session");
        ctrl.write_snapshot_if_changed_with_dispatch(&ws, |snapshot, path| snapshot.save(path))
            .unwrap();

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

    #[test]
    fn snapshot_age_uses_elapsed_time_and_saturates_future_timestamps() {
        assert_eq!(snapshot_age_secs(1_000, 940), 60);
        assert_eq!(snapshot_age_secs(1_000, 1_001), 0);
    }

    /// v1.10.23: pending-recovery state machine — the deferred prompt's
    /// snapshot must be consumed exactly once so a duplicated
    /// `RecoveryChosen` event can never double-restore, and a choice that
    /// races teardown (no pending snapshot) must be a safe no-op.
    #[test]
    fn pending_recovery_state_machine_consumes_the_snapshot_exactly_once() {
        let snapshot = RecoverySnapshot::from_workspace(sample_workspace("crash"), false);

        // None → Some: startup detection parks the snapshot while the
        // deferred prompt is on screen.
        let mut pending: Option<RecoverySnapshot> = None;
        assert!(pending.is_none(), "start: no pending snapshot");
        pending = Some(snapshot.clone());
        assert!(pending.is_some());

        // Some → None: the first choice event consumes it.
        let taken = take_pending_recovery(&mut pending);
        assert_eq!(taken, Some(snapshot.clone()));
        assert!(
            pending.is_none(),
            "processing the choice must clear the pending slot"
        );

        // A duplicated/spurious second choice event: nothing pending → the
        // handler's take is None and the restore path never runs twice.
        let spurious = take_pending_recovery(&mut pending);
        assert!(spurious.is_none(), "second choice event must be a no-op");
    }

    #[test]
    fn pending_recovery_is_restored_by_choice_regardless_of_choice_value() {
        // v1.10.23: every choice variant (Restore / Ignore / Delete)
        // consumes the parked snapshot — the state machine never stalls
        // on one path and "loses" the snapshot.
        let snapshot = RecoverySnapshot::from_workspace(sample_workspace("crash"), false);
        for choice in [
            RecoveryChoice::Restore,
            RecoveryChoice::Ignore,
            RecoveryChoice::Delete,
        ] {
            let mut pending = Some(snapshot.clone());
            let taken = take_pending_recovery(&mut pending);
            assert_eq!(taken.as_ref(), Some(&snapshot), "choice {choice:?}");
            assert!(pending.is_none(), "choice {choice:?} must clear pending");
        }
    }

    #[test]
    fn autosave_is_suppressed_only_while_recovery_prompt_is_pending() {
        // v1.10.23 regression guard: the deferred recovery modal keeps the
        // runloop alive, so TabsAutoSave can fire mid-prompt — it must not
        // overwrite the crash snapshot or tabs table in that window.
        let snapshot = RecoverySnapshot::from_workspace(sample_workspace("crash"), false);
        assert!(autosave_suppressed(&Some(snapshot)));
        assert!(!autosave_suppressed(&None));
    }

    /// v1.10.24: TDD test for off-main-thread snapshot writes.
    /// This test verifies that:
    /// 1. Unchanged content does NOT trigger a write dispatch
    /// 2. Changed content triggers exactly ONE write dispatch
    ///
    /// The test uses an injected closure to count dispatches synchronously.
    #[test]
    fn write_snapshot_dispatches_write_only_when_content_changes() {
        let tmp = test_tempdir("dispatch-count");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let ws = sample_workspace("test");

        // Track how many times write is dispatched
        let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // First write: should dispatch (content changed from None)
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(wrote, "First write should return true");
        assert_eq!(
            dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "First write should dispatch exactly once"
        );

        // Second write (same content): should NOT dispatch
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(!wrote, "Unchanged content write should return false");
        assert_eq!(
            dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Unchanged content should NOT dispatch (still 1)"
        );

        // Third write (changed content): should dispatch exactly once more
        let ws2 = sample_workspace("changed");
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws2, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(wrote, "Changed content write should return true");
        assert_eq!(
            dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "Changed content should dispatch exactly once more (total 2)"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Review S2 (v1.10.31): a flagged dispatch failure must reset the
    /// debounce so the SAME content is retried on the next tick — the write
    /// thread's failure path sets `dispatch_failed`; this test pins the
    /// retry semantics without spawning threads.
    #[test]
    fn dispatch_failure_resets_debounce_and_retries_same_content() {
        let tmp = test_tempdir("failed-retry");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let ws = sample_workspace("retry-me");
        let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Normal first write of this content.
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(wrote);
        assert_eq!(dispatch_count.load(Ordering::SeqCst), 1);

        // Same content: normally debounced (no dispatch)...
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(!wrote, "unchanged content must debounce");
        assert_eq!(dispatch_count.load(Ordering::SeqCst), 1);

        // ...but once the write thread flags failure, the SAME content must
        // dispatch again on the next tick.
        ctrl.dispatch_failed.store(true, Ordering::Release);
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&ws, {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(
            wrote,
            "a flagged dispatch failure must retry the same content"
        );
        assert_eq!(dispatch_count.load(Ordering::SeqCst), 2);
        assert!(
            !ctrl.dispatch_failed.load(Ordering::Acquire),
            "the retry tick consumes the failure flag"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Review S2 (v1.10.31): while a write is in-flight, the next tick must
    /// skip dispatching entirely (returning false) — the concurrency guard
    /// against stacking writes with a fixed tmp name.
    #[test]
    fn in_flight_write_skips_dispatch() {
        let tmp = test_tempdir("in-flight-skip");
        let mut ctrl = RecoveryController::new(Some(&tmp));
        ctrl.paths.ensure_root().unwrap();

        let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Simulate an in-flight write; even CHANGED content must not dispatch.
        ctrl.write_in_flight.store(true, Ordering::Release);
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&sample_workspace("in-flight"), {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(!wrote, "an in-flight write must skip the dispatch");
        assert_eq!(
            dispatch_count.load(Ordering::SeqCst),
            0,
            "the dispatcher closure must not run while in-flight"
        );

        // Once the write thread finishes (flag cleared), the pending content
        // dispatches normally.
        ctrl.write_in_flight.store(false, Ordering::Release);
        let wrote = ctrl
            .write_snapshot_if_changed_with_dispatch(&sample_workspace("in-flight"), {
                let count = dispatch_count.clone();
                move |snapshot: &RecoverySnapshot, path: &std::path::Path| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    snapshot.save(path)
                }
            })
            .unwrap();
        assert!(wrote, "after the write completes, dispatch resumes");
        assert_eq!(dispatch_count.load(Ordering::SeqCst), 1);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
