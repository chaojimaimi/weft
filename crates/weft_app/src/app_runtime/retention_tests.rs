//! Retention × hydration round-trip coverage (v1.11.2 X4, PLAN_v1112 §7.1).
//!
//! Extracted from `app_runtime.rs`'s inline test module to keep that file
//! under its architecture-gate ceiling; child-module privacy still reaches
//! `hydrate_persisted_history` via `super::*`.

use super::*;

/// v1.11.2 X4 round-trip (PLAN_v1112 §7.1 / §8 top risk): save snapshot →
/// retention evicts blocks from memory → restart hydrate must STILL put
/// every DB block back into the SAME tab. `session_produced_block_ids`
/// folding in `evicted_ids` is what guarantees this — if that union
/// regresses, this test fails with blocks silently dropped from restore.
#[test]
fn retention_evicted_blocks_still_hydrate_to_their_tab() {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    // ── Session 1: produce 4 blocks, retention keeps only 2 in memory.
    let mut terminal = Terminal::with_scrollback(24, 80, 100);
    let mut drained = Vec::new();
    {
        let tracker = terminal.block_tracker_mut();
        tracker.set_retained_limit(2);
        for i in 0..4 {
            tracker.on_prompt_start();
            tracker.on_command_start(format!("cmd{i}"));
            tracker.on_command_end(0);
            drained.extend(tracker.drain_unpersisted());
        }
    }
    assert_eq!(terminal.block_tracker().blocks().len(), 2);
    assert_eq!(
        drained.len(),
        4,
        "every finished block reached the DB queue"
    );

    // Persist exactly what the drain produced (the app's real flow).
    let store = {
        let path = std::env::temp_dir().join(format!(
            "weft-retention-roundtrip-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        BlockStore::open(&path).expect("open temp block store")
    };
    for b in &drained {
        store.insert(b).unwrap();
    }

    // Snapshot ids must reference ALL four — including the two evicted
    // from the in-memory Vec.
    let snap_block_ids = terminal.block_tracker().session_produced_block_ids();
    for id in 1u64..=4 {
        assert!(
            snap_block_ids.contains(&id),
            "snapshot must still reference evicted block {id}"
        );
    }

    // ── "Restart": a fresh terminal hydrates from SQLite filtered by
    // the saved per-tab snapshot (the app's startup path).
    let mut restarted = Terminal::with_scrollback(24, 80, 100);
    let persisted = store.recent(1000).unwrap();
    assert_eq!(persisted.len(), 4);
    hydrate_persisted_history(
        &mut restarted,
        &persisted,
        &snap_block_ids,
        Arc::new(AtomicU64::new(100)),
    );
    let hydrated = restarted.block_tracker().blocks();
    assert_eq!(
        hydrated.len(),
        4,
        "all four DB blocks — including evicted-from-memory ones — \
         must return to their original tab"
    );
    let commands: Vec<&str> = hydrated.iter().map(|b| b.command.as_str()).collect();
    assert_eq!(commands, vec!["cmd0", "cmd1", "cmd2", "cmd3"]);
}
