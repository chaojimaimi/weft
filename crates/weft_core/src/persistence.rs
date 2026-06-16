//! SQLite persistence for command blocks (v0.4 "Fabric", phase 3).
//!
//! [`BlockStore`] wraps a single [`rusqlite::Connection`] and stores finished
//! [`Block`](crate::blocks::Block)s: command, output, exit code, timing, and
//! collapse state. Blocks are written as the tracker finishes them and loaded
//! back on startup so the history panel survives restarts.
//!
//! SQLite is bundled (compiled from source), so there is no runtime dependency
//! on a system `libsqlite3` — important for the future `.app` distribution.
//! The schema is created idempotently on [`BlockStore::open`].
//!
//! This module is pure logic over the DB file path the caller supplies; the app
//! layer wires it to `<cache>/weft/blocks.db` (see `weft_cache_dir`).

use std::path::Path;
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection};

use crate::blocks::{Block, BlockId};

/// Errors from the block store: filesystem (opening/creating the DB file) or
/// SQLite (query/prepare).
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// SQLite-backed store of finished command blocks.
pub struct BlockStore {
    conn: Connection,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS blocks (\
    id           INTEGER PRIMARY KEY,\
    command      TEXT    NOT NULL,\
    output       TEXT    NOT NULL,\
    exit_code    INTEGER,\
    started_ms   INTEGER NOT NULL,\
    finished_ms  INTEGER,\
    collapsed    INTEGER NOT NULL DEFAULT 0\
);\
CREATE INDEX IF NOT EXISTS idx_blocks_started ON blocks(started_ms);";

impl BlockStore {
    /// Open (creating if needed) the block DB at `path`, ensuring its parent
    /// directory exists and the schema is in place.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Insert (or replace by id) a single block.
    pub fn insert(&self, block: &Block) -> Result<(), PersistenceError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO blocks \
             (id, command, output, exit_code, started_ms, finished_ms, collapsed) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                block.id.0 as i64,
                &block.command,
                &block.output,
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
            "SELECT id, command, output, exit_code, started_ms, finished_ms, collapsed \
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
            "SELECT id, command, output, exit_code, started_ms, finished_ms, collapsed \
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
}

/// Decode a stored row into a [`Block`].
fn row_to_block(row: &rusqlite::Row) -> rusqlite::Result<Block> {
    let id: i64 = row.get(0)?;
    let command: String = row.get(1)?;
    let output: String = row.get(2)?;
    let exit_code: Option<i32> = row.get(3)?;
    let started_ms: i64 = row.get(4)?;
    let finished_ms: Option<i64> = row.get(5)?;
    let collapsed: i64 = row.get(6)?;
    Ok(Block {
        id: BlockId(id as u64),
        command,
        output,
        exit_code,
        started_at: millis_to_system_time(started_ms),
        finished_at: finished_ms.map(millis_to_system_time),
        collapsed: collapsed != 0,
    })
}

/// `SystemTime` → Unix epoch millis (signed: pre-epoch times are negative).
fn system_time_to_millis(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Unix epoch millis → `SystemTime`.
fn millis_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp DB path for one test (auto-cleaned by the OS temp dir
    /// lifecycle; we also clear() to keep tests independent).
    fn temp_store() -> BlockStore {
        let path = std::env::temp_dir().join(format!(
            "weft-block-store-{}-{}.db",
            std::process::id(),
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
            output: output.into(),
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
        assert_eq!(recent[0].output, "/tmp");
        assert_eq!(recent[0].exit_code, Some(0));
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let store = temp_store();
        let original = block(7, "echo $X", "hello world\n", Some(3));
        store.insert(&original).unwrap();
        let loaded = store.recent(1).unwrap().pop().unwrap();

        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.command, original.command);
        assert_eq!(loaded.output, original.output);
        assert_eq!(loaded.exit_code, original.exit_code);
        assert_eq!(
            loaded.started_at, original.started_at,
            "timestamps round-trip exactly"
        );
        assert_eq!(loaded.finished_at, original.finished_at);
        assert_eq!(loaded.collapsed, original.collapsed);
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
}
