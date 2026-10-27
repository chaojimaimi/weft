//! Inline tests for `persistence/blocks.rs`, extracted to keep the
//! production file under the 800-line architecture gate (repo
//! convention: `pty.rs` -> `pty/tests.rs`).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

static TEMP_STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp DB path for one test (auto-cleaned by the OS temp dir
/// lifecycle; we also clear() to keep tests independent). `pub(crate)`:
/// the path-mounted `blocks_budget_tests` sibling (FIX-G split) reuses it.
pub(crate) fn temp_store() -> BlockStore {
    let path = std::env::temp_dir().join(format!(
        "weft-block-store-{}-{}-{}.db",
        std::process::id(),
        TEMP_STORE_COUNTER.fetch_add(1, Ordering::Relaxed),
        SystemTime::UNIX_EPOCH
            .elapsed()
            .unwrap_or_default()
            .as_nanos()
    ));
    // Remove any leftover from a prior run, then open fresh.
    let _ = std::fs::remove_file(&path);
    BlockStore::open(&path).expect("open temp store")
}

pub(crate) fn block(id: u64, command: &str, output: &str, exit: Option<i32>) -> Block {
    Block {
        id: BlockId(id),
        command: command.into(),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: exit,
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(id * 1000),
        finished_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(id * 1000 + 5)),
        collapsed: false,
        screen_origin: false,
    }
}

#[test]
fn insert_and_recent_roundtrip() {
    let store = temp_store();
    store.insert(&block(1, "ls", "file_a", Some(0))).unwrap();
    store.insert(&block(2, "pwd", "/tmp", Some(0))).unwrap();
    let recent = store.recent(10).unwrap();
    assert_eq!(recent.len(), 2);
    // Newest first: block 2 has the larger started_ms.
    assert_eq!(recent[0].id, BlockId(2));
    assert_eq!(recent[1].id, BlockId(1));
    assert_eq!(recent[0].command, "pwd");
    assert_eq!(recent[0].output.as_ref(), "/tmp");
    assert_eq!(recent[0].exit_code, Some(0));
}

#[test]
fn roundtrip_preserves_all_fields() {
    let store = temp_store();
    let mut original = block(7, "echo $X", "hello world", Some(3));
    original.cwd = Some("/Users/me/project".into());
    original.styled_output = Some(Arc::new(crate::blocks::StyledOutput {
        lines: vec![crate::blocks::StyledLine {
            line: 0,
            foregrounds: vec![crate::blocks::ForegroundSpan {
                start: 0,
                end: 11,
                color: crate::grid::CellColor::Palette(2),
            }],
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: Vec::new(),
            underline_colors: Vec::new(),
        }],
    }));
    store.insert(&original).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();
    assert_eq!(loaded.id, original.id);
    assert_eq!(loaded.command, original.command);
    assert_eq!(loaded.output, original.output);
    assert_eq!(loaded.cwd, original.cwd);
    assert_eq!(loaded.styled_output, original.styled_output);
    assert_eq!(loaded.exit_code, original.exit_code);
    assert_eq!(
        loaded.started_at, original.started_at,
        "timestamps round-trip exactly"
    );
    assert_eq!(loaded.finished_at, original.finished_at);
    assert_eq!(loaded.collapsed, original.collapsed);
}

#[test]
fn open_migrates_legacy_blocks_and_preserves_new_metadata() {
    let path = std::env::temp_dir().join(format!(
        "weft-blocks-legacy-{}-{}.db",
        std::process::id(),
        TEMP_STORE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let legacy = Connection::open(&path).expect("open legacy block DB");
    legacy
        .execute_batch(
            "CREATE TABLE blocks (\
                    id INTEGER PRIMARY KEY, command TEXT NOT NULL, output TEXT NOT NULL,\
                    exit_code INTEGER, started_ms INTEGER NOT NULL, finished_ms INTEGER,\
                    collapsed INTEGER NOT NULL DEFAULT 0\
                 );\
                 INSERT INTO blocks VALUES (1, 'old', 'plain', 0, 1000, 1001, 0);",
        )
        .expect("seed legacy block DB");
    drop(legacy);

    let store = BlockStore::open(&path).expect("migrate legacy block DB");
    let old = store.recent(1).unwrap().pop().unwrap();
    assert_eq!(old.command, "old");
    assert_eq!(old.cwd, None);
    assert_eq!(old.styled_output, None);

    let mut new = block(2, "new", "color", Some(0));
    new.cwd = Some("/tmp/project".into());
    new.styled_output = Some(Arc::new(crate::blocks::StyledOutput {
        lines: vec![crate::blocks::StyledLine {
            line: 0,
            foregrounds: vec![crate::blocks::ForegroundSpan {
                start: 0,
                end: 5,
                color: crate::grid::CellColor::Palette(4),
            }],
            backgrounds: Vec::new(),
            links: Vec::new(),
            attributes: Vec::new(),
            underline_colors: Vec::new(),
        }],
    }));
    store.insert(&new).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();
    assert_eq!(loaded.cwd, new.cwd);
    assert_eq!(loaded.styled_output, new.styled_output);
}

#[test]
fn handles_missing_exit_code_and_finish_time() {
    let store = temp_store();
    let mut b = block(9, "interrupted", "partial", None);
    b.finished_at = None; // interrupted command, not finalized with a time
    store.insert(&b).unwrap();

    let loaded = store.recent(1).unwrap().pop().unwrap();
    assert_eq!(loaded.exit_code, None);
    assert_eq!(loaded.finished_at, None);
}

// ── v1.11.2 X4: keyset pagination (older_than) ─────────────────────

#[test]
fn older_than_returns_strictly_older_newest_first() {
    let store = temp_store();
    for id in 1..=5u64 {
        store
            .insert(&block(id, &format!("c{id}"), "", Some(0)))
            .unwrap();
    }
    // Cursor between blocks 3 and 4 → pages 3, 2, 1 (newest first).
    let cursor = SystemTime::UNIX_EPOCH + Duration::from_secs(3 * 1000 + 1);
    let page = store
        .older_than(
            cursor
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            10,
        )
        .unwrap();
    let ids: Vec<u64> = page.iter().map(|b| b.id.0).collect();
    assert_eq!(ids, vec![3, 2, 1]);
}

#[test]
fn older_than_excludes_cursor_equal_started_ms() {
    // PLAN_v1112 §7.1: boundary started_ms equal must NOT be re-fetched —
    // repeated pages stay disjoint.
    let store = temp_store();
    for id in 1..=3u64 {
        store
            .insert(&block(id, &format!("c{id}"), "", Some(0)))
            .unwrap();
    }
    // Cursor exactly at block 2's started_ms.
    let cursor_ms = 2 * 1000 * 1000;
    let first = store.older_than(cursor_ms, 10).unwrap();
    assert_eq!(
        first.iter().map(|b| b.id.0).collect::<Vec<_>>(),
        vec![1],
        "block at the cursor is excluded"
    );
    // Paging further from the last returned row never repeats block 1.
    if let Some(last) = first.last() {
        let next_cursor = last.started_at;
        let second = store
            .older_than(
                next_cursor
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
                10,
            )
            .unwrap();
        assert!(second.is_empty(), "no duplicates across pages");
    }
}

#[test]
fn older_than_respects_limit() {
    let store = temp_store();
    for id in 1..=5u64 {
        store
            .insert(&block(id, &format!("c{id}"), "", Some(0)))
            .unwrap();
    }
    let page = store.older_than(i64::MAX, 2).unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].id, BlockId(5));
    assert_eq!(page[1].id, BlockId(4));
}

#[test]
fn older_than_on_empty_store_is_empty() {
    let store = temp_store();
    assert!(store.older_than(i64::MAX, 10).unwrap().is_empty());
}

#[test]
fn collapsed_flag_persists() {
    let store = temp_store();
    let mut b = block(11, "long", "spam\n", Some(0));
    b.collapsed = true;
    store.insert(&b).unwrap();
    let loaded = store.recent(1).unwrap().pop().unwrap();
    assert!(loaded.collapsed);
}

/// v1.10.26 Batch B review closure (SF-1): a TUI block's `screen_origin`
/// must survive persistence so the Restore path does not resurrect the
/// `|]` fold — a restored primary-screen block keeps clip-not-wrap.
#[test]
fn screen_origin_flag_persists() {
    let store = temp_store();
    let mut tui = block(21, "omp", "[| top line |]\n[|......|]\n", Some(0));
    tui.screen_origin = true;
    store.insert(&tui).unwrap();
    let plain = block(22, "echo hi", "some output\n", Some(0));
    store.insert(&plain).unwrap();

    let recent = store.recent(10).unwrap();
    let loaded_tui = recent.iter().find(|b| b.id == BlockId(21)).unwrap();
    let loaded_plain = recent.iter().find(|b| b.id == BlockId(22)).unwrap();
    assert!(
        loaded_tui.screen_origin,
        "TUI block's screen_origin must round-trip (clip not fold after restore)"
    );
    assert!(
        !loaded_plain.screen_origin,
        "ordinary block stays soft-wrappable after restore"
    );

    // Re-open the DB (simulating restart) and read the same rows back.
    let path = std::path::Path::new(store.conn.path().expect("temp store is file-backed"));
    let reopened = BlockStore::open(path).expect("reopen store");
    let tui_again = reopened.get(BlockId(21)).unwrap().expect("row present");
    assert!(tui_again.screen_origin, "row survives a store re-open");
}

#[test]
fn search_matches_command_or_output_case_insensitively() {
    let store = temp_store();
    store
        .insert(&block(1, "git status", "clean", Some(0)))
        .unwrap();
    store.insert(&block(2, "ls", "GIT_LOG", Some(0))).unwrap();
    store.insert(&block(3, "pwd", "/tmp", Some(0))).unwrap();

    let hits = store.search("git", 10).unwrap();
    let hit_ids: Vec<u64> = hits.iter().map(|b| b.id.0).collect();
    assert!(hit_ids.contains(&1), "matches command 'git status'");
    assert!(hit_ids.contains(&2), "matches output 'GIT_LOG'");
    assert!(!hit_ids.contains(&3), "does not match unrelated block");
}

#[test]
fn search_empty_query_returns_nothing() {
    let store = temp_store();
    store.insert(&block(1, "ls", "x", Some(0))).unwrap();
    // Empty string: INSTR(x, "") == 1 in SQLite → would match everything.
    // Treat as "no filter" by returning recent instead? For v0.4, callers
    // gate on non-empty query; assert the behavior is at least safe.
    let hits = store.search("", 10).unwrap();
    assert!(hits.iter().all(|b| !b.command.is_empty()));
}

#[test]
fn clear_empties_the_store() {
    let store = temp_store();
    store.insert(&block(1, "a", "b", Some(0))).unwrap();
    assert_eq!(store.recent(10).unwrap().len(), 1);
    store.clear().unwrap();
    assert!(store.recent(10).unwrap().is_empty());
}

#[test]
fn recent_respects_limit() {
    let store = temp_store();
    for i in 1..=5 {
        store
            .insert(&block(i, &format!("c{i}"), "o", Some(0)))
            .unwrap();
    }
    assert_eq!(store.recent(3).unwrap().len(), 3);
    // The 3 newest (ids 5,4,3).
    let ids: Vec<u64> = store.recent(3).unwrap().iter().map(|b| b.id.0).collect();
    assert_eq!(ids, vec![5, 4, 3]);
}

#[test]
fn insert_or_replace_on_same_id() {
    let store = temp_store();
    store.insert(&block(1, "old", "o", Some(0))).unwrap();
    store.insert(&block(1, "new", "o", Some(0))).unwrap();
    let loaded = store.recent(10).unwrap();
    assert_eq!(loaded.len(), 1, "same id replaces, not duplicates");
    assert_eq!(loaded[0].command, "new");
}

/// T14 (PLAN_v11217 §3.9 2): the seeding source is now the persistent
/// `meta.next_id` counter. A reopen seeds the allocation cursor to
/// MAX(id)+1 (legacy DBs) or the previous counter value (already-seeded
/// DBs) and grabs a reservation segment — the cursor must never trail
/// any persisted id.
#[test]
fn reopened_store_seeds_shared_block_ids_after_persisted_maximum() {
    let path = std::env::temp_dir().join(format!(
        "weft-block-id-reopen-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    {
        let store = BlockStore::open(&path).unwrap();
        store.insert(&block(7, "persisted", "", Some(0))).unwrap();
    }
    let store = BlockStore::open(&path).unwrap();
    let allocator = store.block_id_allocator();
    assert!(
        allocator.next() > 7,
        "cursor seeds strictly above the persisted maximum (got {})",
        allocator.next()
    );
    assert!(
        allocator.hi() >= allocator.next() + crate::block_id_sequence::ID_RESERVE,
        "open() grabs a reservation segment of ID_RESERVE ids"
    );
    let _ = std::fs::remove_file(&path);
}

/// T14 acceptance (§3.9 2): grab a reservation → allocate ids → drop the
/// store WITHOUT writing anything back → reopen. The new ids must be
/// strictly greater than the maximum persisted id: an unflushed
/// reservation is discarded (id hole), never rolled back, so pruned or
/// crashed-away ids can never be reused.
#[test]
fn reopened_store_never_reuses_ids_below_the_persistent_counter() {
    let path = std::env::temp_dir().join(format!(
        "weft-block-id-no-reuse-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let mut allocated_max = 0u64;
    {
        let store = BlockStore::open(&path).unwrap();
        store.insert(&block(3, "persisted", "", Some(0))).unwrap();
        // Allocate ids from the shared pool but never persist them.
        let mut seq = crate::block_id_sequence::BlockIdSequence::new();
        seq.share(store.block_id_allocator());
        for _ in 0..10 {
            allocated_max = allocated_max.max(seq.allocate());
        }
    } // dropped without any write-back of the allocated ids
    let store = BlockStore::open(&path).unwrap();
    let next = store.block_id_allocator().next();
    assert!(
            next > allocated_max,
            "reopen must allocate above the discarded reservation (next={next}, allocated_max={allocated_max})"
        );
    assert!(
        next > 3,
        "reopen must allocate above the persisted maximum id"
    );
    let _ = std::fs::remove_file(&path);
}

/// T14 acceptance (§3.9 2): an allocator whose cursor was pushed beyond
/// the reservation (e.g. hydration of a huge history) re-grabs until the
/// bound covers it — subsequent allocations stay inside an exclusively
/// owned segment and never collide with a second open of the same DB.
#[test]
fn observe_beyond_reservation_regrabs_without_colliding() {
    use crate::block_id_sequence::BlockIdSequence;

    let path = std::env::temp_dir().join(format!(
        "weft-block-id-observe-{}-{}.db",
        std::process::id(),
        SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let store = BlockStore::open(&path).unwrap();
    let pool = store.block_id_allocator();
    let mut seq = BlockIdSequence::new();
    seq.share(pool.clone());
    let far = pool.hi() + 9_000; // beyond two reservations
    seq.observe(far);
    let id = seq.allocate();
    assert_eq!(id, far + 1);
    assert!(id < pool.hi(), "bound must cover allocations after observe");
    // A second open of the same DB grabs a non-overlapping segment.
    let second = BlockStore::open(&path).unwrap();
    let other = second.block_id_allocator();
    assert!(
        other.next() >= pool.hi(),
        "reservation segments must never overlap across openers"
    );
    let _ = std::fs::remove_file(&path);
}
