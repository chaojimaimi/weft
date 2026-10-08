//! Log-rotation unit tests (v1.12.24 P0-05).
//!
//! Own file per the repo test-module convention (see `retention_tests.rs`):
//! keeps `app_runtime.rs` at its architecture-gate ceiling while the
//! plan-mandated `rotate_log_if_huge` coverage still ships. Child-module
//! privacy reaches the private fn via `super::*`.

use super::*;

fn temp_log_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "weft-log-rotate-{}-{}-{}.log",
        tag,
        std::process::id(),
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap()
            .as_nanos()
    ))
}

/// v1.12.24 (P0-05): a >32 MiB log rotates at startup — the original path
/// disappears and one `.log.1` generation appears carrying the old bytes.
#[test]
fn huge_log_rotates_to_dot_log_1() {
    let log_path = temp_log_path("huge");
    let rotated_path = log_path.with_extension("log.1");
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&rotated_path);
    // 32 MiB + 1 byte → strictly above the threshold.
    let blob = vec![b'x'; 32 * 1024 * 1024 + 1];
    std::fs::write(&log_path, &blob).unwrap();

    rotate_log_if_huge(&log_path);

    assert!(!log_path.exists(), "the oversized log must move away");
    assert_eq!(
        std::fs::read(&rotated_path).unwrap(),
        blob,
        "the .log.1 generation must carry the old bytes"
    );
    let _ = std::fs::remove_file(&rotated_path);
}

/// A small (or absent) log must be left untouched — no spurious rotation.
#[test]
fn small_log_is_left_in_place() {
    let log_path = temp_log_path("small");
    let rotated_path = log_path.with_extension("log.1");
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&rotated_path);
    std::fs::write(&log_path, b"fresh session\n").unwrap();

    rotate_log_if_huge(&log_path);

    assert!(log_path.exists(), "a small log must stay where it is");
    assert!(
        !rotated_path.exists(),
        "no .log.1 generation for a small log"
    );
    // Absent path: must be a silent no-op (first launch).
    let missing = temp_log_path("missing");
    rotate_log_if_huge(&missing);
    assert!(!missing.exists());
    let _ = std::fs::remove_file(&log_path);
}

/// T15e (PLAN_v11217 §3.10): a `.log.1` older than 7 days is deleted at
/// startup; an in-window generation is kept; an absent one is a no-op.
/// The old case backdates the mtime via `File::set_times` (stable 1.75) —
/// rename preserves mtime, so this is the real on-disk shape.
#[test]
fn stale_rotated_log_removed_fresh_generation_kept() {
    let log_path = temp_log_path("stale");
    let rotated_path = log_path.with_extension("log.1");
    let _ = std::fs::remove_file(&rotated_path);

    std::fs::write(&rotated_path, b"old generation").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 60 * 60);
    let handle = std::fs::File::options()
        .write(true)
        .open(&rotated_path)
        .unwrap();
    handle
        .set_times(std::fs::FileTimes::new().set_modified(old))
        .expect("mtime backdating");
    drop(handle);

    remove_stale_rotated_log(&log_path);
    assert!(!rotated_path.exists(), "a >7-day .log.1 must be deleted");

    std::fs::write(&rotated_path, b"fresh generation").unwrap();
    remove_stale_rotated_log(&log_path);
    assert!(rotated_path.exists(), "an in-window .log.1 must be kept");

    // Absent .log.1: silent no-op.
    let _ = std::fs::remove_file(&rotated_path);
    remove_stale_rotated_log(&log_path);
    assert!(!rotated_path.exists());
    let _ = std::fs::remove_file(&log_path);
}

/// T15e: the staleness predicate truth table — old deletes, in-window
/// keeps, and a future mtime (clock skew) never deletes.
#[test]
fn rotated_log_staleness_truth_table() {
    let now = std::time::SystemTime::now();
    let day = 24 * 60 * 60;
    assert!(rotated_log_is_stale(
        now - std::time::Duration::from_secs(8 * day),
        now
    ));
    assert!(!rotated_log_is_stale(
        now - std::time::Duration::from_secs(day),
        now
    ));
    assert!(!rotated_log_is_stale(
        now + std::time::Duration::from_secs(3_600),
        now
    ));
}
