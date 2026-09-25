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
use crate::settings_validation::{recovery_gate, RecoveryGate};
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
                // Unclean shutdown with a valid snapshot — the user's
                // `[session].recovery` setting (v1.12.19, PLAN_v11217
                // §3.8 T13a) decides between the prompt and a synchronous
                // choice. `config_state` was loaded in `App::new`, long
                // before `resumed()` reaches this detection point.
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
                let gate = recovery_gate(self.config_state.config.session.recovery);
                match gate {
                    RecoveryGate::AutoRestore => {
                        // ① park BEFORE the synchronous apply —
                        //    `apply_recovery_choice`'s first act is
                        //    `take_pending_recovery`; an unparked choice
                        //    would log "arrived without a pending snapshot"
                        //    and no-op (review P1d state-machine ordering).
                        self.pending_recovery = Some(snapshot);
                        info!("auto-restoring after unclean shutdown (session.recovery=auto)");
                        // ② the returned outcome MUST NOT be `Normal`:
                        //    `apply_recovery_choice` already consumed the
                        //    choice synchronously (choice 已同步消费) —
                        //    restore + hydrate ran inside it. Returning
                        //    Normal would make the startup path run
                        //    restore_tab_snapshots + hydrate AGAIN →
                        //    double restore.
                        self.apply_recovery_choice(RecoveryChoice::Restore);
                        return StartupRecoveryOutcome::Pending;
                    }
                    RecoveryGate::NeverIgnore => {
                        // "never" replaces the prompt with a synchronous
                        // Ignore: no restore, and the snapshot is superseded
                        // by the current session's auto-snapshot. NOT Delete
                        // — permanent deletion stays a manual action.
                        self.pending_recovery = Some(snapshot);
                        info!("skipping crash-recovery prompt (session.recovery=never)");
                        // Same non-Normal contract as the auto arm above.
                        self.apply_recovery_choice(RecoveryChoice::Ignore);
                        return StartupRecoveryOutcome::Pending;
                    }
                    RecoveryGate::Prompt => {}
                }
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
                    // v1.11.12 (PLAN_v11112 M-C): decision-loop send — a
                    // failure here leaves the recovery prompt choice undelivered
                    // and startup recovery pending forever, so it must leave a
                    // trace (unlike the by-design-silent wakeups).
                    // (if-let instead of inspect_err: MSRV 1.75 < 1.76)
                    if let Err(e) = proxy.send_event(AppEvent::RecoveryChosen(choice)) {
                        warn!(error = %e, "send_event failed: recovery choice lost; startup recovery stays pending");
                    }
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
        // v1.11.13 (PLAN_v11113 §M2): recovery-path restore complete —
        // flip the activation gate + replay queued cold-start clicks
        // against the now-hydrated block ids.
        self.finish_restore_notifications();
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
#[path = "recovery_controller/tests.rs"]
mod tests;
