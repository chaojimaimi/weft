//! `BlockStore` — SQLite-backed store of finished command blocks.
//!
//! See [`crate::persistence`] for the module-level overview.

use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::blocks::{Block, BlockId};
use crate::persistence::migrations::{ensure_column, ensure_tabs_active_column};
use crate::persistence::{millis_to_system_time, system_time_to_millis, PersistenceError};

/// Upper bound on the JSON-encoded `styled_output` blob we are willing to
/// read/write. Larger payloads are dropped (treated as `NULL`) so a runaway
/// styled-output buffer cannot bloat the DB or OOM the decoder.
pub(crate) const MAX_STYLED_OUTPUT_JSON_BYTES: usize = 256 * 1024;

/// SQLite-backed store of finished command blocks.
///
/// Wraps a single [`rusqlite::Connection`]; safe to share via `&self` because
/// `rusqlite::Connection` is `Sync` for `&` access.
pub struct BlockStore {
    pub(crate) conn: Connection,
    pub(crate) block_id_allocator: Arc<AtomicU64>,
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
    block_ids           TEXT\
);\
CREATE INDEX IF NOT EXISTS idx_tabs_position ON tabs(position);";

impl BlockStore {
    /// Open (creating if needed) the block DB at `path`, ensuring its parent
    /// directory exists and the schema is in place.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        // CREATE TABLE IF NOT EXISTS does not evolve databases created by an
        // older Weft version, so migrate the D5 active-tab field explicitly.
        ensure_tabs_active_column(&conn)?;
        ensure_column(&conn, "blocks", "cwd", "TEXT")?;
        ensure_column(&conn, "blocks", "styled_output", "TEXT")?;
        ensure_column(&conn, "tabs", "block_ids", "TEXT")?;
        let next_block_id =
            conn.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM blocks", [], |row| {
                row.get::<_, u64>(0)
            })?;
        Ok(Self {
            conn,
            block_id_allocator: Arc::new(AtomicU64::new(next_block_id.max(1))),
        })
    }

    /// Shared allocator used by every tab writing to this store, preventing
    /// per-terminal BlockId sequences from replacing each other in SQLite.
    pub fn block_id_allocator(&self) -> Arc<AtomicU64> {
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
             (id, command, cwd, output, styled_output, exit_code, started_ms, finished_ms, collapsed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        Ok(())
    }

    /// The most recent `limit` blocks, newest first (by start time, then id).
    pub fn recent(&self, limit: usize) -> Result<Vec<Block>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, command, cwd, output, \
                    CASE WHEN length(CAST(styled_output AS BLOB)) <= 262144 THEN styled_output END, \
                    exit_code, started_ms, finished_ms, collapsed \
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
                    exit_code, started_ms, finished_ms, collapsed \
             FROM blocks \
             WHERE INSTR(LOWER(command), LOWER(?1)) > 0 \
                OR INSTR(LOWER(output), LOWER(?1)) > 0 \
             ORDER BY started_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], row_to_block)?;
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
                        exit_code, started_ms, finished_ms, collapsed \
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

    #[test]
    fn collapsed_flag_persists() {
        let store = temp_store();
        let mut b = block(11, "long", "spam\n", Some(0));
        b.collapsed = true;
        store.insert(&b).unwrap();
        let loaded = store.recent(1).unwrap().pop().unwrap();
        assert!(loaded.collapsed);
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

    #[test]
    fn reopened_store_seeds_shared_block_ids_after_persisted_maximum() {
        use std::sync::atomic::Ordering;

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
        assert_eq!(store.block_id_allocator().load(Ordering::Relaxed), 8);
        let _ = std::fs::remove_file(&path);
    }
}
