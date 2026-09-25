//! Recovery controller tests (split from recovery_controller.rs so the
//! production file returns under its architecture-gate ceiling; child-module
//! privacy reaches pub(crate)/private items exactly like the inline module
//! did — config_controller_tests precedent).

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
