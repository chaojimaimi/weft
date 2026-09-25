//! Tests for the T14 block-library prune (split from `prune.rs` to keep
//! the implementation file within its architecture-gate budget; `#[path]`
//! keeps it a child module so `super::*` privacy works unchanged).

use super::*;
use crate::blocks::annotations::{AnnotationStore, BlockAnnotation};
use crate::blocks::{Block, BlockId};
use crate::persistence::tabs::TabSnapshot;
use crate::search::{SearchDocument, SearchIndex, SearchQuery};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "weft-prune-{tag}-{}-{}.db",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Fixed synthetic clock: T0 plus `days`.
fn ms_at(t0: i64, days: i64) -> i64 {
    t0 + days * 86_400_000
}

fn block_at(id: u64, started_ms: i64, output: &str) -> Block {
    Block {
        id: BlockId(id),
        command: format!("cmd{id}"),
        cwd: None,
        output: output.into(),
        styled_output: None,
        exit_code: Some(0),
        started_at: if started_ms >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_millis(started_ms as u64)
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_millis(started_ms.unsigned_abs())
        },
        finished_at: None,
        collapsed: false,
        screen_origin: false,
    }
}

// ── pure decision truth table (§3.9 acceptance) ─────────────────────

#[test]
fn prune_plan_truth_table() {
    const DAY: i64 = 86_400_000;
    let now = 1_800_000_000_000;
    let mib = 1024 * 1024;

    // Both gates Off → prune must not run at all.
    assert!(prune_plan(now, 0, 0, 0).is_off());
    // Age on only.
    let age_only = prune_plan(now, 0, 90, 0);
    assert_eq!(age_only.age_cutoff_ms, Some(now - 90 * DAY));
    assert!(!age_only.size_gate_on());
    assert!(!age_only.is_off());
    // Size on, under budget → gate configured but not active.
    let under = prune_plan(now, 100 * mib, 0, 512);
    assert_eq!(under.size_budget_bytes, Some(512 * mib));
    assert!(!under.size_gate_active);
    // Size on, over budget → active.
    let over = prune_plan(now, 600 * mib, 0, 512);
    assert!(over.size_gate_active);
    // Both on, over budget → both active.
    let both = prune_plan(now, 600 * mib, 90, 512);
    assert_eq!(both.age_cutoff_ms, Some(now - 90 * DAY));
    assert!(both.size_gate_active);
    // Both on, under budget → only the age gate will delete.
    let both_under = prune_plan(now, 10 * mib, 90, 512);
    assert!(both_under.age_gate_on());
    assert!(!both_under.size_gate_active);
}

// ── delete_before: batch correctness + boundaries ───────────────────

#[test]
fn delete_before_batch_and_boundaries() {
    let path = temp_path("delete-before");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    // started_ms = 1000..5000 (ids 1..5), all distinct.
    for id in 1..=5u64 {
        store
            .insert(&block_at(id, id as i64 * 1000, "out"))
            .unwrap();
    }

    // Empty-table boundary: a cutoff before every row deletes nothing.
    assert_eq!(store.delete_before(500, 500).unwrap(), 0);

    // Partial match with a small batch forces the batch loop: cutoff
    // 3500 keeps ids 4, 5 (and everything ≥ 3500 — strict `<`).
    assert_eq!(store.delete_before(3500, 2).unwrap(), 3);
    let left = store.recent(10).unwrap();
    let mut ids: Vec<u64> = left.iter().map(|b| b.id.0).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![4, 5], "only blocks older than the cutoff go");

    // started_ms EQUAL to the cutoff must NOT be deleted (strict `<`,
    // aligned with older_than's disjoint-page semantics).
    assert_eq!(store.delete_before(4000, 500).unwrap(), 0);
    assert_eq!(store.delete_before(4001, 500).unwrap(), 1);

    // All-match boundary: i64::MAX cutoff minus the equality guard.
    assert_eq!(store.delete_before(i64::MAX, 500).unwrap(), 1);
    assert!(store.recent(10).unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

// ── prune runtime gates ─────────────────────────────────────────────

#[test]
fn prune_age_gate_deletes_only_old_blocks_and_keeps_tabs() {
    let path = temp_path("age-gate");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    let now = ms_at(t0, 400);
    // 3 old (400 days) + 2 recent (10 days).
    for id in 1..=3u64 {
        store.insert(&block_at(id, t0, "old")).unwrap();
    }
    for id in 4..=5u64 {
        store.insert(&block_at(id, ms_at(t0, 370), "new")).unwrap();
    }
    store
        .save_tabs(&[TabSnapshot {
            position: 0,
            active: true,
            cwd: Some("/tmp".into()),
            block_scroll_offset: 3,
            editor_buffer: String::new(),
            shell_phase: "AtPrompt".into(),
            block_ids: vec![1, 4],
        }])
        .unwrap();

    let report = store
        .prune(prune_plan(now, store.page_bytes().unwrap(), 90, 0))
        .unwrap();
    assert_eq!(report.age_deleted, 3);
    assert_eq!(report.size_deleted, 0);
    assert_eq!(report.terminal, PruneTerminal::AgeDone);
    let left: Vec<u64> = store.recent(10).unwrap().iter().map(|b| b.id.0).collect();
    assert_eq!(left.len(), 2, "recent blocks survive the age gate");

    // The tabs table is untouched (§3.9 4/5: orphan ids are filtered on
    // load; the snapshot itself must never be pruned).
    let tabs = store.load_tabs().unwrap();
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].block_ids, vec![1, 4]);
    let _ = std::fs::remove_file(&path);
}

/// "Everything is inside the age window yet the DB is over budget" —
/// the size gate's legal working quadrant (review P1): the age gate
/// deletes nothing, the absolute size gate still deletes oldest-first.
#[test]
fn prune_size_gate_is_absolute_inside_the_age_window() {
    const BUDGET: i64 = 256 * 1024;
    let path = temp_path("size-absolute");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    // 60 blocks x 8 KiB ≈ 480 KiB of output, all 1 day old.
    for id in 1..=60u64 {
        let started = ms_at(t0, 1) - (60 - id as i64) * 1000;
        store
            .insert(&block_at(id, started, &"x".repeat(8 * 1024)))
            .unwrap();
    }
    // 256 KiB budget ≪ the ~480 KiB seeded; age gate 90 days no-ops.
    let plan = PrunePlan {
        age_cutoff_ms: Some(ms_at(t0, 1) - 90 * 86_400_000),
        size_budget_bytes: Some(BUDGET),
        size_gate_active: true,
    };
    let report = store.prune(plan).unwrap();
    assert_eq!(report.age_deleted, 0, "age window covers every block");
    assert!(
        report.size_deleted > 0,
        "size gate deletes oldest-first anyway"
    );
    assert_eq!(report.terminal, PruneTerminal::BudgetMet);
    assert!(
        store.page_bytes().unwrap() <= BUDGET,
        "budget reached after reclamation"
    );
    // Oldest-first: the survivors are the newest ids.
    let left: Vec<u64> = store.recent(100).unwrap().iter().map(|b| b.id.0).collect();
    assert!(left.iter().all(|id| *id > report.size_deleted as u64));
    let _ = std::fs::remove_file(&path);
}

/// The size gate cannot always reach the budget: when the blocks table
/// is drained and the LIVE data still exceeds the budget (here the
/// never-touched `tabs` table holds the bytes), the pass ends in
/// `BudgetUnreachable` — while `tabs` itself stays byte-identical
/// (§3.9 5: the prune never touches that table). A single block larger
/// than the budget, by contrast, is deleted and reported BudgetMet:
/// after reclamation the budget IS met.
#[test]
fn prune_budget_unreachable_when_blocks_drained_but_live_over_budget() {
    let path = temp_path("unreachable");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    store
        .insert(&block_at(1, t0, &"x".repeat(8 * 1024)))
        .unwrap();
    // 2 MiB of tabs data the prune must never touch.
    store
        .save_tabs(&[TabSnapshot {
            position: 0,
            active: true,
            cwd: Some("/tmp".into()),
            block_scroll_offset: 0,
            editor_buffer: "y".repeat(2 * 1024 * 1024),
            shell_phase: "AtPrompt".into(),
            block_ids: vec![1],
        }])
        .unwrap();
    // 1 MiB budget ≪ the ~2 MiB live tabs data.
    let report = store.prune(plan_size(ms_at(t0, 400), 1)).unwrap();
    assert_eq!(
        report.size_deleted, 1,
        "the block row is deleted (oldest-first)"
    );
    assert_eq!(report.terminal, PruneTerminal::BudgetUnreachable);
    assert!(
        report.bytes_after > 1024 * 1024,
        "tabs data keeps the file over budget"
    );
    let tabs = store.load_tabs().unwrap();
    assert_eq!(tabs.len(), 1, "tabs table untouched");
    assert_eq!(tabs[0].editor_buffer.len(), 2 * 1024 * 1024);
    assert!(store.recent(10).unwrap().is_empty(), "blocks drained");
    let _ = std::fs::remove_file(&path);
}

/// A single block larger than the whole budget is deleted (absolute
/// oldest-first); after reclamation the budget IS met — BudgetMet, not
/// Unreachable (the freelist-aware live measurement converges).
#[test]
fn prune_budget_met_after_deleting_single_oversized_block() {
    let path = temp_path("giant-block");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    store
        .insert(&block_at(1, t0, &"x".repeat(3 * 1024 * 1024)))
        .unwrap();
    let report = store.prune(plan_size(ms_at(t0, 400), 1)).unwrap();
    assert_eq!(report.size_deleted, 1, "the giant block is deleted");
    assert_eq!(report.terminal, PruneTerminal::BudgetMet);
    assert!(
        report.bytes_after <= 1024 * 1024,
        "file fits the budget again"
    );
    assert!(store.recent(10).unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn prune_row_cap_hit_stops_the_pass() {
    let path = temp_path("row-cap");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    for id in 1..=10u64 {
        store
            .insert(&block_at(id, t0 + id as i64 * 1000, "out"))
            .unwrap();
    }
    // Injected cap of 4 (production passes PRUNE_ROW_CAP).
    let report = store.prune_with_cap(age_plan(ms_at(t0, 400)), 4).unwrap();
    assert_eq!(report.age_deleted, 4);
    assert_eq!(report.terminal, PruneTerminal::RowCapHit);
    assert_eq!(store.recent(100).unwrap().len(), 6, "cap stops mid-gate");
    // The next window finishes the job.
    let report = store.prune(age_plan(ms_at(t0, 400))).unwrap();
    assert_eq!(report.age_deleted, 6);
    assert_eq!(report.terminal, PruneTerminal::AgeDone);
    let _ = std::fs::remove_file(&path);
}

/// Both gates Off → prune is a measured no-op (bit-identical behavior).
#[test]
fn prune_with_both_gates_off_is_a_noop() {
    let path = temp_path("gates-off");
    let _ = std::fs::remove_file(&path);
    let mut store = BlockStore::open(&path).unwrap();
    let t0 = 1_700_000_000_000;
    store.insert(&block_at(1, t0, "out")).unwrap();
    let report = store.prune(prune_plan(0, 0, 0, 0)).unwrap();
    assert_eq!(report.deleted(), 0);
    assert_eq!(report.terminal, PruneTerminal::AgeDone);
    assert_eq!(store.recent(10).unwrap().len(), 1);
    // run_block_prune does not even open the DB.
    let missing = temp_path("gates-off-missing");
    let _ = std::fs::remove_file(&missing);
    assert!(matches!(run_block_prune(&missing, 0, 0, 0), Ok(None)));
    assert!(!missing.exists(), "no DB is created when gates are Off");
}

// ── migration + page_bytes ──────────────────────────────────────────

/// A legacy NONE-mode DB is converted once (flag + PRAGMA), the pragma
/// persists, and a second open does not re-migrate (idempotency).
#[test]
fn vacuum_migration_is_one_shot_and_idempotent() {
    let path = temp_path("migration");
    let _ = std::fs::remove_file(&path);
    {
        // Legacy DB created WITHOUT the T14 pragma → auto_vacuum=NONE.
        // Full pre-T14 column shape so SCHEMA's index builds succeed.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE blocks (\
                id INTEGER PRIMARY KEY, command TEXT NOT NULL, output TEXT NOT NULL,\
                exit_code INTEGER, started_ms INTEGER NOT NULL, finished_ms INTEGER,\
                collapsed INTEGER NOT NULL DEFAULT 0\
             );",
        )
        .unwrap();
        drop(conn);
    }
    let mut store = BlockStore::open(&path).unwrap();
    assert!(store.needs_vacuum_migration, "NONE mode flags migration");
    store
        .insert(&block_at(1, 1_700_000_000_000, "out"))
        .unwrap();
    let report = store.prune(age_plan(1_800_000_000_000)).unwrap();
    assert!(!store.needs_vacuum_migration, "migration ran inside prune");
    let mode: i64 = store
        .conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2, "INCREMENTAL persisted");
    assert_eq!(report.deleted(), 1); // the old block still went through the gate
    drop(store);

    // Second open re-reads the pragma: no second migration.
    let store = BlockStore::open(&path).unwrap();
    assert!(
        !store.needs_vacuum_migration,
        "pragma value is the persistent truth"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn page_bytes_tracks_a_fresh_store() {
    let path = temp_path("page-bytes");
    let _ = std::fs::remove_file(&path);
    let store = BlockStore::open(&path).unwrap();
    assert!(store.page_bytes().unwrap() > 0);
    let small = store.page_bytes().unwrap();
    drop(store);
    let store = BlockStore::open(&path).unwrap();
    for id in 1..=20u64 {
        store
            .insert(&block_at(id, id as i64 * 1000, &"x".repeat(4096)))
            .unwrap();
    }
    assert!(store.page_bytes().unwrap() > small);
    let _ = std::fs::remove_file(&path);
}

/// Standalone entry: opens its own connection and returns the report.
#[test]
fn run_block_prune_reports_and_keeps_recent() {
    let path = temp_path("run-prune");
    let _ = std::fs::remove_file(&path);
    {
        let store = BlockStore::open(&path).unwrap();
        let t0 = 1_700_000_000_000;
        store.insert(&block_at(1, t0, "old")).unwrap();
        store.insert(&block_at(2, ms_at(t0, 370), "new")).unwrap();
    }
    let report = run_block_prune(&path, 90, 0, ms_at(1_700_000_000_000, 400))
        .unwrap()
        .expect("gates on → Some(report)");
    assert_eq!(report.age_deleted, 1);
    let store = BlockStore::open(&path).unwrap();
    assert_eq!(store.recent(10).unwrap().len(), 1);
    let _ = std::fs::remove_file(&path);
}

// ── end-to-end shrink (§3.9 acceptance, #[ignore]) ──────────────────

/// Seed output + annotations + FTS docs + a tab snapshot, prune, and
/// assert: row counts drop, the FILE actually shrinks, deleted blocks'
/// annotations/FTS docs vanish while kept blocks' survive, and the
/// tabs table is byte-identical.
#[test]
#[ignore = "file-byte + FTS end-to-end; run with: cargo test -p weft_core persistence::prune -- --ignored"]
fn end_to_end_prune_shrinks_file_and_cascades_three_tables() {
    const KIB: usize = 1024;
    let path = temp_path("e2e");
    let _ = std::fs::remove_file(&path);
    let t0 = 1_700_000_000_000;
    let now = ms_at(t0, 400);

    {
        // Startup order mirror: BlockStore first, then sidecars.
        let store = BlockStore::open(&path).unwrap();
        let annotations = AnnotationStore::open(&path).unwrap();
        let index = SearchIndex::open(Connection::open(&path).unwrap()).unwrap();

        // 150 old blocks x 16 KiB output (≈2.4 MiB) + 50 recent.
        for id in 1..=150u64 {
            store
                .insert(&block_at(id, t0 + id as i64 * 1000, &"x".repeat(16 * KIB)))
                .unwrap();
        }
        for id in 151..=200u64 {
            store
                .insert(&block_at(id, ms_at(t0, 370) + id as i64, "recent out"))
                .unwrap();
        }
        // Annotations on one old (deleted) and one recent (kept) block.
        annotations
            .upsert(&BlockAnnotation {
                block_id: BlockId(5),
                bookmarked: true,
                note: Some("old note".into()),
                tags: vec!["old".into()],
                updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            })
            .unwrap();
        annotations
            .upsert(&BlockAnnotation {
                block_id: BlockId(160),
                bookmarked: true,
                note: Some("keep me".into()),
                tags: vec![],
                updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            })
            .unwrap();
        // FTS documents for both blocks (kind=Block, stable_id=decimal).
        index
            .upsert(&SearchDocument::from_block(
                5,
                "old command",
                "oldbody",
                None,
                t0,
            ))
            .unwrap();
        index
            .upsert(&SearchDocument::from_block(
                160,
                "recent command",
                "recentbody",
                None,
                ms_at(t0, 370),
            ))
            .unwrap();
        // A tab snapshot that must survive byte-identical (§3.9 4/5).
        store
            .save_tabs(&[TabSnapshot {
                position: 0,
                active: true,
                cwd: Some("/tmp/e2e".into()),
                block_scroll_offset: 7,
                editor_buffer: String::new(),
                shell_phase: "AtPrompt".into(),
                block_ids: vec![5, 160],
            }])
            .unwrap();
    }
    // Close everything so WAL checkpoints before the byte measurement.
    let bytes_before = std::fs::metadata(&path).unwrap().len();
    let tabs_before = tabs_dump(&path);
    assert!(bytes_before > 1024 * 1024, "seed produced a real file");

    {
        let mut store = BlockStore::open(&path).unwrap();
        let report = store
            .prune(prune_plan(now, store.page_bytes().unwrap(), 90, 0))
            .unwrap();
        assert_eq!(report.age_deleted, 150);
        assert_eq!(report.size_deleted, 0);
        assert_eq!(report.terminal, PruneTerminal::AgeDone);
    }

    // Row counts + cascade survival.
    let store = BlockStore::open(&path).unwrap();
    assert_eq!(store.recent(1000).unwrap().len(), 50);
    let annotations = AnnotationStore::open(&path).unwrap();
    assert!(
        annotations.get(BlockId(5)).unwrap().is_none(),
        "deleted block's annotation is cascaded away"
    );
    let kept = annotations
        .get(BlockId(160))
        .unwrap()
        .expect("kept block's annotation survives");
    assert_eq!(kept.note.as_deref(), Some("keep me"));
    drop(annotations);
    drop(store);

    // FTS: the deleted block's document is gone, the kept one lives.
    let index = SearchIndex::open(Connection::open(&path).unwrap()).unwrap();
    assert_eq!(index.count_kind(SearchDocumentKind::Block).unwrap(), 1);
    assert!(
        index
            .search(&SearchQuery::new("oldbody"))
            .unwrap()
            .is_empty(),
        "deleted block's FTS doc must not dead-hit the Palette"
    );
    let hits = index.search(&SearchQuery::new("recentbody")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc.stable_id, "160");
    drop(index);

    // tabs table byte-identical (§3.9 4: orphan ids filtered on load,
    // the snapshot itself never pruned).
    let tabs_after = tabs_dump(&path);
    assert_eq!(tabs_before, tabs_after, "tabs rows must not change");

    // The file actually shrank (auto_vacuum INCREMENTAL + vacuum).
    let bytes_after = std::fs::metadata(&path).unwrap().len();
    assert!(
        bytes_after < bytes_before,
        "file must shrink: before={bytes_before}, after={bytes_after}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Dump every tabs row value in a canonical order for byte-faithful
/// before/after comparison.
fn tabs_dump(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, position, active, cwd, block_scroll_offset, \
                    editor_buffer, shell_phase, block_ids \
             FROM tabs ORDER BY id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok(format!(
                "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                row.get::<_, i64>(0),
                row.get::<_, i64>(1),
                row.get::<_, i64>(2),
                row.get::<_, Option<String>>(3),
                row.get::<_, i64>(4),
                row.get::<_, Option<String>>(5),
                row.get::<_, Option<String>>(6),
                row.get::<_, Option<String>>(7),
            ))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

// ── tiny plan helpers for the runtime tests ─────────────────────────

fn age_plan(now_ms: i64) -> PrunePlan {
    prune_plan(now_ms, 0, 90, 0)
}

fn plan_size(now_ms: i64, max_mb: u32) -> PrunePlan {
    prune_plan(now_ms, i64::MAX, 0, max_mb)
}
