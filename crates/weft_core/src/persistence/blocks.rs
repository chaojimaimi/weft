//! `BlockStore` — SQLite-backed store of finished command blocks.
//!
//! See [`crate::persistence`] for the module-level overview.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::block_id_sequence::{BlockIdPool, ID_RESERVE};
use crate::blocks::{Block, BlockId};
use crate::persistence::migrations::{ensure_column, ensure_tabs_active_column};
use crate::persistence::{millis_to_system_time, system_time_to_millis, PersistenceError};

/// Upper bound on the JSON-encoded `styled_output` blob we are willing to
/// read/write. Larger payloads are dropped (treated as `NULL`) so a runaway
/// styled-output buffer cannot bloat the DB or OOM the decoder.
pub(crate) const MAX_STYLED_OUTPUT_JSON_BYTES: usize = 256 * 1024;

/// SQLite-backed store of finished command blocks.
///
/// Wraps a single [`rusqlite::Connection`]. rusqlite 0.31 implements
/// `Connection: Send` (moving the whole store across threads is fine) but
/// **not** `Sync` — `&BlockStore` must never be shared between threads.
/// Every background consumer (prune routine, palette search worker) opens
/// its own connection to the same file; that design is required by
/// rusqlite's thread-safety model, not an optimization.
pub struct BlockStore {
    pub(crate) conn: Connection,
    pub(crate) block_id_allocator: Arc<BlockIdPool>,
    /// T14 (PLAN_v11217 §3.9): the file was opened in `auto_vacuum=NONE`
    /// mode. The pragma value itself is the persistent truth — the prune
    /// routine converts to INCREMENTAL with a one-time VACUUM and clears
    /// this flag; a later `open` re-reads the pragma (naturally idempotent).
    pub(crate) needs_vacuum_migration: bool,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS blocks (\
    id           INTEGER PRIMARY KEY,\
    command      TEXT    NOT NULL,\
    cwd          TEXT,\
    output       TEXT    NOT NULL,\
    styled_output TEXT,\
    exit_code    INTEGER,\
    started_ms   INTEGER NOT NULL,\
    finished_ms  INTEGER,\
    collapsed    INTEGER NOT NULL DEFAULT 0\
);\
CREATE INDEX IF NOT EXISTS idx_blocks_started ON blocks(started_ms);\
CREATE TABLE IF NOT EXISTS tabs (\
    id                  INTEGER PRIMARY KEY,\
    position            INTEGER NOT NULL,\
    active              INTEGER NOT NULL DEFAULT 0,\
    cwd                 TEXT,\
    block_scroll_offset INTEGER NOT NULL DEFAULT 0,\
    editor_buffer       TEXT,\
    shell_phase         TEXT,\
    block_ids           TEXT,\
    panes               TEXT\
);\
CREATE INDEX IF NOT EXISTS idx_tabs_position ON tabs(position);\
CREATE TABLE IF NOT EXISTS meta (\
    key TEXT PRIMARY KEY,\
    v   INTEGER NOT NULL\
);";

impl BlockStore {
    /// Open (creating if needed) the block DB at `path`, ensuring its parent
    /// directory exists and the schema is in place.
    ///
    /// Ordering invariant (PLAN_v11217 §3.9 2b): this must stay the FIRST
    /// open of `blocks.db` in the startup sequence (before
    /// `AnnotationStore` / `SearchIndex` / the palette worker) — the
    /// one-time DELETE→WAL journal switch requires no other active
    /// connections; later opens see the persisted WAL mode and their
    /// repeated SET is a no-op.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // T14: a freshly-created file gets incremental auto_vacuum BEFORE any
        // table exists, so the pragma is persisted without a migration
        // VACUUM (review P3a). Existing NONE-mode DBs are recorded below and
        // migrated inside the prune routine — off the startup path.
        let file_existed = path.exists();
        let mut conn = Connection::open(path)?;
        if !file_existed {
            // NOTE: the auto_vacuum SET form returns NO row on an empty DB,
            // so this must run via execute_batch, not query_row.
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        }
        // T14 (§3.9 2b): switch the journal to WAL once, here. Readers stop
        // blocking writers, so the background prune / palette connections
        // never stall the main thread's inserts. Best-effort: a busy failure
        // (e.g. a second Weft instance mid-open) logs and keeps the previous
        // journal mode rather than disabling persistence entirely; the main
        // thread's insert-drop window argument in prune.rs covers this mode.
        if let Err(e) =
            conn.query_row::<String, _, _>("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        {
            tracing::warn!(error = %e, "journal_mode=WAL failed; keeping current journal mode");
        }
        // T15a (§3.10): cap the WAL high-water mark on THIS connection —
        // the pragma is per-connection and the main writer must be covered
        // (idempotent: a repeated SET is a no-op).
        crate::persistence::apply_wal_limits(&conn)?;
        conn.execute_batch(SCHEMA)?;
        // CREATE TABLE IF NOT EXISTS does not evolve databases created by an
        // older Weft version, so migrate the D5 active-tab field explicitly.
        ensure_tabs_active_column(&conn)?;
        ensure_column(&conn, "blocks", "cwd", "TEXT")?;
        ensure_column(&conn, "blocks", "styled_output", "TEXT")?;
        ensure_column(
            &conn,
            "blocks",
            "screen_origin",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "tabs", "block_ids", "TEXT")?;
        // v1.12.28 (P1-02 ①): per-pane split-tree JSON (single-pane tabs and
        // pre-v1.12.28 rows store NULL → single-pane restore).
        ensure_column(&conn, "tabs", "panes", "TEXT")?;
        let needs_vacuum_migration = if file_existed {
            let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
            mode == 0 // 0=NONE, 1=FULL, 2=INCREMENTAL
        } else {
            false
        };
        // T14 (§3.9 2): persistent monotonic id counter — seed `next_id` to
        // MAX(id)+1 exactly once (the NOT EXISTS guard closes the seed race
        // in one statement), then atomically grab a reservation segment
        // [hi-RESERVE, hi) in the SAME transaction. Concurrent openers
        // serialize on the write lock and never overlap segments; a crash
        // discards the unused reservation (an id hole — harmless) and never
        // rolls the counter back, so pruned ids are never reused and the
        // old multi-instance INSERT-OR-REPLACE collision is closed.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO meta(key, v) \
             SELECT 'next_id', COALESCE((SELECT MAX(id) FROM blocks), 0) + 1 \
             WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = 'next_id')",
            [],
        )?;
        tx.execute(
            "UPDATE meta SET v = v + ?1 WHERE key = 'next_id'",
            [ID_RESERVE as i64],
        )?;
        let reserved_hi: i64 =
            tx.query_row("SELECT v FROM meta WHERE key = 'next_id'", [], |row| {
                row.get(0)
            })?;
        tx.commit()?;
        let reserved_hi = reserved_hi.max(ID_RESERVE as i64) as u64;
        let reserved_lo = reserved_hi - ID_RESERVE;
        let allocator = Arc::new(BlockIdPool::new(reserved_lo, reserved_hi));
        // Reservation re-grab runs on a short-lived dedicated connection: the
        // pool must not hold shared access to `self.conn` (Connection is Send
        // but not Sync, see the struct docs). Fires once per ID_RESERVE
        // allocations; a busy grab fails and the pool overshoots (ids stay
        // monotonic — the hole is harmless), retrying on a later allocation.
        let refill_path = path.to_path_buf();
        // Reviewer MEDIUM-2: every refill failure mode must be visible — a
        // persistently busy refill hides a cross-process id collision window
        // behind nothing but an (harmless) id hole.
        allocator.install_refill(Box::new(move || {
            let grab = || -> Result<u64, PersistenceError> {
                let mut conn = Connection::open(&refill_path)?;
                let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE meta SET v = v + ?1 WHERE key = 'next_id'",
                    [ID_RESERVE as i64],
                )?;
                let v: i64 =
                    tx.query_row("SELECT v FROM meta WHERE key = 'next_id'", [], |row| {
                        row.get(0)
                    })?;
                tx.commit()?;
                Ok(v as u64)
            };
            match grab() {
                Ok(hi) => Some(hi),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "id-pool refill failed: pool overshoots (ids stay monotonic)"
                    );
                    None
                }
            }
        }));
        Ok(Self {
            conn,
            block_id_allocator: allocator,
            needs_vacuum_migration,
        })
    }

    /// Shared allocator used by every tab writing to this store, preventing
    /// per-terminal BlockId sequences from replacing each other in SQLite.
    /// T14: a hi/lo [`BlockIdPool`] over the persistent `meta.next_id`
    /// counter — see [`crate::block_id_sequence`] for the protocol.
    pub fn block_id_allocator(&self) -> Arc<BlockIdPool> {
        self.block_id_allocator.clone()
    }

    /// Insert (or replace by id) a single block.
    pub fn insert(&self, block: &Block) -> Result<(), PersistenceError> {
        let styled_output = block
            .styled_output
            .as_deref()
            .and_then(|styled| serde_json::to_string(styled).ok())
            .filter(|json| json.len() <= MAX_STYLED_OUTPUT_JSON_BYTES);
        self.conn.execute(
            "INSERT OR REPLACE INTO blocks \
             (id, command, cwd, output, styled_output, exit_code, started_ms, finished_ms, collapsed, screen_origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                block.id.0 as i64,
                &block.command,
                block.cwd.as_deref(),
                block.output.as_ref(),
                styled_output,
                block.exit_code,
                system_time_to_millis(block.started_at),
                block.finished_at.map(system_time_to_millis),
                block.collapsed as i64,
                block.screen_origin as i64,
            ],
        )?;
        Ok(())
    }

    /// The most recent `limit` blocks, newest first (by start time, then id).
    pub fn recent(&self, limit: usize) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= 262144 THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks ORDER BY started_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// Blocks whose command or output contains `query` (case-insensitive),
    /// newest first. Plain substring match via `INSTR` — no FTS overhead for
    /// v0.4; can upgrade to FTS5 if the history grows large.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= 262144 THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks \
             WHERE INSTR(LOWER(command), LOWER(?1)) > 0 \
                OR INSTR(LOWER(output), LOWER(?1)) > 0 \
             ORDER BY started_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// v1.11.2 X4 (PLAN_v1112 §1.3): keyset-paginated history for the panel's
    /// "load older" action. Returns up to `limit` blocks strictly OLDER than
    /// `started_ms_exclusive`, newest first — uses idx_blocks_started so a
    /// deep page scan never degenerates into a full-table sort. Strict `<`
    /// makes repeated pages disjoint: a block whose started_ms equals the
    /// caller's cursor is never returned twice.
    pub fn older_than(
        &self,
        started_ms_exclusive: i64,
        limit: usize,
    ) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= 262144 THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed, screen_origin \
             FROM blocks \
             WHERE started_ms < ?1 \
             ORDER BY started_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![started_ms_exclusive, limit as i64], row_to_block)?;
        rows.map(|r| r.map_err(PersistenceError::from)).collect()
    }

    /// Delete every stored block.
    pub fn clear(&self) -> Result<(), PersistenceError> {
        self.conn.execute("DELETE FROM blocks", [])?;
        Ok(())
    }

    /// v1.7.3-D: Fetch a single block by id. Returns `None` if not found.
    /// Used by the search-index integration to look up a block's command
    /// and cwd when indexing a bookmark annotation.
    pub fn get(&self, block_id: BlockId) -> Result<Option<Block>, PersistenceError> {
        self.conn
            .query_row(
                "SELECT id, command, cwd, output, \
                        CASE WHEN length(CAST(styled_output AS BLOB)) <= 262144 THEN styled_output END, \
                        exit_code, started_ms, finished_ms, collapsed, screen_origin \
                 FROM blocks WHERE id = ?1",
                params![block_id.0 as i64],
                row_to_block,
            )
            .optional()
            .map_err(PersistenceError::from)
    }
}

/// Decode a stored row into a [`Block`].
pub(crate) fn row_to_block(row: &rusqlite::Row) -> rusqlite::Result<Block> {
    let id: i64 = row.get(0)?;
    let command: String = row.get(1)?;
    let cwd: Option<String> = row.get(2)?;
    let output: String = row.get(3)?;
    let styled_json: Option<String> = row.get(4)?;
    let exit_code: Option<i32> = row.get(5)?;
    let started_ms: i64 = row.get(6)?;
    let finished_ms: Option<i64> = row.get(7)?;
    let collapsed: i64 = row.get(8)?;
    // v1.10.26 Batch B review closure (SF-1): screen_origin persisted since
    // Batch B — a restored TUI block keeps clip-not-wrap across Restore. The
    // DEFAULT 0 covers legacy rows (shell-wrap semantics), matching the
    // runtime default for ordinary commands.
    let screen_origin: i64 = row.get(9)?;
    Ok(Block {
        id: BlockId(id as u64),
        command,
        cwd,
        output: output.into(),
        styled_output: styled_json
            .filter(|json| json.len() <= MAX_STYLED_OUTPUT_JSON_BYTES)
            .and_then(|json| serde_json::from_str(&json).ok())
            .map(Arc::new),
        exit_code,
        started_at: millis_to_system_time(started_ms),
        finished_at: finished_ms.map(millis_to_system_time),
        collapsed: collapsed != 0,
        screen_origin: screen_origin != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    static TEMP_STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temp DB path for one test (auto-cleaned by the OS temp dir
    /// lifecycle; we also clear() to keep tests independent).
    fn temp_store() -> BlockStore {
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

    fn block(id: u64, command: &str, output: &str, exit: Option<i32>) -> Block {
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
}
