//! app_runtime unit tests, moved verbatim out of `app_runtime.rs`'s
//! inline `mod tests` (v1.13.9 backlog clear zero-behavior file-budget
//! split; repo test-module convention — gate-exempt sibling module).

use super::diagnostics::{panic_log_path, terminal_capability_env};
use super::hydrate::hydrate_persisted_history;
use super::*;

#[test]
fn panic_log_uses_the_user_library_logs_directory() {
    assert_eq!(
        panic_log_path(Some(std::ffi::OsStr::new("/Users/test"))),
        std::path::PathBuf::from("/Users/test/Library/Logs/Weft/panic.log")
    );
}

#[test]
fn resize_commit_keeps_primary_screen_content_after_dimension_only_resize() {
    // v1.10.25 Batch 3 (FIX_SELECTION_AND_RESIZE_REMAINING): the resize
    // path used to clear_screen_all() before the omp repaint, rendering a
    // blank frame (killing CA's stretch transition). Resize is now
    // dimension-only — the old content must survive the commit untouched.
    let mut terminal = Terminal::new(8, 40);
    terminal.process(b"\x1b]7;file://localhost/Users/me/Claude\x07");
    terminal.process(b"\x1b]133;A\x07claude\x1b]133;B\x07\x1b]133;C\x07");
    terminal.process(b"\x1b[?2026h\x1b[2J\x1b[HNEW ICON\x1b[?2026l");
    assert!(terminal.primary_screen_repaint_capable());
    let mut pending = None;
    assert!(commit_pty_resize_result(
        &mut terminal,
        &mut pending,
        (10, 50),
        true
    ));
    assert_eq!(
        (terminal.grid().num_rows, terminal.grid().num_cols),
        (10, 50),
        "dimension-only resize must still commit"
    );
    assert_eq!(terminal.grid().row_text(0), "NEW ICON");
    assert_eq!(terminal.cwd(), Some("/Users/me/Claude"));
    assert_eq!(
        terminal
            .block_tracker()
            .in_flight()
            .map(|live| live.command),
        Some("claude")
    );
}

#[test]
fn terminal_capabilities_override_a_bad_launcher_environment() {
    let env = terminal_capability_env();
    let value = |key: &str| env.iter().find(|(name, _)| name == key).map(|(_, v)| v);
    assert_eq!(value("TERM").map(String::as_str), Some("xterm-256color"));
    assert_eq!(value("COLORTERM").map(String::as_str), Some("truecolor"));
    assert_eq!(value("CLICOLOR").map(String::as_str), Some("1"));
    assert_eq!(value("TERM_PROGRAM").map(String::as_str), Some("Weft"));
}

#[test]
fn unsupported_shell_still_receives_terminal_capabilities() {
    let env = crate::shell_integration_env("/usr/local/bin/fish");
    assert!(env
        .iter()
        .any(|pair| pair == &("TERM".into(), "xterm-256color".into())));
    assert!(env
        .iter()
        .any(|pair| pair == &("COLORTERM".into(), "truecolor".into())));
    assert!(env
        .iter()
        .any(|pair| pair == &("CLICOLOR".into(), "1".into())));
}

use std::time::SystemTime;
use weft_core::blocks::{Block, BlockId};

#[test]
fn synchronized_output_watchdog_wakes_without_another_pty_event() {
    let pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    assert!(schedule_synchronized_output_watchdog(
        pending.clone(),
        std::time::Duration::from_millis(1),
        move || tx.send(()).unwrap()
    ));
    assert!(!schedule_synchronized_output_watchdog(
        pending.clone(),
        std::time::Duration::from_millis(1),
        || {}
    ));
    rx.recv_timeout(std::time::Duration::from_millis(100))
        .unwrap();
    assert!(!pending.load(Ordering::Acquire));
}

fn block(id: u64, command: &str) -> Block {
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: String::new().into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id),
        finished_at: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id)),
        collapsed: false,
        screen_origin: false,
    }
}

#[test]
fn hydrate_gives_editor_and_tracker_only_tab_blocks() {
    // FIX ② (2026-08-19): editor ↑-key recall is per-tab (consistent with
    // block tracker and Warp semantics), not global.
    let mut newest_first = vec![block(2, "❯ echo newest"), block(1, "❯ echo oldest")];
    newest_first[0].output = "large persisted output".repeat(1024).into();
    let allocator =
        std::sync::Arc::new(weft_core::block_id_sequence::BlockIdPool::new(3, 3 + 4096));
    let mut terminal_a = Terminal::new(24, 80);
    let mut terminal_b = Terminal::new(24, 80);

    // Tab A produced blocks [1, 2]; tab B produced only block [1].
    hydrate_persisted_history(&mut terminal_a, &newest_first, &[1, 2], allocator.clone());
    hydrate_persisted_history(&mut terminal_b, &newest_first, &[1], allocator);

    // Both editors now see only their tab's own commands (per-tab isolation).
    assert_eq!(
        terminal_a.editor().history(),
        ["echo newest", "echo oldest"]
    );
    assert_eq!(
        terminal_b.editor().history(),
        ["echo oldest"] // block 2 is NOT in tab B's history
    );

    // Block trackers are per-tab isolated.
    let ids_a: Vec<_> = terminal_a
        .block_tracker()
        .blocks()
        .iter()
        .map(|b| b.id.0)
        .collect();
    let ids_b: Vec<_> = terminal_b
        .block_tracker()
        .blocks()
        .iter()
        .map(|b| b.id.0)
        .collect();
    assert_eq!(ids_a, [1, 2]);
    assert_eq!(ids_b, [1]);

    // Shared Arc<str> output (no clone of the large string).
    assert!(std::sync::Arc::ptr_eq(
        &terminal_a.block_tracker().blocks()[0].output,
        &terminal_b.block_tracker().blocks()[0].output,
    ));
}

#[test]
fn hydrate_with_empty_block_ids_loads_nothing_into_tracker() {
    // FIX ② (2026-08-19): legacy snapshots (no block_ids) → both block tracker
    // and editor history stay empty (per-tab isolation).
    let newest_first = vec![block(1, "❯ ls"), block(2, "❯ echo hi")];
    let allocator =
        std::sync::Arc::new(weft_core::block_id_sequence::BlockIdPool::new(3, 3 + 4096));
    let mut terminal = Terminal::new(24, 80);
    hydrate_persisted_history(&mut terminal, &newest_first, &[], allocator);
    assert!(terminal.block_tracker().blocks().is_empty());
    assert_eq!(terminal.editor().history().len(), 0); // no block_ids → empty history
}

/// v1.8.9 regression test for the recovery-restore history bug.
///
/// Before the fix, `restore_workspace` (recovery Restore button) rebuilt
/// tabs + cwds but left `restored_snapshot = None`, so the hydration loop
/// passed empty `block_ids` and the block tracker (sidebar history) was
/// empty — while the Ignore path (which goes through
/// `restore_tab_snapshots`) loaded the full history. This test verifies
/// the core invariant: when `restored_snapshot` carries `block_ids`,
/// hydrate populates the block tracker; when it's absent, it doesn't.
#[test]
fn hydrate_with_recovered_block_ids_populates_tracker() {
    let newest_first = vec![block(10, "❯ git status"), block(20, "❯ make test")];
    let allocator = std::sync::Arc::new(weft_core::block_id_sequence::BlockIdPool::new(
        30,
        30 + 4096,
    ));

    // Simulate the post-fix recovery path: restored_snapshot was attached
    // with this tab's block_ids = [10, 20].
    let mut terminal = Terminal::new(24, 80);
    hydrate_persisted_history(&mut terminal, &newest_first, &[10, 20], allocator.clone());
    assert_eq!(
        terminal.block_tracker().blocks().len(),
        2,
        "recovery restore with attached block_ids should hydrate the tracker"
    );
    assert_eq!(
        terminal.editor().history().len(),
        2,
        "editor history should also be populated (per-tab isolation)"
    );

    // Contrast with the pre-fix recovery path: restored_snapshot was None,
    // yielding empty block_ids and an empty tracker.
    let mut terminal_bare = Terminal::new(24, 80);
    hydrate_persisted_history(&mut terminal_bare, &newest_first, &[], allocator);
    assert!(
        terminal_bare.block_tracker().blocks().is_empty(),
        "empty block_ids (pre-fix recovery path) leaves the tracker empty"
    );
    assert_eq!(
        terminal_bare.editor().history().len(),
        0,
        "empty block_ids (pre-fix recovery path) also leaves editor history empty"
    );
}

/// v1.10.24 B1 chain regression: the real Restore order (workspace
/// restore sets the cwd fallback, then `attach_recovery_tab_snapshots`
/// attaches the persisted snapshot) must deliver `block_ids` to the
/// hydration loop — the exact connection the v1.8.9 stub-snapshot no-op
/// broke. Extends the hydrate-only test above with the Tab-level chain:
/// `attach_recovery_snapshot` must succeed after
/// `set_restored_cwd_fallback` so `hydrate_tabs_from_history_store`
/// reads the real `block_ids` off `restored_snapshot`.
#[test]
fn cwd_fallback_then_attach_hydrates_tracker_from_injected_block_ids() {
    let mut tab = crate::tab::Tab::empty();
    tab.set_terminal_for_test(Terminal::with_scrollback(24, 80, 1000));

    // Workspace restore: cwd fallback only — no stub snapshot.
    tab.set_restored_cwd_fallback(Some("/saved".into()));
    assert!(
        tab.restored_snapshot.is_none(),
        "v1.8.9 no-op regression: no stub snapshot from the cwd fallback"
    );

    // attach_recovery_tab_snapshots: the real attach must succeed.
    assert!(
        tab.attach_recovery_snapshot(&weft_core::persistence::TabSnapshot {
            position: 0,
            active: false,
            cwd: Some("/saved".into()),
            block_scroll_offset: 0,
            editor_buffer: String::new(),
            shell_phase: "AtPrompt".into(),
            block_ids: vec![10, 20],
            panes: None,
        })
    );

    // hydrate_tabs_from_history_store reads block_ids off the snapshot.
    let tab_block_ids: Vec<u64> = tab
        .restored_snapshot
        .as_ref()
        .map(|snap| snap.block_ids.clone())
        .unwrap_or_default();
    assert_eq!(
        tab_block_ids,
        vec![10, 20],
        "the attached snapshot must inject block_ids (v1.8.9 chain)"
    );

    let newest_first = vec![block(10, "❯ git status"), block(20, "❯ make test")];
    let allocator = std::sync::Arc::new(weft_core::block_id_sequence::BlockIdPool::new(
        30,
        30 + 4096,
    ));
    let mut guard = tab.lock_terminal().unwrap();
    hydrate_persisted_history(&mut guard, &newest_first, &tab_block_ids, allocator);
    // T10 P1 (D9 rule 2): release the guard before the re-locking assert.
    drop(guard);
    assert_eq!(
        tab.lock_terminal().unwrap().block_tracker().blocks().len(),
        2,
        "injected block_ids must hydrate the tracker (v1.8.9 chain)"
    );
}

/// FIX ② (2026-08-19): Verify that per-tab filtering preserves newest-first order.
/// When a tab has blocks [2, 5, 10] from a global list [1..20], the editor history
/// must still show them newest-first (10 → 5 → 2), not chronological (2 → 5 → 10).
#[test]
fn hydrate_preserves_newest_first_order_after_filtering() {
    // Global history: blocks 1 (oldest) through 10 (newest)
    let mut newest_first = Vec::new();
    for i in (1..=10).rev() {
        newest_first.push(block(i, &format!("❯ cmd{}", i)));
    }

    let allocator = std::sync::Arc::new(weft_core::block_id_sequence::BlockIdPool::new(
        11,
        11 + 4096,
    ));
    let mut terminal = Terminal::new(24, 80);

    // Tab only produced blocks [2, 5, 10] (non-contiguous, not the newest globally)
    hydrate_persisted_history(&mut terminal, &newest_first, &[2, 5, 10], allocator);

    // Editor history must preserve newest-first order among the tab's blocks:
    // global order is 10→5→2 (since 10 is newest, 5 is middle, 2 is oldest among them)
    assert_eq!(
        terminal.editor().history(),
        ["cmd10", "cmd5", "cmd2"],
        "filtering must preserve newest-first order (not chronological)"
    );

    // Block tracker should also have the correct order
    let ids: Vec<_> = terminal
        .block_tracker()
        .blocks()
        .iter()
        .map(|b| b.id.0)
        .collect();
    assert_eq!(ids, [2, 5, 10], "block tracker expects chronological order");
}
