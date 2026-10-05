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
        Arc::new(weft_core::block_id_sequence::BlockIdPool::new(
            100,
            100 + 4096,
        )),
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

// ── v1.12.24 (N-2 + N-3): lineage snapshot × hydrate recall ────────────

use weft_core::blocks::{Block, BlockId};

/// Minimal persisted-style block, mirroring the shape `BlockStore` queries
/// return (local twin of the inline tests' `block()` helper — child modules
/// cannot see each other's private items).
fn persisted_block(id: u64, command: &str) -> Block {
    Block {
        id: BlockId(id),
        command: command.to_string(),
        cwd: None,
        output: String::new().into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id),
        finished_at: Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(id)),
        collapsed: false,
        screen_origin: false,
    }
}

/// The user's 30-second scenario, miniaturized. Restore hydrates generation 1
/// (blocks 1-3), a fresh command finalizes generation 2 (block 4), and the
/// pre-removal snapshot's lineage ids (what `persist_tabs_snapshot_now` →
/// `save_tabs` now lands in the tabs DB; pre-fix: session-only ids [4])
/// rehydrate a fresh terminal into recalling BOTH generations on ↑ —
/// previously every restart-restore cycle dropped one recall generation.
#[test]
fn lineage_snapshot_rehydrates_recall_for_both_generations() {
    use std::sync::Arc;

    // Generation 1 in the DB (newest-first): blocks 1-3.
    let newest_first = vec![
        persisted_block(3, "❯ three"),
        persisted_block(2, "❯ two"),
        persisted_block(1, "❯ one"),
    ];
    let allocator = Arc::new(weft_core::block_id_sequence::BlockIdPool::new(4, 4 + 4096));
    let mut terminal_a = Terminal::new(24, 80);
    hydrate_persisted_history(&mut terminal_a, &newest_first, &[1, 2, 3], allocator);
    assert_eq!(terminal_a.editor().history(), ["three", "two", "one"]);

    // Generation 2: the user runs "four" this session (headless).
    let tracker = terminal_a.block_tracker_mut();
    tracker.on_prompt_start();
    tracker.on_command_start("four".to_string());
    tracker.on_command_end(0);
    // The snapshot written by the pre-removal save carries both generations.
    assert_eq!(terminal_a.block_tracker().lineage_block_ids(), [1, 2, 3, 4]);

    // Restart: a fresh hydrate from the saved lineage ids recalls both.
    let mut db_newest_first = vec![persisted_block(4, "four")];
    db_newest_first.extend(newest_first);
    let allocator_b = Arc::new(weft_core::block_id_sequence::BlockIdPool::new(5, 5 + 4096));
    let mut terminal_b = Terminal::new(24, 80);
    hydrate_persisted_history(
        &mut terminal_b,
        &db_newest_first,
        &[1, 2, 3, 4],
        allocator_b,
    );
    assert_eq!(
        terminal_b.editor().history(),
        ["four", "three", "two", "one"],
        "↑ recall after restart must include BOTH generations"
    );
}
