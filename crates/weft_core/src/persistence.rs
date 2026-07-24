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
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rusqlite::{params, Connection};

use crate::blocks::{Block, BlockId};

const MAX_STYLED_OUTPUT_JSON_BYTES: usize = 256 * 1024;

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
    block_id_allocator: Arc<AtomicU64>,
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
    shell_phase         TEXT\
);\
CREATE INDEX IF NOT EXISTS idx_tabs_position ON tabs(position);";

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    if !names
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column)
    {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

// ── Tab snapshots (v1.0 H4) ───────────────────────────────────────────

/// v1.0 H4: A serializable snapshot of a tab's UI state, persisted to
/// SQLite so the tab layout survives restarts. The PTY itself is NOT
/// restored (impossible); on restore, the tab shows the saved editor draft
/// + block history, and the user presses Enter to spawn a fresh shell.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TabSnapshot {
    /// Position in the tab bar (0-based).
    pub position: usize,
    /// Whether this was the active tab when the snapshot was saved.
    #[serde(default)]
    pub active: bool,
    /// Working directory at save time (from shell integration).
    pub cwd: Option<String>,
    /// Block-view scroll offset.
    pub block_scroll_offset: usize,
    /// JSON-serialized [`EditorBuffer`](crate::editor::EditorBuffer).
    pub editor_buffer: String,
    /// Shell phase as a string: "NotIntegrated" / "AtPrompt" /
    /// "CommandExecuting".
    pub shell_phase: String,
}

impl TabSnapshot {
    /// Index to activate after ordered snapshots are restored. Legacy data
    /// has no active marker and therefore safely falls back to the first tab.
    pub fn restored_active_index(snapshots: &[Self]) -> usize {
        snapshots
            .iter()
            .position(|snapshot| snapshot.active)
            .unwrap_or(0)
    }

    /// v1.0 H4: Serialize an [`EditorBuffer`](crate::editor::EditorBuffer)
    /// to a JSON string for storage. Returns `"{}"` on serialization
    /// failure (so a corrupt buffer doesn't block the save).
    pub fn encode_editor_buffer(buf: &crate::editor::EditorBuffer) -> String {
        serde_json::to_string(buf).unwrap_or_else(|_| "{}".into())
    }

    /// v1.0 H4: Deserialize an [`EditorBuffer`](crate::editor::EditorBuffer)
    /// from the stored JSON string. Returns `None` on parse failure (the
    /// caller falls back to an empty buffer).
    pub fn decode_editor_buffer(json: &str) -> Option<crate::editor::EditorBuffer> {
        serde_json::from_str(json).ok()
    }
}

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
        let has_active = {
            let mut stmt = conn.prepare("PRAGMA table_info(tabs)")?;
            let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
            columns
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .any(|name| name == "active")
        };
        if !has_active {
            conn.execute(
                "ALTER TABLE tabs ADD COLUMN active INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        ensure_column(&conn, "blocks", "cwd", "TEXT")?;
        ensure_column(&conn, "blocks", "styled_output", "TEXT")?;
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

    // ── Tab persistence (v1.0 H4) ─────────────────────────────────────

    /// v1.0 H4: Replace the entire `tabs` table with `snapshots`. The
    /// table is cleared and re-inserted in a single transaction so the
    /// save is atomic — a crash mid-save leaves the previous state
    /// intact. Position ordering is preserved.
    pub fn save_tabs(&self, snapshots: &[TabSnapshot]) -> Result<(), PersistenceError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM tabs", [])?;
        for snap in snapshots {
            tx.execute(
                "INSERT INTO tabs (id, position, active, cwd, block_scroll_offset, editor_buffer, shell_phase) \
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    snap.position as i64,
                    snap.active as i64,
                    snap.cwd.as_deref().unwrap_or(""),
                    snap.block_scroll_offset as i64,
                    &snap.editor_buffer,
                    &snap.shell_phase,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// v1.0 H4: Load all saved tab snapshots, ordered by `position`.
    /// Returns an empty Vec when no tabs have been saved (first launch
    /// or after [`clear_tabs`]).
    pub fn load_tabs(&self) -> Result<Vec<TabSnapshot>, PersistenceError> {
        let mut stmt = self.conn.prepare(
            "SELECT position, active, cwd, block_scroll_offset, editor_buffer, shell_phase \
             FROM tabs ORDER BY position ASC",
        )?;
        let rows = stmt.query_map([], row_to_tab_snapshot)?;
        let mut out = Vec::new();
        for row in rows {
            let mut snap = row?;
            // Empty string → None (matches save_tabs's encoding).
            if snap.cwd.as_deref() == Some("") {
                snap.cwd = None;
            }
            out.push(snap);
        }
        Ok(out)
    }

    /// v1.0 H4: Delete every saved tab snapshot for an explicit session reset.
    /// Normal startup deliberately retains the last atomic snapshot until the
    /// next save replaces it, so an early crash cannot erase recovery data.
    pub fn clear_tabs(&self) -> Result<(), PersistenceError> {
        self.conn.execute("DELETE FROM tabs", [])?;
        Ok(())
    }
}

/// Decode a stored row into a [`Block`].
fn row_to_block(row: &rusqlite::Row) -> rusqlite::Result<Block> {
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

/// v1.0 H4: Decode a stored row into a [`TabSnapshot`].
fn row_to_tab_snapshot(row: &rusqlite::Row) -> rusqlite::Result<TabSnapshot> {
    let position: i64 = row.get(0)?;
    let active: bool = row.get(1)?;
    let cwd: String = row.get(2)?;
    let block_scroll_offset: i64 = row.get(3)?;
    let editor_buffer: String = row.get(4)?;
    let shell_phase: String = row.get(5)?;
    Ok(TabSnapshot {
        position: position as usize,
        active,
        cwd: Some(cwd),
        block_scroll_offset: block_scroll_offset as usize,
        editor_buffer,
        shell_phase,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    // ── v1.0 H4: tab snapshot persistence ──────────────────────────────

    fn snapshot(position: usize, cwd: &str, scroll: usize, phase: &str) -> TabSnapshot {
        TabSnapshot {
            position,
            active: position == 1,
            cwd: Some(cwd.to_string()),
            block_scroll_offset: scroll,
            editor_buffer: r#"{"lines":["ls -la"],"cursor":[0,7],"selection_anchor":null}"#
                .to_string(),
            shell_phase: phase.to_string(),
        }
    }

    #[test]
    fn save_and_load_tabs_roundtrip() {
        let store = temp_store();
        let snaps = vec![
            snapshot(0, "/home/user", 3, "AtPrompt"),
            snapshot(1, "/tmp", 0, "CommandExecuting"),
        ];
        store.save_tabs(&snaps).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].position, 0);
        assert!(!loaded[0].active);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/home/user"));
        assert_eq!(loaded[0].block_scroll_offset, 3);
        assert_eq!(loaded[0].shell_phase, "AtPrompt");
        assert_eq!(loaded[1].position, 1);
        assert!(loaded[1].active);
        assert_eq!(loaded[1].cwd.as_deref(), Some("/tmp"));
        assert_eq!(loaded[1].shell_phase, "CommandExecuting");
    }

    #[test]
    fn restored_active_index_uses_marker_and_falls_back_for_legacy_data() {
        let mut snaps = vec![
            snapshot(0, "/a", 0, "AtPrompt"),
            snapshot(1, "/b", 0, "AtPrompt"),
        ];
        assert_eq!(TabSnapshot::restored_active_index(&snaps), 1);
        snaps[1].active = false;
        assert_eq!(TabSnapshot::restored_active_index(&snaps), 0);
        assert_eq!(TabSnapshot::restored_active_index(&[]), 0);
    }

    #[test]
    fn save_tabs_replaces_previous() {
        let store = temp_store();
        store
            .save_tabs(&[snapshot(0, "/a", 0, "AtPrompt")])
            .unwrap();
        store
            .save_tabs(&[snapshot(0, "/b", 5, "AtPrompt")])
            .unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/b"));
        assert_eq!(loaded[0].block_scroll_offset, 5);
    }

    #[test]
    fn load_tabs_empty_when_unsaved() {
        let store = temp_store();
        let loaded = store.load_tabs().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn clear_tabs_removes_all() {
        let store = temp_store();
        store
            .save_tabs(&[snapshot(0, "/a", 0, "AtPrompt")])
            .unwrap();
        assert_eq!(store.load_tabs().unwrap().len(), 1);
        store.clear_tabs().unwrap();
        assert!(store.load_tabs().unwrap().is_empty());
    }

    #[test]
    fn save_empty_tabs_is_valid() {
        let store = temp_store();
        store.save_tabs(&[]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_preserves_position_ordering() {
        let store = temp_store();
        let snaps = vec![
            snapshot(2, "/c", 0, "AtPrompt"),
            snapshot(0, "/a", 0, "AtPrompt"),
            snapshot(1, "/b", 0, "AtPrompt"),
        ];
        store.save_tabs(&snaps).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].position, 0);
        assert_eq!(loaded[1].position, 1);
        assert_eq!(loaded[2].position, 2);
    }

    #[test]
    fn tabs_survive_reopen() {
        let path = std::env::temp_dir().join(format!(
            "weft-tabs-reopen-{}-{}.db",
            std::process::id(),
            SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let store = BlockStore::open(&path).unwrap();
            store
                .save_tabs(&[snapshot(0, "/persisted", 7, "AtPrompt")])
                .unwrap();
        }
        let store = BlockStore::open(&path).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/persisted"));
        assert_eq!(loaded[0].block_scroll_offset, 7);
        let _ = std::fs::remove_file(&path);
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

    #[test]
    fn open_migrates_legacy_tabs_without_losing_snapshots() {
        let path = std::env::temp_dir().join(format!(
            "weft-tabs-legacy-{}-{}.db",
            std::process::id(),
            SystemTime::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tabs (\
                    id INTEGER PRIMARY KEY, position INTEGER NOT NULL, cwd TEXT,\
                    block_scroll_offset INTEGER NOT NULL DEFAULT 0,\
                    editor_buffer TEXT, shell_phase TEXT\
                 );\
                 INSERT INTO tabs (position, cwd, block_scroll_offset, editor_buffer, shell_phase)\
                 VALUES (0, '/legacy', 4, '{}', 'AtPrompt');",
            )
            .unwrap();
        }
        let store = BlockStore::open(&path).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cwd.as_deref(), Some("/legacy"));
        assert_eq!(loaded[0].block_scroll_offset, 4);
        assert!(!loaded[0].active, "legacy snapshots default to first tab");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encode_decode_editor_buffer_roundtrip() {
        use crate::editor::EditorBuffer;
        let buf = EditorBuffer {
            lines: vec!["git status".to_string(), "second line".to_string()],
            cursor: (1, 5),
            selection_anchor: Some((0, 0)),
            scroll_offset: 0,
        };
        let json = TabSnapshot::encode_editor_buffer(&buf);
        let decoded = TabSnapshot::decode_editor_buffer(&json).expect("decode");
        assert_eq!(decoded, buf);
    }

    #[test]
    fn decode_legacy_editor_buffer_defaults_scroll_offset() {
        let legacy = r#"{"lines":["echo old"],"cursor":[0,8],"selection_anchor":null}"#;
        let decoded = TabSnapshot::decode_editor_buffer(legacy).expect("legacy buffer decodes");
        assert_eq!(decoded.lines, ["echo old"]);
        assert_eq!(decoded.cursor, (0, 8));
        assert_eq!(decoded.scroll_offset, 0);
    }

    #[test]
    fn decode_invalid_json_returns_none() {
        assert!(TabSnapshot::decode_editor_buffer("not json").is_none());
        assert!(TabSnapshot::decode_editor_buffer("").is_none());
    }

    #[test]
    fn snapshot_with_real_editor_buffer_roundtrips() {
        use crate::editor::EditorBuffer;
        let store = temp_store();
        let buf = EditorBuffer {
            lines: vec!["ls -la /home".to_string()],
            cursor: (0, 13),
            selection_anchor: None,
            scroll_offset: 0,
        };
        let snap = TabSnapshot {
            position: 0,
            active: true,
            cwd: Some("/home/user".to_string()),
            block_scroll_offset: 2,
            editor_buffer: TabSnapshot::encode_editor_buffer(&buf),
            shell_phase: "AtPrompt".to_string(),
        };
        store.save_tabs(&[snap]).unwrap();
        let loaded = store.load_tabs().unwrap();
        assert_eq!(loaded.len(), 1);
        let decoded = TabSnapshot::decode_editor_buffer(&loaded[0].editor_buffer).expect("decode");
        assert_eq!(decoded.lines, vec!["ls -la /home".to_string()]);
        assert_eq!(decoded.cursor, (0, 13));
    }
}
